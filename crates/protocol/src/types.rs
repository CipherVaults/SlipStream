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
pub struct MediaProbeResult {
    pub title: String,
    pub source_url: String,
    pub extractor: String,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub is_playlist: bool,
    pub playlist_count: Option<u32>,
    pub formats: Vec<MediaFormatOption>,
    pub subtitles: Vec<SubtitleTrack>,
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
