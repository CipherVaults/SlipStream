use crate::{EngineError, EngineProxyConfig, Result};
use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE,
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, IF_RANGE,
    LAST_MODIFIED, LOCATION, ORIGIN, RANGE, REFERER, RETRY_AFTER, SET_COOKIE, USER_AGENT,
};
use reqwest::{Client, Method, Response, StatusCode, Version};
use serde::{Deserialize, Serialize};
use slipstream_protocol::{is_private_or_loopback_ip, sanitize_filename, validate_http_url};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio_util::io::StreamReader;
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
            .filter(|value| parse_http_date(value).is_some())
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

fn parse_http_date(value: &str) -> Option<DateTime<Utc>> {
    if value.ends_with(" GMT") {
        return DateTime::parse_from_rfc2822(value)
            .map(|date| date.to_utc())
            .or_else(|_| {
                NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
                    .map(|date| date.and_utc())
            })
            .ok();
    }
    NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|date| date.and_utc())
}

pub(crate) struct Metadata {
    pub total: Option<u64>,
    pub ranges: bool,
    pub validators: Validators,
    pub filename: Option<String>,
    pub protocol: &'static str,
    pub response: Option<Response>,
    pub connect_rtt_ms: Option<u32>,
}

#[derive(Clone, Copy)]
struct ConnectRtt(u32);

pub(crate) fn connect_rtt_ms(response: &Response) -> Option<u32> {
    response.extensions().get::<ConnectRtt>().map(|rtt| rtt.0)
}

#[derive(Clone)]
pub(crate) struct Source {
    pub url: Url,
    pub headers: HeaderMap,
}

pub(crate) struct Http {
    clients: HttpClients,
    strict_clients: Option<HttpClients>,
    proxy_hosts: Arc<HashSet<String>>,
    allow_loopback: bool,
}

struct HttpClients {
    client: Client,
    ipv4_client: Client,
    http1_client: Client,
    http1_ipv4_client: Client,
}

