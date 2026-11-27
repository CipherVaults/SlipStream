use chrono::Utc;
use slipstream_protocol::{
    default_download_categories, AudioTrackKind, DownloadCategory, MediaFormatOption,
    MediaProbeResult, SegmentSnapshot, SegmentState, TelemetryBatch, TransferKind,
    TransferSnapshot, TransferStatus, UI_SEGMENT_COUNT,
};
use std::collections::HashSet;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavFilter {
    All,
    Active,
    Completed,
    Queued,
    Torrents,
    Paused,
}

impl NavFilter {
    pub fn from_nav_id(raw: &str) -> Self {
        match raw {
            "active" => Self::Active,
            "completed" => Self::Completed,
            "queued" => Self::Queued,
            "torrents" => Self::Torrents,
            "paused" => Self::Paused,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavCounts {
    pub all: i32,
    pub active: i32,
    pub completed: i32,
    pub queued: i32,
    pub torrents: i32,
    pub paused: i32,
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn format_speed(bps: u64) -> String {
    if bps == 0 {
        "0 B/s".into()
    } else {
        format!("{}/s", format_bytes(bps))
    }
}

pub fn format_eta(seconds: Option<u64>) -> String {
    let Some(secs) = seconds else {
        return "--".into();
    };
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {rem:02}s")
        }
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hours}h {mins:02}m")
    }
}

pub fn format_duration(seconds: Option<u64>) -> String {
    let Some(secs) = seconds else {
        return String::new();
    };
    if secs == 0 {
        return String::new();
    }
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{rem:02}")
    } else {
        format!("{mins}:{rem:02}")
    }
}

pub fn build_sparkline_paths(samples: &[u64], width: f32, height: f32) -> (String, String) {
    let w = width.max(20.0);
    let h = height.max(12.0);
    if samples.len() < 2 {
        let y = h - 3.0;
        return (
            format!("M 0 {y:.1} L {w:.1} {y:.1}"),
            format!("M 0 {y:.1} L {w:.1} {y:.1} L {w:.1} {h:.1} L 0 {h:.1} Z"),
        );
    }

    let max_val = samples.iter().copied().max().unwrap_or(0).max(1) as f32;
    let usable_h = (h - 6.0).max(4.0);
    let step_x = w / ((samples.len() - 1) as f32);

    let pts: Vec<(f32, f32)> = samples
        .iter()
        .enumerate()
        .map(|(i, &val)| {
            let x = (i as f32) * step_x;
            let norm = (val as f32 / max_val).clamp(0.0, 1.0);
            let y = (h - 3.0) - norm * usable_h;
            (x, y)
        })
        .collect();

    let mut line = format!("M {:.1} {:.1}", pts[0].0, pts[0].1);
    for pair in pts.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        let cx = (x0 + x1) * 0.5;
        line.push_str(&format!(
            " C {cx:.1} {y0:.1}, {cx:.1} {y1:.1}, {x1:.1} {y1:.1}"
        ));
    }

    let last_x = pts.last().map(|p| p.0).unwrap_or(w);
    let fill = format!("{line} L {last_x:.1} {h:.1} L 0 {h:.1} Z");
    (line, fill)
}

pub fn is_torrent_input(raw: &str) -> bool {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("magnet:") || lower.ends_with(".torrent")
}

pub fn looks_like_http_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some()
}

pub fn url_origin(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!(
        "{}://{}{}",
        parsed.scheme(),
        host.to_ascii_lowercase(),
        port
    ))
}

pub fn should_clear_seeded_headers(seed_origin: Option<&str>, edited_source: &str) -> bool {
    let Some(expected) = seed_origin else {
        return true;
    };
    url_origin(edited_source).as_deref() != Some(expected)
}

pub fn looks_like_direct_file_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let lower_path = parsed.path().to_ascii_lowercase();
    const EXTS: [&str; 22] = [
        ".zip", ".7z", ".rar", ".tar", ".gz", ".xz", ".bz2", ".exe", ".msi", ".msix", ".iso",
        ".img", ".dmg", ".pkg", ".deb", ".rpm", ".apk", ".pdf", ".mp4", ".mkv", ".webm", ".bin",
    ];
    EXTS.iter().any(|ext| lower_path.ends_with(ext))
}

pub fn expand_wildcard_preview(raw: &str) -> usize {
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_torrent_input(trimmed) {
        return 0;
    }
    if !(trimmed.contains('[') && trimmed.contains("..")) {
        return 0;
    }
    crate::expand_wildcard_urls(trimmed)
        .map(|urls| urls.len())
        .unwrap_or(0)
}

