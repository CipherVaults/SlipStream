use crate::TorrentError;
use slipstream_protocol::{
    is_private_or_loopback_ip, sanitize_filename, validate_magnet_uri, UrlSecurityError,
};
use std::{collections::BTreeSet, net::IpAddr, time::Duration};
use tokio::{net::lookup_host, time::timeout};
use url::{Host, Url};

const MAX_MAGNET_TRACKERS: usize = 8;
const TRACKER_DNS_TIMEOUT: Duration = Duration::from_secs(2);

// `so` is peer/user input. Bound expansion independently of the index values.
const MAX_SELECTED_INDICES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMagnet {
    pub info_hash_hex: String,
    pub is_v2: bool,
    /// A hybrid URI may advertise both hashes. The v1 hash can be used with librqbit.
    pub info_hash_v1_hex: Option<String>,
    pub info_hash_v2_hex: Option<String>,
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub exact_length: Option<u64>,
    pub select_only: Option<Vec<usize>>,
    pub exact_sources: Vec<String>,
}

impl ParsedMagnet {
    pub fn safe_display_name(&self) -> String {
        self.display_name
            .as_deref()
            .and_then(|name| sanitize_filename(name).ok())
            .unwrap_or_else(|| self.info_hash_hex.clone())
    }

    pub(crate) fn v1_uri(&self, trackers: &[String]) -> Option<String> {
        let hash = self.info_hash_v1_hex.as_ref()?;
        let mut uri = Url::parse("magnet:?").expect("constant magnet URI");
        uri.query_pairs_mut()
            .append_pair("xt", &format!("urn:btih:{hash}"));
        for tracker in trackers {
            uri.query_pairs_mut().append_pair("tr", tracker);
        }
        Some(uri.into())
    }

    pub(crate) async fn resolve_trackers(
        &self,
        udp_only: bool,
        allow_loopback: bool,
    ) -> Vec<String> {
        let mut urls = Vec::new();
        for tracker in &self.trackers {
            if let Some(url) = resolve_tracker(tracker, udp_only, allow_loopback)
                .await
                .filter(|url| !urls.contains(url))
            {
                urls.push(url);
            }
        }
        urls
    }
}

pub fn parse_magnet_uri(raw: &str) -> Result<ParsedMagnet, TorrentError> {
    parse_magnet_uri_with_loopback(raw, false)
}

pub(crate) fn parse_magnet_uri_with_loopback(
    raw: &str,
    allow_loopback: bool,
) -> Result<ParsedMagnet, TorrentError> {
    let verified = validate_magnet_uri(raw)?;
    let uri = Url::parse(&verified.raw).map_err(|_| UrlSecurityError::InvalidMagnet)?;
    let mut magnet = ParsedMagnet {
        info_hash_hex: String::new(),
        is_v2: false,
        info_hash_v1_hex: None,
        info_hash_v2_hex: None,
        display_name: verified.display_name,
        trackers: Vec::new(),
        exact_length: None,
        select_only: None,
        exact_sources: Vec::new(),
    };
    let mut selection = BTreeSet::new();
    let mut expansion_budget = MAX_SELECTED_INDICES;
    for (key, value) in uri.query_pairs() {
        match key.as_ref() {
            "xt" => {
                if let Some(hash) = value.strip_prefix("urn:btih:").and_then(normalize_btih) {
                    magnet.info_hash_hex = hash.clone();
                    magnet.is_v2 = false;
                    magnet.info_hash_v1_hex = Some(hash);
                } else if let Some(hash) = value
                    .strip_prefix("urn:btmh:1220")
                    .filter(|hash| is_hex(hash, 64))
                {
                    let hash = hash.to_ascii_lowercase();
                    magnet.info_hash_hex = hash.clone();
                    magnet.is_v2 = true;
                    magnet.info_hash_v2_hex = Some(hash);
                }
            }
            "tr" if magnet.trackers.len() < MAX_MAGNET_TRACKERS
                && tracker_url(&value, allow_loopback).is_some()
                && !magnet.trackers.iter().any(|tracker| tracker == &value) =>
            {
                magnet.trackers.push(value.into_owned());
            }
            "xs" if !value.is_empty() => magnet.exact_sources.push(value.into_owned()),
            "xl" => magnet.exact_length = Some(decimal(&value)?),
            "so" => {
                parse_selection(&value, &mut selection, &mut expansion_budget)?;
                magnet.select_only = Some(Vec::new());
            }
            _ => {}
        }
    }
    if magnet.select_only.is_some() {
        magnet.select_only = Some(selection.into_iter().collect());
    }
    Ok(magnet)
}

