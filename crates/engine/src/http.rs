use crate::{EngineError, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_DISPOSITION,
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, IF_RANGE, LAST_MODIFIED,
    LOCATION, PROXY_AUTHORIZATION, RANGE, REFERER,
};
use reqwest::{Client, Method, Response, StatusCode, Version};
use serde::{Deserialize, Serialize};
use slipstream_protocol::{is_private_or_loopback_ip, sanitize_filename, validate_http_url};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use url::{Host, Url};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    pub(crate) fn from_response(response: &Response) -> Self {
        let read = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        Self {
            etag: read(ETAG),
            last_modified: read(LAST_MODIFIED),
        }
    }

    pub(crate) fn if_range(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|tag| tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"'))
            .or(self.last_modified.as_deref())
    }

    pub(crate) fn changed(&self, response: &Response, conditional: bool) -> bool {
        let actual = Self::from_response(response);
        let changed = |saved: &Option<String>, received: &Option<String>| {
            saved
                .as_ref()
                .zip(received.as_ref())
                .is_some_and(|(saved, received)| saved != received)
        };
        changed(&self.etag, &actual.etag)
            || changed(&self.last_modified, &actual.last_modified)
            || (conditional && self.if_range() != actual.if_range_for(self))
    }

    fn if_range_for(&self, saved: &Self) -> Option<&str> {
        if saved.etag.as_deref() == saved.if_range() {
            self.etag.as_deref()
        } else {
            self.last_modified.as_deref()
        }
    }
}

pub(crate) struct Metadata {
    pub total: Option<u64>,
    pub ranges: bool,
    pub validators: Validators,
    pub filename: Option<String>,
    pub protocol: &'static str,
}

#[derive(Clone)]
pub(crate) struct Source {
    pub url: Url,
    pub headers: HeaderMap,
}

pub(crate) struct Http {
    client: Client,
    allow_loopback: bool,
}

struct CheckedDns {
    allow_loopback: bool,
}

impl Resolve for CheckedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(std::io::Error::other("DNS returned no addresses").into());
            }
            for addr in &addrs {
                check_ip(addr.ip(), allow_loopback)?;
            }
            // Reqwest dials these exact addresses. No second DNS lookup occurs.
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn check_ip(ip: IpAddr, allow_loopback: bool) -> Result<()> {
    let mapped = matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some());
    if mapped || (is_private_or_loopback_ip(ip) && !(allow_loopback && ip.is_loopback())) {
        return Err(EngineError::BlockedAddress(ip));
    }
    Ok(())
}

fn check_destination(url: Url, allow_loopback: bool) -> Result<Url> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EngineError::InvalidRequest(
            "URL credentials are unsupported".into(),
        ));
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => check_ip(ip.into(), allow_loopback)?,
        Some(Host::Ipv6(ip)) => check_ip(ip.into(), allow_loopback)?,
        _ => {}
    }
    Ok(url)
}

/// Resolve a redirect location and apply the URL and literal IP policy.
/// The HTTP client also applies the IP policy to every DNS result before dialing.
pub fn validate_redirect_target(base: &Url, location: &str, allow_loopback: bool) -> Result<Url> {
    if location.chars().any(char::is_control) {
        return Err(slipstream_protocol::UrlSecurityError::InvalidFormat.into());
    }
    let target = base
        .join(location)
        .map_err(|error| slipstream_protocol::UrlSecurityError::ParseError(error.to_string()))?;
    check_destination(validate_http_url(target.as_str())?, allow_loopback)
}

impl Source {
    pub(crate) fn new(
        raw_url: &str,
        headers: &HashMap<String, String>,
        referrer: Option<&str>,
        allow_loopback: bool,
    ) -> Result<Self> {
        let url = check_destination(validate_http_url(raw_url)?, allow_loopback)?;
        let mut request_headers = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| EngineError::InvalidRequest("invalid HTTP header name".into()))?;
            // The engine owns routing, framing, compression and preconditions.
            if name.as_str().starts_with("if-")
                || matches!(
                    name.as_str(),
                    "host"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "proxy-connection"
                        | "expect"
                        | "range"
                        | "accept-encoding"
                        | "keep-alive"
                )
            {
                continue;
            }
            let value = HeaderValue::from_str(value)
                .map_err(|_| EngineError::InvalidRequest(format!("invalid {name} header value")))?;
            request_headers.insert(name, value);
        }
        if let Some(referrer) = referrer {
            let referrer = HeaderValue::from_str(referrer)
                .map_err(|_| EngineError::InvalidRequest("invalid referrer header value".into()))?;
            request_headers.insert(REFERER, referrer);
        }
        request_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        Ok(Self {
            url,
            headers: request_headers,
        })
    }
}

impl Http {
    pub(crate) fn new(allow_loopback: bool, timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_deflate()
            .no_brotli()
            .no_zstd()
            .referer(false)
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .dns_resolver(Arc::new(CheckedDns { allow_loopback }))
            .build()
            .map_err(EngineError::Http)?;
        Ok(Self {
            client,
            allow_loopback,
        })
    }

