use std::net::IpAddr;
use std::path::{Path, PathBuf};
use thiserror::Error;
use url::Url;

const MAX_FILENAME_BYTES: usize = 200;

const WINDOWS_RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PathSecurityError {
    #[error("filename or path is empty")]
    Empty,
    #[error("filename contains NUL byte")]
    NulByte,
    #[error("path contains directory traversal ('..' or '.' segment)")]
    Traversal,
    #[error("path contains forbidden directory separator")]
    PathSeparator,
    #[error("path contains NTFS Alternate Data Stream colon (':')")]
    NtfsAlternateDataStream,
    #[error("filename contains Unicode BiDi/RTL override control character")]
    BidiControl,
    #[error("filename contains invalid control or Windows filesystem character")]
    InvalidCharacter,
    #[error("filename matches a Windows reserved device name")]
    ReservedDeviceName,
    #[error("filename has trailing dot or whitespace")]
    TrailingDotOrSpace,
    #[error("filename segment exceeds maximum safe byte length")]
    TooLong,
    #[error("path escapes base directory")]
    EscapesBaseDir,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UrlSecurityError {
    #[error("URL is empty or contains invalid characters")]
    InvalidFormat,
    #[error("URL parse failure: {0}")]
    ParseError(String),
    #[error("disallowed URL scheme '{0}' (only http and https allowed)")]
    DisallowedScheme(String),
    #[error("URL must contain a valid host")]
    MissingHost,
    #[error("disallowed magnet URI")]
    InvalidMagnet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetMeta {
    pub raw: String,
    pub info_hash_hex: String,
    pub display_name: Option<String>,
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn is_windows_forbidden_char(ch: char) -> bool {
    matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

fn is_reserved_windows_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let upper = stem.trim().to_ascii_uppercase();
    WINDOWS_RESERVED_STEMS.contains(&upper.as_str())
}

/// Strictly validates a single filename segment without mutating it.
pub fn validate_safe_filename_component(raw: &str) -> Result<&str, PathSecurityError> {
    if raw.is_empty() || raw.trim().is_empty() {
        return Err(PathSecurityError::Empty);
    }
    if raw == "." || raw == ".." {
        return Err(PathSecurityError::Traversal);
    }
    if raw.len() > MAX_FILENAME_BYTES {
        return Err(PathSecurityError::TooLong);
    }
    if raw.ends_with('.') || raw.ends_with(' ') || raw.starts_with(' ') {
        return Err(PathSecurityError::TrailingDotOrSpace);
    }

    for ch in raw.chars() {
        if ch == '\0' {
            return Err(PathSecurityError::NulByte);
        }
        if ch == '/' || ch == '\\' {
            return Err(PathSecurityError::PathSeparator);
        }
        if ch == ':' {
            return Err(PathSecurityError::NtfsAlternateDataStream);
        }
        if is_bidi_control(ch) {
            return Err(PathSecurityError::BidiControl);
        }
        if ch.is_control() || is_windows_forbidden_char(ch) {
            return Err(PathSecurityError::InvalidCharacter);
        }
    }

    if is_reserved_windows_device(raw) {
        return Err(PathSecurityError::ReservedDeviceName);
    }

    Ok(raw)
}

/// Validates and lightly normalizes an untrusted single filename from HTTP `Content-Disposition`
/// or URL path segments while strictly rejecting explicit security violations.
pub fn sanitize_filename(raw: &str) -> Result<String, PathSecurityError> {
    if raw.contains('\0') {
        return Err(PathSecurityError::NulByte);
    }
    if raw.chars().any(is_bidi_control) {
        return Err(PathSecurityError::BidiControl);
    }
    if raw.contains('/') || raw.contains('\\') {
        return Err(PathSecurityError::PathSeparator);
    }
    if raw.contains(':') {
        return Err(PathSecurityError::NtfsAlternateDataStream);
    }
    let trimmed = raw.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return Err(PathSecurityError::Empty);
    }
    if trimmed == "." || trimmed == ".." {
        return Err(PathSecurityError::Traversal);
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || is_windows_forbidden_char(c))
    {
        return Err(PathSecurityError::InvalidCharacter);
    }
    if is_reserved_windows_device(trimmed) {
        return Err(PathSecurityError::ReservedDeviceName);
    }
    if trimmed.len() > MAX_FILENAME_BYTES {
        return Err(PathSecurityError::TooLong);
    }
    Ok(trimmed.to_string())
}

/// Converts a human media/page title into a safe Windows filename with the requested extension.
/// Unlike `sanitize_filename`, punctuation like `:` or `/` in video titles is replaced cleanly.
pub fn sanitize_title_to_filename(
    title: &str,
    extension: &str,
) -> Result<String, PathSecurityError> {
    let ext = validate_safe_filename_component(extension.trim_start_matches('.'))?;
    let mut cleaned = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch == '\0' || ch.is_control() || is_bidi_control(ch) {
            continue;
        }
        if is_windows_forbidden_char(ch) {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let mut stem = cleaned.trim().trim_matches('.').trim().to_string();
    while stem.contains("..") {
        stem = stem.replace("..", ".");
    }
    let stem = stem.trim_end_matches(['.', ' ']);
    let mut final_stem = if stem.is_empty() || stem == "." || stem == ".." {
        "download".to_string()
    } else {
        stem.to_string()
    };
    if is_reserved_windows_device(&final_stem) {
        final_stem = format!("_{final_stem}");
    }
    let max_stem_bytes = MAX_FILENAME_BYTES.saturating_sub(ext.len() + 1);
    if final_stem.len() > max_stem_bytes {
        let mut end = max_stem_bytes;
        while end > 0 && !final_stem.is_char_boundary(end) {
            end -= 1;
        }
        final_stem.truncate(end);
        final_stem = final_stem.trim_end_matches(['.', ' ']).to_string();
        if final_stem.is_empty() {
            final_stem = "download".to_string();
        }
    }
    let candidate = format!("{final_stem}.{ext}");
    validate_safe_filename_component(&candidate)?;
    Ok(candidate)
}

/// Safely joins an untrusted relative path (single filename or multi-segment `.torrent` path)
/// strictly beneath `base_dir`, rejecting traversal, UNC prefixes, drive letters, ADS, and BiDi tricks.
pub fn safe_join_under(base_dir: &Path, rel_path: &str) -> Result<PathBuf, PathSecurityError> {
    if rel_path.is_empty() {
        return Err(PathSecurityError::Empty);
    }
    if rel_path.starts_with('/') || rel_path.starts_with('\\') {
        return Err(PathSecurityError::PathSeparator);
    }
    if rel_path.contains(':') {
        return Err(PathSecurityError::NtfsAlternateDataStream);
    }
    if rel_path.contains('\0') {
        return Err(PathSecurityError::NulByte);
    }

    let mut out = base_dir.to_path_buf();
    let normalized = rel_path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts.is_empty() {
        return Err(PathSecurityError::Empty);
    }
    for part in parts {
        if part.is_empty() {
            return Err(PathSecurityError::PathSeparator);
        }
        let validated = validate_safe_filename_component(part)?;
        out.push(validated);
    }
    Ok(out)
}

/// Validates an HTTP or HTTPS URL strictly.
pub fn validate_http_url(raw: &str) -> Result<Url, UrlSecurityError> {
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(UrlSecurityError::InvalidFormat);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(UrlSecurityError::InvalidFormat);
    }
    let parsed = Url::parse(trimmed).map_err(|e| UrlSecurityError::ParseError(e.to_string()))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(UrlSecurityError::DisallowedScheme(scheme.to_string()));
    }
    match parsed.host_str() {
        Some(h) if !h.is_empty() => Ok(parsed),
        _ => Err(UrlSecurityError::MissingHost),
    }
}

/// Returns true if an IP address belongs to loopback, link-local, RFC1918 private, CGNAT,
/// documentation, multicast, unspecified, or IPv4-mapped equivalents (SSRF defense).
pub fn is_private_or_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let is_cgnat = o[0] == 100 && (o[1] & 0b1100_0000) == 64;
            let is_benchmark = o[0] == 198 && (o[1] == 18 || o[1] == 19);
            let is_this_net = o[0] == 0;
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.is_multicast()
                || is_cgnat
                || is_benchmark
                || is_this_net
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_or_loopback_ip(IpAddr::V4(mapped));
            }
            let seg = v6.segments();
            let is_unique_local = (seg[0] & 0xfe00) == 0xfc00;
            let is_link_local = (seg[0] & 0xffc0) == 0xfe80;
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local
                || is_link_local
        }
    }
}

