use crate::settings::{read_json, write_json_atomically};
use futures_util::{stream, StreamExt};
use quick_xml::{
    events::{BytesStart, Event},
    Reader, XmlVersion,
};
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    header::LOCATION,
    redirect::Policy,
    Client, Response, StatusCode,
};
use sha2::{Digest, Sha256};
use slipstream_protocol::{is_private_or_loopback_ip, validate_http_url, validate_magnet_uri};
use std::{
    collections::{HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::lookup_host,
    time::timeout,
};
use url::{Host, Url};

const FEED_BYTES: usize = 2 * 1024 * 1024;
const MAX_ITEMS: usize = 500;
const MAX_SEEN: usize = 5_000;
const MAX_REDIRECTS: usize = 5;
const MAX_FIELD_BYTES: usize = 8192;
const IO_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq, Eq)]
pub struct RssFeedItem {
    pub title: String,
    pub item_key: String,
    pub torrent_or_magnet_url: String,
}

#[derive(Default)]
pub struct RssSeenStore {
    order: VecDeque<(String, String)>,
    pairs: HashSet<(String, String)>,
}

impl RssSeenStore {
    pub fn load(data_dir: &Path) -> io::Result<Self> {
        let entries: Vec<(String, String)> = read_json(&data_dir.join("rss_seen.json"))?;
        let mut store = Self::default();
        for (feed_id, key) in entries {
            store.mark_seen(&feed_id, &key);
        }
        Ok(store)
    }

    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        write_json_atomically(&data_dir.join("rss_seen.json"), &self.order)
    }

    pub fn contains(&self, feed_id: &str, item_key: &str) -> bool {
        self.pairs
            .contains(&(feed_id.to_owned(), item_key.to_owned()))
    }

    pub fn mark_seen(&mut self, feed_id: &str, item_key: &str) {
        let pair = (feed_id.to_owned(), item_key.to_owned());
        if self.pairs.insert(pair.clone()) {
            self.order.push_back(pair);
            if self.order.len() > MAX_SEEN {
                self.pairs
                    .remove(&self.order.pop_front().expect("non-empty seen store"));
            }
        }
    }
}

pub fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let text = text.to_lowercase();
    if !pattern.contains(['*', '?']) {
        return text.contains(&pattern);
    }
    let pattern: Vec<_> = pattern.chars().collect();
    let text: Vec<_> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(ch) if *ch == '?' || *ch == text[t] => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((star_index, matched_end)) = star else {
                    return false;
                };
                t = matched_end + 1;
                p = star_index + 1;
                star = Some((star_index, t));
            }
        }
    }
    pattern[p..].iter().all(|ch| *ch == '*')
}

pub(crate) fn decode_reference(entity: &str) -> Option<char> {
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let (digits, radix) = entity.strip_prefix("#x").map_or_else(
                || entity.strip_prefix('#').map(|digits| (digits, 10)),
                |digits| Some((digits, 16)),
            )?;
            let value = digits
                .bytes()
                .all(|digit| match radix {
                    16 => digit.is_ascii_hexdigit(),
                    _ => digit.is_ascii_digit(),
                })
                .then(|| u32::from_str_radix(digits, radix).ok())
                .flatten()?;
            let value = char::from_u32(value)?;
            if !matches!(value, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
            {
                return None;
            }
            value
        }
    })
}

#[derive(Default)]
struct ItemFields {
    title: String,
    id: String,
    link: String,
    source: Option<(u8, String)>,
}

