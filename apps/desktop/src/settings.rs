use crate::paths::{
    ensure_safe_directory, invalid, local_drive_path, validate_configured_exe,
    validate_existing_ancestors,
};
use serde::{de::DeserializeOwned, Serialize};
use slipstream_engine::numbered_filename;
use slipstream_protocol::{
    default_download_categories, is_private_or_loopback_ip, sanitize_filename,
    validate_http_proxy_url, validate_http_url, AppSettings, DownloadCategory,
};
use std::{
    collections::HashSet,
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
};
use uuid::Uuid;

pub(crate) const SETTINGS_FILE: &str = "settings.json";

pub(crate) fn read_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    read_json_from(fs::File::open(path))
}

pub(crate) fn read_json_from<T: DeserializeOwned + Default>(
    file: io::Result<impl Read>,
) -> io::Result<T> {
    match file {
        Ok(file) => Ok(serde_json::from_reader(io::BufReader::new(file))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_json_atomically(path: &Path, value: &impl Serialize) -> io::Result<()> {
    write_atomically(path, &serde_json::to_vec_pretty(value)?)
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_file_name(format!(".slipstream-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn normalize_tool_path(value: &mut Option<String>) -> io::Result<()> {
    if let Some(raw) = value.as_deref() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            *value = None;
        } else {
            let clean = validate_configured_exe(trimmed)?;
            *value = Some(
                clean
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode tool path"))?,
            );
        }
    }
    Ok(())
}

pub(crate) fn normalize(settings: &mut AppSettings, data_dir: &Path) -> io::Result<()> {
    settings.default_connections = settings.default_connections.clamp(1, 32);
    settings.max_concurrent_downloads = settings.max_concurrent_downloads.clamp(1, 32);
    settings.global_speed_limit_bps = normalize_rate(settings.global_speed_limit_bps);
    settings.sort_column = crate::view_model::transfer_sort_column(&settings.sort_column).into();
    normalize_tool_path(&mut settings.ytdlp_path)?;
    normalize_tool_path(&mut settings.ffmpeg_path)?;

    let canonical = if settings.default_download_dir.trim().is_empty() {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|dir| dir.join("Downloads"))
            .and_then(|dir| ensure_safe_directory(&dir).ok())
            .map_or_else(|| ensure_safe_directory(&data_dir.join("Downloads")), Ok)?
    } else {
        ensure_safe_directory(Path::new(&settings.default_download_dir))?
    };
    let final_path = local_drive_path(&canonical)?;
    settings.default_download_dir = final_path
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-Unicode download folder"))?;
    normalize_categories(&mut settings.categories)?;
    normalize_phase2_automation(settings)?;
    normalize_phase3_bittorrent(settings)?;
    normalize_phase4_network_and_media(settings)
}

pub(crate) fn normalize_phase4_network_and_media(settings: &mut AppSettings) -> io::Result<()> {
    use slipstream_protocol::ProxyMode;

    let proxy = &mut settings.proxy;
    if proxy
        .url
        .as_deref()
        .is_some_and(|raw| raw.chars().any(char::is_control))
    {
        return Err(invalid("invalid HTTP proxy URL"));
    }
    proxy.url = proxy
        .url
        .take()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty());
    if let Some(raw) = &proxy.url {
        let url = validate_http_proxy_url(raw).map_err(|_| invalid("invalid HTTP proxy URL"))?;
        if url.password().is_some() {
            return Err(invalid("proxy URL must not include a password"));
        }
        proxy.url = Some(url.into());
    } else if proxy.mode == ProxyMode::Manual {
        return Err(invalid("manual proxy requires a URL"));
    }
    proxy.username = proxy
        .username
        .take()
        .map(|user| user.trim().to_owned())
        .filter(|user| !user.is_empty());
    for host in &mut proxy.bypass_hosts {
        *host = normalized_host_pattern(host)?;
        if host.contains([',', ';']) {
            return Err(invalid("invalid proxy bypass host"));
        }
    }

    let mut ids = HashSet::new();
    for entry in &mut settings.site_logins {
        entry.id = category_id(&entry.id);
        if entry.id.is_empty() || !ids.insert(entry.id.clone()) {
            return Err(invalid("site login IDs must be unique and nonempty"));
        }
        entry.host_pattern = normalized_host_pattern(&entry.host_pattern)?;
        if entry.host_pattern.contains([':', '/', '\\', '?', '#']) {
            return Err(invalid("invalid site login host pattern"));
        }
        entry.username = entry.username.trim().to_owned();
    }

    let cookies = &mut settings.cookie_passthrough;
    if !slipstream_media::args::COOKIE_BROWSERS.contains(&cookies.browser.as_str()) {
        return Err(invalid("unsupported cookie browser"));
    }
    for host in &mut cookies.allowed_hosts {
        *host = normalized_host_pattern(host)?;
        let domain = host.strip_prefix("*.").unwrap_or(host);
        if domain.len() > 253
            || !domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(invalid("invalid cookie allowlist domain"));
        }
    }

    let media = &mut settings.media_power;
    for category in &media.sponsorblock_remove {
        if !slipstream_media::args::SPONSORBLOCK_CATEGORIES.contains(&category.as_str()) {
            return Err(invalid("unsupported SponsorBlock category"));
        }
    }
    let mut seen = HashSet::new();
    media
        .sponsorblock_remove
        .retain(|category| seen.insert(category.clone()));
    let template = &media.filename_template;
    if template.chars().any(char::is_control)
        || template.contains(['/', '\\', '<', '>', ':', '"', '|', '?', '*'])
        || template.contains("..")
    {
        return Err(invalid("invalid media filename template"));
    }
    Ok(())
}