    async fn request(
        &self,
        source: &Source,
        method: Method,
        range: Option<(u64, u64)>,
        if_range: Option<&str>,
    ) -> Result<Response> {
        let mut url = source.url.clone();
        let initial_origin = url.origin();
        let mut headers = source.headers.clone();
        if let Some((start, last)) = range {
            headers.insert(
                RANGE,
                HeaderValue::from_str(&format!("bytes={start}-{last}")).unwrap(),
            );
        }
        if let Some(value) = if_range {
            headers.insert(
                IF_RANGE,
                HeaderValue::from_str(value).map_err(|_| {
                    EngineError::InvalidResponse("invalid saved If-Range validator")
                })?,
            );
        }
        for hop in 0..=10 {
            let response = self
                .client
                .request(method.clone(), url.clone())
                .headers(headers.clone())
                .send()
                .await
                .map_err(request_error)?;
            if !matches!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                return Ok(response);
            }
            if hop == 10 {
                return Err(EngineError::InvalidResponse("too many redirects"));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(EngineError::InvalidResponse(
                    "missing or invalid redirect location",
                ))?;
            url = validate_redirect_target(&url, location, self.allow_loopback)?;
            if url.origin() != initial_origin {
                for name in [
                    COOKIE,
                    HeaderName::from_static("cookie2"),
                    AUTHORIZATION,
                    PROXY_AUTHORIZATION,
                ] {
                    headers.remove(name);
                }
            }
        }
        unreachable!("request loop returns after its final hop")
    }

    pub(crate) async fn probe(&self, source: &Source, force_get: bool) -> Result<Metadata> {
        let mut head_name = None;
        if !force_get {
            let head = match self.request(source, Method::HEAD, None, None).await {
                Ok(response) => Some(response),
                Err(error) if error.is_transient() => None,
                Err(error) => return Err(error),
            };
            if let Some(response) = head.filter(|response| response.status() == StatusCode::OK) {
                ensure_identity(&response)?;
                head_name = filename(&response)?;
                let total = length(&response)?;
                let range_hint = response
                    .headers()
                    .get_all("accept-ranges")
                    .iter()
                    .any(|value| {
                        value.to_str().is_ok_and(|value| {
                            value
                                .split(',')
                                .any(|unit| unit.trim().eq_ignore_ascii_case("bytes"))
                        })
                    });
                if total == Some(0) || (total.is_some() && range_hint) {
                    return Ok(Metadata {
                        total,
                        ranges: range_hint,
                        validators: Validators::from_response(&response),
                        filename: head_name,
                        protocol: protocol(&response),
                    });
                }
            }
        }

        let response = self
            .request(source, Method::GET, Some((0, 0)), None)
            .await?;
        let (total, ranges) = match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let total = content_range(&response, 0, 0)?;
                (total, total.is_some())
            }
            StatusCode::OK => (length(&response)?, false),
            StatusCode::RANGE_NOT_SATISFIABLE
                if response
                    .headers()
                    .get(CONTENT_RANGE)
                    .is_some_and(|range| range == "bytes */0") =>
            {
                (Some(0), true)
            }
            status => return Err(EngineError::Status(status)),
        };
        if response.status().is_success() {
            ensure_identity(&response)?;
        }
        Ok(Metadata {
            total,
            ranges,
            validators: Validators::from_response(&response),
            filename: filename(&response)?.or(head_name),
            protocol: protocol(&response),
        })
    }

    pub(crate) async fn get(
        &self,
        source: &Source,
        range: Option<(u64, u64)>,
        total: Option<u64>,
        validators: &Validators,
        conditional: bool,
    ) -> Result<Response> {
        let if_range = conditional.then(|| validators.if_range()).flatten();
        let response = self.request(source, Method::GET, range, if_range).await?;
        if let Some((start, last)) = range {
            match response.status() {
                StatusCode::OK
                | StatusCode::PRECONDITION_FAILED
                | StatusCode::RANGE_NOT_SATISFIABLE => return Err(EngineError::RestartRequired),
                StatusCode::PARTIAL_CONTENT => {}
                status => return Err(EngineError::Status(status)),
            }
            ensure_identity(&response)?;
            if validators.changed(&response, if_range.is_some())
                || total != content_range(&response, start, last)?
            {
                return Err(EngineError::RestartRequired);
            }
        } else {
            if response.status() != StatusCode::OK {
                return Err(EngineError::Status(response.status()));
            }
            ensure_identity(&response)?;
            if validators.changed(&response, false)
                || total
                    .zip(length(&response)?)
                    .is_some_and(|(old, new)| old != new)
            {
                return Err(EngineError::RestartRequired);
            }
        }
        Ok(response)
    }
}

pub(crate) fn request_error(error: reqwest::Error) -> EngineError {
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(EngineError::BlockedAddress(ip)) = cause.downcast_ref::<EngineError>() {
            return EngineError::BlockedAddress(*ip);
        }
        source = cause.source();
    }
    EngineError::Http(error.without_url())
}

