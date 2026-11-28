use crate::{EngineError, Result};
use reqwest::{NoProxy, Proxy};
use slipstream_protocol::validate_http_proxy_url;
use std::collections::HashSet;
use url::{Host, Url};

#[derive(Clone, Default)]
pub enum EngineProxyConfig {
    #[default]
    Direct,
    System,
    Manual {
        url: String,
        username: Option<String>,
        password: Option<String>,
        bypass_hosts: Vec<String>,
    },
}

impl EngineProxyConfig {
    pub(crate) fn proxies(&self) -> Result<ConfiguredProxies> {
        match self {
            Self::Direct => Ok(ConfiguredProxies::default()),
            Self::System => system_proxies(),
            Self::Manual {
                url,
                username,
                password,
                bypass_hosts,
            } => {
                let url = proxy_url(url)?;
                let mut proxy = Proxy::all(url.clone()).map_err(proxy_error)?;
                if let Some(user) = username {
                    proxy = proxy.basic_auth(user, password.as_deref().unwrap_or_default());
                }
                let bypass = bypass_hosts
                    .iter()
                    .map(|host| host.strip_prefix("*.").unwrap_or(host))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut configured = ConfiguredProxies::default();
                configured.push(proxy.no_proxy(NoProxy::from_string(&bypass)), &url);
                Ok(configured)
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ConfiguredProxies {
    pub rules: Vec<Proxy>,
    pub hosts: HashSet<String>,
}

impl ConfiguredProxies {
    fn push(&mut self, proxy: Proxy, endpoint: &Url) {
        if let Some(Host::Domain(host)) = endpoint.host() {
            self.hosts
                .insert(host.trim_end_matches('.').to_ascii_lowercase());
        }
        self.rules.push(proxy);
    }
}

fn proxy_url(raw: &str) -> Result<url::Url> {
    let invalid = || EngineError::InvalidRequest("invalid HTTP proxy URL".into());
    let url = validate_http_proxy_url(raw).map_err(|_| invalid())?;
    if url.password().is_some() {
        return Err(invalid());
    }
    Ok(url)
}

fn proxy_error(_: reqwest::Error) -> EngineError {
    EngineError::InvalidRequest("could not configure HTTP proxy".into())
}

#[cfg(not(windows))]
fn system_proxies() -> Result<ConfiguredProxies> {
    Ok(ConfiguredProxies::default())
}

/// Resolve the Windows Internet Settings HTTP proxy for a specific URL.
pub fn system_http_proxy_for(target: &Url) -> Result<Option<String>> {
    #[cfg(windows)]
    if let Some(settings) = read_system_http_proxy()? {
        return Ok(settings.proxy_for(target).map(String::from));
    }
    let _ = target;
    Ok(None)
}

#[cfg(windows)]
fn system_proxies() -> Result<ConfiguredProxies> {
    let mut proxies = ConfiguredProxies::default();
    if let Some(settings) = read_system_http_proxy()? {
        for (scheme, endpoint) in settings.servers {
            let bypass = settings.bypass.clone();
            let address = endpoint.clone();
            let proxy = Proxy::custom(move |target| {
                (applies_to(&scheme, target) && !bypasses(&bypass, target)).then(|| address.clone())
            });
            proxies.push(proxy, &endpoint);
        }
    }
    Ok(proxies)
}

#[cfg(windows)]
fn read_system_http_proxy() -> Result<Option<SystemProxySettings>> {
    use std::ffi::c_void;
    use std::ptr::null_mut;

    // RegGetValueW is available on every supported Windows version. Engine
    // does not otherwise depend on the Windows bindings.
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            key: *mut c_void,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            value_type: *mut u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    const HKCU: *mut c_void = 0x8000_0001_u32 as i32 as isize as *mut c_void;
    const RRF_RT_REG_SZ: u32 = 2;
    const RRF_RT_REG_DWORD: u32 = 16;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    let read = |name: &str, flags, data, mut size| {
        let name = wide(name);
        let status = unsafe {
            RegGetValueW(
                HKCU,
                subkey.as_ptr(),
                name.as_ptr(),
                flags,
                null_mut(),
                data,
                &mut size,
            )
        };
        match status {
            0 => Ok(Some(size)),
            ERROR_FILE_NOT_FOUND => Ok(None),
            code => Err(std::io::Error::from_raw_os_error(code)),
        }
    };
    let mut enabled = 0_u32;
    read(
        "ProxyEnable",
        RRF_RT_REG_DWORD,
        (&mut enabled as *mut u32).cast(),
        4,
    )?;
    if enabled == 0 {
        return Ok(None);
    }
    let read_string = |name| -> Result<String> {
        let Some(size) = read(name, RRF_RT_REG_SZ, null_mut(), 0)? else {
            return Ok(String::new());
        };
        if size > 65_536 {
            return Err(EngineError::InvalidRequest(
                "Windows proxy setting is too long".into(),
            ));
        }
        let mut value = vec![0_u16; size.div_ceil(2) as usize];
        read(name, RRF_RT_REG_SZ, value.as_mut_ptr().cast(), size)?;
        let end = value.iter().position(|&ch| ch == 0).unwrap_or(value.len());
        String::from_utf16(&value[..end])
            .map_err(|_| EngineError::InvalidRequest("invalid Windows proxy setting".into()))
    };
    let server = read_string("ProxyServer")?;
    let bypass = read_string("ProxyOverride")?;
    SystemProxySettings::parse(&server, &bypass).map(Some)
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(any(windows, test))]
struct SystemProxySettings {
    servers: Vec<(String, Url)>,
    bypass: Vec<String>,
}

#[cfg(any(windows, test))]
impl SystemProxySettings {
    fn parse(server: &str, bypass: &str) -> Result<Self> {
        let mut servers = Vec::new();
        for entry in server
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (scheme, address) = entry.split_once('=').unwrap_or(("", entry));
            let scheme = scheme.trim().to_ascii_lowercase();
            if !matches!(scheme.as_str(), "" | "http" | "https") {
                continue;
            }
            let address = address.trim();
            let endpoint = if address.contains("://") {
                proxy_url(address)?
            } else {
                proxy_url(&format!("http://{address}"))?
            };
            servers.push((scheme, endpoint));
        }
        let bypass = bypass
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        Ok(Self { servers, bypass })
    }

    fn proxy_for(&self, target: &Url) -> Option<Url> {
        if bypasses(&self.bypass, target) {
            return None;
        }
        self.servers
            .iter()
            .find_map(|(scheme, endpoint)| applies_to(scheme, target).then(|| endpoint.clone()))
    }
}

#[cfg(any(windows, test))]
fn applies_to(scheme: &str, target: &Url) -> bool {
    scheme.is_empty() || scheme == target.scheme()
}

#[cfg(any(windows, test))]
fn bypasses(patterns: &[String], target: &Url) -> bool {
    let host = target.host_str().unwrap_or_default().trim_end_matches('.');
    patterns.iter().any(|pattern| {
        if pattern == "<local>" {
            return !host.contains(['.', ':']);
        }
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            return host == pattern;
        };
        let Some(mut remainder) = host.strip_prefix(prefix) else {
            return false;
        };
        let mut parts = suffix.split('*').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return remainder.ends_with(part);
            }
            let Some((_, tail)) = remainder.split_once(part) else {
                return false;
            };
            remainder = tail;
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase4_constructs_proxy_configurations() {
        assert!(EngineProxyConfig::default()
            .proxies()
            .unwrap()
            .rules
            .is_empty());
        let config = EngineProxyConfig::Manual {
            url: "http://127.0.0.1:8080".into(),
            username: Some("user".into()),
            password: Some("password".into()),
            bypass_hosts: vec!["example.org".into()],
        };
        assert_eq!(config.proxies().unwrap().rules.len(), 1);
        assert!(proxy_url("http://user:pass@localhost:8080").is_err());
        assert!(proxy_url("http://localhost:0").is_err());
        let system = SystemProxySettings::parse(
            "http=localhost:80;https=localhost:81;socks=localhost:82",
            "*.example.org;<local>",
        )
        .unwrap();
        assert_eq!(system.servers.len(), 2);
        assert_eq!(
            system
                .proxy_for(&Url::parse("https://example.net").unwrap())
                .unwrap()
                .port(),
            Some(81)
        );
        assert!(system
            .proxy_for(&Url::parse("https://cdn.example.org").unwrap())
            .is_none());
        assert!(system
            .proxy_for(&Url::parse("http://intranet").unwrap())
            .is_none());
    }
}