fn normalized_host_pattern(raw: &str) -> io::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || raw.chars().any(char::is_control)
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(invalid("invalid host pattern"));
    }
    Ok(trimmed.to_ascii_lowercase())
}

pub(crate) fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.strip_suffix(suffix)
            .is_some_and(|part| part.len() > 1 && part.ends_with('.'))
    } else {
        host == pattern
    }
}

pub(crate) fn normalize_rate(rate: Option<u64>) -> Option<u64> {
    rate.filter(|&rate| rate > 0).map(clamp_rate)
}

fn clamp_rate(rate: u64) -> u64 {
    rate.clamp(16_384, u64::from(u32::MAX))
}

fn watch_directory(raw: &str) -> io::Result<PathBuf> {
    if raw.chars().any(char::is_control) {
        return Err(invalid("watch folder path contains control characters"));
    }
    let folder = local_drive_path(Path::new(raw.trim()))?;
    match validate_existing_ancestors(&folder) {
        Ok(()) => {}
        // A configured removable drive can be offline at startup.
        Err(error) if matches!(error.raw_os_error(), Some(15 | 21 | 55)) => {}
        Err(error) => return Err(error),
    }
    Ok(folder)
}

fn normalize_phase2_automation(settings: &mut AppSettings) -> io::Result<()> {
    settings.alt_speed_limit_bps = normalize_rate(settings.alt_speed_limit_bps);
    settings.schedule.start_minute_of_day %= 1440;
    settings.schedule.end_minute_of_day %= 1440;
    settings.schedule.window_speed_limit_bps =
        normalize_rate(settings.schedule.window_speed_limit_bps);
    settings.bandwidth_guard.guard_speed_limit_bps =
        clamp_rate(settings.bandwidth_guard.guard_speed_limit_bps);

    let mut seen_procs = HashSet::new();
    settings.bandwidth_guard.watched_processes = settings
        .bandwidth_guard
        .watched_processes
        .iter()
        .filter_map(|raw| {
            let trimmed = raw.trim().to_ascii_lowercase();
            if trimmed.is_empty()
                || trimmed.contains(['/', '\\', '\0', ':'])
                || !seen_procs.insert(trimmed.clone())
            {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect();

    if settings.queues.is_empty() {
        settings.queues = slipstream_protocol::default_download_queues();
    }
    let mut seen_queues = HashSet::new();
    let mut cleaned_queues = Vec::with_capacity(settings.queues.len());
    for mut queue in std::mem::take(&mut settings.queues) {
        let mut id = category_id(&queue.id);
        if id.is_empty() {
            id = category_id(&queue.name);
        }
        id.truncate(32);
        if id.is_empty() || !seen_queues.insert(id.clone()) {
            continue;
        }
        queue.id = id;
        queue.name = sanitize_filename(queue.name.trim()).unwrap_or_else(|_| queue.id.clone());
        queue.max_concurrent = queue.max_concurrent.clamp(1, 16);
        queue.speed_limit_bps = normalize_rate(queue.speed_limit_bps);
        cleaned_queues.push(queue);
    }
    if !seen_queues.contains("default") {
        if let Some(default_spec) = slipstream_protocol::default_download_queues()
            .into_iter()
            .next()
        {
            cleaned_queues.insert(0, default_spec);
        }
    }
    settings.queues = cleaned_queues;

    let mut seen_rules = HashSet::new();
    for rule in &mut settings.watch_folders {
        let mut id = category_id(&rule.id);
        if id.is_empty() {
            id = "watch".to_owned();
        }
        if !seen_rules.insert(id.clone()) {
            return Err(invalid("duplicate watch folder rule ID"));
        }
        rule.id = id;
        let folder = watch_directory(&rule.folder)?;
        rule.folder = folder
            .into_os_string()
            .into_string()
            .map_err(|_| invalid("non-Unicode watch folder"))?;
        rule.target_save_dir = rule
            .target_save_dir
            .as_deref()
            .filter(|p| !p.trim().is_empty() || p.chars().any(char::is_control))
            .map(|p| {
                watch_directory(p)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode watch target directory"))
            })
            .transpose()?;
        rule.queue_id = rule
            .queue_id
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty() && settings.queues.iter().any(|spec| spec.id == *q))
            .map(str::to_owned);
    }
    Ok(())
}

fn normalize_phase3_bittorrent(settings: &mut AppSettings) -> io::Result<()> {
    let network = &mut settings.torrent_network;
    if network.listen_port_start == 0 {
        network.listen_port_end = 0;
    } else {
        if network.listen_port_start < 1024 || network.listen_port_start == u16::MAX {
            return Err(invalid(
                "torrent listen port start must be 0 or between 1024 and 65534",
            ));
        }
        if network.listen_port_end < network.listen_port_start {
            return Err(invalid(
                "torrent listen port end must be >= listen port start",
            ));
        }
    }
    network.global_upload_limit_bps = normalize_rate(network.global_upload_limit_bps);

    network.socks5_proxy_url = network
        .socks5_proxy_url
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let parsed = url::Url::parse(raw)
                .map_err(|error| invalid(format!("invalid SOCKS5 proxy URL: {error}")))?;
            if !matches!(parsed.scheme(), "socks5" | "socks5h") {
                return Err(invalid("SOCKS5 proxy URL must use socks5:// or socks5h://"));
            }
            if parsed.host_str().is_none() || parsed.port().is_none_or(|port| port == 0) {
                return Err(invalid("SOCKS5 proxy URL must include host and port"));
            }
            if parsed.query().is_some() || parsed.fragment().is_some() {
                return Err(invalid(
                    "SOCKS5 proxy URL must not contain query or fragment components",
                ));
            }
            Ok(parsed.to_string().trim_end_matches('/').to_owned())
        })
        .transpose()?;

    network.blocklist_path = normalize_blocklist_path(network.blocklist_path.as_deref())?;

    network.vpn_adapter_name = network
        .vpn_adapter_name
        .as_deref()
        .filter(|raw| !raw.trim().is_empty() || raw.chars().any(char::is_control))
        .map(|raw| {
            if raw.chars().any(char::is_control) {
                return Err(invalid("VPN adapter name contains control characters"));
            }
            Ok(raw.trim().to_owned())
        })
        .transpose()?;

    let seeding = &mut settings.torrent_seeding;
    seeding.max_seed_ratio_hundredths = seeding
        .max_seed_ratio_hundredths
        .filter(|&ratio| ratio > 0)
        .map(|ratio| ratio.clamp(1, 100_000));
    seeding.max_seed_minutes = seeding
        .max_seed_minutes
        .filter(|&minutes| minutes > 0)
        .map(|minutes| minutes.clamp(1, 525_600));

    let mut seen_rss = HashSet::new();
    for rule in &mut settings.rss_feeds {
        let mut id = category_id(&rule.id);
        if id.is_empty() {
            id = category_id(&rule.name);
        }
        if id.is_empty() {
            id = "rss".to_owned();
        }
        id.truncate(48);
        if !seen_rss.insert(id.clone()) {
            return Err(invalid("duplicate RSS feed rule ID"));
        }
        rule.id = id;
        rule.name = sanitize_filename(rule.name.trim()).unwrap_or_else(|_| rule.id.clone());
        let feed_url = rule.feed_url.trim();
        let parsed_feed = validate_http_url(feed_url)
            .map_err(|error| invalid(format!("invalid RSS feed URL: {error}")))?;
        if !parsed_feed.username().is_empty() || parsed_feed.password().is_some() {
            return Err(invalid("RSS feed URL must not embed credentials"));
        }
        if let Some(host) = parsed_feed.host_str() {
            let trimmed_host = host.trim_end_matches('.');
            if trimmed_host.eq_ignore_ascii_case("localhost")
                || trimmed_host.to_ascii_lowercase().ends_with(".localhost")
            {
                return Err(invalid("RSS feed URL cannot target localhost"));
            }
            if let Ok(ip) = trimmed_host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
            {
                if is_private_or_loopback_ip(ip) {
                    return Err(invalid(
                        "RSS feed URL cannot target a private or loopback IP",
                    ));
                }
            }
        }
        rule.feed_url = parsed_feed.into();
        rule.include_pattern = rule.include_pattern.trim().to_owned();
        rule.exclude_pattern = rule.exclude_pattern.trim().to_owned();
        if rule.include_pattern.chars().any(char::is_control)
            || rule.exclude_pattern.chars().any(char::is_control)
        {
            return Err(invalid("RSS pattern contains control characters"));
        }
        rule.save_dir = rule
            .save_dir
            .as_deref()
            .filter(|raw| !raw.trim().is_empty() || raw.chars().any(char::is_control))
            .map(|raw| {
                watch_directory(raw)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode RSS save directory"))
            })
            .transpose()?;
        rule.poll_interval_minutes = rule.poll_interval_minutes.clamp(5, 1440);
    }
    Ok(())
}

