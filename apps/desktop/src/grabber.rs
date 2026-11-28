use crate::{
    checksum_discovery::url_filename,
    rss::{self, FeedHttpClient},
};
use futures_util::{stream, StreamExt};
use quick_xml::events::BytesStart;
use reqwest::header::CONTENT_TYPE;
use slipstream_protocol::{SiteGrabItem, SiteGrabRequest, SiteGrabResult};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use url::Url;

const MAX_PAGES: usize = 25;
const MAX_ITEMS: usize = 500;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINKS_PER_PAGE: usize = 10_000;

pub async fn grab_site(
    client: &FeedHttpClient,
    req: &SiteGrabRequest,
    allow_loopback: bool,
) -> Result<SiteGrabResult, String> {
    tokio::time::timeout(Duration::from_secs(45), crawl(client, req, allow_loopback))
        .await
        .map_err(|_| "site grab timed out".to_owned())?
}

async fn crawl(
    client: &FeedHttpClient,
    req: &SiteGrabRequest,
    allow_loopback: bool,
) -> Result<SiteGrabResult, String> {
    let mut root = rss::checked_url(&req.url, allow_loopback)?;
    root.set_fragment(None);
    let origin = req.same_origin_only.then(|| root.origin());
    let depth_limit = req.max_depth.min(2);
    let extensions: HashSet<_> = req
        .extensions
        .iter()
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect();
    let mut queued = HashSet::from([root.clone()]);
    let mut visited = HashSet::new();
    let mut items_seen = HashSet::new();
    let mut pages = VecDeque::from([(root, 0)]);
    let mut result = SiteGrabResult {
        scanned_pages: 0,
        items: Vec::new(),
    };

    while let Some((url, depth)) = pages.pop_front() {
        if visited.contains(&url) {
            continue;
        }
        let page = fetch_page(client, &url, origin.as_ref(), allow_loopback).await;
        let (base, html) = match page {
            Ok(page) => page,
            Err(error) if depth == 0 => return Err(error),
            Err(_) => continue,
        };
        if !visited.insert(base.clone()) {
            continue;
        }
        result.scanned_pages += 1;
        let mut candidates = Vec::new();
        for (raw, kind) in links(&String::from_utf8_lossy(&html)) {
            if raw.chars().any(char::is_control) {
                continue;
            }
            let Some(mut url) = base
                .join(raw.trim())
                .ok()
                .and_then(|url| rss::checked_url(url.as_str(), allow_loopback).ok())
            else {
                continue;
            };
            url.set_fragment(None);
            if origin
                .as_ref()
                .is_some_and(|origin| *origin != url.origin())
            {
                continue;
            }
            let filename = url_filename(&url);
            let ext = filename
                .as_deref()
                .and_then(|name| name.rsplit_once('.'))
                .map(|(_, ext)| ext.to_ascii_lowercase());
            let page_link = kind == "link"
                && ext.as_deref().is_none_or(|ext| {
                    matches!(
                        ext,
                        "html" | "htm" | "xhtml" | "php" | "asp" | "aspx" | "jsp"
                    )
                });
            if page_link
                && depth < depth_limit
                && queued.len() < MAX_PAGES
                && queued.insert(url.clone())
            {
                pages.push_back((url.clone(), depth + 1));
            }
            let Some(filename) = filename else { continue };
            if (page_link && extensions.is_empty())
                || (!extensions.is_empty()
                    && !ext.as_ref().is_some_and(|ext| extensions.contains(ext)))
            {
                continue;
            }
            let matches = |pattern: &str| {
                rss::wildcard_matches(pattern, &filename)
                    || rss::wildcard_matches(pattern, url.as_str())
            };
            if (!req.include_pattern.trim().is_empty() && !matches(&req.include_pattern))
                || (!req.exclude_pattern.trim().is_empty() && matches(&req.exclude_pattern))
                || !items_seen.insert(url.clone())
            {
                continue;
            }
            candidates.push((url, filename, kind.to_owned()));
        }
        let checked = stream::iter(candidates)
            .map(|(url, filename, kind)| async move {
                rss::check_http_host(&url, allow_loopback).await.ok()?;
                Some((url, filename, kind))
            })
            .buffered(16)
            .filter_map(std::future::ready);
        let accepted = checked
            .take(MAX_ITEMS - result.items.len())
            .collect::<Vec<_>>()
            .await;
        let source_page: String = base.into();
        result.items.extend(
            accepted
                .into_iter()
                .map(|(url, filename, kind)| SiteGrabItem {
                    url: url.into(),
                    filename,
                    kind,
                    source_page: source_page.clone(),
                }),
        );
        if result.items.len() == MAX_ITEMS {
            break;
        }
    }
    Ok(result)
}

async fn fetch_page(
    client: &FeedHttpClient,
    url: &Url,
    origin: Option<&url::Origin>,
    allow_loopback: bool,
) -> Result<(Url, Vec<u8>), String> {
    let response =
        rss::open_response(client, url.as_str(), allow_loopback, MAX_PAGE_BYTES, origin).await?;
    if response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|raw| raw.to_str().ok())
        .is_some_and(|raw| {
            let mime = raw.split(';').next().unwrap_or_default().trim();
            !mime.eq_ignore_ascii_case("text/html")
                && !mime.eq_ignore_ascii_case("application/xhtml+xml")
        })
    {
        return Err("site grab URL did not return HTML".into());
    }
    let mut base = response.url().clone();
    base.set_fragment(None);
    Ok((base, rss::read_response(response, MAX_PAGE_BYTES).await?))
}