pub(crate) fn allowed_tracker_url(raw: &str) -> bool {
    tracker_url(raw, false).is_some()
}

fn tracker_url(raw: &str, allow_loopback: bool) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "udp") || raw.chars().any(char::is_control) {
        return None;
    }
    let allowed = match tracker_host(&url)? {
        Host::Ipv4(ip) => tracker_ip_allowed(ip.into(), allow_loopback),
        Host::Ipv6(ip) => tracker_ip_allowed(ip.into(), allow_loopback),
        Host::Domain(host) => {
            let host = host.trim_end_matches('.');
            !host.is_empty()
                && (allow_loopback || (host != "localhost" && !host.ends_with(".localhost")))
        }
    };
    allowed.then_some(url)
}

fn tracker_host(url: &Url) -> Option<Host<String>> {
    // Normalize UDP's opaque host to recognize alternate IPv4 spellings.
    Host::parse(url.host_str()?).ok()
}

fn tracker_ip_allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    let ip = ip.to_canonical();
    (allow_loopback && ip.is_loopback())
        || (!is_private_or_loopback_ip(ip)
            && !matches!(ip, IpAddr::V6(ip) if ip.segments()[..2] == [0x2001, 0xdb8]))
}

async fn resolve_tracker(raw: &str, udp_only: bool, allow_loopback: bool) -> Option<String> {
    let mut url = tracker_url(raw, allow_loopback)?;
    let udp = url.scheme() == "udp";
    if udp_only && !udp {
        return None;
    }
    let port = url.port_or_known_default().filter(|port| *port != 0)?;
    let host = match tracker_host(&url)? {
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
        Host::Domain(host) => host,
    };
    let mut addrs = timeout(TRACKER_DNS_TIMEOUT, lookup_host((host.as_str(), port)))
        .await
        .ok()?
        .ok()?;
    let first = addrs.next()?.ip();
    if !tracker_ip_allowed(first, allow_loopback) {
        return None;
    }
    let mut v4 = match first.to_canonical() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    };
    for addr in addrs {
        if !tracker_ip_allowed(addr.ip(), allow_loopback) {
            return None;
        }
        if let IpAddr::V4(ip) = addr.ip().to_canonical() {
            v4.get_or_insert(ip);
        }
    }
    if udp {
        // librqbit uses an IPv4 UDP socket. Pin the checked address to avoid a second DNS lookup.
        url.set_ip_host(IpAddr::V4(v4?)).ok()?;
    }
    Some(url.into())
}

fn is_hex(hash: &str, len: usize) -> bool {
    hash.len() == len && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_btih(hash: &str) -> Option<String> {
    if is_hex(hash, 40) {
        return Some(hash.to_ascii_lowercase());
    }
    if hash.len() != 32 {
        return None;
    }
    let mut bytes = Vec::with_capacity(20);
    let mut accumulator = 0u16;
    let mut bits = 0;
    for byte in hash.bytes().map(|byte| byte.to_ascii_uppercase()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return None,
        };
        accumulator = (accumulator << 5) | u16::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((accumulator >> bits) as u8);
        }
    }
    Some(hex::encode(bytes))
}

fn decimal<T: std::str::FromStr>(value: &str) -> Result<T, UrlSecurityError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(UrlSecurityError::InvalidMagnet);
    }
    value.parse().map_err(|_| UrlSecurityError::InvalidMagnet)
}