pub(crate) fn normalize_blocklist_path(path: Option<&str>) -> io::Result<Option<String>> {
    path.filter(|raw| !raw.trim().is_empty() || raw.chars().any(char::is_control))
        .map(|raw| {
            if raw.chars().any(char::is_control) {
                return Err(invalid("blocklist path contains control characters"));
            }
            local_drive_path(Path::new(raw.trim()))?
                .into_os_string()
                .into_string()
                .map_err(|_| invalid("non-Unicode blocklist path"))
        })
        .transpose()
}

pub(crate) fn category_id(raw: &str) -> String {
    raw.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
}

pub(crate) fn normalize_categories(categories: &mut Vec<DownloadCategory>) -> io::Result<()> {
    if categories.is_empty() {
        *categories = default_download_categories();
        return Ok(());
    }
    let mut ids = HashSet::new();
    for category in categories.iter_mut() {
        category.name =
            sanitize_filename(&category.name).map_err(|error| invalid(error.to_string()))?;
        category.id = category_id(&category.id);
        if category.id.is_empty() {
            category.id = category_id(&category.name);
        }
        if category.id.is_empty() || !ids.insert(category.id.clone()) {
            return Err(invalid("category IDs must be non-empty and unique"));
        }
        let mut seen = HashSet::new();
        category.extensions = category
            .extensions
            .iter()
            .map(|extension| {
                extension
                    .trim()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|extension| {
                !extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .filter(|extension| seen.insert(extension.clone()))
            .collect();
        category.save_dir = category
            .save_dir
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| {
                local_drive_path(&ensure_safe_directory(Path::new(path))?)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode category folder"))
            })
            .transpose()?;
    }
    if !ids.contains("general") {
        categories.insert(0, GENERAL_CATEGORY.clone());
    }
    Ok(())
}

static GENERAL_CATEGORY: LazyLock<DownloadCategory> = LazyLock::new(|| {
    default_download_categories()
        .into_iter()
        .next()
        .expect("default general category")
});

pub fn resolve_category_dir(settings: &AppSettings, category_id: &str) -> PathBuf {
    let base = PathBuf::from(&settings.default_download_dir);
    let Some(category) = settings
        .categories
        .iter()
        .find(|category| category.id == category_id)
    else {
        return base;
    };
    if let Some(directory) = &category.save_dir {
        PathBuf::from(directory)
    } else if category.id == "general" {
        base
    } else {
        base.join(&category.name)
    }
}

pub fn match_category_for_filename<'a>(
    settings: &'a AppSettings,
    filename: &str,
) -> &'a DownloadCategory {
    let extension = Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str());
    extension
        .and_then(|extension| {
            settings.categories.iter().find(|category| {
                category
                    .extensions
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(extension))
            })
        })
        .or_else(|| {
            settings
                .categories
                .iter()
                .find(|category| category.id == "general")
        })
        .unwrap_or(&GENERAL_CATEGORY)
}