impl HttpClients {
    fn select(&self, profile: FetchProfile) -> &Client {
        match (profile.ipv4_only, profile.http1_only) {
            (false, false) => &self.client,
            (true, false) => &self.ipv4_client,
            (false, true) => &self.http1_client,
            (true, true) => &self.http1_ipv4_client,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FetchProfile {
    pub(crate) ipv4_only: bool,
    pub(crate) http1_only: bool,
    strip_cookies: bool,
    strip_referer: bool,
}

impl FetchProfile {
    const IPV4_REQUIRED: &str = "IPv4-only requests require an IPv4 destination";

    pub(crate) fn ipv4_unavailable(self, error: &EngineError) -> bool {
        self.ipv4_only
            && match error {
                EngineError::Http(error) => error.is_connect(),
                EngineError::InvalidRequest(message) => message == Self::IPV4_REQUIRED,
                _ => false,
            }
    }

    pub(crate) fn should_fallback_http1(self, error: &EngineError) -> bool {
        if self.http1_only {
            return false;
        }
        match error {
            EngineError::Status { status, .. } => status.as_u16() == 421,
            EngineError::Http(err) => {
                let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
                while let Some(cause) = current {
                    let msg = cause.to_string().to_ascii_lowercase();
                    if msg.contains("http2")
                        || msg.contains("h2")
                        || msg.contains("refused_stream")
                        || msg.contains("enhance_your_calm")
                        || msg.contains("protocol_error")
                        || msg.contains("goaway")
                        || msg.contains("reset by peer")
                    {
                        return true;
                    }
                    current = cause.source();
                }
                err.is_request()
            }
            _ => false,
        }
    }

    pub(crate) fn fallback(
        mut self,
        source: &Source,
        sent_cookie: bool,
        ipv4_exhausted: bool,
    ) -> Option<Self> {
        if !self.ipv4_only && !ipv4_exhausted {
            self.ipv4_only = true;
        } else if !self.strip_cookies && (sent_cookie || source.headers.contains_key(COOKIE)) {
            self.strip_cookies = true;
        } else if !self.strip_referer
            && [REFERER, ORIGIN]
                .iter()
                .any(|name| source.headers.contains_key(name))
        {
            self.strip_cookies = true;
            self.strip_referer = true;
        } else {
            return None;
        }
        Some(self)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeMode {
    HeadThenRange,
    Range,
    Full,
}

struct CheckedDns {
    allow_loopback: bool,
    ipv4_only: bool,
    proxy_hosts: Arc<HashSet<String>>,
}

#[derive(Clone)]
struct SentCookie;

impl Resolve for CheckedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        let ipv4_only = self.ipv4_only;
        let trusted_proxy = self
            .proxy_hosts
            .contains(name.as_str().trim_end_matches('.'));
        Box::pin(async move {
            let mut addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if !trusted_proxy {
                for addr in &addrs {
                    check_ip(addr.ip(), allow_loopback)?;
                }
            }
            if ipv4_only {
                addrs.retain(SocketAddr::is_ipv4);
            }
            if addrs.is_empty() {
                let message = if ipv4_only {
                    "DNS returned no IPv4 addresses"
                } else {
                    "DNS returned no addresses"
                };
                return Err(io::Error::other(message).into());
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

pub(crate) fn validate_download_url(raw: &str, allow_loopback: bool) -> Result<Url> {
    check_destination(validate_http_url(raw)?, allow_loopback)
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
    pub(crate) fn for_mirror(&self, raw: &str, allow_loopback: bool) -> Result<Self> {
        let url = validate_download_url(raw, allow_loopback)?;
        let mut headers = self.headers.clone();
        apply_redirect_header_policy(&mut headers, &self.url, &url);
        Ok(Self { url, headers })
    }

    pub(crate) fn new(
        raw_url: &str,
        headers: &HashMap<String, String>,
        referrer: Option<&str>,
        allow_loopback: bool,
    ) -> Result<Self> {
        let url = validate_download_url(raw_url, allow_loopback)?;
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
        request_headers
            .entry(ACCEPT_LANGUAGE)
            .or_insert(HeaderValue::from_static("en-US,en;q=0.9"));
        request_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        Ok(Self {
            url,
            headers: request_headers,
        })
    }
}

impl Http {
    pub(crate) fn new(
        allow_loopback: bool,
        timeout: Duration,
        config: &EngineProxyConfig,
    ) -> Result<Self> {
        let proxies = config.proxies()?;
        let proxy_hosts = Arc::new(proxies.hosts);
        let build = |ipv4_only, http1_only, proxy_hosts| {
            let mut builder = Client::builder()
                .use_rustls_tls()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .http1_title_case_headers()
                .no_gzip()
                .no_deflate()
                .no_brotli()
                .no_zstd()
                .referer(false)
                .connect_timeout(timeout)
                .read_timeout(timeout)
                .dns_resolver(Arc::new(CheckedDns {
                    allow_loopback,
                    ipv4_only,
                    proxy_hosts,
                }));
            if http1_only {
                builder = builder.http1_only();
            }
            for proxy in &proxies.rules {
                builder = builder.proxy(proxy.clone());
            }
            builder.build().map_err(EngineError::Http)
        };
        let client_set = |hosts: &Arc<HashSet<String>>| -> Result<HttpClients> {
            Ok(HttpClients {
                client: build(false, false, hosts.clone())?,
                ipv4_client: build(true, false, hosts.clone())?,
                http1_client: build(false, true, hosts.clone())?,
                http1_ipv4_client: build(true, true, hosts.clone())?,
            })
        };
        Ok(Self {
            clients: client_set(&proxy_hosts)?,
            strict_clients: (!proxy_hosts.is_empty())
                .then(|| client_set(&Arc::default()))
                .transpose()?,
            proxy_hosts,
            allow_loopback,
        })
    }

    fn client(&self, url: &Url, profile: FetchProfile) -> &Client {
        // Proxy endpoints may be private. If the destination itself names a proxy,
        // use strict DNS so a bypass rule cannot grant it that exception.
        match (url.host(), &self.strict_clients) {
            (Some(Host::Domain(host)), Some(strict))
                if self.proxy_hosts.contains(host.trim_end_matches('.')) =>
            {
                strict.select(profile)
            }
            _ => self.clients.select(profile),
        }
    }

    async fn request(
        &self,
        source: &Source,
        profile: FetchProfile,
        method: Method,
        range: Option<(u64, u64)>,
        if_range: Option<&str>,
    ) -> Result<Response> {
        let mut url = source.url.clone();
        let mut headers = if profile.strip_referer {
            [USER_AGENT, ACCEPT, ACCEPT_LANGUAGE, ACCEPT_ENCODING]
                .into_iter()
                .filter_map(|name| source.headers.get(&name).map(|value| (name, value.clone())))
                .collect()
        } else {
            source.headers.clone()
        };
        if profile.strip_cookies {
            headers.remove(COOKIE);
            headers.remove("cookie2");
        }
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
        let mut first_rtt = None;
        for hop in 0..=10 {
            if profile.ipv4_only && matches!(url.host(), Some(Host::Ipv6(_))) {
                return Err(EngineError::InvalidRequest(
                    FetchProfile::IPV4_REQUIRED.into(),
                ));
            }
            let started = Instant::now();
            let mut response = self
                .client(&url, profile)
                .request(method.clone(), url.clone())
                .headers(headers.clone())
                .send()
                .await
                .map_err(request_error)?;
            let rtt = *first_rtt.get_or_insert_with(|| {
                u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
            });
            if !matches!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                response.extensions_mut().insert(ConnectRtt(rtt));
                if response.status() == StatusCode::FORBIDDEN && headers.contains_key(COOKIE) {
                    response.extensions_mut().insert(SentCookie);
                }
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
            if !profile.strip_cookies && url.origin() == next.origin() {
                merge_redirect_cookies(&mut headers, response.headers(), &next);
            }
            apply_redirect_header_policy(&mut headers, &url, &next);
            url = next;
        }
        unreachable!("request loop returns after its final hop")
    }

    pub(crate) async fn probe(
        &self,
        source: &Source,
        profile: FetchProfile,
        mode: ProbeMode,
    ) -> Result<Metadata> {
        let mut head_name = None;
        let mut initial_rtt = None;
        if mode == ProbeMode::HeadThenRange {
            let head = match self
                .request(source, profile, Method::HEAD, None, None)
                .await
            {
                Ok(response) => Some(response),
                Err(error)
                    if error.is_transient()
                        && !profile.ipv4_unavailable(&error)
                        && !profile.should_fallback_http1(&error) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            initial_rtt = head.as_ref().and_then(connect_rtt_ms);
            if let Some(response) = head.filter(|response| {
                response.status() == StatusCode::OK && ensure_identity(response).is_ok()
            }) {
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
                        response: None,
                        connect_rtt_ms: initial_rtt,
                    });
                }
            }
        }

        if mode != ProbeMode::Full {
            let response = self
                .request(source, profile, Method::GET, Some((0, 0)), None)
                .await?;
            initial_rtt = initial_rtt.or_else(|| connect_rtt_ms(&response));
            let total = match response.status() {
                StatusCode::PARTIAL_CONTENT => {
                    ensure_identity(&response)?;
                    content_range(&response, 0, 0)?
                }
                StatusCode::OK if !response.headers().contains_key(CONTENT_RANGE) => {
                    return full_metadata(response, head_name, initial_rtt);
                }
                StatusCode::OK
                | StatusCode::FORBIDDEN
                | StatusCode::BAD_REQUEST
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::PRECONDITION_FAILED
                | StatusCode::NOT_IMPLEMENTED => None,
                StatusCode::RANGE_NOT_SATISFIABLE => response
                    .headers()
                    .get(CONTENT_RANGE)
                    .is_some_and(|range| range == "bytes */0")
                    .then_some(0),
                _ => return Err(status_error(&response)),
            };
            if total.is_some() {
                return Ok(Metadata {
                    total,
                    ranges: true,
                    validators: Validators::from_response(&response),
                    filename: filename(&response)?.or(head_name),
                    protocol: protocol(&response),
                    response: None,
                    connect_rtt_ms: initial_rtt,
                });
            }
        }
        let response = self
            .request(source, profile, Method::GET, None, None)
            .await?;
        full_metadata(response, head_name, initial_rtt)
    }

    pub(crate) async fn get_or_restart(
        &self,
        source: &Source,
        profile: FetchProfile,
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
        let response = self
            .request(source, profile, Method::GET, range, if_range)
            .await?;
        if let Some((start, last)) = range {
            match response.status() {
                StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE => {
                    let unchanged = !validators.changed(&response, false)
                        && (response.status() == StatusCode::RANGE_NOT_SATISFIABLE
                            || (if_range.is_none()
                                && (response.headers().contains_key(CONTENT_RANGE)
                                    || !total
                                        .zip(full_length(&response)?)
                                        .is_some_and(|(old, new)| old != new))));
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
                _ => return Err(status_error(&response)),
            }
            ensure_identity(&response)?;
            if validators.changed(&response, if_range.is_some())
                || total != content_range(&response, start, last)?
            {
                return Ok(GetOutcome::Restart(RestartReason::RepresentationChanged));
            }
        } else {
            let length = full_length(&response)?;
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

fn apply_redirect_header_policy(headers: &mut HeaderMap, previous: &Url, next: &Url) {
    if previous.origin() == next.origin() {
        return;
    }
    let mut safe = HeaderMap::new();
    for (name, value) in headers.iter() {
        if matches!(
            name.as_str(),
            "user-agent" | "accept" | "accept-encoding" | "accept-language" | "range" | "if-range"
        ) {
            safe.append(name.clone(), value.clone());
        }
    }
    if previous.scheme() == "https" && next.scheme() == "http" {
        headers.remove(REFERER);
    }
    let origin = headers
        .get(REFERER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| validate_http_url(value).ok())
        .and_then(|url| {
            HeaderValue::from_str(&format!("{}/", url.origin().ascii_serialization())).ok()
        });
    if let Some(origin) = origin {
        safe.insert(REFERER, origin);
    }
    *headers = safe;
}

fn merge_redirect_cookies(headers: &mut HeaderMap, response: &HeaderMap, next: &Url) {
    let now = Utc::now();
    let mut incoming = response
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| redirect_cookie(value, next, now))
        .peekable();
    if incoming.peek().is_none() {
        return;
    }
    let mut pairs: Vec<_> = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';').map(str::trim))
        .filter(|pair| !pair.is_empty())
        .collect();
    for (name, pair, expired) in incoming {
        pairs.retain(|existing| !existing.split_once('=').is_some_and(|(key, _)| key == name));
        if !expired {
            pairs.push(pair);
        }
    }
    if pairs.is_empty() {
        headers.remove(COOKIE);
    } else if let Ok(mut value) = HeaderValue::from_str(&pairs.join("; ")) {
        value.set_sensitive(true);
        headers.insert(COOKIE, value);
    }
}

fn redirect_cookie<'a>(
    value: &'a HeaderValue,
    next: &Url,
    now: DateTime<Utc>,
) -> Option<(&'a str, &'a str, bool)> {
    let mut fields = value.to_str().ok()?.split(';');
    let pair = fields.next()?.trim();
    let (name, _) = pair.split_once('=')?;
    HeaderName::from_bytes(name.as_bytes()).ok()?;
    let mut expired = false;
    for field in fields {
        let (attribute, value) = field.split_once('=').unwrap_or((field, ""));
        let value = value.trim();
        match attribute.trim().to_ascii_lowercase().as_str() {
            "secure" if next.scheme() != "https" => return None,
            "domain" => {
                let host = next.host_str()?.to_ascii_lowercase();
                let domain = value
                    .strip_prefix('.')
                    .unwrap_or(value)
                    .to_ascii_lowercase();
                let matches_suffix = matches!(next.host(), Some(Host::Domain(_)))
                    && host
                        .strip_suffix(&domain)
                        .is_some_and(|prefix| prefix.ends_with('.'));
                if domain.is_empty() || (host != domain && !matches_suffix) {
                    return None;
                }
            }
            "path" => {
                let matches_path = next.path().strip_prefix(value).is_some_and(|suffix| {
                    suffix.is_empty() || value.ends_with('/') || suffix.starts_with('/')
                });
                if value.is_empty() || !matches_path {
                    return None;
                }
            }
            "max-age" => {
                let digits = value.strip_prefix('-').unwrap_or(value);
                expired |= !digits.is_empty()
                    && digits.bytes().all(|byte| byte.is_ascii_digit())
                    && (value.starts_with('-') || digits.bytes().all(|byte| byte == b'0'));
            }
            "expires" => {
                let date = parse_http_date(value).or_else(|| {
                    NaiveDateTime::parse_from_str(value, "%a, %d-%b-%Y %H:%M:%S GMT")
                        .ok()
                        .map(|date| date.and_utc())
                });
                expired |= date.is_some_and(|date| date <= now);
            }
            _ => {}
        }
    }
    Some((name, pair, expired))
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
        StatusCode::OK if !response.headers().contains_key(CONTENT_RANGE) => {
            if ContentCoding::from_response(response)? == ContentCoding::Identity {
                length(response)
            } else {
                Ok(None)
            }
        }
        StatusCode::OK => Err(EngineError::InvalidResponse("unexpected Content-Range")),
        StatusCode::PARTIAL_CONTENT => {
            // A plain GET can only use a 206 response that covers the complete representation.
            ensure_identity(response)?;
            let incomplete = || EngineError::InvalidResponse("incomplete response to plain GET");
            let total = length(response)?
                .filter(|&total| total > 0)
                .ok_or_else(incomplete)?;
            if content_range(response, 0, total - 1)? != Some(total) {
                return Err(incomplete());
            }
            Ok(Some(total))
        }
        _ => Err(status_error(response)),
    }
}

fn full_metadata(
    response: Response,
    head_name: Option<String>,
    initial_rtt: Option<u32>,
) -> Result<Metadata> {
    Ok(Metadata {
        total: full_length(&response)?,
        ranges: false,
        validators: Validators::from_response(&response),
        filename: filename(&response)?.or(head_name),
        protocol: protocol(&response),
        connect_rtt_ms: initial_rtt.or_else(|| connect_rtt_ms(&response)),
        response: Some(response),
    })
}

fn status_error(response: &Response) -> EngineError {
    let status = response.status();
    let retry_after = if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, Utc::now()))
    } else {
        None
    };
    EngineError::Status {
        status,
        retry_after,
        sent_cookie: response.extensions().get::<SentCookie>().is_some(),
    }
}

fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    let delay = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        parse_http_date(value)?
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default()
    };
    Some(delay.clamp(Duration::from_millis(250), Duration::from_secs(10)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentCoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
}

impl ContentCoding {
    pub(crate) fn from_response(response: &Response) -> Result<Self> {
        if ensure_identity(response).is_ok() {
            return Ok(Self::Identity);
        }
        let mut codings = response.headers().get_all(CONTENT_ENCODING).iter();
        if let Some(value) = codings.next().filter(|_| codings.next().is_none()) {
            for (name, coding) in [
                (&b"gzip"[..], Self::Gzip),
                (&b"deflate"[..], Self::Deflate),
                (&b"br"[..], Self::Brotli),
            ] {
                if value.as_bytes().eq_ignore_ascii_case(name) {
                    return Ok(coding);
                }
            }
        }
        Err(EngineError::InvalidResponse("unsupported Content-Encoding"))
    }

    pub(crate) fn reader(self, response: Response) -> Pin<Box<dyn AsyncRead + Send>> {
        let reader =
            StreamReader::new(response.bytes_stream().fuse().map_err(|error| {
                io::Error::new(io::ErrorKind::UnexpectedEof, error.without_url())
            }));
        match self {
            Self::Identity => Box::pin(reader),
            Self::Gzip => {
                let mut decoder = GzipDecoder::new(reader);
                decoder.multiple_members(true);
                Box::pin(VerifiedCompressedReader::new(
                    decoder,
                    GzipDecoder::get_pin_mut,
                ))
            }
            Self::Deflate => Box::pin(VerifiedCompressedReader::new(
                ZlibDecoder::new(reader),
                ZlibDecoder::get_pin_mut,
            )),
            Self::Brotli => Box::pin(VerifiedCompressedReader::new(
                BrotliDecoder::new(reader),
                BrotliDecoder::get_pin_mut,
            )),
        }
    }
}

struct VerifiedCompressedReader<D, R> {
    decoder: D,
    transport: fn(Pin<&mut D>) -> Pin<&mut R>,
    verified_eof: bool,
}

impl<D, R> VerifiedCompressedReader<D, R> {
    fn new(decoder: D, transport: fn(Pin<&mut D>) -> Pin<&mut R>) -> Self {
        Self {
            decoder,
            transport,
            verified_eof: false,
        }
    }
}

impl<D: AsyncRead + Unpin, R: AsyncBufRead> AsyncRead for VerifiedCompressedReader<D, R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.verified_eof || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut decoder = Pin::new(&mut this.decoder);
        let before = buf.filled().len();
        ready!(decoder.as_mut().poll_read(cx, buf))?;
        if buf.filled().len() == before {
            // A finished compressed member can leave buffered bytes or an HTTP error unread.
            let mut transport = (this.transport)(decoder);
            if !ready!(transport.as_mut().poll_fill_buf(cx))?.is_empty() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected trailing bytes after compressed stream",
                )));
            }
            this.verified_eof = true;
        }
        Poll::Ready(Ok(()))
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

    #[tokio::test]
    async fn checked_ipv4_dns_returns_only_ipv4_destinations() {
        let resolver = CheckedDns {
            allow_loopback: true,
            ipv4_only: true,
            proxy_hosts: Arc::default(),
        };
        let addresses: Vec<_> = resolver
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(SocketAddr::is_ipv4));
        assert!(CheckedDns {
            allow_loopback: false,
            ..resolver
        }
        .resolve("localhost".parse().unwrap())
        .await
        .is_err());
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_with_bounded_delays() {
        let now = parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(parse_retry_after("2", now), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:04 GMT", now),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:27:59 GMT", now),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse_retry_after("0", now),
            Some(Duration::from_millis(250))
        );
        assert_eq!(parse_retry_after("100", now), Some(Duration::from_secs(10)));
        assert_eq!(parse_retry_after("later", now), None);
    }

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
    fn source_sanitizes_credentials_from_referrer() {
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
    }

    #[test]
    fn redirect_filters_headers_by_origin_and_downgrade() {
        let origin = Url::parse("https://cdn.example.com/start").unwrap();
        let same_origin = origin.join("/other").unwrap();
        let cross_origin = Url::parse("https://mirror.example.com/file").unwrap();
        let downgrade = Url::parse("http://cdn.example.com/file").unwrap();
        let allowed = [
            "user-agent",
            "accept",
            "accept-encoding",
            "accept-language",
            "range",
            "if-range",
        ];
        let stripped = [
            "cookie",
            "cookie2",
            "authorization",
            "proxy-authorization",
            "x-api-key",
            "x-auth-token",
            "origin",
            "x-custom",
        ];
        let mut headers = HeaderMap::new();
        for name in allowed.into_iter().chain(stripped) {
            headers.insert(name, HeaderValue::from_static("value"));
        }
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://user:pw@app.example.com:8443/page?secret=1#fragment"),
        );
        let original = headers.clone();
        apply_redirect_header_policy(&mut headers, &origin, &same_origin);
        assert_eq!(headers, original);

        apply_redirect_header_policy(&mut headers, &origin, &cross_origin);
        assert_eq!(headers.len(), allowed.len() + 1);
        for name in allowed {
            assert_eq!(headers[name], original[name]);
        }
        assert_eq!(headers[REFERER], "https://app.example.com:8443/");

        let mut headers = original;
        apply_redirect_header_policy(&mut headers, &origin, &downgrade);
        assert_eq!(headers.len(), allowed.len());
        assert!(!headers.contains_key(REFERER));
        assert!(!headers.contains_key("origin"));
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