fn parse_selection(
    value: &str,
    selection: &mut BTreeSet<usize>,
    expansion_budget: &mut usize,
) -> Result<(), UrlSecurityError> {
    for entry in value.split(',') {
        let (start, end) = entry.split_once('-').unwrap_or((entry, entry));
        let start: usize = decimal(start)?;
        let end: usize = decimal(end)?;
        let count = end
            .checked_sub(start)
            .and_then(|span| span.checked_add(1))
            .ok_or(UrlSecurityError::InvalidMagnet)?;
        *expansion_budget = expansion_budget
            .checked_sub(count)
            .ok_or(UrlSecurityError::InvalidMagnet)?;
        selection.extend(start..=end);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const PUBLIC_TRACKERS: &[&str] = &[
        "http://tracker.example.com/announce",
        "https://tracker.example.net/announce",
        "udp://tracker.example.org:6969/announce",
    ];
    pub(crate) const BLOCKED_TRACKERS: &[&str] = &[
        "file:///tmp/tracker",
        "ftp://tracker.example.org",
        "http://localhost/announce",
        "udp://FoO.LocalHost.:6969/announce",
        "https://127.0.0.1/announce",
        "udp://0x7f000001:6969",
        "http://10.0.0.1/announce",
        "http://169.254.169.254/announce",
        "http://100.64.0.1/announce",
        "http://192.0.2.1/announce",
        "udp://0.0.0.0:6969",
        "udp://224.0.0.1:6969",
        "http://[::1]/announce",
        "udp://[::ffff:127.0.0.1]:6969",
        "http://[fe80::1]/announce",
        "http://[fc00::1]/announce",
        "https://[2001:db8::1]/announce",
    ];

    #[test]
    fn filters_trackers_without_rejecting_the_magnet() {
        let mut magnet =
            Url::parse("magnet:?xt=urn:btih:0000000000000000000000000000000000000000").unwrap();
        magnet.query_pairs_mut().extend_pairs(
            PUBLIC_TRACKERS
                .iter()
                .chain(BLOCKED_TRACKERS)
                .map(|url| ("tr", *url)),
        );
        let parsed = parse_magnet_uri(magnet.as_str()).unwrap();
        assert_eq!(parsed.trackers, PUBLIC_TRACKERS);
    }

    #[test]
    fn caps_distinct_magnet_trackers() {
        let mut magnet =
            Url::parse("magnet:?xt=urn:btih:0000000000000000000000000000000000000000").unwrap();
        let trackers: Vec<_> = (0..10)
            .map(|i| format!("udp://tracker{i}.example:6969/announce"))
            .collect();
        magnet.query_pairs_mut().extend_pairs(
            BLOCKED_TRACKERS
                .iter()
                .copied()
                .chain(std::iter::once(trackers[0].as_str()))
                .chain(trackers.iter().map(String::as_str))
                .map(|url| ("tr", url)),
        );
        assert_eq!(
            parse_magnet_uri(magnet.as_str()).unwrap().trackers,
            trackers[..8]
        );
    }

    #[tokio::test]
    async fn pins_loopback_udp_tracker_only_when_explicitly_allowed() {
        let tracker = "udp://localhost:6969/announce";
        assert_eq!(resolve_tracker(tracker, true, false).await, None);
        let pinned = resolve_tracker(tracker, true, true).await.unwrap();
        let pinned = Url::parse(&pinned).unwrap();
        assert!(pinned
            .host_str()
            .unwrap()
            .parse::<IpAddr>()
            .unwrap()
            .is_loopback());
        assert_eq!(pinned.port(), Some(6969));
    }

    #[test]
    fn parses_hashes_selection_and_optional_fields() {
        let btih = "MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43U";
        let btmh = "ab".repeat(32);
        let uri = format!("magnet:?xt=urn:btih:{btih}&xt=urn:btmh:1220{btmh}&dn=Tokyo+%F0%9F%97%BC&tr=udp%3A%2F%2Ftracker.test%3A60&tr=https%3A%2F%2Ftracker.test&xl=50&so=0,2-4&so=3,7&xs=https%3A%2F%2Fexample.test%2Fmetadata");
        let parsed = parse_magnet_uri(&uri).unwrap();
        assert_eq!(
            parsed.info_hash_v1_hex,
            Some(hex::encode(b"abcdefghijklmnopqrst"))
        );
        assert_eq!(parsed.info_hash_hex, btmh);
        assert!(parsed.is_v2);
        assert_eq!(parsed.safe_display_name(), "Tokyo 🗼");
        assert_eq!(parsed.select_only, Some(vec![0, 2, 3, 4, 7]));
        assert_eq!(
            parsed.trackers,
            ["udp://tracker.test:60", "https://tracker.test"]
        );
        assert_eq!(parsed.exact_length, Some(50));
        assert_eq!(parsed.exact_sources, ["https://example.test/metadata"]);
        let hash = "AB".repeat(20);
        let parsed =
            parse_magnet_uri(&format!("magnet:?xt=urn:btih:{hash}&dn=..%2Fescape")).unwrap();
        assert!(!parsed.is_v2);
        assert_eq!(parsed.safe_display_name(), hash.to_ascii_lowercase());
    }

    #[test]
    fn rejects_bad_magnets_and_ranges() {
        for invalid in [
            "magnet:",
            "magnet:?xt=urn:btmh:1230aa",
            "magnet:?xt=urn:btih:x",
            "https://example.test/torrent",
            "magnet:\0",
        ] {
            assert!(parse_magnet_uri(invalid).is_err());
        }
        for invalid in ["3-2", "0-100000", "2,,4", "2-3-4", "-3", ""] {
            assert!(parse_magnet_uri(&format!(
                "magnet:?xt=urn:btih:{}&so={invalid}",
                "ab".repeat(20)
            ))
            .is_err());
        }
    }
}