pub fn classify_icon_kind(filename: &str, kind: TransferKind) -> i32 {
    if kind == TransferKind::Torrent {
        return 7;
    }
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if kind == TransferKind::Media
        || matches!(
            ext.as_str(),
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "flv" | "wmv" | "ts"
        )
    {
        return 1;
    }
    if matches!(
        ext.as_str(),
        "mp3" | "flac" | "wav" | "m4a" | "ogg" | "opus" | "aac" | "wma"
    ) {
        return 2;
    }
    if matches!(
        ext.as_str(),
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab"
    ) {
        return 3;
    }
    if matches!(
        ext.as_str(),
        "exe"
            | "msi"
            | "msix"
            | "appx"
            | "dmg"
            | "pkg"
            | "deb"
            | "rpm"
            | "apk"
            | "bin"
            | "bat"
            | "cmd"
    ) {
        return 4;
    }
    if matches!(ext.as_str(), "iso" | "img") {
        return 5;
    }
    if matches!(
        ext.as_str(),
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" | "csv"
    ) {
        return 6;
    }
    0
}

pub fn compute_nav_counts(transfers: &[TransferSnapshot]) -> NavCounts {
    let mut counts = NavCounts::default();
    for t in transfers {
        counts.all += 1;
        match t.status {
            TransferStatus::Active | TransferStatus::Probing | TransferStatus::Remuxing => {
                counts.active += 1;
            }
            TransferStatus::Completed => counts.completed += 1,
            TransferStatus::Queued => counts.queued += 1,
            TransferStatus::Paused => counts.paused += 1,
            TransferStatus::Seeding => counts.active += 1,
            TransferStatus::Failed | TransferStatus::Canceled => {}
        }
        if t.kind == TransferKind::Torrent {
            counts.torrents += 1;
        }
    }
    counts
}

pub fn matches_filter(t: &TransferSnapshot, nav: NavFilter, query_lower: &str) -> bool {
    let nav_ok = match nav {
        NavFilter::All => true,
        NavFilter::Active => matches!(
            t.status,
            TransferStatus::Active
                | TransferStatus::Probing
                | TransferStatus::Remuxing
                | TransferStatus::Seeding
        ),
        NavFilter::Completed => t.status == TransferStatus::Completed,
        NavFilter::Queued => t.status == TransferStatus::Queued,
        NavFilter::Torrents => t.kind == TransferKind::Torrent,
        NavFilter::Paused => t.status == TransferStatus::Paused,
    };
    if !nav_ok {
        return false;
    }
    if query_lower.is_empty() {
        return true;
    }
    t.filename.to_ascii_lowercase().contains(query_lower)
        || t.source_domain.to_ascii_lowercase().contains(query_lower)
        || t.source_url.to_ascii_lowercase().contains(query_lower)
        || t.playlist_title
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains(query_lower)
}

pub fn format_transfer_percent(t: &TransferSnapshot) -> (f32, String) {
    if t.status == TransferStatus::Completed {
        return (100.0, "Done".into());
    }
    let Some(total) = t.total_bytes.filter(|&v| v > 0) else {
        return if t.downloaded_bytes > 0 {
            (0.0, format_bytes(t.downloaded_bytes))
        } else if t.status == TransferStatus::Paused {
            (0.0, "Paused".into())
        } else {
            (0.0, "0%".into())
        };
    };
    let pct = ((t.downloaded_bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32;
    (pct, format!("{pct:.0}%"))
}

pub fn pair_video_with_best_audio(
    video: &MediaFormatOption,
    formats: &[MediaFormatOption],
) -> String {
    if video.has_audio {
        return video.format_id.clone();
    }
    let mut best_audio: Option<&MediaFormatOption> = None;
    for candidate in formats {
        if !candidate.is_audio_only && candidate.has_video {
            continue;
        }
        if !candidate.has_audio {
            continue;
        }
        let better = match best_audio {
            None => true,
            Some(prev) => {
                let prev_rank = (
                    audio_kind_rank(prev.audio_track_kind.unwrap_or_default()),
                    prev.bitrate_kbps.unwrap_or(0.0) as u64,
                );
                let cur_rank = (
                    audio_kind_rank(candidate.audio_track_kind.unwrap_or_default()),
                    candidate.bitrate_kbps.unwrap_or(0.0) as u64,
                );
                cur_rank > prev_rank
            }
        };
        if better {
            best_audio = Some(candidate);
        }
    }
    match best_audio {
        Some(audio) => format!("{}+{}", video.format_id, audio.format_id),
        None => video.format_id.clone(),
    }
}

fn audio_kind_rank(kind: AudioTrackKind) -> u8 {
    match kind {
        AudioTrackKind::Original => 5,
        AudioTrackKind::Default => 4,
        AudioTrackKind::Unspecified => 3,
        AudioTrackKind::Dubbed => 2,
        AudioTrackKind::Descriptive => 1,
    }
}

pub fn video_format_options(probe: &MediaProbeResult) -> Vec<MediaFormatOption> {
    let mut seen = HashSet::new();
    let mut out: Vec<MediaFormatOption> = probe
        .formats
        .iter()
        .filter(|f| f.has_video)
        .filter(|f| seen.insert(f.format_id.clone()))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.height
            .unwrap_or(0)
            .cmp(&a.height.unwrap_or(0))
            .then_with(|| (b.fps.unwrap_or(0.0) as u32).cmp(&(a.fps.unwrap_or(0.0) as u32)))
            .then_with(|| {
                b.filesize_bytes
                    .unwrap_or(0)
                    .cmp(&a.filesize_bytes.unwrap_or(0))
            })
    });
    out
}