pub fn next_numbered_filename(dir: &Path, filename: &str) -> String {
    let filename = sanitize_filename(filename).unwrap_or_else(|_| "download".into());
    for number in 0.. {
        let candidate = numbered_filename(&filename, number);
        if fs::symlink_metadata(dir.join(&candidate)).is_err() {
            return candidate;
        }
    }
    unreachable!("filename suffixes are unbounded")
}

pub(crate) fn torrent_rate(limit: Option<u64>) -> Option<u64> {
    limit
        .filter(|&rate| rate > 0)
        .map(|rate| rate.clamp(16_384, u64::from(u32::MAX)))
}

#[cfg(test)]
mod phase4_tests {
    use super::*;
    use slipstream_protocol::{ProxyMode, SiteAuthScheme, SiteLoginEntry};

    #[test]
    fn normalizes_network_settings_and_limits_cookie_scope() {
        let mut settings = AppSettings::default();
        settings.proxy.mode = ProxyMode::Manual;
        settings.proxy.url = Some(" https://Proxy.Example:8443 ".into());
        settings.proxy.bypass_hosts = vec![" Local.Example ".into()];
        settings.cookie_passthrough.enabled = true;
        settings.cookie_passthrough.allowed_hosts = vec![" *.EXAMPLE.ORG ".into()];
        settings.media_power.sponsorblock_remove = vec!["intro".into(), "intro".into()];
        settings.media_power.filename_template = "{title} - {uploader}".into();
        settings.site_logins.push(SiteLoginEntry {
            id: " Example Login ".into(),
            host_pattern: " *.EXAMPLE.ORG ".into(),
            username: " name ".into(),
            has_password: false,
            auth_scheme: SiteAuthScheme::Basic,
            enabled: true,
        });
        normalize_phase4_network_and_media(&mut settings).unwrap();
        assert_eq!(
            settings.proxy.url.as_deref(),
            Some("https://proxy.example:8443/")
        );
        assert_eq!(settings.proxy.bypass_hosts, ["local.example"]);
        assert_eq!(settings.site_logins[0].id, "example-login");
        assert_eq!(settings.site_logins[0].username, "name");
        assert_eq!(settings.site_logins[0].host_pattern, "*.example.org");
        assert_eq!(settings.media_power.sponsorblock_remove, ["intro"]);
        let host = &settings.cookie_passthrough.allowed_hosts[0];
        assert!(host_pattern_matches(host, "media.example.org"));
        assert!(!host_pattern_matches(host, "example.org"));
        assert!(!host_pattern_matches(host, "fakeexample.org"));
        assert!(!host_pattern_matches(host, "media.example.org.example.com"));
        settings.site_logins.push(settings.site_logins[0].clone());
        assert!(normalize_phase4_network_and_media(&mut settings).is_err());
    }

