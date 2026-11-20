use serde::Serialize;
use slipstream_protocol::NATIVE_HOST_NAME;
use std::{io, path::Path};

pub const DEFAULT_FIREFOX_ID: &str = "slipstream@ciphervaults.com";

#[derive(Debug, Clone, Copy)]
pub enum Browser {
    Chrome,
    Edge,
    Brave,
    Firefox,
}

impl Browser {
    pub const ALL: [Self; 4] = [Self::Chrome, Self::Edge, Self::Brave, Self::Firefox];

    pub fn directory(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Firefox => "firefox",
        }
    }

    #[cfg(windows)]
    pub(crate) fn registry_subkey(self) -> &'static str {
        match self {
            Self::Chrome => r"Google\Chrome",
            Self::Edge => r"Microsoft\Edge",
            Self::Brave => r"BraveSoftware\Brave-Browser",
            Self::Firefox => r"Mozilla",
        }
    }
}

#[derive(Serialize)]
pub struct HostManifest<'a> {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "type")]
    transport: &'static str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_origins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_extensions: Option<&'a [String]>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub fn chromium_origin(value: &str) -> io::Result<String> {
    let id = value
        .strip_prefix("chrome-extension://")
        .unwrap_or_default();
    let id = id.strip_suffix('/').unwrap_or(id);
    if id.len() != 32 || !id.bytes().all(|byte| (b'a'..=b'p').contains(&byte)) {
        return Err(invalid(
            "expected chrome-extension:// followed by a 32-character Chromium extension ID",
        ));
    }
    Ok(format!("chrome-extension://{id}/"))
}

pub fn valid_firefox_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 255 {
        return false;
    }
    if let Some((local, domain)) = id.split_once('@') {
        return !domain.is_empty()
            && [local, domain].into_iter().all(|part| {
                part.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            });
    }
    let Some(guid) = id.strip_prefix('{').and_then(|id| id.strip_suffix('}')) else {
        return false;
    };
    guid.len() == 36
        && guid.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub fn generate<'a>(
    browser: Browser,
    executable: &'a Path,
    origins: &[String],
    firefox_ids: &'a [String],
) -> io::Result<HostManifest<'a>> {
    let path = executable
        .to_str()
        .filter(|path| executable.is_absolute() && !path.contains('\0'))
        .ok_or_else(|| invalid("manifest executable path must be absolute Unicode"))?;
    let (allowed_origins, allowed_extensions) = match browser {
        Browser::Firefox => {
            if firefox_ids.is_empty() || !firefox_ids.iter().all(|id| valid_firefox_id(id)) {
                return Err(invalid("expected a valid Firefox extension ID"));
            }
            (None, Some(firefox_ids))
        }
        _ => {
            if origins.is_empty() {
                return Err(invalid(
                    "provide at least one --chrome-origin for the Chromium manifests",
                ));
            }
            (
                Some(
                    origins
                        .iter()
                        .map(|origin| chromium_origin(origin))
                        .collect::<io::Result<_>>()?,
                ),
                None,
            )
        }
    };
    Ok(HostManifest {
        name: NATIVE_HOST_NAME,
        description: "SlipStream desktop integration",
        transport: "stdio",
        path,
        allowed_origins,
        allowed_extensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_browser_specific_allowlists() {
        let path = std::env::current_exe().unwrap();
        let origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";
        let firefox = [DEFAULT_FIREFOX_ID.into()];
        for browser in Browser::ALL {
            let json =
                serde_json::to_value(generate(browser, &path, &[origin.into()], &firefox).unwrap())
                    .unwrap();
            assert_eq!(json["name"], NATIVE_HOST_NAME);
            assert_eq!(json["type"], "stdio");
            assert_eq!(json["path"], path.to_str().unwrap());
            if matches!(browser, Browser::Firefox) {
                assert_eq!(json["allowed_extensions"][0], DEFAULT_FIREFOX_ID);
                assert!(json.get("allowed_origins").is_none());
            } else {
                assert_eq!(json["allowed_origins"][0], origin);
                assert!(json.get("allowed_extensions").is_none());
            }
        }
    }

    #[test]
    fn rejects_unpinned_origins_and_ids() {
        let path = std::env::current_exe().unwrap();
        for origin in [
            "chrome-extension://*/",
            "https://abcdefghijklmnopabcdefghijklmnop/",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop/path",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop/?x",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop//",
        ] {
            assert!(generate(Browser::Chrome, &path, &[origin.into()], &[]).is_err());
        }
        for id in ["*", "slipstream@*.com", "user@host/path", "user@first@host"] {
            assert!(generate(Browser::Firefox, &path, &[], &[id.into()]).is_err());
        }
        assert!(valid_firefox_id("{a4f9c637-10d9-4a44-8dbb-d566fbf54c9f}"));
    }
}