pub fn parse_speed_limit_bps(raw: &str, unit_mbps: bool) -> Result<Option<u64>, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return Ok(None);
    }
    let val: f64 = trimmed
        .parse()
        .map_err(|_| "Enter a valid positive number.")?;
    if !val.is_finite() || val < 0.0 {
        return Err("Enter a valid positive number.");
    }
    if val == 0.0 {
        return Ok(None);
    }
    let mult = if unit_mbps { 1_048_576.0 } else { 1024.0 };
    Ok(Some((val * mult).round() as u64))
}

pub fn parse_speed_limit_slider_mbps(raw: &str) -> Result<Option<u64>, &'static str> {
    parse_speed_limit_bps(raw, true)
}

pub fn sanitize_category_slug(raw: &str) -> String {
    raw.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
}

pub fn category_by_id_or_index(
    categories: &[DownloadCategory],
    index: usize,
) -> Option<&DownloadCategory> {
    categories.get(index).or_else(|| categories.first())
}

pub fn demo_telemetry_batch() -> TelemetryBatch {
    let now = Utc::now();
    let make_segs =
        |done_until: usize, active_idx: usize, done_bytes: u64| -> Vec<SegmentSnapshot> {
            (0..16)
                .map(|i| SegmentSnapshot {
                    index: i as u8,
                    start_byte: i as u64 * 100_000_000,
                    end_byte: (i as u64 + 1) * 100_000_000 - 1,
                    downloaded_bytes: if i < done_until {
                        100_000_000
                    } else if i == active_idx {
                        done_bytes
                    } else {
                        0
                    },
                    speed_bps: if i == active_idx { 1_850_000 } else { 0 },
                    state: if i < done_until {
                        SegmentState::Completed
                    } else if i == active_idx {
                        SegmentState::Active
                    } else {
                        SegmentState::Pending
                    },
                })
                .collect()
        };

    let transfers = vec![
        TransferSnapshot {
            id: "demo-1".into(),
            kind: TransferKind::Torrent,
            status: TransferStatus::Active,
            filename: "ubuntu-24.04.2-desktop-amd64.iso".into(),
            source_url: "https://releases.ubuntu.com/24.04.2/ubuntu-24.04.2-desktop-amd64.iso"
                .into(),
            source_domain: "releases.ubuntu.com".into(),
            save_dir: "C:\\Users\\moham\\Downloads".into(),
            total_bytes: Some(6_174_015_488),
            downloaded_bytes: 4_815_732_080,
            uploaded_bytes: 512_000_000,
            download_speed_bps: 18_420_000,
            upload_speed_bps: 1_420_000,
            eta_seconds: Some(74),
            active_connections: 16,
            active_peers: 42,
            protocol_label: "BitTorrent v2".into(),
            format_tags: vec!["ISO".into(), "42 peers".into()],
            ui_segments: [
                1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.92, 0.78, 0.44, 0.19, 0.0,
            ],
            segments: make_segs(11, 12, 78_000_000),
            sha256_verified: Some(true),
            output_superseded: false,
            resumable: true,
            queue_id: "default".into(),
            error_message: None,
            completion_notice: None,
            playlist_id: None,
            playlist_title: None,
            playlist_paused: None,
            playlist_index: None,
            playlist_total: None,
            speed_limit_bps: None,
            connect_rtt_ms: Some(38),
            description: None,
            created_at: now,
            updated_at: now,
        },
        TransferSnapshot {
            id: "demo-2".into(),
            kind: TransferKind::Media,
            status: TransferStatus::Active,
            filename: "Alpine_Aerial_Master_4K_HDR.mkv".into(),
            source_url: "https://media.example.org/watch?v=alpine4k".into(),
            source_domain: "media.example.org".into(),
            save_dir: "C:\\Users\\moham\\Downloads".into(),
            total_bytes: Some(1_514_000_000),
            downloaded_bytes: 635_880_000,
            uploaded_bytes: 0,
            download_speed_bps: 14_200_000,
            upload_speed_bps: 0,
            eta_seconds: Some(61),
            active_connections: 16,
            active_peers: 0,
            protocol_label: "DASH + FFmpeg".into(),
            format_tags: vec!["2160p".into(), "H.265".into()],
            ui_segments: [
                1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.72, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ],
            segments: make_segs(6, 6, 72_000_000),
            sha256_verified: None,
            output_superseded: false,
            resumable: true,
            queue_id: "default".into(),
            error_message: None,
            completion_notice: None,
            playlist_id: None,
            playlist_title: None,
            playlist_paused: None,
            playlist_index: None,
            playlist_total: None,
            speed_limit_bps: None,
            connect_rtt_ms: Some(52),
            description: None,
            created_at: now,
            updated_at: now,
        },
        TransferSnapshot {
            id: "demo-3".into(),
            kind: TransferKind::Http,
            status: TransferStatus::Paused,
            filename: "SlipStream-SDK-Symbols-v2.0.7z".into(),
            source_url: "https://cdn.example.org/sdk/SlipStream-SDK-Symbols-v2.0.7z".into(),
            source_domain: "cdn.example.org".into(),
            save_dir: "C:\\Users\\moham\\Downloads".into(),
            total_bytes: Some(920_000_000),
            downloaded_bytes: 312_800_000,
            uploaded_bytes: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            active_connections: 0,
            active_peers: 0,
            protocol_label: "HTTP/2 Range".into(),
            format_tags: vec!["7Z".into()],
            ui_segments: [
                1.0, 1.0, 1.0, 1.0, 1.0, 0.44, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ],
            segments: make_segs(5, 5, 44_000_000),
            sha256_verified: None,
            output_superseded: false,
            resumable: true,
            queue_id: "default".into(),
            error_message: None,
            completion_notice: None,
            playlist_id: None,
            playlist_title: None,
            playlist_paused: None,
            playlist_index: None,
            playlist_total: None,
            speed_limit_bps: None,
            connect_rtt_ms: Some(29),
            description: None,
            created_at: now,
            updated_at: now,
        },
        TransferSnapshot {
            id: "demo-4".into(),
            kind: TransferKind::Http,
            status: TransferStatus::Completed,
            filename: "rustup-init-x86_64-pc-windows-msvc.exe".into(),
            source_url:
                "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
                    .into(),
            source_domain: "static.rust-lang.org".into(),
            save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
            total_bytes: Some(13_840_384),
            downloaded_bytes: 13_840_384,
            uploaded_bytes: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            active_connections: 0,
            active_peers: 0,
            protocol_label: "HTTPS".into(),
            format_tags: vec!["EXE".into()],
            ui_segments: [1.0; UI_SEGMENT_COUNT],
            segments: Vec::new(),
            sha256_verified: Some(true),
            output_superseded: false,
            resumable: true,
            queue_id: "default".into(),
            error_message: None,
            completion_notice: Some("Defender quick scan clean • SHA-256 verified".into()),
            playlist_id: None,
            playlist_title: None,
            playlist_paused: None,
            playlist_index: None,
            playlist_total: None,
            speed_limit_bps: None,
            connect_rtt_ms: None,
            description: None,
            created_at: now,
            updated_at: now,
        },
        TransferSnapshot {
            id: "demo-5".into(),
            kind: TransferKind::Http,
            status: TransferStatus::Completed,
            filename: "vulkan-sdk-1.4.304-windows.exe".into(),
            source_url: "https://sdk.lunarg.com/sdk/download/1.4.304.0/windows/VulkanSDK.exe"
                .into(),
            source_domain: "sdk.lunarg.com".into(),
            save_dir: "C:\\Users\\moham\\Downloads\\Programs".into(),
            total_bytes: Some(412_560_112),
            downloaded_bytes: 412_560_112,
            uploaded_bytes: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            active_connections: 0,
            active_peers: 0,
            protocol_label: "HTTP/2".into(),
            format_tags: vec!["EXE".into()],
            ui_segments: [1.0; UI_SEGMENT_COUNT],
            segments: Vec::new(),
            sha256_verified: None,
            output_superseded: false,
            resumable: true,
            queue_id: "default".into(),
            error_message: None,
            completion_notice: None,
            playlist_id: None,
            playlist_title: None,
            playlist_paused: None,
            playlist_index: None,
            playlist_total: None,
            speed_limit_bps: None,
            connect_rtt_ms: None,
            description: None,
            created_at: now,
            updated_at: now,
        },
    ];

    TelemetryBatch {
        aggregate_down_bps: 32_620_000,
        aggregate_up_bps: 1_420_000,
        active_count: 2,
        active_threads: 32,
        speed_limit_bps: None,
        transfers,
        timestamp: now,
    }
}

