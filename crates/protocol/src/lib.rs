pub mod security;
pub mod types;

pub use security::{
    is_private_or_loopback_ip, safe_join_under, sanitize_filename, sanitize_title_to_filename,
    validate_http_proxy_url, validate_http_url, validate_magnet_uri,
    validate_safe_filename_component, MagnetMeta, PathSecurityError, UrlSecurityError,
};
pub use types::*;

#[cfg(test)]
mod tests {
    use super::NativeBridgeRequest;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn open_desktop_window_round_trips_with_optional_headers() {
        let url = "https://example.com/video.m3u8";
        let legacy = json!({"type": "open_desktop_window", "open_picker_url": url});
        let mut with_headers = legacy.clone();
        with_headers["headers"] = json!({"Referer": "https://example.com/watch"});
        for (wire, expected_headers) in [
            (legacy, HashMap::new()),
            (
                with_headers,
                HashMap::from([("Referer".into(), "https://example.com/watch".into())]),
            ),
        ] {
            let request: NativeBridgeRequest = serde_json::from_value(wire.clone()).unwrap();
            let NativeBridgeRequest::OpenDesktopWindow {
                open_picker_url,
                headers,
            } = &request
            else {
                panic!("expected OpenDesktopWindow");
            };
            assert_eq!(open_picker_url.as_deref(), Some(url));
            assert_eq!(*headers, expected_headers);
            assert_eq!(serde_json::to_value(request).unwrap(), wire);
        }
    }
}
