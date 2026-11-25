use crate::{EngineError, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, AUTHORIZATION,
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, IF_RANGE,
    LAST_MODIFIED, LOCATION, ORIGIN, PROXY_AUTHORIZATION, RANGE, REFERER, USER_AGENT,
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
    #[serde(default)]
    pub etag_present: bool,
    #[serde(default)]
    pub etag_raw: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    pub(crate) fn from_response(response: &Response) -> Self {
        let etags: Vec<_> = response.headers().get_all(ETAG).iter().collect();
        let etag_present = !etags.is_empty();
        let etag = (etags.len() == 1)
            .then(|| etags[0].to_str().ok().map(str::to_owned))
            .flatten();
        let etag_raw = (etag_present && etag.is_none()).then(|| {
            etags
                .iter()
                .map(|value| hex::encode(value.as_bytes()))
                .collect::<Vec<_>>()
                .join(",")
        });
        let last_modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self {
            etag,
            etag_present,
            etag_raw,
            last_modified,
        }
    }

    pub(crate) fn has_strong_etag(&self) -> bool {
        self.etag
            .as_deref()
            .is_some_and(|tag| tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"'))
    }

    pub(crate) fn if_range(&self) -> Option<&str> {
        if self.has_strong_etag() {
            return self.etag.as_deref();
        }
        if self.etag_present || self.etag.is_some() {
            return None;
        }
        self.last_modified
            .as_deref()
            .filter(|value| is_valid_http_date(value))
    }

    pub(crate) fn changed(&self, response: &Response, conditional: bool) -> bool {
        let actual = Self::from_response(response);
        let changed = |saved: &Option<String>, received: &Option<String>| {
            saved
                .as_ref()
                .zip(received.as_ref())
                .is_some_and(|(saved, received)| saved != received)
        };
        let raw_changed = (self.etag_present && actual.etag_present)
            && (self.etag != actual.etag || self.etag_raw != actual.etag_raw);
        raw_changed
            || changed(&self.etag, &actual.etag)
            || changed(&self.last_modified, &actual.last_modified)
            || (conditional
                && (self.if_range().is_none() || self.if_range() != actual.if_range_for(self)))
    }

    fn if_range_for(&self, saved: &Self) -> Option<&str> {
        if saved.has_strong_etag() {
            self.etag.as_deref()
        } else if self.etag_present || self.etag.is_some() {
            None
        } else {
            self.last_modified.as_deref()
        }
    }
}

fn is_valid_http_date(value: &str) -> bool {
    let valid_imf = value.ends_with(" GMT") && chrono::DateTime::parse_from_rfc2822(value).is_ok();
    let valid_rfc850 = value.ends_with(" GMT")
        && chrono::NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT").is_ok();
    let valid_asctime =
        chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y").is_ok();
    valid_imf || valid_rfc850 || valid_asctime
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
            let value = if name == REFERER {
                if value.trim().is_empty() {
                    continue;
                }
                let sanitized = sanitize_http_url_credentials(value).ok_or_else(|| {
                    EngineError::InvalidRequest("invalid referer header value".into())
                })?;
                HeaderValue::from_str(&sanitized).map_err(|_| {
                    EngineError::InvalidRequest("invalid referer header value".into())
                })?
            } else {
                HeaderValue::from_str(value).map_err(|_| {
                    EngineError::InvalidRequest(format!("invalid {name} header value"))
                })?
            };
            request_headers.insert(name, value);
        }
        if let Some(referrer) = referrer.filter(|raw| !raw.trim().is_empty()) {
            let sanitized = sanitize_http_url_credentials(referrer).ok_or_else(|| {
                EngineError::InvalidRequest("invalid referrer header value".into())
            })?;
            let referrer = HeaderValue::from_str(&sanitized)
                .map_err(|_| EngineError::InvalidRequest("invalid referrer header value".into()))?;
            request_headers.insert(REFERER, referrer);
        }
        request_headers.entry(USER_AGENT).or_insert(HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        ));
        request_headers
            .entry(ACCEPT)
            .or_insert(HeaderValue::from_static("*/*"));
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
            let next = validate_redirect_target(&url, location, self.allow_loopback)?;
            apply_redirect_header_policy(&mut headers, &initial_origin, &url, &next);
            url = next;
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

        let mut response = self
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
            StatusCode::FORBIDDEN
            | StatusCode::BAD_REQUEST
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::RANGE_NOT_SATISFIABLE => {
                drop(response);
                response = self.request(source, Method::GET, None, None).await?;
                (full_length(&response)?, false)
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

    pub(crate) async fn get_or_restart(
        &self,
        source: &Source,
        range: Option<(u64, u64)>,
        total: Option<u64>,
        validators: &Validators,
        conditional: bool,
    ) -> Result<GetOutcome> {
        let if_range = if conditional {
            let Some(value) = validators.if_range() else {
                return Ok(GetOutcome::Restart(RestartReason::RepresentationChanged));
            };
            Some(value)
        } else {
            None
        };
        let response = self.request(source, Method::GET, range, if_range).await?;
        if let Some((start, last)) = range {
            match response.status() {
                StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE => {
                    let unchanged = if_range.is_none()
                        && !validators.changed(&response, false)
                        && (response.status() == StatusCode::RANGE_NOT_SATISFIABLE
                            || !total
                                .zip(length(&response)?)
                                .is_some_and(|(old, new)| old != new));
                    let reason = if unchanged {
                        RestartReason::RangeUnsupported
                    } else {
                        RestartReason::RepresentationChanged
                    };
                    return Ok(GetOutcome::Restart(reason));
                }
                StatusCode::PRECONDITION_FAILED => {
                    return Ok(GetOutcome::Restart(RestartReason::RepresentationChanged));
                }
                StatusCode::PARTIAL_CONTENT => {}
                status => return Err(EngineError::Status(status)),
            }
            ensure_identity(&response)?;
            if validators.changed(&response, if_range.is_some())
                || total != content_range(&response, start, last)?
            {
                return Ok(GetOutcome::Restart(RestartReason::RepresentationChanged));
            }
        } else {
            let length = full_length(&response)?;
            ensure_identity(&response)?;
            if validators.changed(&response, false)
                || total.zip(length).is_some_and(|(old, new)| old != new)
            {
                return Ok(GetOutcome::Restart(RestartReason::RepresentationChanged));
            }
        }
        Ok(GetOutcome::Response(response))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartReason {
    RepresentationChanged,
    RangeUnsupported,
}

pub(crate) enum GetOutcome {
    Response(Response),
    Restart(RestartReason),
}

pub(crate) fn sanitize_http_url_credentials(raw: &str) -> Option<String> {
    let cleaned = raw.replace(['\r', '\n', '\0'], "");
    let mut parsed = validate_http_url(&cleaned).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
    }
    parsed.set_fragment(None);
    Some(parsed.into())
}

fn apply_redirect_header_policy(
    headers: &mut HeaderMap,
    initial_origin: &url::Origin,
    previous: &Url,
    next: &Url,
) {
    if previous.scheme() == "https" && next.scheme() == "http" {
        headers.remove(REFERER);
        headers.remove(ORIGIN);
    }
    if next.origin() != *initial_origin {
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

fn full_length(response: &Response) -> Result<Option<u64>> {
    match response.status() {
        StatusCode::OK => length(response),
        StatusCode::PARTIAL_CONTENT => {
            // A plain GET can only use a 206 response that covers the complete representation.
            let incomplete = || EngineError::InvalidResponse("incomplete response to plain GET");
            let total = length(response)?
                .filter(|&total| total > 0)
                .ok_or_else(incomplete)?;
            if content_range(response, 0, total - 1)? != Some(total) {
                return Err(incomplete());
            }
            Ok(Some(total))
        }
        status => Err(EngineError::Status(status)),
    }
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

    #[test]
    fn redirect_strips_page_headers_on_https_downgrade_and_credentials_from_referrer() {
        let https_origin = validate_http_url("https://cdn.example.com/start").unwrap();
        let https_other = validate_http_url("https://mirror.example.com/file").unwrap();
        let http_other = validate_http_url("http://mirror.example.com/file").unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_static("sid=secret"));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(ORIGIN, HeaderValue::from_static("https://app.example.com"));
        let clean_ref =
            sanitize_http_url_credentials("https://user:pass@app.example.com/page#secret").unwrap();
        assert_eq!(clean_ref, "https://app.example.com/page");
        headers.insert(REFERER, HeaderValue::from_str(&clean_ref).unwrap());

        let header_src = Source::new(
            "https://cdn.example.com/file",
            &[(
                "Referer".into(),
                "https://alice:pw@ref.example.com/doc#frag".into(),
            )]
            .into(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            header_src
                .headers
                .get(REFERER)
                .and_then(|v| v.to_str().ok()),
            Some("https://ref.example.com/doc")
        );

        apply_redirect_header_policy(
            &mut headers,
            &https_origin.origin(),
            &https_origin,
            &https_other,
        );
        assert!(headers.get(COOKIE).is_none());
        assert!(headers.get(AUTHORIZATION).is_none());
        assert_eq!(
            headers.get(REFERER).and_then(|v| v.to_str().ok()),
            Some("https://app.example.com/page")
        );
        assert_eq!(headers.get(ORIGIN).unwrap(), "https://app.example.com");

        apply_redirect_header_policy(
            &mut headers,
            &https_origin.origin(),
            &https_other,
            &http_other,
        );
        assert!(headers.get(REFERER).is_none());
        assert!(headers.get(ORIGIN).is_none());
    }

    #[test]
    fn validators_block_weak_or_non_ascii_etag_fallback_and_validate_http_dates() {
        let weak = Validators {
            etag: Some("W/\"weak\"".into()),
            etag_present: true,
            etag_raw: None,
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };
        assert_eq!(weak.if_range(), None);

        let non_ascii = Validators {
            etag: None,
            etag_present: true,
            etag_raw: Some("22ff22".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };
        assert_eq!(non_ascii.if_range(), None);

        let invalid_date = Validators {
            etag: None,
            etag_present: false,
            etag_raw: None,
            last_modified: Some("not-a-valid-date".into()),
        };
        assert_eq!(invalid_date.if_range(), None);

        let valid_date = Validators {
            etag: None,
            etag_present: false,
            etag_raw: None,
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };
        assert_eq!(valid_date.if_range(), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
    }
}