impl ItemFields {
    fn offer_source(
        &mut self,
        raw: &str,
        kind: &str,
        is_enclosure: bool,
        base: &Url,
        allow_loopback: bool,
    ) {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > MAX_FIELD_BYTES || raw.chars().any(char::is_control) {
            return;
        }
        let Ok(url) = base.join(raw) else { return };
        let rank =
            if url.scheme() == "magnet" {
                if validate_magnet_uri(url.as_str()).is_err() {
                    return;
                }
                3
            } else {
                if checked_url(url.as_str(), allow_loopback).is_err() {
                    return;
                }
                if kind.split(';').next().is_some_and(|kind| {
                    kind.trim().eq_ignore_ascii_case("application/x-bittorrent")
                }) || url.path().to_ascii_lowercase().ends_with(".torrent")
                {
                    2
                } else if is_enclosure {
                    return;
                } else {
                    1
                }
            };
        if self
            .source
            .as_ref()
            .is_none_or(|(previous, _)| rank > *previous)
        {
            self.source = Some((rank, url.into()));
        }
    }

    fn offer_link_attributes(
        &mut self,
        element: &BytesStart<'_>,
        atom: bool,
        base: &Url,
        allow_loopback: bool,
    ) -> Result<(), String> {
        let mut source = None;
        let mut kind = None;
        let mut rel = None;
        for attr in element.attributes() {
            let attr = attr.map_err(|error| error.to_string())?;
            let target = match attr.key.as_ref() {
                b"href" if atom => &mut source,
                b"url" if !atom => &mut source,
                b"type" => &mut kind,
                b"rel" if atom => &mut rel,
                _ => continue,
            };
            *target = Some(
                attr.normalized_value(XmlVersion::Implicit1_0)
                    .map_err(|error| error.to_string())?,
            );
        }
        if let Some(source) = source {
            let alternate = atom
                && rel
                    .as_deref()
                    .is_none_or(|rel| rel.eq_ignore_ascii_case("alternate"));
            self.offer_source(
                &source,
                kind.as_deref().unwrap_or_default(),
                !alternate,
                base,
                allow_loopback,
            );
        }
        Ok(())
    }

    fn finish(mut self, base: &Url, allow_loopback: bool) -> Option<RssFeedItem> {
        let link = std::mem::take(&mut self.link);
        self.offer_source(&link, "", false, base, allow_loopback);
        let (_, torrent_or_magnet_url) = self.source?;
        let id = self.id.trim();
        let key = if id.is_empty() {
            &torrent_or_magnet_url
        } else {
            id
        };
        Some(RssFeedItem {
            title: self.title.trim().to_owned(),
            item_key: hex::encode(Sha256::digest(key)),
            torrent_or_magnet_url,
        })
    }
}

#[derive(Clone, Copy)]
enum TextField {
    Title,
    Id,
    Link,
}

