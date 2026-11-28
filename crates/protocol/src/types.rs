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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_superseded: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirror_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_discover_checksum: bool,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_start_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_end_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub split_chapters: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sponsorblock_categories: Vec<String>,
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
pub struct TorrentPeerEntry {
    pub address: String,
    pub state: String,
    pub downloaded_bytes: u64,
    pub download_speed_bps: u64,
    pub pieces_downloaded: u32,
    pub incoming: bool,
    pub errors: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentPeerSummary {
    pub live: u32,
    pub connecting: u32,
    pub queued: u32,
    pub seen: u32,
    pub dead: u32,
    pub upload_speed_bps: u64,
    pub ratio: f64,
    pub completed_pieces: u32,
    pub total_pieces: u32,
    pub trackers: Vec<String>,
    pub peers: Vec<TorrentPeerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentPreviewResult {
    pub info_hash: String,
    pub name: String,
    pub version: String,
    pub total_bytes: u64,
    pub piece_length: u64,
    pub total_pieces: usize,
    pub private: bool,
    pub trackers: Vec<String>,
    pub files: Vec<TorrentFileEntry>,
    pub seen_peers: u32,
    pub is_magnet: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTorrentRequest {
    pub source: String, // magnet:?... or path to .torrent file
    pub save_dir: Option<String>,
    pub selected_file_indices: Option<Vec<usize>>,
    pub sequential: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_paused: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostQueueAction {
    #[default]
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

fn default_sort_column() -> String {
    "date".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub category: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeedSample {
    pub timestamp_ms: i64,
    pub down_bps: u64,
    pub up_bps: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Direct,
    System,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub url: Option<String>,
    pub username: Option<String>,
    pub has_password: bool,
    pub bypass_hosts: Vec<String>,
    pub apply_to_media: bool,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::Direct,
            url: None,
            username: None,
            has_password: false,
            bypass_hosts: Vec::new(),
            apply_to_media: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteAuthScheme {
    #[default]
    Basic,
    Bearer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteLoginEntry {
    pub id: String,
    pub host_pattern: String,
    pub username: String,
    #[serde(default)]
    pub has_password: bool,
    #[serde(default)]
    pub auth_scheme: SiteAuthScheme,
    #[serde(default = "default_show_dialog")]
    pub enabled: bool,
}

impl Default for SiteLoginEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            host_pattern: String::new(),
            username: String::new(),
            has_password: false,
            auth_scheme: SiteAuthScheme::default(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CookiePassThroughSettings {
    pub enabled: bool,
    pub browser: String,
    pub allowed_hosts: Vec<String>,
}

impl Default for CookiePassThroughSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            browser: "chrome".into(),
            allowed_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaPowerSettings {
    pub sponsorblock_remove: Vec<String>,
    pub embed_chapters: bool,
    pub split_chapters: bool,
    pub embed_thumbnail: bool,
    pub embed_metadata: bool,
    pub filename_template: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteGrabRequest {
    pub url: String,
    pub max_depth: u8,
    pub same_origin_only: bool,
    pub extensions: Vec<String>,
    pub include_pattern: String,
    pub exclude_pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteGrabItem {
    pub url: String,
    pub filename: String,
    pub kind: String,
    pub source_page: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteGrabResult {
    pub scanned_pages: usize,
    pub items: Vec<SiteGrabItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetalinkFileEntry {
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub mirrors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub launch_on_startup: bool,
    #[serde(default)]
    pub launch_minimized: bool,
    #[serde(default = "default_show_dialog")]
    pub notify_on_complete: bool,
    #[serde(default)]
    pub sound_on_complete: bool,
    #[serde(default)]
    pub show_drop_basket: bool,
    #[serde(default)]
    pub compact_table_view: bool,
    #[serde(default = "default_sort_column")]
    pub sort_column: String,
    #[serde(default)]
    pub sort_ascending: bool,
    #[serde(default)]
    pub alt_speed_enabled: bool,
    #[serde(default = "default_alt_speed_limit_bps")]
    pub alt_speed_limit_bps: Option<u64>,
    #[serde(default)]
    pub schedule: ScheduleSettings,
    #[serde(default)]
    pub bandwidth_guard: BandwidthGuardSettings,
    #[serde(default = "default_download_queues")]
    pub queues: Vec<DownloadQueueSpec>,
    #[serde(default)]
    pub watch_folders: Vec<WatchFolderRule>,
    #[serde(default)]
    pub auto_extract: AutoExtractSettings,
    #[serde(default)]
    pub torrent_network: TorrentNetworkSettings,
    #[serde(default)]
    pub torrent_seeding: TorrentSeedingSettings,
    #[serde(default)]
    pub rss_feeds: Vec<RssFeedRule>,
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default)]
    pub site_logins: Vec<SiteLoginEntry>,
    #[serde(default)]
    pub cookie_passthrough: CookiePassThroughSettings,
    #[serde(default)]
    pub media_power: MediaPowerSettings,
    #[serde(default = "default_show_dialog")]
    pub auto_discover_checksums: bool,
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
            launch_on_startup: false,
            launch_minimized: false,
            notify_on_complete: true,
            sound_on_complete: false,
            show_drop_basket: false,
            compact_table_view: false,
            sort_column: default_sort_column(),
            sort_ascending: false,
            alt_speed_enabled: false,
            alt_speed_limit_bps: default_alt_speed_limit_bps(),
            schedule: ScheduleSettings::default(),
            bandwidth_guard: BandwidthGuardSettings::default(),
            queues: default_download_queues(),
            watch_folders: Vec::new(),
            auto_extract: AutoExtractSettings::default(),
            torrent_network: TorrentNetworkSettings::default(),
            torrent_seeding: TorrentSeedingSettings::default(),
            rss_feeds: Vec::new(),
            proxy: ProxySettings::default(),
            site_logins: Vec::new(),
            cookie_passthrough: CookiePassThroughSettings::default(),
            media_power: MediaPowerSettings::default(),
            auto_discover_checksums: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    #[default]
    Disabled,
    OffPeakWindow,
    SnailWindow,
}

fn default_schedule_start_minute() -> u16 {
    22 * 60
}

fn default_schedule_end_minute() -> u16 {
    7 * 60
}

pub fn default_alt_speed_limit_bps() -> Option<u64> {
    Some(512 * 1024)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSettings {
    #[serde(default)]
    pub mode: ScheduleMode,
    #[serde(default = "default_schedule_start_minute")]
    pub start_minute_of_day: u16,
    #[serde(default = "default_schedule_end_minute")]
    pub end_minute_of_day: u16,
    #[serde(default)]
    pub weekdays_only: bool,
    #[serde(default = "default_alt_speed_limit_bps")]
    pub window_speed_limit_bps: Option<u64>,
    #[serde(default)]
    pub post_window_pause: bool,
    #[serde(default)]
    pub stop_torrents_at_end: bool,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            mode: ScheduleMode::Disabled,
            start_minute_of_day: default_schedule_start_minute(),
            end_minute_of_day: default_schedule_end_minute(),
            weekdays_only: false,
            window_speed_limit_bps: default_alt_speed_limit_bps(),
            post_window_pause: false,
            stop_torrents_at_end: false,
        }
    }
}

fn default_guard_processes() -> Vec<String> {
    [
        "zoom.exe",
        "teams.exe",
        "ms-teams.exe",
        "discord.exe",
        "obs64.exe",
        "valorant.exe",
        "cs2.exe",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_guard_speed_limit_bps() -> u64 {
    256 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandwidthGuardSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_show_dialog")]
    pub throttle_on_fullscreen: bool,
    #[serde(default = "default_guard_processes")]
    pub watched_processes: Vec<String>,
    #[serde(default = "default_guard_speed_limit_bps")]
    pub guard_speed_limit_bps: u64,
    #[serde(default)]
    pub pause_instead_of_throttle: bool,
}

impl Default for BandwidthGuardSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            throttle_on_fullscreen: true,
            watched_processes: default_guard_processes(),
            guard_speed_limit_bps: default_guard_speed_limit_bps(),
            pause_instead_of_throttle: false,
        }
    }
}

fn default_queue_concurrency() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadQueueSpec {
    pub id: String,
    pub name: String,
    #[serde(default = "default_queue_concurrency")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub speed_limit_bps: Option<u64>,
    #[serde(default)]
    pub post_action: PostQueueAction,
    #[serde(default)]
    pub paused: bool,
}

pub fn default_download_queues() -> Vec<DownloadQueueSpec> {
    vec![
        DownloadQueueSpec {
            id: "default".into(),
            name: "Main Queue".into(),
            max_concurrent: 3,
            speed_limit_bps: None,
            post_action: PostQueueAction::None,
            paused: false,
        },
        DownloadQueueSpec {
            id: "bulk".into(),
            name: "Bulk / Night Queue".into(),
            max_concurrent: 2,
            speed_limit_bps: None,
            post_action: PostQueueAction::None,
            paused: false,
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchFolderRule {
    pub id: String,
    pub folder: String,
    #[serde(default)]
    pub target_save_dir: Option<String>,
    #[serde(default)]
    pub queue_id: Option<String>,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default = "default_show_dialog")]
    pub enabled: bool,
    #[serde(default)]
    pub delete_torrent_after_add: bool,
    #[serde(default)]
    pub start_paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoExtractSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_show_dialog")]
    pub create_subfolder: bool,
    #[serde(default)]
    pub delete_archive_after_extract: bool,
    #[serde(default)]
    pub overwrite_existing: bool,
}

impl Default for AutoExtractSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            create_subfolder: true,
            delete_archive_after_extract: false,
            overwrite_existing: false,
        }
    }
}

fn default_listen_port_start() -> u16 {
    6881
}

fn default_listen_port_end() -> u16 {
    6891
}

fn default_rss_poll_interval_minutes() -> u16 {
    30
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentNetworkSettings {
    #[serde(default = "default_listen_port_start")]
    pub listen_port_start: u16,
    #[serde(default = "default_listen_port_end")]
    pub listen_port_end: u16,
    #[serde(default = "default_show_dialog")]
    pub enable_dht: bool,
    #[serde(default)]
    pub enable_upnp: bool,
    #[serde(default)]
    pub global_upload_limit_bps: Option<u64>,
    #[serde(default)]
    pub socks5_proxy_url: Option<String>,
    #[serde(default)]
    pub blocklist_path: Option<String>,
    #[serde(default)]
    pub vpn_kill_switch_enabled: bool,
    #[serde(default)]
    pub vpn_adapter_name: Option<String>,
}

impl Default for TorrentNetworkSettings {
    fn default() -> Self {
        Self {
            listen_port_start: default_listen_port_start(),
            listen_port_end: default_listen_port_end(),
            enable_dht: true,
            enable_upnp: false,
            global_upload_limit_bps: None,
            socks5_proxy_url: None,
            blocklist_path: None,
            vpn_kill_switch_enabled: false,
            vpn_adapter_name: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentSeedingSettings {
    #[serde(default)]
    pub max_seed_ratio_hundredths: Option<u32>,
    #[serde(default)]
    pub max_seed_minutes: Option<u32>,
    #[serde(default = "default_show_dialog")]
    pub stop_seeding_on_limit: bool,
}

impl Default for TorrentSeedingSettings {
    fn default() -> Self {
        Self {
            max_seed_ratio_hundredths: None,
            max_seed_minutes: None,
            stop_seeding_on_limit: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RssFeedRule {
    pub id: String,
    pub name: String,
    pub feed_url: String,
    #[serde(default)]
    pub include_pattern: String,
    #[serde(default)]
    pub exclude_pattern: String,
    #[serde(default)]
    pub save_dir: Option<String>,
    #[serde(default = "default_rss_poll_interval_minutes")]
    pub poll_interval_minutes: u16,
    #[serde(default = "default_show_dialog")]
    pub enabled: bool,
    #[serde(default = "default_show_dialog")]
    pub auto_start: bool,
    #[serde(default)]
    pub last_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTorrentRequest {
    pub source_path: String,
    pub output_path: String,
    #[serde(default)]
    pub trackers: Vec<String>,
    #[serde(default)]
    pub piece_length_kib: u32,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTorrentResult {
    pub output_path: String,
    pub info_hash: String,
    pub total_bytes: u64,
    pub piece_count: usize,
    pub file_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationStatus {
    pub alt_speed_active: bool,
    pub effective_speed_limit_bps: Option<u64>,
    pub schedule_window_active: bool,
    pub guard_active: bool,
    pub guard_reason: Option<String>,
    pub active_queue_actions: usize,
    #[serde(default)]
    pub vpn_kill_switch_active: bool,
    #[serde(default)]
    pub vpn_adapter_connected: bool,
    #[serde(default)]
    pub vpn_adapter_name: Option<String>,
    #[serde(default)]
    pub rss_feeds_enabled: u32,
    #[serde(default)]
    pub rss_matches_added: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsBackupBundle {
    pub schema_version: u32,
    pub exported_at_epoch_ms: i64,
    pub app_version: String,
    pub settings: AppSettings,
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
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
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
    fn v3_transfer_metadata_and_settings_default_and_round_trip() {
        let ui_segments = [0.0_f32; UI_SEGMENT_COUNT];
        let mut wire = json!({
            "id": "transfer", "kind": "http", "status": "queued", "filename": "archive.zip",
            "source_url": "https://example.com/archive.zip", "source_domain": "example.com",
            "save_dir": "Downloads", "total_bytes": null, "downloaded_bytes": 0, "uploaded_bytes": 0,
            "download_speed_bps": 0, "upload_speed_bps": 0, "eta_seconds": null,
            "active_connections": 0, "active_peers": 0, "protocol_label": "HTTPS",
            "format_tags": [], "ui_segments": ui_segments, "segments": [],
            "sha256_verified": null, "resumable": false, "queue_id": "default",
            "error_message": null, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        });
        let legacy: TransferSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert!(legacy.category_id.is_none());
        assert!(legacy.tags.is_empty());
        assert_eq!(to_value(legacy).unwrap(), wire);

        wire["category_id"] = json!("compressed");
        wire["tags"] = json!(["work", "backup"]);
        let decoded: TransferSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(decoded.category_id.as_deref(), Some("compressed"));
        assert_eq!(decoded.tags, vec!["work", "backup"]);
        assert_eq!(to_value(decoded).unwrap(), wire);

        let legacy_settings: AppSettings = serde_json::from_str(
            r#"{
            "theme":"dark","accent":"blue","default_download_dir":"Downloads",
            "default_connections":16,"global_speed_limit_bps":null,"max_concurrent_downloads":4,
            "auto_defender_scan":true,"close_to_tray_hibernate":true,"monitor_clipboard":false,
            "ytdlp_path":null,"ffmpeg_path":null
        }"#,
        )
        .unwrap();
        assert!(!legacy_settings.launch_on_startup);
        assert!(!legacy_settings.launch_minimized);
        assert!(legacy_settings.notify_on_complete);
        assert!(!legacy_settings.sound_on_complete);
        assert!(!legacy_settings.show_drop_basket);
        assert!(!legacy_settings.compact_table_view);
        assert_eq!(legacy_settings.sort_column, "date");
        assert!(!legacy_settings.sort_ascending);
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

    #[test]
    fn phase2_automation_settings_default_and_round_trip() {
        let legacy: AppSettings = serde_json::from_str(
            r#"{
            "theme":"dark","accent":"blue","default_download_dir":"Downloads",
            "default_connections":16,"global_speed_limit_bps":null,"max_concurrent_downloads":4,
            "auto_defender_scan":true,"close_to_tray_hibernate":true,"monitor_clipboard":false,
            "ytdlp_path":null,"ffmpeg_path":null
        }"#,
        )
        .unwrap();
        assert!(!legacy.alt_speed_enabled);
        assert_eq!(legacy.alt_speed_limit_bps, Some(512 * 1024));
        assert_eq!(legacy.schedule.mode, ScheduleMode::Disabled);
        assert_eq!(legacy.schedule.start_minute_of_day, 22 * 60);
        assert_eq!(legacy.schedule.end_minute_of_day, 7 * 60);
        assert!(!legacy.bandwidth_guard.enabled);
        assert!(legacy.bandwidth_guard.throttle_on_fullscreen);
        assert_eq!(legacy.queues.len(), 2);
        assert_eq!(legacy.queues[0].id, "default");
        assert_eq!(legacy.queues[1].id, "bulk");
        assert!(legacy.watch_folders.is_empty());
        assert!(!legacy.auto_extract.enabled);
        assert!(legacy.auto_extract.create_subfolder);

        let bundle = SettingsBackupBundle {
            schema_version: 1,
            exported_at_epoch_ms: 1_795_800_000_000,
            app_version: "3.0.0-dev".into(),
            settings: AppSettings::default(),
        };
        let encoded = serde_json::to_string(&bundle).unwrap();
        let decoded: SettingsBackupBundle = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn phase4_network_media_settings_default_and_round_trip() {
        let defaults = AppSettings::default();
        let mut legacy = serde_json::to_value(&defaults).unwrap();
        for field in [
            "proxy",
            "site_logins",
            "cookie_passthrough",
            "media_power",
            "auto_discover_checksums",
        ] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(
            serde_json::from_value::<AppSettings>(legacy).unwrap(),
            defaults
        );
        assert_eq!(defaults.proxy.mode, ProxyMode::Direct);
        assert!(defaults.proxy.apply_to_media);
        assert_eq!(defaults.cookie_passthrough.browser, "chrome");
        assert!(defaults.auto_discover_checksums);

        let mut settings = defaults;
        settings.proxy.mode = ProxyMode::Manual;
        settings.proxy.url = Some("http://proxy.example:8080".into());
        settings
            .cookie_passthrough
            .allowed_hosts
            .push("*.example.org".into());
        settings
            .media_power
            .sponsorblock_remove
            .push("intro".into());
        settings.site_logins.push(
            serde_json::from_str(r#"{"id":"one","host_pattern":"example.org","username":"user"}"#)
                .unwrap(),
        );
        assert_eq!(settings.site_logins[0].auth_scheme, SiteAuthScheme::Basic);
        assert!(settings.site_logins[0].enabled);
        assert_eq!(
            serde_json::from_str::<AppSettings>(&serde_json::to_string(&settings).unwrap())
                .unwrap(),
            settings
        );
    }

    #[test]
    fn phase3_bittorrent_settings_default_and_round_trip() {
        let legacy: AppSettings = serde_json::from_str(
            r#"{
            "theme":"dark","accent":"blue","default_download_dir":"Downloads",
            "default_connections":16,"global_speed_limit_bps":null,"max_concurrent_downloads":4,
            "auto_defender_scan":true,"close_to_tray_hibernate":true,"monitor_clipboard":false,
            "ytdlp_path":null,"ffmpeg_path":null
        }"#,
        )
        .unwrap();
        assert_eq!(legacy.torrent_network, TorrentNetworkSettings::default());
        assert_eq!(legacy.torrent_network.listen_port_start, 6881);
        assert_eq!(legacy.torrent_network.listen_port_end, 6891);
        assert!(legacy.torrent_network.enable_dht);
        assert!(!legacy.torrent_network.enable_upnp);
        assert!(!legacy.torrent_network.vpn_kill_switch_enabled);
        assert_eq!(legacy.torrent_seeding, TorrentSeedingSettings::default());
        assert!(legacy.torrent_seeding.stop_seeding_on_limit);
        assert!(legacy.rss_feeds.is_empty());

        let mut custom = AppSettings::default();
        custom.torrent_network.listen_port_start = 51413;
        custom.torrent_network.listen_port_end = 51420;
        custom.torrent_network.enable_upnp = true;
        custom.torrent_network.global_upload_limit_bps = Some(256 * 1024);
        custom.torrent_network.socks5_proxy_url = Some("socks5h://127.0.0.1:1080".into());
        custom.torrent_network.blocklist_path = Some(r"C:\blocklists\ip.p2p".into());
        custom.torrent_network.vpn_kill_switch_enabled = true;
        custom.torrent_network.vpn_adapter_name = Some("WireGuard Tunnel".into());
        custom.torrent_seeding.max_seed_ratio_hundredths = Some(200);
        custom.torrent_seeding.max_seed_minutes = Some(1440);
        custom.torrent_seeding.stop_seeding_on_limit = false;
        custom.rss_feeds.push(RssFeedRule {
            id: "linux-isos".into(),
            name: "Ubuntu".into(),
            feed_url: "https://example.org/rss.xml".into(),
            include_pattern: "*desktop*amd64*".into(),
            exclude_pattern: "*beta*".into(),
            save_dir: Some(r"C:\Downloads\ISOs".into()),
            poll_interval_minutes: 15,
            enabled: true,
            auto_start: true,
            last_checked_at: None,
            last_error: None,
        });

        let encoded = serde_json::to_string(&custom).unwrap();
        let decoded: AppSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, custom);

        let status_legacy: AutomationStatus = serde_json::from_str(
            r#"{"alt_speed_active":false,"effective_speed_limit_bps":null,"schedule_window_active":false,"guard_active":false,"guard_reason":null,"active_queue_actions":0}"#,
        )
        .unwrap();
        assert!(!status_legacy.vpn_kill_switch_active);
        assert_eq!(status_legacy.rss_matches_added, 0);
    }
}
