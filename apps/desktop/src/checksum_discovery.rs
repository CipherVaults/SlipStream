use crate::rss::{self, FeedHttpClient};
use slipstream_protocol::sanitize_filename;
use std::time::Duration;
use url::Url;

const MAX_BYTES: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(4);

pub async fn discover_sha256(
    client: &FeedHttpClient,
    raw_url: &str,
    allow_loopback: bool,
) -> Option<String> {
    let url = rss::checked_url(raw_url, allow_loopback).ok()?;
    let name = url_filename(&url)?;
    let origin = url.origin();
    let directory = url.join(".").ok()?;
    tokio::time::timeout(TIMEOUT, async {
        for sibling in [
            "SHA256SUMS".into(),
            "sha256sums.txt".into(),
            format!("{name}.sha256"),
            format!("{name}.sha256sum"),
        ] {
            let mut target = directory.clone();
            target
                .path_segments_mut()
                .ok()?
                .pop_if_empty()
                .push(&sibling);
            let Ok(response) = rss::open_response(
                client,
                target.as_str(),
                allow_loopback,
                MAX_BYTES,
                Some(&origin),
            )
            .await
            else {
                continue;
            };
            let Ok(bytes) = rss::read_response(response, MAX_BYTES).await else {
                continue;
            };
            if let Some(digest) = checksum_for_filename(&bytes, &name) {
                return Some(digest);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

pub(crate) fn url_filename(url: &Url) -> Option<String> {
    let raw = url.path_segments()?.next_back()?;
    let mut raw = raw.as_bytes().iter().copied();
    let mut decoded = Vec::new();
    while let Some(byte) = raw.next() {
        if byte == b'%' {
            let digits = [raw.next()?, raw.next()?];
            let mut output = [0];
            hex::decode_to_slice(digits, &mut output).ok()?;
            decoded.push(output[0]);
        } else {
            decoded.push(byte);
        }
    }
    sanitize_filename(std::str::from_utf8(&decoded).ok()?).ok()
}

fn checksum_for_filename(bytes: &[u8], name: &str) -> Option<String> {
    std::str::from_utf8(bytes).ok()?.lines().find_map(|line| {
        let hash = line.get(..64)?;
        if !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let raw = line.get(64..)?.strip_prefix([' ', '\t'])?;
        let raw = raw.trim_start_matches([' ', '\t']);
        let raw = raw.strip_prefix('*').unwrap_or(raw);
        let raw = raw.strip_prefix("./").unwrap_or(raw);
        (raw == name).then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rss::tests::serve_routes;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn matches_same_origin_manifest_by_decoded_basename() {
        let digest = "aB".repeat(32);
        let manifest = format!("{digest}  other.zip\n{digest} *archive one.zip\n");
        let (base, _server) = serve_routes(move |path| match path {
            "/releases/SHA256SUMS" => (
                "302 Found",
                "Location: /releases/list\r\n".into(),
                Vec::new(),
            ),
            "/releases/list" => ("200 OK", String::new(), manifest.clone().into_bytes()),
            _ => ("404 Not Found", String::new(), Vec::new()),
        })
        .await;
        let client = rss::feed_http_client(true).unwrap();
        assert_eq!(
            discover_sha256(
                &client,
                &format!("{base}/releases/archive%20one.zip?token=x"),
                true
            )
            .await,
            Some(digest.to_ascii_lowercase())
        );
        assert!(discover_sha256(
            &client,
            &format!("{base}/releases/archive%20one.zip"),
            false
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn rejects_cross_origin_redirects_and_oversized_manifests() {
        let reached = Arc::new(AtomicBool::new(false));
        let hit = reached.clone();
        let (cross, _cross_server) = serve_routes(move |_| {
            hit.store(true, Ordering::Relaxed);
            ("404 Not Found", String::new(), Vec::new())
        })
        .await;
        let (base, _server) = serve_routes(move |path| match path {
            "/SHA256SUMS" => (
                "302 Found",
                format!("Location: {cross}/checksum\r\n"),
                Vec::new(),
            ),
            "/sha256sums.txt" => (
                "200 OK",
                String::new(),
                format!("{}  archive.zip\n{}", "a".repeat(64), "x".repeat(MAX_BYTES)).into_bytes(),
            ),
            _ => ("404 Not Found", String::new(), Vec::new()),
        })
        .await;
        let client = rss::feed_http_client(true).unwrap();
        assert_eq!(
            discover_sha256(&client, &format!("{base}/archive.zip"), true).await,
            None
        );
        assert!(!reached.load(Ordering::Relaxed));
    }
}