fn parse_feed(bytes: &[u8], base: &Url, allow_loopback: bool) -> Result<Vec<RssFeedItem>, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::<Vec<u8>>::new();
    let mut item = ItemFields::default();
    let mut item_depth = 0;
    let mut field = None;
    let mut count = 0;
    let mut items = Vec::new();
    let mut root_seen = false;
    loop {
        let event = reader.read_event().map_err(|error| error.to_string())?;
        match &event {
            Event::Start(tag) => {
                let name = tag.local_name();
                let name = name.as_ref();
                let parent = stack.last().map(Vec::as_slice);
                let depth = stack.len() + 1;
                if depth == 1 {
                    if root_seen || !matches!(name, b"rss" | b"feed") {
                        return Err("Expected an RSS or Atom feed".into());
                    }
                    root_seen = true;
                }
                if depth > 64 {
                    return Err("RSS XML is nested too deeply".into());
                }
                let rss_item = depth == 3
                    && name == b"item"
                    && parent == Some(b"channel")
                    && stack[0] == b"rss";
                let atom_item = depth == 2 && name == b"entry" && parent == Some(b"feed");
                if (rss_item || atom_item) && count < MAX_ITEMS {
                    item_depth = depth;
                    count += 1;
                } else if item_depth != 0 && depth == item_depth + 1 {
                    let atom = item_depth == 2;
                    field = match name {
                        b"title" => Some(TextField::Title),
                        b"id" if atom => Some(TextField::Id),
                        b"guid" if !atom => Some(TextField::Id),
                        b"link" if !atom => Some(TextField::Link),
                        b"link" if atom => {
                            item.offer_link_attributes(tag, true, base, allow_loopback)?;
                            None
                        }
                        b"enclosure" if !atom => {
                            item.offer_link_attributes(tag, false, base, allow_loopback)?;
                            None
                        }
                        _ => None,
                    };
                }
                stack.push(name.to_vec());
            }
            Event::End(_) => {
                if stack.len() == item_depth + 1 {
                    field = None;
                }
                if item_depth != 0 && stack.len() == item_depth {
                    items.extend(std::mem::take(&mut item).finish(base, allow_loopback));
                    item_depth = 0;
                }
                stack.pop();
            }
            Event::DocType(_) => return Err("RSS XML must not contain a document type".into()),
            Event::Eof => {
                return if root_seen && stack.is_empty() {
                    Ok(items)
                } else {
                    Err("RSS XML is incomplete".into())
                };
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if field.is_some() => {
                let value = match field {
                    Some(TextField::Title) => &mut item.title,
                    Some(TextField::Id) => &mut item.id,
                    Some(TextField::Link) => &mut item.link,
                    None => unreachable!(),
                };
                if let Event::GeneralRef(reference) = &event {
                    let reference = reference.decode().map_err(|error| error.to_string())?;
                    value.push(decode_reference(&reference).ok_or("Invalid RSS XML entity")?);
                } else {
                    let decoded = match &event {
                        Event::Text(text) => text.decode(),
                        Event::CData(text) => text.decode(),
                        _ => unreachable!(),
                    }
                    .map_err(|error| error.to_string())?;
                    value.push_str(&decoded);
                }
                if value.len() > MAX_FIELD_BYTES {
                    return Err("RSS XML field exceeds the size limit".into());
                }
            }
            _ => {}
        }
    }
}

fn permitted_ip(ip: IpAddr, allow_loopback: bool) -> bool {
    let ip = ip.to_canonical();
    !is_private_or_loopback_ip(ip) || (allow_loopback && ip.is_loopback())
}

fn check_host(host: &str, allow_loopback: bool) -> Result<(), String> {
    let hostname = host.trim_end_matches('.');
    if !allow_loopback
        && (hostname.eq_ignore_ascii_case("localhost")
            || hostname.to_ascii_lowercase().ends_with(".localhost"))
    {
        return Err("RSS URLs cannot target localhost".into());
    }
    Ok(())
}

pub(crate) fn checked_url(raw: &str, allow_loopback: bool) -> Result<Url, String> {
    let url = validate_http_url(raw).map_err(|error| error.to_string())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("RSS URLs must not embed credentials".into());
    }
    match url.host() {
        Some(Host::Domain(host)) => check_host(host, allow_loopback)?,
        Some(Host::Ipv4(ip)) if !permitted_ip(ip.into(), allow_loopback) => {
            return Err("RSS URLs cannot target private or loopback addresses".into());
        }
        Some(Host::Ipv6(ip)) if !permitted_ip(ip.into(), allow_loopback) => {
            return Err("RSS URLs cannot target private or loopback addresses".into());
        }
        _ => {}
    }
    Ok(url)
}

async fn public_addresses(
    host: &str,
    port: u16,
    allow_loopback: bool,
) -> Result<Vec<SocketAddr>, String> {
    check_host(host, allow_loopback)?;
    let addresses: Vec<_> = timeout(IO_TIMEOUT, lookup_host((host, port)))
        .await
        .map_err(|_| "RSS host lookup timed out")?
        .map_err(|_| "RSS host could not be resolved")?
        .collect();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !permitted_ip(address.ip(), allow_loopback))
    {
        return Err("RSS host resolved to an empty, private, or loopback address".into());
    }
    Ok(addresses)
}

struct FeedResolver(bool);