pub fn demo_categories() -> Vec<DownloadCategory> {
    default_download_categories()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units_cleanly() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(15 * 1024 * 1024), "15.0 MB");
        assert_eq!(format_eta(Some(65)), "1m 05s");
        assert_eq!(format_duration(Some(3661)), "1:01:01");
    }

    #[test]
    fn sparkline_produces_cubic_and_fill_paths() {
        let (line, fill) = build_sparkline_paths(&[100, 300, 200, 400], 180.0, 28.0);
        assert!(line.starts_with("M "));
        assert!(line.contains(" C "));
        assert!(fill.ends_with(" Z"));
    }

    #[test]
    fn header_origin_resets_only_on_origin_mismatch() {
        assert!(!should_clear_seeded_headers(
            Some("https://example.com"),
            "https://example.com/other/file.mp4"
        ));
        assert!(should_clear_seeded_headers(
            Some("https://example.com"),
            "https://evil.test/video.mp4"
        ));
        assert!(should_clear_seeded_headers(
            Some("https://example.com"),
            "not-a-url"
        ));
        assert!(should_clear_seeded_headers(
            None,
            "https://example.com/file.mp4"
        ));
    }

    #[test]
    fn video_audio_pairing_picks_original_or_default_track() {
        let video = MediaFormatOption {
            format_id: "137".into(),
            label: "1080p".into(),
            container: "mp4".into(),
            video_codec: Some("avc1".into()),
            audio_codec: None,
            height: Some(1080),
            fps: Some(60.0),
            filesize_bytes: None,
            bitrate_kbps: None,
            audio_track_kind: None,
            is_audio_only: false,
            has_video: true,
            has_audio: false,
        };
        let dubbed = MediaFormatOption {
            format_id: "140-d".into(),
            label: "aac dubbed".into(),
            container: "m4a".into(),
            video_codec: None,
            audio_codec: Some("mp4a".into()),
            height: None,
            fps: None,
            filesize_bytes: None,
            bitrate_kbps: Some(256.0),
            audio_track_kind: Some(AudioTrackKind::Dubbed),
            is_audio_only: true,
            has_video: false,
            has_audio: true,
        };
        let original = MediaFormatOption {
            format_id: "251".into(),
            label: "opus orig".into(),
            container: "webm".into(),
            video_codec: None,
            audio_codec: Some("opus".into()),
            height: None,
            fps: None,
            filesize_bytes: None,
            bitrate_kbps: Some(160.0),
            audio_track_kind: Some(AudioTrackKind::Original),
            is_audio_only: true,
            has_video: false,
            has_audio: true,
        };
        assert_eq!(
            pair_video_with_best_audio(&video, &[dubbed, original]),
            "137+251"
        );
    }

    #[test]
    fn speed_limit_parsing_handles_kb_mb_and_blank() {
        assert_eq!(parse_speed_limit_bps("", false), Ok(None));
        assert_eq!(parse_speed_limit_bps("0", true), Ok(None));
        assert_eq!(parse_speed_limit_bps("512", false), Ok(Some(512 * 1024)));
        assert_eq!(parse_speed_limit_bps("2", true), Ok(Some(2 * 1_048_576)));
        assert!(parse_speed_limit_bps("-5", false).is_err());
    }
}
