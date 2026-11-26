use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const NATIVE_HOST_NAME: &str = "com.ciphervaults.slipstream";
pub const MAX_NATIVE_MESSAGE_BYTES: usize = 1_048_576; // 1 MiB strict framing cap
pub const UI_SEGMENT_COUNT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    Http,
    Media,
    Torrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Probing,
    Active,
    Remuxing,
    Paused,
    Seeding,
    Completed,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Pending,
    Active,
    Split,
    Completed,
    Paused,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSnapshot {
    pub index: u8,
    pub start_byte: u64,
    pub end_byte: u64,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub state: SegmentState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSnapshot {
    pub id: String,
    pub kind: TransferKind,
    pub status: TransferStatus,
    pub filename: String,
    pub source_url: String,
    pub source_domain: String,
    pub save_dir: String,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub eta_seconds: Option<u64>,
    pub active_connections: u16,
    pub active_peers: u32,
    pub protocol_label: String,
    pub format_tags: Vec<String>,
    /// Always 16 normalized progress values [0.0..=1.0] for the UI micro-pill bar.
    pub ui_segments: [f32; UI_SEGMENT_COUNT],
    pub segments: Vec<SegmentSnapshot>,
    pub sha256_verified: Option<bool>,
    pub resumable: bool,
    pub queue_id: String,
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_notice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_limit_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_rtt_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryBatch {
    pub aggregate_down_bps: u64,
    pub aggregate_up_bps: u64,
    pub active_count: u32,
    pub active_threads: u32,
    pub speed_limit_bps: Option<u64>,
    pub transfers: Vec<TransferSnapshot>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddHttpDownloadRequest {
    pub url: String,
    pub filename: Option<String>,
    pub save_dir: Option<String>,
    pub max_connections: Option<u8>,
    pub queue_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub referrer: Option<String>,
    pub expected_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_paused: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overwrite_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateMatch {
    pub transfer_id: String,
    pub filename: String,
    pub status: TransferStatus,
    pub save_dir: String,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDownloadPreview {
    pub pending_id: String,
    pub url: String,
    pub source_domain: String,
    pub filename: String,
    pub suggested_category_id: String,
    pub suggested_save_dir: String,
    pub expected_size_bytes: Option<u64>,
    pub duplicate: Option<DuplicateMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmPendingDownloadRequest {
    pub pending_id: String,
    pub filename: String,
    pub save_dir: String,
    pub category_id: String,
    pub remember_category_path: bool,
    pub start_immediately: bool,
    pub overwrite_existing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCompletionOptions {
    pub show_complete_dialog: bool,
    pub auto_close_progress: bool,
    pub open_folder_on_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshDownloadUrlRequest {
    pub transfer_id: String,
    pub new_url: String,
    pub headers: HashMap<String, String>,
    pub referrer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioPreset {
    Mp3_320,
    M4aAac,
    Opus,
    Flac,
    Wav,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaContainer {
    Mp4,
    Mkv,
    Webm,
    Mp3,
    M4a,
    Flac,
    Opus,
    Wav,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AudioTrackKind {
    Descriptive,
    Dubbed,
    #[default]
    Unspecified,
    Default,
    Original,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFormatOption {
    pub format_id: String,
    pub label: String,
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub filesize_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_track_kind: Option<AudioTrackKind>,
    pub is_audio_only: bool,
    pub has_video: bool,
    pub has_audio: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub lang: String,
    pub name: String,
    pub is_auto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistEntry {
    /// 1-based index within the playlist.
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaProbeResult {
    pub title: String,
    pub source_url: String,
    pub extractor: String,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub is_playlist: bool,
    pub playlist_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlist_entries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlist_items: Vec<PlaylistEntry>,
    pub formats: Vec<MediaFormatOption>,
    pub subtitles: Vec<SubtitleTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedPlaylistEntry {
    /// 1-based index within the playlist.
    pub index: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMediaDownloadRequest {
    pub url: String,
    pub format_id: Option<String>,
    pub audio_only_preset: Option<AudioPreset>,
    pub target_container: Option<MediaContainer>,
    pub embed_subtitles_langs: Vec<String>,
    pub lossless_remux: bool,
    pub normalize_audio: bool,
    pub save_dir: Option<String>,
    pub filename: Option<String>,
    pub playlist_items: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub download_playlist: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_entries: Option<Vec<SelectedPlaylistEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_folder_name: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileEntry {
    pub index: usize,
    pub relative_path: String,
    pub size_bytes: u64,
    pub downloaded_bytes: u64,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTorrentRequest {
    pub source: String, // magnet:?... or path to .torrent file
    pub save_dir: Option<String>,
    pub selected_file_indices: Option<Vec<usize>>,
    pub sequential: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentMode {
    Blue,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostQueueAction {
    None,
    Notify,
    Sleep,
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    #[default]
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateProgressStage {
    Downloading,
    Verifying,
    Installing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateProgress {
    pub stage: UpdateProgressStage,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Custom,
    Managed,
    Bundled,
    SystemPath,
    #[default]
    Missing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Firefox,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserStatus {
    pub id: BrowserKind,
    pub name: String,
    pub installed: bool,
    pub native_host_registered: bool,
    pub extension_url: String,
    pub bundled_extension_dir: Option<String>,
    pub extension_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserSetupResult {
    pub browser_id: BrowserKind,
    pub launched_browser: bool,
    pub revealed_extension_dir: bool,
    pub native_host_registered: bool,
    pub extension_dir: String,
    pub extension_page_url: String,
    pub extension_id: String,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatus {
    pub available: bool,
    pub current_version: String,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadCategory {
    pub id: String,
    pub name: String,
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_dir: Option<String>,
}

pub fn default_download_categories() -> Vec<DownloadCategory> {
    let categories: [(&str, &str, &[&str]); 6] = [
        ("general", "General", &[]),
        (
            "programs",
            "Programs",
            &[
                "exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "apk", "bin", "bat",
                "cmd",
            ],
        ),
        (
            "compressed",
            "Compressed",
            &[
                "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso", "img",
            ],
        ),
        (
            "video",
            "Video",
            &[
                "mp4", "mkv", "webm", "avi", "mov", "m4v", "flv", "wmv", "ts",
            ],
        ),
        (
            "music",
            "Music",
            &["mp3", "flac", "wav", "m4a", "ogg", "opus", "aac", "wma"],
        ),
        (
            "documents",
            "Documents",
            &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub", "csv",
            ],
        ),
    ];
    categories
        .into_iter()
        .map(|(id, name, extensions)| DownloadCategory {
            id: id.to_owned(),
            name: name.to_owned(),
            extensions: extensions
                .iter()
                .map(|extension| (*extension).to_owned())
                .collect(),
            save_dir: None,
        })
        .collect()
}

fn default_show_dialog() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub theme: ThemeMode,
    pub accent: AccentMode,
    pub default_download_dir: String,
    pub default_connections: u8,
    pub global_speed_limit_bps: Option<u64>,
    pub max_concurrent_downloads: u8,
    pub auto_defender_scan: bool,
    pub close_to_tray_hibernate: bool,
    pub monitor_clipboard: bool,
    pub ytdlp_path: Option<String>,
    pub ffmpeg_path: Option<String>,
    #[serde(default)]
    pub update_mode: UpdateMode,
    #[serde(default = "default_show_dialog")]
    pub show_download_dialog: bool,
    #[serde(default = "default_show_dialog")]
    pub show_progress_dialog: bool,
    #[serde(default = "default_show_dialog")]
    pub show_complete_dialog: bool,
    #[serde(default = "default_download_categories")]
    pub categories: Vec<DownloadCategory>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            accent: AccentMode::Blue,
            default_download_dir: String::new(),
            default_connections: 16,
            global_speed_limit_bps: None,
            max_concurrent_downloads: 4,
            auto_defender_scan: true,
            close_to_tray_hibernate: true,
            monitor_clipboard: false,
            ytdlp_path: None,
            ffmpeg_path: None,
            update_mode: UpdateMode::Auto,
            show_download_dialog: default_show_dialog(),
            show_progress_dialog: default_show_dialog(),
            show_complete_dialog: default_show_dialog(),
            categories: default_download_categories(),
        }
    }
}

/// Requests sent from the Browser Native Messaging host over the authenticated Named Pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeBridgeRequest {
    Authenticate {
        token: String,
        client_id: String,
    },
    Ping,
    GetStatus,
    AddHttpDownload {
        request: AddHttpDownloadRequest,
    },
    ProbeMedia {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playlist: Option<bool>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    AddMediaDownload {
        request: AddMediaDownloadRequest,
    },
    AddTorrent {
        request: AddTorrentRequest,
    },
    RefreshDownloadUrl {
        request: RefreshDownloadUrlRequest,
    },
    PauseTransfer {
        transfer_id: String,
    },
    ResumeTransfer {
        transfer_id: String,
    },
    OpenDesktopWindow {
        open_picker_url: Option<String>,
    },
    GrabBatchUrls {
        urls: Vec<String>,
        referrer: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeBridgeResponse {
    Ack {
        ok: bool,
        message: Option<String>,
    },
    Status {
        batch: TelemetryBatch,
        theme: ThemeMode,
        accent: AccentMode,
    },
    TransferCreated {
        transfer_id: String,
    },
    MediaProbed {
        probe: MediaProbeResult,
    },
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod media_wire_tests {
    use super::*;
    use serde_json::{json, to_value};

    #[test]
    fn legacy_http_requests_and_settings_supply_dialog_defaults() {
        let wire = json!({
            "url": "https://example.com/file", "filename": null, "save_dir": null,
            "max_connections": null, "queue_id": null, "headers": {}, "referrer": null,
            "expected_sha256": null
        });
        let request: AddHttpDownloadRequest = serde_json::from_value(wire.clone()).unwrap();
        assert!(!request.start_paused && !request.overwrite_existing);
        assert_eq!(request.expected_size_bytes, None);
        assert_eq!(to_value(request).unwrap(), wire);

        let defaults = to_value(AppSettings::default()).unwrap();
        let mut legacy = defaults.clone();
        for key in [
            "show_download_dialog",
            "show_progress_dialog",
            "show_complete_dialog",
            "categories",
        ] {
            legacy.as_object_mut().unwrap().remove(key);
        }
        let settings: AppSettings = serde_json::from_value(legacy).unwrap();
        assert!(
            settings.show_download_dialog
                && settings.show_progress_dialog
                && settings.show_complete_dialog
        );
        assert_eq!(settings.categories, default_download_categories());
        assert_eq!(to_value(settings).unwrap(), defaults);
    }

    #[test]
    fn media_download_playlist_fields_default_and_round_trip() {
        let request: NativeBridgeRequest = serde_json::from_str(
            r#"{
            "type":"add_media_download",
            "request":{"url":"https://example.com/video","embed_subtitles_langs":[],
                       "lossless_remux":true,"normalize_audio":false}
        }"#,
        )
        .unwrap();
        let NativeBridgeRequest::AddMediaDownload { mut request } = request else {
            panic!("wrong request")
        };
        assert!(!request.download_playlist);
        assert_eq!(request.max_height, None);
        assert_eq!(request.playlist_items, None);
        assert!(request.selected_entries.is_none());
        assert!(request.playlist_folder_name.is_none());
        assert!(request.headers.is_empty());
        let legacy = to_value(&request).unwrap();
        for key in [
            "download_playlist",
            "max_height",
            "selected_entries",
            "playlist_folder_name",
            "headers",
        ] {
            assert!(legacy.get(key).is_none(), "unexpected {key}");
        }
        request.download_playlist = true;
        request.max_height = Some(1080);
        request.selected_entries = Some(vec![SelectedPlaylistEntry {
            index: 2,
            title: "Second clip".into(),
            url: Some("https://example.com/second".into()),
        }]);
        request.playlist_folder_name = Some("Playlist".into());
        request
            .headers
            .insert("User-Agent".into(), "SlipStream".into());
        let wire = to_value(NativeBridgeRequest::AddMediaDownload { request }).unwrap();
        let decoded: NativeBridgeRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(to_value(decoded).unwrap(), wire);
        let wire = &wire["request"];
        assert_eq!(wire["download_playlist"], true);
        assert_eq!(wire["max_height"], 1080);
        assert_eq!(
            wire["selected_entries"],
            json!([
                {"index": 2, "title": "Second clip", "url": "https://example.com/second"}
            ])
        );
        assert_eq!(wire["playlist_folder_name"], "Playlist");
        assert_eq!(wire["headers"], json!({"User-Agent": "SlipStream"}));
    }

    #[test]
    fn media_probe_keeps_legacy_playlist_titles_and_round_trips_items() {
        let probe: MediaProbeResult = serde_json::from_str(
            r#"{
            "title":"Older probe","source_url":"https://example.com/video","extractor":"generic",
            "is_playlist":false,"formats":[],"subtitles":[]
        }"#,
        )
        .unwrap();
        assert!(probe.playlist_entries.is_empty());
        assert!(probe.playlist_items.is_empty());
        let mut wire = to_value(probe).unwrap();
        assert!(wire.get("playlist_entries").is_none());
        assert!(wire.get("playlist_items").is_none());

        wire["is_playlist"] = json!(true);
        wire["playlist_entries"] = json!(["First clip", "Second clip"]);
        let older_playlist: MediaProbeResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            older_playlist.playlist_entries,
            ["First clip", "Second clip"]
        );
        assert!(older_playlist.playlist_items.is_empty());
        assert_eq!(to_value(older_playlist).unwrap(), wire);

        wire["playlist_items"] = json!([
            {"index": 1, "title": "First clip"},
            {"index": 2, "id": "clip02", "title": "Second clip", "url": "https://example.com/second",
             "duration_seconds": 42, "thumbnail_url": "https://example.com/second.jpg"}
        ]);
        let decoded: MediaProbeResult = serde_json::from_value(wire.clone()).unwrap();
        assert!(decoded.playlist_items[0].id.is_none());
        assert!(decoded.playlist_items[0].url.is_none());
        assert!(decoded.playlist_items[0].duration_seconds.is_none());
        assert!(decoded.playlist_items[0].thumbnail_url.is_none());
        assert_eq!(to_value(decoded).unwrap(), wire);
    }

    #[test]
    fn transfer_snapshot_playlist_fields_default_and_round_trip() {
        let ui_segments = [0.0_f32; UI_SEGMENT_COUNT];
        let mut wire = json!({
            "id": "transfer", "kind": "media", "status": "queued", "filename": "clip.mp4",
            "source_url": "https://example.com/video", "source_domain": "example.com", "save_dir": "Downloads",
            "total_bytes": null, "downloaded_bytes": 0, "uploaded_bytes": 0, "download_speed_bps": 0,
            "upload_speed_bps": 0, "eta_seconds": null, "active_connections": 0, "active_peers": 0,
            "protocol_label": "Media", "format_tags": [], "ui_segments": ui_segments,
            "segments": [], "sha256_verified": null, "resumable": false, "queue_id": "default",
            "error_message": null, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        });
        let legacy: TransferSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert!(legacy.playlist_id.is_none());
        assert!(legacy.playlist_title.is_none());
        assert!(legacy.playlist_paused.is_none());
        assert!(legacy.playlist_index.is_none());
        assert!(legacy.playlist_total.is_none());
        assert_eq!(to_value(legacy).unwrap(), wire);

        wire["playlist_id"] = json!("playlist01");
        wire["playlist_title"] = json!("Playlist");
        wire["playlist_paused"] = json!(true);
        wire["playlist_index"] = json!(2);
        wire["playlist_total"] = json!(10);
        let decoded: TransferSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(to_value(decoded).unwrap(), wire);
    }

    #[test]
    fn selected_playlist_entries_accept_index_only_wire_data() {
        for wire in [
            json!({"index": 8, "title": "Video 8"}),
            json!({"index": 8, "title": "Video 8", "url": null}),
        ] {
            let entry: SelectedPlaylistEntry = serde_json::from_value(wire).unwrap();
            assert!(entry.url.is_none());
            assert_eq!(
                to_value(entry).unwrap(),
                json!({"index": 8, "title": "Video 8"})
            );
        }
    }

    #[test]
    fn native_probe_media_fields_default_and_round_trip() {
        let mut wire = json!({"type": "probe_media", "url": "https://example.com/video"});
        let legacy: NativeBridgeRequest = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(
            legacy,
            NativeBridgeRequest::ProbeMedia { playlist: None, ref headers, .. } if headers.is_empty()
        ));
        assert_eq!(to_value(legacy).unwrap(), wire);

        for playlist in [true, false] {
            wire["playlist"] = json!(playlist);
            let decoded: NativeBridgeRequest = serde_json::from_value(wire.clone()).unwrap();
            assert!(
                matches!(decoded, NativeBridgeRequest::ProbeMedia { playlist: Some(value), .. } if value == playlist)
            );
            assert_eq!(to_value(decoded).unwrap(), wire);
        }

        wire["headers"] = json!({"Referer": "https://example.com/", "Accept-Language": "en-US"});
        let decoded: NativeBridgeRequest = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(
            decoded,
            NativeBridgeRequest::ProbeMedia { ref headers, .. }
                if headers.get("Referer").is_some_and(|value| value == "https://example.com/")
        ));
        assert_eq!(to_value(decoded).unwrap(), wire);
    }
}