impl Resolve for FeedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.0;
        Box::pin(async move {
            let addresses = public_addresses(name.as_str(), 0, allow_loopback)
                .await
                .map_err(io::Error::other)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// A direct client with checked DNS and caller-managed redirects.
pub struct FeedHttpClient {
    client: Client,
    allow_loopback: bool,
}

pub fn feed_http_client(allow_loopback: bool) -> Result<FeedHttpClient, String> {
    Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        // Apply the address policy at connection time as well as during URL
        // validation, so a changed DNS answer cannot bypass the check.
        .dns_resolver(Arc::new(FeedResolver(allow_loopback)))
        .timeout(IO_TIMEOUT)
        .build()
        .map(|client| FeedHttpClient {
            client,
            allow_loopback,
        })
        .map_err(|error| error.to_string())
}

pub(crate) async fn check_http_host(url: &Url, allow_loopback: bool) -> Result<(), String> {
    if let Some(Host::Domain(host)) = url.host() {
        public_addresses(
            host,
            url.port_or_known_default().unwrap_or(80),
            allow_loopback,
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn open_response(
    client: &FeedHttpClient,
    raw: &str,
    allow_loopback: bool,
    cap: usize,
    origin: Option<&url::Origin>,
) -> Result<Response, String> {
    if client.allow_loopback != allow_loopback {
        return Err("HTTP client loopback policy does not match request".into());
    }
    let mut url = checked_url(raw, allow_loopback)?;
    for hop in 0..=MAX_REDIRECTS {
        if origin.is_some_and(|origin| *origin != url.origin()) {
            return Err("cross-origin HTTP target rejected".into());
        }
        check_http_host(&url, allow_loopback).await?;
        let response = client
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if response.url() != &url
            || response
                .remote_addr()
                .is_some_and(|addr| !permitted_ip(addr.ip(), allow_loopback))
        {
            return Err("RSS client followed a redirect or connected to a blocked address".into());
        }
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            if hop == MAX_REDIRECTS {
                return Err("RSS URL exceeded the redirect limit".into());
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or("RSS redirect has no valid Location header")?;
            if location.chars().any(char::is_control) {
                return Err("invalid redirect location".into());
            }
            let next = url.join(location).map_err(|_| "Invalid RSS redirect URL")?;
            url = checked_url(next.as_str(), allow_loopback)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("RSS request returned HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > cap as u64)
        {
            return Err(format!("RSS response exceeds {cap} bytes"));
        }
        return Ok(response);
    }
    unreachable!("redirect limit is checked inside the loop")
}

pub(crate) async fn read_response(mut response: Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut remaining = cap;
    while let Some(chunk) = next_chunk(&mut response, &mut remaining, cap).await? {
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn next_chunk(
    response: &mut Response,
    remaining: &mut usize,
    cap: usize,
) -> Result<Option<slipstream_torrent::Bytes>, String> {
    let chunk = response
        .chunk()
        .await
        .map_err(|error| error.without_url().to_string())?;
    if let Some(chunk) = &chunk {
        *remaining = remaining
            .checked_sub(chunk.len())
            .ok_or_else(|| format!("RSS response exceeds {cap} bytes"))?;
    }
    Ok(chunk)
}

/// Use a client from [feed_http_client] with the same loopback policy.
pub async fn fetch_feed_items(
    client: &FeedHttpClient,
    feed_url: &str,
    allow_loopback: bool,
) -> Result<Vec<RssFeedItem>, String> {
    timeout(IO_TIMEOUT, async {
        let response = open_response(client, feed_url, allow_loopback, FEED_BYTES, None).await?;
        let url = response.url().clone();
        let bytes = read_response(response, FEED_BYTES).await?;
        let items = parse_feed(&bytes, &url, allow_loopback)?;
        Ok(stream::iter(items)
            .map(|item| async move {
                if item.torrent_or_magnet_url.starts_with("magnet:") {
                    return Some(item);
                }
                let url = checked_url(&item.torrent_or_magnet_url, allow_loopback).ok()?;
                check_http_host(&url, allow_loopback).await.ok()?;
                Some(item)
            })
            .buffered(16)
            .filter_map(std::future::ready)
            .collect()
            .await)
    })
    .await
    .map_err(|_| "RSS feed request timed out".to_owned())?
}

pub(crate) async fn stage_torrent(
    client: &FeedHttpClient,
    url: &str,
    data_dir: &Path,
    allow_loopback: bool,
) -> Result<slipstream_torrent::Bytes, String> {
    use slipstream_torrent::{Bytes, MAX_TORRENT_BYTES};
    timeout(IO_TIMEOUT, async {
        let mut response =
            open_response(client, url, allow_loopback, MAX_TORRENT_BYTES, None).await?;
        let staged =
            tempfile::NamedTempFile::new_in(data_dir).map_err(|error| error.to_string())?;
        let mut file =
            tokio::fs::File::from_std(staged.reopen().map_err(|error| error.to_string())?);
        let mut remaining = MAX_TORRENT_BYTES;
        while let Some(chunk) = next_chunk(&mut response, &mut remaining, MAX_TORRENT_BYTES).await?
        {
            file.write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        file.flush().await.map_err(|error| error.to_string())?;
        file.rewind().await.map_err(|error| error.to_string())?;
        let length = MAX_TORRENT_BYTES - remaining;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Bytes::from(bytes))
    })
    .await
    .map_err(|_| "RSS torrent request timed out".to_owned())?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) async fn serve_routes(
        handler: impl Fn(&str) -> (&'static str, String, Vec<u8>) + Send + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut connection, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 512];
                while request.len() <= 8192 && !request.ends_with(b"\r\n\r\n") {
                    let Ok(count) = connection.read(&mut buffer).await else {
                        break;
                    };
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or_default();
                let (status, headers, body) = handler(path);
                let header = format!(
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
                    body.len()
                );
                if connection.write_all(header.as_bytes()).await.is_ok() {
                    let _ = connection.write_all(&body).await;
                }
            }
        });
        (url, server)
    }
    use std::fmt::Write;
    use tokio::net::TcpListener;

    const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

    fn base() -> Url {
        Url::parse("https://example.org/releases/feed.xml").unwrap()
    }

    #[test]
    fn rss_decodes_entities_and_prefers_magnets_and_torrent_enclosures() {
        let xml = format!(
            r#"
            <rss version="2.0"><channel>
              <item>
                <title>&lt;Linux&gt; &amp; &quot;Live&quot; &apos;&#73;&#x53;O&apos;<![CDATA[ & friends]]></title>
                <guid>item &amp; 1</guid>
                <link>{MAGNET}&amp;dn=Linux</link>
                <enclosure url="image.png" type="image/png"/>
                <enclosure url="/linux.torrent" type="application/x-bittorrent"/>
              </item>
              <item>
                <title>Second</title><link>post.html</link>
                <enclosure url="download?id=3&amp;name=2" type="application/x-bittorrent"/>
              </item>
              <item><title>Without a source</title></item>
            </channel></rss>"#
        );
        let items = parse_feed(xml.as_bytes(), &base(), false).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, r#"<Linux> & "Live" 'ISO' & friends"#);
        assert_eq!(items[0].item_key, hex::encode(Sha256::digest(b"item & 1")));
        assert_eq!(items[0].torrent_or_magnet_url, format!("{MAGNET}&dn=Linux"));
        assert_eq!(
            items[1].torrent_or_magnet_url,
            "https://example.org/releases/download?id=3&name=2"
        );
    }

    #[test]
    fn atom_reads_ids_titles_and_alternate_links_without_accepting_unsafe_urls() {
        let xml = br#"<a:feed xmlns:a="http://www.w3.org/2005/Atom">
          <a:entry><a:title>Fedora</a:title><a:id>entry-1</a:id>
            <a:link href="page" rel="alternate"/>
            <a:link href="http://127.0.0.1/fedora.torrent" rel="enclosure"/>
            <a:link href="http://user:secret@example.org/fedora.torrent" rel="enclosure"/>
            <a:link href="fedora.torrent" rel="enclosure"/>
          </a:entry>
          <a:entry><a:title>Fallback</a:title><a:link href="../page"/></a:entry>
        </a:feed>"#;
        let items = parse_feed(xml, &base(), false).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_key, hex::encode(Sha256::digest(b"entry-1")));
        assert_eq!(
            items[0].torrent_or_magnet_url,
            "https://example.org/releases/fedora.torrent"
        );
        assert_eq!(items[1].torrent_or_magnet_url, "https://example.org/page");
    }

    #[test]
    fn xml_rejects_document_types_and_caps_item_count() {
        let mut xml = "<rss><channel>".to_owned();
        for id in 0..MAX_ITEMS + 1 {
            write!(xml, "<item><guid>{id}</guid><link>{MAGNET}</link></item>").unwrap();
        }
        xml.push_str("</channel></rss>");
        assert_eq!(
            parse_feed(xml.as_bytes(), &base(), false).unwrap().len(),
            MAX_ITEMS
        );
        assert!(parse_feed(
            br#"<!DOCTYPE rss SYSTEM "file:///C:/secret"><rss/>"#,
            &base(),
            false
        )
        .is_err());
        assert!(parse_feed(
            b"<rss><channel><item><title>&#xD800;</title></item></channel></rss>",
            &base(),
            false
        )
        .is_err());
    }

    #[test]
    fn wildcard_filters_are_case_insensitive_and_plain_text_matches_substrings() {
        assert!(wildcard_matches("", "Fedora 42"));
        assert!(wildcard_matches("dOrA", "Fedora 42"));
        assert!(wildcard_matches("fedora ??.*", "Fedora 42.ISO"));
        assert!(wildcard_matches("*BETA*", "Fedora 42.beta.iso"));
        assert!(!wildcard_matches("*BETA*", "Fedora 42.ISO"));
        assert!(!wildcard_matches("fedora ?.iso", "Fedora 42.ISO"));
        assert!(wildcard_matches("**", ""));
        assert!(!wildcard_matches("?", ""));
    }

    #[test]
    fn seen_store_is_fifo_across_feeds_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RssSeenStore::load(dir.path()).unwrap();
        store.mark_seen("a", "duplicate-key");
        store.mark_seen("b", "duplicate-key");
        for index in 0..MAX_SEEN - 2 {
            store.mark_seen("a", &index.to_string());
        }
        store.mark_seen("a", "duplicate-key");
        store.mark_seen("c", "new");
        assert!(!store.contains("a", "duplicate-key"));
        assert!(store.contains("b", "duplicate-key"));
        store.save(dir.path()).unwrap();
        let mut loaded = RssSeenStore::load(dir.path()).unwrap();
        assert!(loaded.contains("c", "new"));
        loaded.mark_seen("a", "another");
        assert!(!loaded.contains("b", "duplicate-key"));
        assert_eq!(loaded.order.len(), MAX_SEEN);
    }

    async fn serve_once(response: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/feed", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let _ = socket.write_all(&response).await;
        });
        (url, task)
    }

    #[tokio::test]
    async fn feed_fetch_rejects_loopback_and_checks_redirect_targets() {
        let strict = feed_http_client(false).unwrap();
        for url in [
            "http://127.0.0.1:9/feed",
            "http://sub.localhost./feed",
            "http://user:password@example.org/feed",
        ] {
            assert!(fetch_feed_items(&strict, url, false).await.is_err());
        }
        let (url, server) = serve_once(b"HTTP/1.1 302 Found\r\nLocation: http://192.168.1.1/feed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()).await;
        let error = fetch_feed_items(&feed_http_client(true).unwrap(), &url, true)
            .await
            .unwrap_err();
        assert!(error.contains("private"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn feed_fetch_rejects_a_chunked_body_over_two_mib() {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            FEED_BYTES + 1
        )
        .into_bytes();
        response.resize(response.len() + FEED_BYTES + 1, b'x');
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = serve_once(response).await;
        let error = fetch_feed_items(&feed_http_client(true).unwrap(), &url, true)
            .await
            .unwrap_err();
        assert!(error.contains("exceeds 2097152 bytes"), "{error}");
        server.await.unwrap();
    }
}