    #[test]
    fn rejects_invalid_proxy_cookie_hosts_and_media_settings() {
        let mut defaults = AppSettings::default();
        defaults.cookie_passthrough.enabled = true;
        normalize_phase4_network_and_media(&mut defaults).unwrap();
        let mut missing = defaults.clone();
        missing.proxy.mode = ProxyMode::Manual;
        assert!(normalize_phase4_network_and_media(&mut missing).is_err());
        for url in [
            "http://user:pass@proxy.test",
            "http://proxy.test:0",
            "http://proxy.test:65536",
            "https://proxy.test?",
            "https://proxy.test#",
            "http:proxy.test",
            "socks5://proxy.test",
        ] {
            let mut settings = defaults.clone();
            settings.proxy.url = Some(url.into());
            assert!(
                normalize_phase4_network_and_media(&mut settings).is_err(),
                "{url}"
            );
        }
        for host in [
            "*",
            "",
            "x y",
            "https://example.org",
            "**.example.org",
            "example.org/path",
            "example.org\n",
        ] {
            let mut settings = defaults.clone();
            settings.cookie_passthrough.allowed_hosts.push(host.into());
            assert!(
                normalize_phase4_network_and_media(&mut settings).is_err(),
                "{host}"
            );
        }
        let mut settings = defaults.clone();
        settings.cookie_passthrough.browser = "chrome:/profile".into();
        assert!(normalize_phase4_network_and_media(&mut settings).is_err());
        let mut settings = defaults.clone();
        settings.media_power.sponsorblock_remove.push("all".into());
        assert!(normalize_phase4_network_and_media(&mut settings).is_err());
        for template in ["../{title}", "{title}\\data", "{title}:stream", "{title}?"] {
            let mut settings = defaults.clone();
            settings.media_power.filename_template = template.into();
            assert!(
                normalize_phase4_network_and_media(&mut settings).is_err(),
                "{template}"
            );
        }
    }
}