fn links(mut html: &str) -> Vec<(String, &'static str)> {
    let mut result = Vec::new();
    while result.len() < MAX_LINKS_PER_PAGE {
        let Some((_, rest)) = html.split_once('<') else {
            break;
        };
        if let Some(comment) = rest.strip_prefix("!--") {
            let Some((_, tail)) = comment.split_once("-->") else {
                break;
            };
            html = tail;
            continue;
        }
        if !rest
            .as_bytes()
            .first()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || matches!(ch, b'!' | b'?'))
        {
            html = rest;
            continue;
        }
        let mut quote = None;
        let Some(end) = rest.bytes().position(|ch| {
            match quote {
                Some(mark) if mark == ch => quote = None,
                None if matches!(ch, b'\'' | b'"') => quote = Some(ch),
                None if ch == b'>' => return true,
                _ => {}
            }
            false
        }) else {
            break;
        };
        let tag = &rest[..end];
        html = &rest[end + 1..];
        let name_len = tag
            .find(|ch: char| ch.is_ascii_whitespace() || ch == '/')
            .unwrap_or(tag.len());
        let name = tag[..name_len].to_ascii_lowercase();
        let (attr_name, kind) = match name.as_str() {
            "a" => (b"href".as_slice(), "link"),
            "source" => (b"src".as_slice(), "source"),
            "audio" => (b"src".as_slice(), "audio"),
            "video" => (b"src".as_slice(), "video"),
            "img" => (b"src".as_slice(), "image"),
            "script" | "style" | "textarea" | "title" => {
                let closing = format!("</{name}");
                let Some(end) = html
                    .as_bytes()
                    .windows(closing.len())
                    .position(|candidate| candidate.eq_ignore_ascii_case(closing.as_bytes()))
                else {
                    break;
                };
                html = &html[end + 1..];
                continue;
            }
            _ => continue,
        };
        let tag = BytesStart::from_content(tag, name_len);
        if let Some(value) = tag
            .html_attributes()
            .flatten()
            .find(|attr| attr.key.as_ref().eq_ignore_ascii_case(attr_name))
            .and_then(|attr| {
                let raw = std::str::from_utf8(&attr.value).ok()?;
                Some(
                    quick_xml::escape::unescape(raw)
                        .map_or_else(|_| raw.to_owned(), |value| value.into_owned()),
                )
            })
        {
            result.push((value, kind));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rss::tests::serve_routes;

    fn html(body: impl Into<String>) -> (&'static str, String, Vec<u8>) {
        (
            "200 OK",
            "Content-Type: text/html\r\n".into(),
            body.into().into_bytes(),
        )
    }

    fn request(url: String, max_depth: u8) -> SiteGrabRequest {
        SiteGrabRequest {
            url,
            max_depth,
            same_origin_only: true,
            extensions: vec![" .ZIP ".into()],
            include_pattern: "*keep*".into(),
            exclude_pattern: "*skip*".into(),
        }
    }

    #[tokio::test]
    async fn crawls_depth_and_filters_links_and_origins() {
        let (cross, _cross_server) = serve_routes(|_| html("<a href=/keep-cross.zip>")).await;
        let (base, _server) = serve_routes(move |path| match path {
            "/" => html(format!(
                r#"<script>let a = '<a href=/keep-script.zip>';</script>
                <A HREF=level1.html><img src=/keep.png><video src='/keep-0.zip'>
                <source src=/keep-skip.zip><a href={cross}/keep-other.zip><a href=go.html>"#
            )),
            "/level1.html" => html("<audio src=/keep-1.zip><a href=level2.html>"),
            "/level2.html" => html("<a href=/keep-2.zip><a href=level3.html>"),
            "/go.html" => (
                "302 Found",
                format!("Location: {cross}/page.html\r\n"),
                Vec::new(),
            ),
            _ => panic!("crawl exceeded depth"),
        })
        .await;
        let client = rss::feed_http_client(true).unwrap();
        for depth in [0, 1, 2, 100] {
            let result = grab_site(&client, &request(base.clone(), depth), true)
                .await
                .unwrap();
            assert_eq!(result.scanned_pages, usize::from(depth.min(2)) + 1);
            assert_eq!(result.items.len(), result.scanned_pages);
            assert_eq!(result.items[0].filename, "keep-0.zip");
        }
        assert!(grab_site(&client, &request(base.clone(), 2), false)
            .await
            .is_err());
        let mut req = request(base, 1);
        req.same_origin_only = false;
        let result = grab_site(&client, &req, true).await.unwrap();
        assert!(result
            .items
            .iter()
            .any(|item| item.filename == "keep-other.zip"));
        assert!(result
            .items
            .iter()
            .any(|item| item.filename == "keep-cross.zip"));
    }

    #[tokio::test]
    async fn caps_pages_and_items() {
        let (base, _server) = serve_routes(|path| {
            if path == "/" {
                return html(
                    (0..40)
                        .map(|i| format!("<a href=/{i}.html>"))
                        .collect::<String>(),
                );
            }
            html(if path == "/0.html" {
                (0..550)
                    .map(|i| format!("<img src=/keep-{i}.zip>"))
                    .collect::<String>()
            } else {
                String::new()
            })
        })
        .await;
        let client = rss::feed_http_client(true).unwrap();
        let mut req = request(base, 2);
        let result = grab_site(&client, &req, true).await.unwrap();
        assert_eq!(result.items.len(), 500);
        req.include_pattern = "nothing-matches".into();
        let result = grab_site(&client, &req, true).await.unwrap();
        assert_eq!(result.scanned_pages, 25);
        assert!(result.items.is_empty());
    }
}