pub(crate) fn protocol(response: &Response) -> &'static str {
    match response.version() {
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

fn length(response: &Response) -> Result<Option<u64>> {
    response
        .headers()
        .get(CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|length| *length <= i64::MAX as u64)
                .ok_or(EngineError::InvalidResponse("invalid Content-Length"))
        })
        .transpose()
}

fn ensure_identity(response: &Response) -> Result<()> {
    for encoding in response.headers().get_all(CONTENT_ENCODING) {
        if !encoding.as_bytes().eq_ignore_ascii_case(b"identity") {
            return Err(EngineError::InvalidResponse("unsupported Content-Encoding"));
        }
    }
    Ok(())
}

fn content_range(response: &Response, start: u64, last: u64) -> Result<Option<u64>> {
    let malformed = || EngineError::InvalidResponse("unexpected Content-Range");
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(malformed)?;
    let (unit, value) = value.split_once(' ').ok_or_else(malformed)?;
    let (bounds, size) = value.split_once('/').ok_or_else(malformed)?;
    let (first, end) = bounds.split_once('-').ok_or_else(malformed)?;
    if !unit.eq_ignore_ascii_case("bytes")
        || first.parse::<u64>().ok() != Some(start)
        || end.parse::<u64>().ok() != Some(last)
        || last < start
        || length(response)?.is_some_and(|length| length != last - start + 1)
    {
        return Err(malformed());
    }
    if size == "*" {
        return Ok(None);
    }
    size.parse::<u64>()
        .ok()
        .filter(|size| *size > last && *size <= i64::MAX as u64)
        .map(Some)
        .ok_or_else(malformed)
}

pub(crate) fn filename(response: &Response) -> Result<Option<String>> {
    response
        .headers()
        .get(CONTENT_DISPOSITION)
        .map(|value| {
            let value = std::str::from_utf8(value.as_bytes())
                .map_err(|_| EngineError::InvalidResponse("invalid Content-Disposition text"))?;
            disposition_filename(value)
        })
        .transpose()
        .map(Option::flatten)
}

fn disposition_filename(value: &str) -> Result<Option<String>> {
    let mut quoted = false;
    let mut escaped = false;
    let mut plain = None;
    let mut extended = None;
    for field in value
        .split(|ch| {
            if escaped {
                escaped = false;
                return false;
            }
            match ch {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                ';' if !quoted => return true,
                _ => {}
            }
            false
        })
        .skip(1)
    {
        let Some((key, value)) = field.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key.trim().eq_ignore_ascii_case("filename*") {
            extended = value.split_once('\'').and_then(|(charset, rest)| {
                if !charset.eq_ignore_ascii_case("utf-8") {
                    return None;
                }
                let (_, name) = rest.split_once('\'')?;
                decode_percent(name)
            });
        } else if key.trim().eq_ignore_ascii_case("filename") {
            plain = Some(
                value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value),
            );
        }
    }
    match extended.as_deref().or(plain) {
        Some(raw) => sanitize_filename(raw).map(Some).map_err(Into::into),
        None => Ok(None),
    }
}

fn decode_percent(value: &str) -> Option<String> {
    let mut input = value.as_bytes().iter().copied();
    let mut bytes = Vec::with_capacity(value.len());
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = char::from(input.next()?).to_digit(16)?;
            let lo = char::from(input.next()?).to_digit(16)?;
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

pub(crate) fn filename_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(decode_percent)
        .and_then(|name| sanitize_filename(&name).ok())
        .unwrap_or_else(|| "download".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_filenames_are_sanitized() {
        assert_eq!(
            disposition_filename(
                "attachment; filename=\"fallback\"; filename*=UTF-8'en'caf%C3%A9.zip"
            )
            .unwrap()
            .as_deref(),
            Some("café.zip")
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"a;b.zip\"")
                .unwrap()
                .as_deref(),
            Some("a;b.zip")
        );
        for bad in ["../evil", "CON.exe", "x:Zone.Identifier", "\u{202e}evil"] {
            assert!(disposition_filename(&format!("attachment; filename=\"{bad}\"")).is_err());
        }
        assert!(disposition_filename("attachment; filename*=UTF-8''..%2Fevil").is_err());
    }

    #[test]
    fn public_origin_cannot_redirect_to_restricted_ip_addresses() {
        let origin = validate_http_url("https://example.com/dir/file").unwrap();
        assert_eq!(
            validate_redirect_target(&origin, "../public", false)
                .unwrap()
                .as_str(),
            "https://example.com/public"
        );
        for host in [
            "127.0.0.1",
            "[::1]",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "[::ffff:8.8.8.8]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(matches!(
                validate_redirect_target(&origin, &format!("http://{host}"), false),
                Err(EngineError::BlockedAddress(_))
            ));
        }
        assert!(validate_redirect_target(&origin, "http://127.0.0.1", true).is_ok());
        assert!(validate_redirect_target(&origin, "http://10.0.0.1", true).is_err());
    }
}