#[cfg(test)]
mod phase3_tests {
    use super::normalize;
    use slipstream_protocol::{AppSettings, RssFeedRule};
    use std::{env, fs, path::PathBuf};
    use uuid::Uuid;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("slipstream-settings-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn phase3_normalizes_ports_proxy_seeding_and_rss_and_rejects_bad_inputs() {
        let dir = TestDir::new();
        let dir_path = &dir.0;
        let mut settings = AppSettings {
            default_download_dir: dir_path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        settings.torrent_network.listen_port_start = 0;
        settings.torrent_network.listen_port_end = 9999;
        settings.torrent_network.global_upload_limit_bps = Some(100);
        settings.torrent_network.socks5_proxy_url =
            Some("  socks5h://proxy.example.org:1080/  ".into());
        settings.torrent_network.vpn_adapter_name = Some("  WireGuard  ".into());
        settings.torrent_seeding.max_seed_ratio_hundredths = Some(0);
        settings.torrent_seeding.max_seed_minutes = Some(999_999);
        settings.rss_feeds.push(RssFeedRule {
            id: "My Feed!".into(),
            name: "Linux Releases".into(),
            feed_url: "https://example.org/feed.xml".into(),
            include_pattern: " *iso* ".into(),
            exclude_pattern: " *alpha* ".into(),
            save_dir: None,
            poll_interval_minutes: 1,
            enabled: true,
            auto_start: true,
            last_checked_at: None,
            last_error: None,
        });

        normalize(&mut settings, dir_path).unwrap();
        assert_eq!(settings.torrent_network.listen_port_end, 0);
        assert_eq!(
            settings.torrent_network.global_upload_limit_bps,
            Some(16_384)
        );
        assert_eq!(
            settings.torrent_network.socks5_proxy_url.as_deref(),
            Some("socks5h://proxy.example.org:1080")
        );
        assert_eq!(
            settings.torrent_network.vpn_adapter_name.as_deref(),
            Some("WireGuard")
        );
        assert_eq!(settings.torrent_seeding.max_seed_ratio_hundredths, None);
        assert_eq!(settings.torrent_seeding.max_seed_minutes, Some(525_600));
        assert_eq!(settings.rss_feeds[0].id, "my-feed");
        assert_eq!(settings.rss_feeds[0].poll_interval_minutes, 5);
        assert_eq!(settings.rss_feeds[0].include_pattern, "*iso*");

        let mut bad_proxy = settings.clone();
        bad_proxy.torrent_network.socks5_proxy_url = Some("http://127.0.0.1:8080".into());
        assert!(normalize(&mut bad_proxy, dir_path).is_err());

        let mut bad_rss = settings.clone();
        bad_rss.rss_feeds[0].feed_url = "http://127.0.0.1:8080/rss.xml".into();
        assert!(normalize(&mut bad_rss, dir_path).is_err());
    }
}