/// Validates a `magnet:?xt=urn:btih:...` (or `btmh:`) URI and extracts its info-hash and optional display name.
pub fn validate_magnet_uri(raw: &str) -> Result<MagnetMeta, UrlSecurityError> {
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(UrlSecurityError::InvalidFormat);
    }
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).map_err(|_| UrlSecurityError::InvalidMagnet)?;
    if parsed.scheme() != "magnet" {
        return Err(UrlSecurityError::InvalidMagnet);
    }

    let mut info_hash_hex: Option<String> = None;
    let mut display_name: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        if k == "xt" {
            if let Some(hash) = v.strip_prefix("urn:btih:") {
                let valid_hex = hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit());
                let valid_b32 = hash.len() == 32
                    && hash
                        .chars()
                        .all(|c| matches!(c.to_ascii_uppercase(), 'A'..='Z' | '2'..='7'));
                if valid_hex || valid_b32 {
                    info_hash_hex = Some(hash.to_ascii_lowercase());
                }
            } else if let Some(hash) = v.strip_prefix("urn:btmh:1220") {
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    info_hash_hex = Some(hash.to_ascii_lowercase());
                }
            }
        } else if k == "dn" && !v.trim().is_empty() {
            display_name = Some(v.into_owned());
        }
    }

    match info_hash_hex {
        Some(hash) => Ok(MagnetMeta {
            raw: trimmed.to_string(),
            info_hash_hex: hash,
            display_name,
        }),
        None => Err(UrlSecurityError::InvalidMagnet),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn accepts_benign_and_unicode_filenames() {
        let valid_cases = [
            "Ubuntu-26.04-desktop-amd64.iso",
            "report.v1.2..final.pdf",
            "Alpine Cinema (4K 60fps) [AV1].mp4",
            "ملف_تجريبي_1080p.mp4",
            "東京建築_4K_HDR.mkv",
            "Design Kit 🚀 v2.zip",
        ];
        for name in valid_cases {
            assert_eq!(sanitize_filename(name).unwrap(), name);
            assert_eq!(validate_safe_filename_component(name).unwrap(), name);
        }
    }

    #[test]
    fn rejects_adversarial_filenames() {
        let bad = [
            "",
            ".",
            "..",
            "../evil.exe",
            "..\\..\\Windows\\System32\\cmd.exe",
            "sub/file.zip",
            "file.mp4:Zone.Identifier",
            "evil.mp4:stream",
            "CON",
            "con.mp4",
            "NUL.txt",
            "COM1.exe",
            "LPT9",
            "\u{202E}cod.exe",
            "bad\0name.zip",
            "bad*name.zip",
        ];
        for case in bad {
            assert!(
                sanitize_filename(case).is_err(),
                "expected sanitize_filename to reject {case:?}"
            );
        }
    }

    #[test]
    fn safe_join_under_validates_torrent_subpaths() {
        let base = Path::new(r"D:\Downloads\Torrents");
        let joined = safe_join_under(base, "Blender_5.2/Textures/Wood (4K).png").unwrap();
        assert!(joined.starts_with(base));

        for evil in [
            "../escape.exe",
            "foo/../../bar.exe",
            "/absolute/file.txt",
            "C:\\Windows\\win.ini",
            "folder/CON.bin",
            "folder/file.dat:ads",
            "folder/\u{202E}gpj.exe",
        ] {
            assert!(
                safe_join_under(base, evil).is_err(),
                "expected safe_join_under to reject {evil:?}"
            );
        }
    }

    #[test]
    fn title_sanitizer_neutralizes_punctuation() {
        let out = sanitize_title_to_filename("Tokyo 2026: Day 1 / Intro? \u{202E}", "mp4").unwrap();
        assert_eq!(out, "Tokyo 2026_ Day 1 _ Intro_.mp4");
        assert!(validate_safe_filename_component(&out).is_ok());

        let reserved = sanitize_title_to_filename("CON", "mp4").unwrap();
        assert_eq!(reserved, "_CON.mp4");
    }

    #[test]
    fn validates_http_urls_and_blocks_private_ips() {
        assert!(validate_http_url("https://example.com/a/b?x=1&y=2").is_ok());
        assert!(validate_http_url("http://cdn.site.org/file.zip").is_ok());
        assert!(validate_http_url("file:///C:/Windows/System32/calc.exe").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
        assert!(validate_http_url("smb://10.0.0.1/share/x").is_err());

        assert!(is_private_or_loopback_ip(IpAddr::V4(Ipv4Addr::new(
            127, 0, 0, 1
        ))));
        assert!(is_private_or_loopback_ip(IpAddr::V4(Ipv4Addr::new(
            10, 0, 5, 2
        ))));
        assert!(is_private_or_loopback_ip(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_private_or_loopback_ip(IpAddr::V4(Ipv4Addr::new(
            100, 64, 1, 1
        ))));
        assert!(is_private_or_loopback_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_or_loopback_ip(IpAddr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        )));
        assert!(!is_private_or_loopback_ip(IpAddr::V4(Ipv4Addr::new(
            1, 1, 1, 1
        ))));
    }

    #[test]
    fn validates_magnet_uris() {
        let valid =
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Blender+Assets";
        let meta = validate_magnet_uri(valid).unwrap();
        assert_eq!(
            meta.info_hash_hex,
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(meta.display_name.as_deref(), Some("Blender Assets"));

        assert!(validate_magnet_uri("magnet:?xt=urn:btih:invalid").is_err());
        assert!(validate_magnet_uri("https://example.com/file.torrent").is_err());
    }
}
