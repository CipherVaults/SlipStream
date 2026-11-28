use crate::{
    automation::{compute_effective_decision, scan_watch_folders, WatchFolderTracker},
    extractor::{extract_archive_after_open, is_supported_archive},
    media_jobs::{MediaJob, MediaJobEvent, MediaJobRunOptions, PlaylistJobMeta, ScanRequest},
    paths::{
        build_defender_scan_args, canonical_directory, confined_target, decode_sha256,
        delete_files, ensure_playlist_subfolder, ensure_safe_directory, ensure_safe_tree,
        hash_file, local_drive_path, media_title_stem, open_local_file, reparse, reveal_action,
        validate_existing_ancestors, with_completed_file, write_zone_identifier, RevealAction,
    },
    pending_downloads::{
        PendingConfirmation, PendingDownloads, PendingHttpEntry, ScrubbedRequest,
        PENDING_UNAVAILABLE,
    },
    rss::{self, RssSeenStore},
    settings::{
        category_id, host_pattern_matches, match_category_for_filename, normalize,
        normalize_blocklist_path, normalize_categories, normalize_rate, read_json,
        resolve_category_dir, torrent_rate, write_atomically, write_json_atomically, SETTINGS_FILE,
    },
    tools::{
        defender_executable, discover_ytdlp_for_probe, explorer_executable, toolchain_status,
        ToolchainCache, ToolchainStatus,
    },
    vault::{proxy_secret_key, site_login_secret_key, SecretVault},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Datelike, Timelike, Utc};
use futures_util::{stream, StreamExt};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use slipstream_engine::{
    DownloadEngine, EngineConfig, EngineEvent, EngineProxyConfig, ExtendedHttpOptions, QueueConfig,
};
use slipstream_media::{
    args::{canonical_media_headers, validate_playlist_items},
    probe::{MAX_PLAYLIST_PREVIEW_ENTRIES, MAX_PLAYLIST_TITLE_CHARS},
    probe_url_with_binary, DownloadOptions, ProbeOptions,
};
use slipstream_native_host::{
    pipe::{error_response, PipeRequestHandler, PipeServer},
    registry::DEFAULT_BASE_KEY,
    security::default_pipe_name,
};
use slipstream_protocol::{
    is_private_or_loopback_ip, sanitize_filename, validate_http_url, validate_magnet_uri,
    AddHttpDownloadRequest, AddMediaDownloadRequest, AddTorrentRequest, AppSettings,
    AutomationStatus, ConfirmPendingDownloadRequest, CreateTorrentRequest, CreateTorrentResult,
    DownloadCategory, DownloadQueueSpec, LogEntry, MediaProbeResult, MetalinkFileEntry,
    NativeBridgeRequest, NativeBridgeResponse, PendingDownloadPreview, PostQueueAction, ProxyMode,
    RefreshDownloadUrlRequest, RssFeedRule, SelectedPlaylistEntry, SettingsBackupBundle,
    SiteAuthScheme, SiteGrabRequest, SiteGrabResult, SiteLoginEntry, TelemetryBatch,
    TorrentFileEntry, TorrentNetworkSettings, TorrentPeerSummary, TorrentPreviewResult,
    TorrentSeedingSettings, TransferCompletionOptions, TransferKind, TransferSnapshot,
    TransferStatus, UpdateMode, MAX_NATIVE_MESSAGE_BYTES,
};
use slipstream_torrent::{
    Bytes, CancellationToken, TorrentAddOptions, TorrentManager, TorrentManagerConfig,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::{Display, Write},
    future::{pending, Future},
    io::Read,
    os::windows::{ffi::OsStrExt, fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command as StdCommand, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{
        broadcast, mpsc, watch, Mutex as AsyncMutex, MutexGuard, Notify, RwLock as AsyncRwLock,
        Semaphore,
    },
    task::JoinHandle,
    time::{timeout, Instant},
};
use url::Url;
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT, System::Threading::CREATE_NO_WINDOW,
};

type Result<T> = std::result::Result<T, String>;

const MAX_BACKUP_BYTES: usize = 2 * 1024 * 1024;
const DISMISSED_FILE: &str = "dismissed-http.json";
const TRANSFER_META_FILE: &str = "transfer_meta.json";
const LOG_CAPACITY: usize = 500;
const MAX_IMPORT_URLS: usize = 500;
const TELEMETRY_INTERVAL: Duration = Duration::from_millis(250);
const TORRENT_PREVIEW_TTL: Duration = Duration::from_secs(600);
const TORRENT_PREVIEW_CAPACITY: usize = 8;
const LOCAL_PICKER_STARTUP_CAPACITY: usize = 64;
const DEFENDER_SCAN_QUEUE_CAPACITY: usize = 128;
const DEFENDER_SCAN_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_CACHE_TTL: Duration = Duration::from_secs(300);
const PROBE_CACHE_CAPACITY: usize = 128;
const MAX_SELECTED_ENTRIES: usize = 512;
const MAX_BATCH_URLS: usize = 512;
const MAX_CONCURRENT_PROBES: usize = 4;
const VPN_TRAFFIC_BLOCKED: &str =
    "VPN kill switch blocked torrent traffic. Connect the configured VPN adapter to continue.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenPickerEvent {
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

pub trait UiBridge: Send + Sync + 'static {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<()>;
    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<()>;
    fn emit_settings_changed(&self) {}
    fn has_main_window(&self) -> bool;
    fn has_telemetry_windows(&self) -> bool {
        self.has_main_window()
    }
    fn supports_dialogs(&self) -> bool {
        false
    }
    fn open_confirm_dialog(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    fn close_confirm_dialog(&self, _id: &str) {}
    fn open_progress_dialog(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    fn close_progress_dialog(&self, _id: &str) {}
    fn open_complete_dialog(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    fn trigger_ready_check(&self) {}
    fn request_quit(&self);
}

#[derive(Default)]
pub struct DaemonOptions {
    pub pipe_name: Option<String>,
    pub allow_loopback: bool,
    pub allow_power_actions: bool,
    pub registry_base_key: Option<String>,
    pub resource_dir: Option<PathBuf>,
    pub torrent: TorrentManagerConfig,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchAddSummary {
    pub added: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl BatchAddSummary {
    fn skip_with_error(&mut self, line: usize, error: &str) {
        self.skipped += 1;
        if self.errors.len() < 10 {
            self.errors
                .push(format!("Line {line}: {}", log_text(error, 512)));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionAlert {
    pub transfer_id: String,
    pub title: String,
    pub filename: String,
    pub save_dir: String,
    pub kind: TransferKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct TransferMetadata {
    category_id: Option<String>,
    tags: Vec<String>,
}

struct TorrentSession {
    manager: Arc<TorrentManager>,
    base: PathBuf,
    config: TorrentManagerConfig,
    network: TorrentNetworkSettings,
    stopped: bool,
}

#[derive(Default)]
struct Picker {
    generation: u64,
    ready: bool,
    pending: VecDeque<OpenPickerEvent>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProbeKey {
    url: String,
    playlist: bool,
    headers: Vec<(String, String)>,
}

impl ProbeKey {
    fn new(url: &str, playlist: bool, headers: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            url: url.to_owned(),
            playlist,
            headers: canonical_media_headers(headers).map_err(message)?,
        })
    }

    fn from_probe_headers(url: &str, playlist: bool, headers: &HashMap<String, String>) -> Self {
        let mut headers: Vec<_> = filter_media_headers(headers).into_iter().collect();
        headers.sort_unstable();
        Self {
            url: url.to_owned(),
            playlist,
            headers,
        }
    }
}

pub(crate) fn filter_media_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
    let mut canonical: Vec<_> = headers
        .iter()
        .filter_map(|(name, value)| {
            canonical_media_headers(&HashMap::from([(name.clone(), value.clone())]))
                .ok()?
                .pop()
        })
        .collect();
    canonical.sort_unstable();
    canonical.dedup();
    canonical
        .chunk_by(|a, b| a.0 == b.0)
        .filter_map(|group| match group {
            [header] => Some(header.clone()),
            _ => None,
        })
        .collect()
}

type ProbeResult = Result<Arc<MediaProbeResult>>;

#[derive(Default)]
struct ProbeCache {
    entries: HashMap<ProbeKey, (Instant, Arc<MediaProbeResult>)>,
    in_flight: HashMap<ProbeKey, watch::Sender<Option<ProbeResult>>>,
}

impl ProbeCache {
    fn get(&self, key: &ProbeKey) -> Option<Arc<MediaProbeResult>> {
        let (created, probe) = self.entries.get(key)?;
        (created.elapsed() < PROBE_CACHE_TTL).then(|| probe.clone())
    }

    fn insert(&mut self, key: ProbeKey, probe: Arc<MediaProbeResult>) {
        let now = Instant::now();
        self.entries
            .retain(|_, (created, _)| now.duration_since(*created) < PROBE_CACHE_TTL);
        if self.entries.len() >= PROBE_CACHE_CAPACITY && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (created, _))| *created)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (now, probe));
    }
}

struct PlaylistCoordinator {
    title: String,
    directory: PathBuf,
    job_ids: Vec<String>,
    paused: bool,
}

struct PlaylistSelection {
    entries: Vec<SelectedPlaylistEntry>,
    probe: Option<Arc<MediaProbeResult>>,
}

type PendingMediaJob = (Arc<MediaJob>, Option<Arc<MediaProbeResult>>);

pub struct AppState {
    data_dir: PathBuf,
    vault: Mutex<SecretVault>,
    manage_startup: bool,
    registry_base_key: String,
    resource_dir: Option<PathBuf>,
    allow_loopback: bool,
    ui: Arc<dyn UiBridge>,
    engine: DownloadEngine,
    torrent: AsyncRwLock<TorrentSession>,
    torrent_config: TorrentManagerConfig,
    media: RwLock<HashMap<String, Arc<MediaJob>>>,
    toolchain_cache: Arc<ToolchainCache>,
    probe_cache: Arc<Mutex<ProbeCache>>,
    probe_permits: Arc<Semaphore>,
    media_queue: Mutex<VecDeque<PendingMediaJob>>,
    playlists: Mutex<HashMap<String, PlaylistCoordinator>>,
    media_finished: mpsc::UnboundedSender<MediaJobEvent>,
    pending_downloads: Mutex<PendingDownloads>,
    pending_changed: Notify,
    completion_options: Mutex<HashMap<String, TransferCompletionOptions>>,
    completion_alerts: broadcast::Sender<CompletionAlert>,
    settings: RwLock<AppSettings>,
    transfer_meta: RwLock<HashMap<String, TransferMetadata>>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    dismissed_http: RwLock<HashSet<String>>,
    verified: RwLock<HashMap<String, bool>>,
    operations: AsyncMutex<()>,
    picker: Mutex<Picker>,
    window_changed: Notify,
    torrent_changed: Notify,
    torrent_previews: Mutex<HashMap<String, (Instant, Bytes, String)>>,
    torrent_completions: Mutex<HashSet<String>>,
    torrent_announced: Mutex<HashSet<String>>,
    torrent_failures: Mutex<HashSet<String>>,
    torrent_completed_files: Mutex<HashMap<String, TorrentCompletedFiles>>,
    scans: mpsc::Sender<ScanRequest>,
    stop: watch::Sender<bool>,
    tasks: AsyncMutex<Option<Vec<JoinHandle<()>>>>,
    startup_update_checked: AtomicBool,
    quitting: AtomicBool,
    automation_status: RwLock<AutomationStatus>,
    automation_lock: AsyncMutex<()>,
    settings_revision: AtomicU64,
    last_effective_limits: Mutex<Option<(Option<u64>, Option<u64>)>>,
    schedule_torrent_paused_ids: Mutex<HashSet<String>>,
    automation_stops_torrents: AtomicBool,
    allow_power_actions: bool,
    active_extractions: Arc<AtomicUsize>,
    auto_paused_ids: Mutex<HashSet<String>>,
    last_decision_paused: AtomicBool,
    vpn_paused_torrents: Mutex<HashSet<String>>,
    vpn_kill_switch_active: AtomicBool,
    rss_matches_added: AtomicU64,
    last_rss_poll: Mutex<HashMap<String, DateTime<Utc>>>,
    rss_seen: AsyncMutex<RssSeenStore>,
    rss_client: rss::FeedHttpClient,
}

#[derive(Default)]
struct TorrentCompletedFiles {
    marked: HashSet<String>,
    scanned: HashSet<String>,
    extraction_started: HashSet<String>,
}

struct ActiveExtraction(Arc<AtomicUsize>);

impl Drop for ActiveExtraction {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn message(error: impl Display) -> String {
    error.to_string()
}

fn active(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Probing
            | TransferStatus::Active
            | TransferStatus::Remuxing
            | TransferStatus::Seeding
    )
}

fn seed_limit_reached(settings: &TorrentSeedingSettings, ratio: f64, seconds: u64) -> bool {
    settings.stop_seeding_on_limit
        && (settings
            .max_seed_ratio_hundredths
            .is_some_and(|cap| ratio * 100.0 >= f64::from(cap))
            || settings
                .max_seed_minutes
                .is_some_and(|cap| seconds >= u64::from(cap) * 60))
}

async fn vpn_adapter_connected(name: Option<String>) -> bool {
    let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
        return false;
    };
    tokio::task::spawn_blocking(move || crate::win32::is_network_adapter_up(&name))
        .await
        .unwrap_or(false)
}

fn log_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn push_log(logs: &Mutex<VecDeque<LogEntry>>, level: &str, category: &str, message: String) {
    let mut logs = logs.lock().unwrap();
    if logs.len() == LOG_CAPACITY {
        logs.pop_front();
    }
    logs.push_back(LogEntry {
        timestamp: chrono::Utc::now(),
        level: log_text(level, 32),
        category: log_text(category, 64),
        message: log_text(&message, 512),
    });
}

fn sanitize_transfer_tags(tags: Vec<String>) -> Vec<String> {
    let mut sanitized = Vec::new();
    for tag in tags {
        let clean: String = tag
            .chars()
            .filter(|ch| !ch.is_control() && !matches!(ch, '\u{2028}' | '\u{2029}'))
            .collect();
        let tag: String = clean.trim().chars().take(24).collect();
        let tag = tag.trim_end();
        if !tag.is_empty() && !sanitized.iter().any(|existing| existing == tag) {
            sanitized.push(tag.to_owned());
            if sanitized.len() == 8 {
                break;
            }
        }
    }
    sanitized
}

fn apply_transfer_metadata(
    snapshot: &mut TransferSnapshot,
    settings: &AppSettings,
    metadata: &HashMap<String, TransferMetadata>,
) {
    let entry = metadata.get(&snapshot.id);
    snapshot.category_id = Some(
        entry
            .and_then(|meta| meta.category_id.clone())
            .unwrap_or_else(|| {
                match_category_for_filename(settings, &snapshot.filename)
                    .id
                    .clone()
            }),
    );
    snapshot.tags = entry.map_or_else(Vec::new, |meta| meta.tags.clone());
}

fn unique_ids(ids: &[String]) -> impl Iterator<Item = &String> {
    let mut seen = HashSet::new();
    ids.iter().filter(move |id| seen.insert(id.as_str()))
}

async fn run_bulk(ids: &[String], mut operation: impl AsyncFnMut(&str) -> Result<()>) -> usize {
    let mut succeeded = 0;
    for id in unique_ids(ids) {
        succeeded += usize::from(operation(id).await.is_ok());
    }
    succeeded
}

fn batch_url_lines(raw_text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut in_block = false;
    let mut block_has_url = false;
    raw_text
        .trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter_map(move |(i, raw)| {
            let line = raw.trim();
            match line {
                "<" => {
                    in_block = true;
                    block_has_url = false;
                    return None;
                }
                ">" => {
                    in_block = false;
                    return None;
                }
                "" => return None,
                _ => {}
            }
            if line.starts_with('#')
                || line.split_once(':').is_some_and(|(prefix, _)| {
                    prefix.eq_ignore_ascii_case("cookie")
                        || prefix.eq_ignore_ascii_case("referer")
                        || prefix.eq_ignore_ascii_case("referrer")
                })
                || (in_block && block_has_url)
            {
                return None;
            }
            block_has_url = true;
            Some((i + 1, line))
        })
}

async fn export_text(out_path: &str, contents: String) -> Result<()> {
    if out_path
        .split(['/', '\\'])
        .any(|part| matches!(part, "." | ".."))
    {
        return Err("Export path must not contain traversal".into());
    }
    let target = local_drive_path(Path::new(out_path)).map_err(message)?;
    tokio::task::spawn_blocking(move || {
        let parent = target
            .parent()
            .ok_or("Export requires a parent directory")?;
        let name = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Export requires a filename")?;
        let parent = canonical_directory(parent).map_err(message)?;
        if let Some(existing) = confined_target(&parent, name).map_err(message)? {
            if !existing.is_file() || existing.parent() != Some(parent.as_path()) {
                return Err("Export destination must be a regular file".into());
            }
        }
        write_atomically(&parent.join(name), contents.as_bytes()).map_err(message)
    })
    .await
    .map_err(message)?
}

impl AppState {
    pub async fn start(
        data_dir: PathBuf,
        ui: Arc<dyn UiBridge>,
        mut options: DaemonOptions,
    ) -> Result<Arc<Self>> {
        let data_dir = std::path::absolute(data_dir).map_err(message)?;
        std::fs::create_dir_all(&data_dir).map_err(message)?;
        let manage_startup = cfg!(not(test))
            && !options.allow_loopback
            && options.pipe_name.is_none()
            && options.registry_base_key.is_none()
            && std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .and_then(|path| canonical_directory(&path.join("SlipStream")).ok())
                .is_some_and(|production| {
                    canonical_directory(&data_dir).is_ok_and(|dir| dir == production)
                });
        let mut settings: AppSettings =
            read_json(&data_dir.join(SETTINGS_FILE)).map_err(message)?;
        if let Err(error) =
            normalize_blocklist_path(settings.torrent_network.blocklist_path.as_deref())
        {
            tracing::warn!(%error, "ignoring invalid torrent blocklist path at startup");
            settings.torrent_network.blocklist_path = None;
        }
        normalize(&mut settings, &data_dir).map_err(message)?;
        let vault = SecretVault::load(&data_dir).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not load local secret vault");
            SecretVault::default()
        });
        sync_secret_flags(&mut settings, &vault);
        let proxy = http_proxy_config(&vault, &settings)?;
        let rss_client = rss::feed_http_client(options.allow_loopback)?;
        let rss_seen = RssSeenStore::load(&data_dir).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not load RSS history");
            RssSeenStore::default()
        });
        let dismissed_http: HashSet<String> =
            read_json(&data_dir.join(DISMISSED_FILE)).map_err(message)?;
        let meta_path = data_dir.join(TRANSFER_META_FILE);
        let mut transfer_meta: HashMap<String, TransferMetadata> =
            read_json(&meta_path).map_err(message)?;
        let pipe_name = options
            .pipe_name
            .map_or_else(default_pipe_name, Ok)
            .map_err(message)?;
        let server =
            PipeServer::bind(&pipe_name, &data_dir.join("session.token")).map_err(message)?;
        let local_picker_server = PipeServer::bind(
            &crate::local_picker_pipe_name(&pipe_name),
            &data_dir.join(crate::LOCAL_PICKER_TOKEN_FILE),
        )
        .map_err(message)?;
        let (stop, _) = watch::channel(false);
        let local_picker_target = Arc::new(Mutex::new(LocalPickerTarget::Starting(Vec::new())));
        let local_handler = LocalPickerHandler(local_picker_target.clone());
        let local_stopped = stop.subscribe();
        let local_picker_task = tokio::spawn(async move {
            if let Err(error) = local_picker_server
                .run(local_handler, stopped_signal(local_stopped))
                .await
            {
                tracing::error!(%error, "local picker pipe server stopped");
            }
        });
        options.torrent.allow_loopback |= options.allow_loopback;
        let torrent = match TorrentSession::new(&options.torrent, &settings).await {
            Ok(torrent) => torrent,
            Err(error) => {
                stop.send_replace(true);
                let _ = local_picker_task.await;
                return Err(error);
            }
        };
        let engine = match DownloadEngine::new_with_proxy(
            EngineConfig {
                db_path: data_dir.join("slipstream.sqlite"),
                default_download_dir: PathBuf::from(&settings.default_download_dir),
                default_connections: settings.default_connections,
                max_concurrent_downloads: usize::from(settings.max_concurrent_downloads),
                global_speed_limit_bps: settings.global_speed_limit_bps,
                allow_loopback: options.allow_loopback,
                ..EngineConfig::default()
            },
            proxy,
        )
        .await
        {
            Ok(engine) => engine,
            Err(error) => {
                torrent.manager.shutdown().await;
                return Err(message(error));
            }
        };
        let http_events = engine.subscribe_events();
        let http_snapshots = engine.snapshots();
        let scanned_http = http_snapshots
            .iter()
            .filter(|snapshot| snapshot.status == TransferStatus::Completed)
            .map(|snapshot| snapshot.id.clone())
            .collect();
        if !transfer_meta.is_empty() {
            let live_ids: HashSet<_> = http_snapshots
                .into_iter()
                .filter(|snapshot| !dismissed_http.contains(&snapshot.id))
                .chain(torrent.manager.list_snapshots())
                .map(|snapshot| snapshot.id)
                .collect();
            let count = transfer_meta.len();
            transfer_meta.retain(|id, meta| {
                meta.tags = sanitize_transfer_tags(std::mem::take(&mut meta.tags));
                live_ids.contains(id)
            });
            if count != transfer_meta.len() {
                if let Err(error) = write_json_atomically(&meta_path, &transfer_meta) {
                    tracing::warn!(%error, "could not prune transfer metadata");
                }
            }
        }
        let (scans, scan_requests) = mpsc::channel(DEFENDER_SCAN_QUEUE_CAPACITY);
        let (media_finished, media_events) = mpsc::unbounded_channel();
        let ready = ui.has_main_window();
        let registry_base_key = options
            .registry_base_key
            .unwrap_or_else(|| DEFAULT_BASE_KEY.to_owned());
        let state = Arc::new(Self {
            data_dir,
            vault: Mutex::new(vault),
            manage_startup,
            registry_base_key,
            resource_dir: options.resource_dir,
            allow_loopback: options.allow_loopback,
            ui,
            engine,
            torrent: AsyncRwLock::new(torrent),
            torrent_config: options.torrent,
            media: RwLock::default(),
            toolchain_cache: Arc::default(),
            probe_cache: Arc::default(),
            probe_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES)),
            media_queue: Mutex::default(),
            playlists: Mutex::default(),
            media_finished,
            pending_downloads: Mutex::default(),
            pending_changed: Notify::new(),
            completion_options: Mutex::default(),
            completion_alerts: broadcast::channel(128).0,
            settings: RwLock::new(settings),
            transfer_meta: RwLock::new(transfer_meta),
            logs: Arc::default(),
            dismissed_http: RwLock::new(dismissed_http),
            verified: RwLock::default(),
            operations: AsyncMutex::new(()),
            picker: Mutex::new(Picker {
                ready,
                ..Picker::default()
            }),
            window_changed: Notify::new(),
            torrent_changed: Notify::new(),
            torrent_previews: Mutex::default(),
            torrent_completions: Mutex::default(),
            torrent_announced: Mutex::default(),
            torrent_failures: Mutex::default(),
            torrent_completed_files: Mutex::default(),
            scans,
            stop,
            tasks: AsyncMutex::new(Some(Vec::new())),
            startup_update_checked: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            automation_status: RwLock::default(),
            automation_lock: AsyncMutex::new(()),
            settings_revision: AtomicU64::new(0),
            last_effective_limits: Mutex::default(),
            schedule_torrent_paused_ids: Mutex::default(),
            automation_stops_torrents: AtomicBool::new(false),
            allow_power_actions: cfg!(not(test)) && options.allow_power_actions,
            active_extractions: Arc::default(),
            auto_paused_ids: Mutex::default(),
            last_decision_paused: AtomicBool::new(false),
            vpn_paused_torrents: Mutex::default(),
            vpn_kill_switch_active: AtomicBool::new(false),
            rss_matches_added: AtomicU64::new(0),
            last_rss_poll: Mutex::default(),
            rss_seen: AsyncMutex::new(rss_seen),
            rss_client,
        });
        let weak = Arc::downgrade(&state);
        state
            .engine
            .set_site_authorization_provider(move |url| weak.upgrade()?.site_authorization(url));
        state.sync_queues_to_engine(&state.settings().queues)?;
        state.evaluate_and_apply_automation(0).await;
        let runtime = Arc::downgrade(&state);
        let pipe_logs = state.logs.clone();
        let stopped = state.stop.subscribe();
        let tasks = vec![
            tokio::spawn(async move {
                if let Err(error) = server
                    .run(NativeHandler(runtime), stopped_signal(stopped))
                    .await
                {
                    tracing::error!(%error, "named pipe server stopped");
                    push_log(
                        &pipe_logs,
                        "error",
                        "pipe",
                        format!("Named pipe server stopped: {error}"),
                    );
                }
            }),
            local_picker_task,
            tokio::spawn(emit_telemetry(state.clone(), state.stop.subscribe())),
            tokio::spawn(expire_pending_downloads(
                state.clone(),
                state.stop.subscribe(),
            )),
            tokio::spawn(watch_completions(
                state.clone(),
                state.stop.subscribe(),
                http_events,
                scanned_http,
            )),
            tokio::spawn(scan_completed(
                Arc::downgrade(&state),
                state.stop.subscribe(),
                scan_requests,
            )),
            tokio::spawn(watch_media_jobs(
                Arc::downgrade(&state),
                state.stop.subscribe(),
                media_events,
            )),
            tokio::spawn(run_automation_loop(
                Arc::downgrade(&state),
                state.stop.subscribe(),
            )),
            tokio::spawn(run_rss_loop(Arc::downgrade(&state), state.stop.subscribe())),
        ];
        *state.tasks.lock().await = Some(tasks);
        if let Some(source) = crate::take_initial_picker_source() {
            if let Err(error) = state.open_picker_local_source(source).await {
                tracing::warn!(%error, "could not open the initial torrent source");
            }
        }
        let has_pending = {
            let mut target = local_picker_target.lock().unwrap();
            let starting = std::mem::replace(
                &mut *target,
                LocalPickerTarget::Ready(Arc::downgrade(&state)),
            );
            let LocalPickerTarget::Starting(events) = starting else {
                unreachable!("picker target is published once");
            };
            let has_pending = !events.is_empty();
            state.picker.lock().unwrap().pending.extend(events);
            has_pending
        };
        if has_pending {
            if let Err(error) = state.show_queued_picker().await {
                tracing::warn!(%error, "could not open queued torrent sources");
            }
        }
        Ok(state)
    }

    async fn operation(&self) -> Result<MutexGuard<'_, ()>> {
        let guard = self.operations.lock().await;
        if *self.stop.borrow() {
            Err("SlipStream is shutting down".into())
        } else {
            Ok(guard)
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn registry_base_key(&self) -> &str {
        &self.registry_base_key
    }

    pub(crate) fn resource_dir(&self) -> Option<&Path> {
        self.resource_dir.as_deref()
    }

    pub fn settings(&self) -> AppSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn log_event(&self, level: &str, category: &str, message: impl Into<String>) {
        push_log(&self.logs, level, category, message.into());
    }

    pub fn get_logs(&self) -> Vec<LogEntry> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }

    pub fn clear_logs(&self) {
        self.logs.lock().unwrap().clear();
    }

    pub async fn export_logs(&self, out_path: &str) -> Result<usize> {
        let logs = self.get_logs();
        let mut text = String::new();
        for entry in &logs {
            writeln!(
                text,
                "{} [{}] [{}] {}",
                entry.timestamp.to_rfc3339(),
                entry.level,
                entry.category,
                entry.message
            )
            .unwrap();
        }
        export_text(out_path, text).await?;
        Ok(logs.len())
    }

    pub fn subscribe_completion_alerts(&self) -> broadcast::Receiver<CompletionAlert> {
        self.completion_alerts.subscribe()
    }

    pub async fn save_settings(&self, settings: AppSettings) -> Result<()> {
        let _operation = self.operation().await?;
        self.save_settings_locked(settings).await
    }

    pub(crate) async fn update_settings(&self, edit: impl FnOnce(&mut AppSettings)) -> Result<()> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        edit(&mut settings);
        self.save_settings_locked(settings).await
    }

    pub async fn set_proxy_password(&self, password: Option<String>) -> Result<()> {
        self.update_settings_with_proxy_password(Some(password), |_| {})
            .await
    }

    pub(crate) async fn update_settings_with_proxy_password(
        &self,
        password: Option<Option<String>>,
        edit: impl FnOnce(&mut AppSettings),
    ) -> Result<()> {
        let _operation = self.operation().await?;
        let previous = self.settings();
        let mut settings = previous.clone();
        edit(&mut settings);
        match password {
            Some(password) => {
                let previous_proxy = http_proxy_config(&self.vault.lock().unwrap(), &previous)?;
                self.save_settings_with_vault(settings, Some(previous_proxy), |vault| {
                    if let Some(password) = password {
                        vault.set_secret(&self.data_dir, proxy_secret_key(), &password)
                    } else {
                        vault.delete_secret(&self.data_dir, proxy_secret_key())
                    }
                })
                .await
            }
            None => self.save_settings_locked(settings).await,
        }
    }

    pub async fn upsert_site_login(
        &self,
        mut entry: SiteLoginEntry,
        password: Option<String>,
    ) -> Result<()> {
        let _operation = self.operation().await?;
        entry.id = category_id(&entry.id);
        if entry.id.is_empty() {
            entry.id = Uuid::new_v4().to_string();
        }
        if password
            .as_deref()
            .is_some_and(|password| site_auth_header(&entry, password).is_none())
        {
            return Err("Invalid site login password or token".into());
        }
        let key = site_login_secret_key(&entry.id);
        let mut settings = self.settings();
        if let Some(existing) = settings
            .site_logins
            .iter_mut()
            .find(|existing| existing.id == entry.id)
        {
            *existing = entry;
        } else {
            settings.site_logins.push(entry);
        }
        if let Some(password) = password {
            self.save_settings_with_vault(settings, None, |vault| {
                vault.set_secret(&self.data_dir, &key, &password)
            })
            .await
        } else {
            self.save_settings_locked(settings).await
        }
    }

    pub async fn delete_site_login(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        let id = category_id(id);
        let mut settings = self.settings();
        settings.site_logins.retain(|entry| entry.id != id);
        self.save_settings_with_vault(settings, None, |vault| {
            vault.delete_secret(&self.data_dir, &site_login_secret_key(&id))
        })
        .await
    }

    async fn save_settings_with_vault(
        &self,
        mut settings: AppSettings,
        previous_proxy: Option<EngineProxyConfig>,
        edit: impl FnOnce(&mut SecretVault) -> std::io::Result<()>,
    ) -> Result<()> {
        normalize(&mut settings, &self.data_dir).map_err(message)?;
        let previous = {
            let mut vault = self.vault.lock().unwrap();
            let previous = vault.clone();
            edit(&mut vault).map_err(message)?;
            sync_secret_flags(&mut settings, &vault);
            previous
        };
        let result = self
            .save_normalized_settings(settings, previous_proxy)
            .await;
        if result.is_err() {
            let mut vault = self.vault.lock().unwrap();
            if let Err(error) = vault.restore(&self.data_dir, previous) {
                tracing::warn!(%error, "could not restore local secret vault");
            }
        } else {
            self.ui.emit_settings_changed();
        }
        result
    }

    async fn save_settings_locked(&self, mut settings: AppSettings) -> Result<()> {
        normalize(&mut settings, &self.data_dir).map_err(message)?;
        sync_secret_flags(&mut settings, &self.vault.lock().unwrap());
        self.save_normalized_settings(settings, None).await
    }

    async fn save_normalized_settings(
        &self,
        settings: AppSettings,
        previous_proxy: Option<EngineProxyConfig>,
    ) -> Result<()> {
        let startup = (settings.launch_on_startup, settings.launch_minimized);
        self.apply_settings_with_proxy(settings, previous_proxy)
            .await?;
        if self.manage_startup {
            if let Err(error) = crate::win32::set_launch_on_startup(startup.0, startup.1) {
                tracing::warn!(%error, "could not update Windows startup settings");
                self.log_event(
                    "warn",
                    "settings",
                    format!("Windows startup update failed: {error}"),
                );
            }
        }
        self.invalidate_toolchain_cache().await;
        Ok(())
    }

    pub(crate) async fn invalidate_toolchain_cache(&self) {
        self.toolchain_cache.invalidate().await;
        *self.probe_cache.lock().unwrap() = ProbeCache::default();
    }

    pub async fn set_speed_limit(&self, limit_bps: Option<u64>) -> Result<()> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        settings.global_speed_limit_bps = normalize_rate(limit_bps);
        self.apply_settings(settings).await
    }

    async fn apply_settings(&self, settings: AppSettings) -> Result<()> {
        self.apply_settings_with_proxy(settings, None).await
    }

    async fn apply_settings_with_proxy(
        &self,
        settings: AppSettings,
        previous_proxy: Option<EngineProxyConfig>,
    ) -> Result<()> {
        let previous = self.settings();
        let proxy_change = if previous_proxy.is_some() || settings.proxy != previous.proxy {
            let vault = self.vault.lock().unwrap();
            let rollback =
                previous_proxy.map_or_else(|| http_proxy_config(&vault, &previous), Ok)?;
            Some((http_proxy_config(&vault, &settings)?, rollback))
        } else {
            None
        };
        let network = &settings.torrent_network;
        let blocklist_changed = network.blocklist_path != previous.torrent_network.blocklist_path;
        let mut torrent = self.torrent.write().await;
        let previous_config = torrent.config.clone();
        let previous_network = torrent.network.clone();
        let needs_replacement = torrent.needs_reconfigure(&settings);
        let replaced = needs_replacement && torrent.manager.list_snapshots().is_empty();
        if replaced {
            self.replace_torrent_session(&mut torrent, &settings, blocklist_changed)
                .await?;
        } else if blocklist_changed {
            blocklist_url(network.blocklist_path.as_deref())?;
        }
        let persist = (|| {
            if let Some((proxy, _)) = &proxy_change {
                self.engine
                    .set_proxy_config(proxy.clone())
                    .map_err(message)?;
            }
            self.persist_settings_and_queues(&settings)
        })();
        if let Err(error) = persist {
            if let Some((_, rollback)) = proxy_change {
                if let Err(error) = self.engine.set_proxy_config(rollback) {
                    tracing::warn!(%error, "could not restore HTTP proxy configuration");
                }
            }
            if replaced {
                torrent
                    .replace(previous_config, &previous_network)
                    .await
                    .map_err(|rollback| {
                        format!("{error}. Could not restore torrent session: {rollback}")
                    })?;
            }
            return Err(error);
        }
        self.engine
            .set_max_concurrent_downloads(usize::from(settings.max_concurrent_downloads))
            .await
            .map_err(message)?;
        if needs_replacement && !replaced {
            self.log_event(
                "info",
                "torrent",
                "Torrent session settings will apply when the torrent list is empty",
            );
        }
        drop(torrent);
        *self.settings.write().unwrap() = settings;
        let revision = self.settings_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.evaluate_and_apply_automation(revision).await;
        let _ = self.media_finished.send(MediaJobEvent::QueueChanged(None));
        self.log_event("info", "settings", "Settings updated");
        Ok(())
    }

    async fn replace_torrent_session(
        &self,
        torrent: &mut TorrentSession,
        settings: &AppSettings,
        strict_blocklist: bool,
    ) -> Result<()> {
        let mut config =
            TorrentSession::configured(&self.torrent_config, settings, strict_blocklist)?;
        if let Some((effective, _)) = *self.last_effective_limits.lock().unwrap() {
            config.global_download_limit_bps = torrent_rate(effective);
        }
        torrent.replace(config, &settings.torrent_network).await
    }

    async fn refresh_torrent_session(&self, settings: &AppSettings) -> Result<()> {
        let mut torrent = self.torrent.write().await;
        if torrent.needs_reconfigure(settings) && torrent.manager.list_snapshots().is_empty() {
            self.replace_torrent_session(&mut torrent, settings, false)
                .await?;
        }
        Ok(())
    }

    pub fn automation_status(&self) -> AutomationStatus {
        let mut status = self.automation_status.read().unwrap().clone();
        status.rss_matches_added = self.rss_matches_added.load(Ordering::SeqCst);
        status
    }

    pub async fn toggle_alt_speed(&self) -> Result<bool> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        settings.alt_speed_enabled = !settings.alt_speed_enabled;
        let enabled = settings.alt_speed_enabled;
        self.apply_settings(settings).await?;
        self.log_event(
            "info",
            "automation",
            if enabled {
                "Snail / Alt Speed enabled"
            } else {
                "Snail / Alt Speed disabled"
            },
        );
        Ok(enabled)
    }

    pub async fn set_transfers_queue(&self, ids: &[String], queue_id: &str) -> Result<usize> {
        let _operation = self.operation().await?;
        let settings = self.settings();
        if !settings.queues.iter().any(|q| q.id == queue_id) {
            return Err(format!("Unknown queue '{queue_id}'"));
        }
        if ids.is_empty() {
            return Err("Select at least one HTTP transfer to assign to a queue".into());
        }
        let torrent = self.torrent.read().await;
        for id in ids {
            if self.http_snapshot(id).is_none() {
                if self.media_job(id).is_some() || torrent.manager.snapshot(id).is_ok() {
                    return Err(
                        "Queue assignment supports HTTP transfers only. No transfers were moved."
                            .into(),
                    );
                }
                return Err(format!("Transfer '{id}' was not found"));
            }
        }
        self.engine
            .assign_queue_batch(ids, queue_id)
            .map_err(message)
    }

    pub async fn start_queue(&self, queue_id: &str) -> Result<()> {
        self.set_queue_paused(queue_id, false).await
    }

    pub async fn pause_queue(&self, queue_id: &str) -> Result<()> {
        self.set_queue_paused(queue_id, true).await
    }

    async fn set_queue_paused(&self, queue_id: &str, paused: bool) -> Result<()> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        let queue = settings
            .queues
            .iter_mut()
            .find(|q| q.id == queue_id)
            .ok_or_else(|| format!("Unknown queue '{queue_id}'"))?;
        queue.paused = paused;
        self.persist_settings_and_queues(&settings)?;
        *self.settings.write().unwrap() = settings;
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        self.log_event(
            "info",
            "queue",
            format!(
                "Queue {state} ({queue_id})",
                state = if paused { "paused" } else { "resumed" }
            ),
        );
        Ok(())
    }

    fn persist_settings_and_queues(&self, settings: &AppSettings) -> Result<()> {
        let path = self.data_dir.join(SETTINGS_FILE);
        let previous = self.settings();
        write_json_atomically(&path, settings).map_err(message)?;
        if let Err(error) = self.sync_queues_to_engine(&settings.queues) {
            return match write_json_atomically(&path, &previous) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!(
                    "{error}. Could not restore the previous settings file: {rollback}"
                )),
            };
        }
        Ok(())
    }

    pub async fn export_settings_backup(&self, path: &Path) -> Result<()> {
        let _operation = self.operation().await?;
        if path
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | ".."))
        {
            return Err("Backup path must not contain traversal".into());
        }
        let target = local_drive_path(path).map_err(message)?;
        let mut settings = self.settings();
        settings.ytdlp_path = None;
        settings.ffmpeg_path = None;
        settings.proxy.has_password = false;
        for entry in &mut settings.site_logins {
            entry.has_password = false;
        }
        settings.cookie_passthrough.enabled = false;
        settings.torrent_network.blocklist_path = None;
        settings.torrent_network.socks5_proxy_url = settings
            .torrent_network
            .socks5_proxy_url
            .as_deref()
            .and_then(|raw| {
                let mut url = Url::parse(raw).ok()?;
                url.set_password(None).ok()?;
                url.set_username("").ok()?;
                Some(url.into())
            });
        let bundle = SettingsBackupBundle {
            schema_version: 1,
            exported_at_epoch_ms: chrono::Utc::now().timestamp_millis(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            settings,
        };
        let payload = serde_json::to_vec(&bundle).map_err(message)?;
        if payload.len() > MAX_BACKUP_BYTES {
            return Err("Settings backup exceeds maximum size (2 MiB)".into());
        }
        tokio::task::spawn_blocking(move || {
            let parent = target
                .parent()
                .ok_or("Backup requires a parent directory")?;
            let name = target
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("Backup requires a filename")?;
            validate_existing_ancestors(parent).map_err(message)?;
            let parent = canonical_directory(parent).map_err(message)?;
            if let Some(existing) = confined_target(&parent, name).map_err(message)? {
                let meta = std::fs::symlink_metadata(&existing).map_err(message)?;
                if reparse(&meta) || !meta.is_file() || existing.parent() != Some(parent.as_path())
                {
                    return Err("Backup destination must be a regular local file".into());
                }
            }
            write_atomically(&parent.join(name), &payload).map_err(message)
        })
        .await
        .map_err(message)?
    }

    pub async fn import_settings_backup(&self, path: &Path) -> Result<AppSettings> {
        if path
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | ".."))
        {
            return Err("Backup path must not contain traversal".into());
        }
        let clean = local_drive_path(path).map_err(message)?;
        let mut settings = tokio::task::spawn_blocking(move || -> Result<AppSettings> {
            let parent = clean
                .parent()
                .ok_or("Backup path requires a parent directory")?;
            let canonical = canonical_directory(parent).map_err(message)?;
            let expected = canonical.join(clean.file_name().ok_or("Backup requires a filename")?);
            let file = open_local_file(&expected, false).map_err(message)?;
            crate::paths::verify_handle_path(&file, &clean).map_err(message)?;
            let mut bytes = Vec::new();
            file.take((MAX_BACKUP_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(message)?;
            if bytes.len() > MAX_BACKUP_BYTES {
                return Err("Settings backup exceeds maximum size (2 MiB)".into());
            }
            let bundle: SettingsBackupBundle = serde_json::from_slice(&bytes).map_err(message)?;
            if bundle.schema_version != 1 {
                return Err(format!(
                    "Unsupported backup schema version {}",
                    bundle.schema_version
                ));
            }
            Ok(bundle.settings)
        })
        .await
        .map_err(message)??;
        let _operation = self.operation().await?;
        let current = self.settings();
        settings.ytdlp_path = current.ytdlp_path;
        settings.ffmpeg_path = current.ffmpeg_path;
        settings.torrent_network.blocklist_path = current.torrent_network.blocklist_path;
        settings.cookie_passthrough.enabled = false;
        crate::settings::normalize_phase4_network_and_media(&mut settings).map_err(message)?;
        {
            let vault = self.vault.lock().unwrap();
            if vault.has_secret(proxy_secret_key()) {
                // A portable file cannot change the endpoint that receives a saved password.
                settings.proxy.url = current.proxy.url;
                settings.proxy.username = current.proxy.username;
                if settings.proxy.url.is_none() && settings.proxy.mode == ProxyMode::Manual {
                    settings.proxy.mode = current.proxy.mode;
                }
            }
            for entry in &mut settings.site_logins {
                if vault.has_secret(&site_login_secret_key(&entry.id))
                    && !current.site_logins.iter().any(|local| {
                        local.id == entry.id
                            && local.host_pattern == entry.host_pattern
                            && local.auth_scheme == entry.auth_scheme
                            && local.username == entry.username
                    })
                {
                    entry.id = Uuid::new_v4().to_string();
                }
            }
        }
        self.save_settings_locked(settings).await?;
        Ok(self.settings())
    }

    fn sync_queues_to_engine(&self, queues: &[DownloadQueueSpec]) -> Result<()> {
        let configs = queues
            .iter()
            .map(|spec| QueueConfig {
                id: spec.id.clone(),
                max_concurrent_downloads: spec.max_concurrent.max(1),
                post_action: spec.post_action,
                paused: spec.paused,
                speed_limit_bps: spec.speed_limit_bps,
            })
            .collect();
        self.engine.sync_queue_configs(configs).map_err(message)
    }

    // Callers serialize with transfer and settings operations so a manual
    // pause cannot race with automation taking or releasing pause ownership.
    async fn evaluate_and_apply_automation(&self, revision: u64) {
        let _automation = self.automation_lock.lock().await;
        if self.settings_revision.load(Ordering::SeqCst) != revision {
            return;
        }
        let settings = self.settings();
        let now = chrono::Local::now();
        let minute_of_day = (now.hour() * 60 + now.minute()) as u16;
        let (fullscreen_active, matched_proc) = if settings.bandwidth_guard.enabled {
            let watched = settings.bandwidth_guard.watched_processes.clone();
            tokio::task::spawn_blocking(move || {
                (
                    crate::win32::is_fullscreen_app_active(),
                    crate::win32::find_running_watched_process(&watched),
                )
            })
            .await
            .unwrap_or((false, None))
        } else {
            (false, None)
        };
        let active_queue_actions = settings
            .queues
            .iter()
            .filter(|q| q.post_action != PostQueueAction::None)
            .count();
        let mut decision = compute_effective_decision(
            &settings,
            minute_of_day,
            now.weekday(),
            fullscreen_active,
            matched_proc.as_deref(),
            active_queue_actions,
        );

        let network = &settings.torrent_network;
        let effective = decision.status.effective_speed_limit_bps;
        let upload = network.global_upload_limit_bps;
        let last_effective = *self.last_effective_limits.lock().unwrap();
        if last_effective != Some((effective, upload)) {
            let http = self.engine.set_global_speed_limit(effective).await;
            let mut session = self.torrent.write().await;
            let torrent = session
                .manager
                .set_global_rate_limits(torrent_rate(effective), upload);
            if torrent.is_ok() {
                session.config.global_download_limit_bps = torrent_rate(effective);
                session.config.global_upload_limit_bps = upload;
            }
            match (http, torrent) {
                (Ok(()), Ok(())) => {
                    *self.last_effective_limits.lock().unwrap() = Some((effective, upload));
                }
                (http, torrent) => {
                    tracing::debug!(?http, ?torrent, "could not update effective speed limit")
                }
            }
        }

        let connected = vpn_adapter_connected(network.vpn_adapter_name.clone()).await;
        let vpn_blocked = network.vpn_kill_switch_enabled && !connected;
        if self
            .vpn_kill_switch_active
            .swap(vpn_blocked, Ordering::SeqCst)
            != vpn_blocked
        {
            let event = if vpn_blocked {
                VPN_TRAFFIC_BLOCKED
            } else {
                "VPN kill switch released its torrent traffic block"
            };
            self.log_event("info", "torrent", event);
        }
        decision.status.vpn_kill_switch_active = vpn_blocked;
        decision.status.vpn_adapter_connected = connected;
        decision
            .status
            .vpn_adapter_name
            .clone_from(&network.vpn_adapter_name);
        decision.status.rss_feeds_enabled = settings
            .rss_feeds
            .iter()
            .filter(|rule| rule.enabled)
            .count() as u32;

        let pause_all = decision.should_pause_transfers;
        self.last_decision_paused.store(pause_all, Ordering::SeqCst);
        self.automation_stops_torrents
            .store(decision.stop_torrents, Ordering::SeqCst);
        self.engine.set_scheduling_paused(pause_all);
        let mut snapshots = self.engine.snapshots();
        snapshots.extend(
            self.media
                .read()
                .unwrap()
                .values()
                .map(|job| job.snapshot()),
        );
        let manager = self.torrent.read().await.manager.clone();
        snapshots.extend(manager.list_snapshots());
        let live: HashSet<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.id.as_str())
            .collect();
        for owners in [
            &self.auto_paused_ids,
            &self.schedule_torrent_paused_ids,
            &self.vpn_paused_torrents,
        ] {
            owners
                .lock()
                .unwrap()
                .retain(|id| live.contains(id.as_str()));
        }

        let mut paused = 0;
        let mut resumed = 0;
        for snapshot in snapshots {
            let id = &snapshot.id;
            let is_torrent = snapshot.kind == TransferKind::Torrent;
            if is_torrent
                && snapshot.status == TransferStatus::Seeding
                && manager.stats(id).is_ok_and(|report| {
                    seed_limit_reached(
                        &settings.torrent_seeding,
                        report.ratio,
                        report.seeding_seconds,
                    )
                })
                && manager.pause(id).await.is_ok()
            {
                self.clear_automation_pause(id);
                self.log_event("info", "torrent", format!("Paused at seeding limit ({id})"));
                continue;
            }
            let stop_torrent = decision.stop_torrents && is_torrent;
            let vpn_pause = vpn_blocked && is_torrent;
            if pause_all || stop_torrent || vpn_pause {
                // Media queue dispatch is gated separately. Preserve queued jobs
                // and playlist order while the running media job is stopped.
                if (active(snapshot.status)
                    || (snapshot.status == TransferStatus::Queued
                        && snapshot.kind != TransferKind::Media))
                    && self.pause_transfer_inner(id).await.is_ok()
                {
                    let owner = if vpn_pause {
                        &self.vpn_paused_torrents
                    } else if stop_torrent {
                        &self.schedule_torrent_paused_ids
                    } else {
                        &self.auto_paused_ids
                    };
                    owner.lock().unwrap().insert(id.clone());
                    paused += usize::from(!vpn_pause);
                }
            } else if self.automation_owns_pause(id)
                && (snapshot.status != TransferStatus::Paused
                    || self.resume_transfer_inner(id).await.is_ok())
            {
                self.clear_automation_pause(id);
                resumed += usize::from(snapshot.status == TransferStatus::Paused);
            }
        }
        if !pause_all {
            let _ = self.media_finished.send(MediaJobEvent::QueueChanged(None));
        }
        if paused > 0 {
            self.log_event(
                "info",
                "automation",
                format!("Paused {paused} transfers by schedule or bandwidth guard"),
            );
        }
        if resumed > 0 {
            self.log_event(
                "info",
                "automation",
                format!("Resumed {resumed} transfers after automation pause ended"),
            );
        }
        *self.automation_status.write().unwrap() = decision.status;
    }

    async fn check_torrent_network(&self) -> Result<()> {
        let network = self.settings.read().unwrap().torrent_network.clone();
        if network.vpn_kill_switch_enabled
            && (self.vpn_kill_switch_active.load(Ordering::SeqCst)
                || !vpn_adapter_connected(network.vpn_adapter_name).await)
        {
            return Err(VPN_TRAFFIC_BLOCKED.into());
        }
        Ok(())
    }

    fn automation_owns_pause(&self, id: &str) -> bool {
        self.auto_paused_ids.lock().unwrap().contains(id)
            || self
                .schedule_torrent_paused_ids
                .lock()
                .unwrap()
                .contains(id)
            || self.vpn_paused_torrents.lock().unwrap().contains(id)
    }

    fn clear_automation_pause(&self, id: &str) {
        self.auto_paused_ids.lock().unwrap().remove(id);
        self.schedule_torrent_paused_ids.lock().unwrap().remove(id);
        self.vpn_paused_torrents.lock().unwrap().remove(id);
    }

    fn record_torrent_automation_pause(&self, id: &str) -> bool {
        let owner = if self.automation_stops_torrents.load(Ordering::SeqCst) {
            &self.schedule_torrent_paused_ids
        } else if self.last_decision_paused.load(Ordering::SeqCst) {
            &self.auto_paused_ids
        } else {
            return false;
        };
        owner.lock().unwrap().insert(id.to_owned());
        true
    }

    fn maybe_auto_extract(&self, kind: TransferKind, candidate: PathBuf, id: &str) {
        let mut settings = self.settings.read().unwrap().auto_extract.clone();
        if !settings.enabled || !is_supported_archive(&candidate) {
            return;
        }
        if kind == TransferKind::Torrent {
            settings.delete_archive_after_extract = false;
            // A different archive member may also be a file the torrent seeds.
            settings.overwrite_existing = false;
        }
        let logs = self.logs.clone();
        let id = id.to_owned();
        let filename = candidate
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let http = (kind == TransferKind::Http).then(|| self.engine.clone());
        self.active_extractions.fetch_add(1, Ordering::SeqCst);
        let extraction = ActiveExtraction(self.active_extractions.clone());
        tokio::spawn(async move {
            let outputs = if let Some(engine) = http {
                let guard = engine.lock_outputs().await;
                if !engine.snapshot(&id).is_ok_and(|item| {
                    item.status == TransferStatus::Completed && !item.output_superseded
                }) {
                    return;
                }
                Some(guard)
            } else {
                None
            };
            tokio::task::spawn_blocking(move || {
                let _extraction = extraction;
                match extract_archive_after_open(&candidate, &settings, || drop(outputs)) {
                    Ok(summary) => push_log(
                        &logs,
                        "info",
                        "extract",
                        format!(
                            "Extracted {} files from '{}' ({}) into {}",
                            summary.files_extracted,
                            filename,
                            id,
                            summary.destination_dir.display()
                        ),
                    ),
                    Err(error) => push_log(
                        &logs,
                        "warn",
                        "extract",
                        format!("Auto-extract skipped for '{filename}': {error}"),
                    ),
                }
            })
            .await
            .ok();
        });
    }

    async fn on_queue_drained(&self, queue_id: &str) {
        let Ok(_operation) = self.operation().await else {
            return;
        };
        let Some(action) = self
            .settings
            .read()
            .unwrap()
            .queues
            .iter()
            .find(|q| q.id == queue_id)
            .map(|q| q.post_action)
        else {
            return;
        };
        if action == PostQueueAction::None {
            return;
        }
        self.log_event(
            "info",
            "queue",
            format!("Queue '{queue_id}' drained (action: {action:?})"),
        );
        match action {
            PostQueueAction::None => {}
            PostQueueAction::Notify => {
                if self.ui.supports_dialogs() {
                    let _ = self.ui.show_main_window().await;
                }
            }
            PostQueueAction::Sleep | PostQueueAction::Shutdown if self.allow_power_actions => {
                let busy = |status| active(status) || status == TransferStatus::Queued;
                if self.active_extractions.load(Ordering::SeqCst) > 0
                    || self
                        .engine
                        .snapshots()
                        .into_iter()
                        .any(|item| busy(item.status))
                    || self
                        .media
                        .read()
                        .unwrap()
                        .values()
                        .any(|job| busy(job.status()))
                    || self
                        .torrent
                        .read()
                        .await
                        .manager
                        .list_snapshots()
                        .into_iter()
                        .any(|item| busy(item.status))
                {
                    self.log_event(
                        "info",
                        "queue",
                        "Skipped power action because transfers or extractions remain active",
                    );
                    return;
                }
                if let Err(error) =
                    tokio::task::spawn_blocking(move || crate::win32::execute_power_action(action))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                {
                    self.log_event(
                        "warn",
                        "queue",
                        format!("Post-queue action failed: {error}"),
                    );
                }
            }
            PostQueueAction::Sleep | PostQueueAction::Shutdown => {}
        }
    }

    pub async fn snapshot(&self) -> TelemetryBatch {
        let mut batch = {
            let _outputs = self.engine.lock_outputs().await;
            self.engine.telemetry_batch()
        };
        let torrent = self.torrent.read().await;
        {
            let dismissed = self.dismissed_http.read().unwrap();
            batch
                .transfers
                .retain(|snapshot| !dismissed.contains(&snapshot.id));
        }
        batch.transfers.extend(
            self.media
                .read()
                .unwrap()
                .values()
                .map(|job| job.snapshot()),
        );
        batch.transfers.extend(torrent.manager.list_snapshots());
        let verified = self.verified.read().unwrap();
        batch
            .transfers
            .sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        batch.aggregate_down_bps = 0;
        batch.aggregate_up_bps = 0;
        batch.active_count = 0;
        batch.active_threads = 0;
        let playlists = self.playlists.lock().unwrap();
        let settings = self.settings.read().unwrap();
        let metadata = self.transfer_meta.read().unwrap();
        for transfer in &mut batch.transfers {
            apply_transfer_metadata(transfer, &settings, &metadata);
            transfer.playlist_paused = transfer
                .playlist_id
                .as_ref()
                .and_then(|id| playlists.get(id))
                .map(|playlist| playlist.paused);
            if let Some(result) = verified
                .get(&transfer.id)
                .filter(|_| !transfer.output_superseded)
            {
                transfer.sha256_verified = Some(*result);
            }
            batch.aggregate_down_bps = batch
                .aggregate_down_bps
                .saturating_add(transfer.download_speed_bps);
            batch.aggregate_up_bps = batch
                .aggregate_up_bps
                .saturating_add(transfer.upload_speed_bps);
            batch.active_count += u32::from(active(transfer.status));
            batch.active_threads = batch
                .active_threads
                .saturating_add(u32::from(transfer.active_connections));
        }
        batch
    }

    fn media_job(&self, id: &str) -> Option<Arc<MediaJob>> {
        self.media.read().unwrap().get(id).cloned()
    }

    fn http_snapshot(&self, id: &str) -> Option<TransferSnapshot> {
        if self.dismissed_http.read().unwrap().contains(id) {
            return None;
        }
        self.engine.snapshot(id).ok()
    }

    pub async fn add_http_download(&self, request: AddHttpDownloadRequest) -> Result<String> {
        let _operation = self.operation().await?;
        self.enqueue_http(
            ScrubbedRequest(Some(request)),
            &self.settings(),
            ExtendedHttpOptions::default(),
        )
        .await
    }

    fn site_authorization(&self, url: &Url) -> Option<HeaderValue> {
        if url.scheme() != "https" {
            return None;
        }
        let host = url.host_str()?;
        let settings = self.settings.read().unwrap();
        let vault = self.vault.lock().unwrap();
        settings
            .site_logins
            .iter()
            .filter(|entry| {
                entry.enabled
                    && entry.has_password
                    && host_pattern_matches(&entry.host_pattern, host)
            })
            .find_map(|entry| {
                let secret = read_vault_secret(&vault, &site_login_secret_key(&entry.id))?;
                site_auth_header(entry, &secret)
            })
    }

    pub async fn parse_metalink_file(&self, path: &Path) -> Result<Vec<MetalinkFileEntry>> {
        let path = path.to_path_buf();
        let allow_loopback = self.allow_loopback;
        tokio::task::spawn_blocking(move || {
            crate::metalink::parse_metalink_file(&path, allow_loopback)
        })
        .await
        .map_err(message)?
    }

    pub async fn enqueue_metalink_entries(
        &self,
        entries: Vec<MetalinkFileEntry>,
        save_dir: Option<String>,
        queue_id: Option<String>,
    ) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in entries {
            let mut seen = HashSet::new();
            let filtered_mirrors: Vec<url::Url> = entry
                .mirrors
                .iter()
                .filter_map(|raw| {
                    let mut url = rss::checked_url(raw, self.allow_loopback).ok()?;
                    url.set_fragment(None);
                    seen.insert(url.clone()).then_some(url)
                })
                .collect();
            let mirrors = stream::iter(filtered_mirrors)
                .map(|url| async move {
                    rss::check_http_host(&url, self.allow_loopback).await.ok()?;
                    Some(String::from(url))
                })
                .buffered(8)
                .filter_map(std::future::ready)
                .take(9)
                .collect::<Vec<_>>()
                .await;
            let mut mirrors = mirrors.into_iter();
            let Some(url) = mirrors.next() else { continue };
            let id = self
                .add_http_download(AddHttpDownloadRequest {
                    url,
                    filename: Some(entry.filename.clone()),
                    save_dir: save_dir.clone(),
                    queue_id: queue_id.clone(),
                    expected_sha256: entry.sha256,
                    expected_size_bytes: entry.size_bytes,
                    mirror_urls: mirrors.collect(),
                    ..Default::default()
                })
                .await
                .map_err(|error| {
                    format!(
                        "Queued {} Metalink files before {} failed: {error}",
                        ids.len(),
                        entry.filename
                    )
                })?;
            ids.push(id);
        }
        if ids.is_empty() {
            return Err("Metalink contains no files with usable HTTP mirrors".into());
        }
        Ok(ids)
    }

    pub async fn grab_site_urls(&self, request: SiteGrabRequest) -> Result<SiteGrabResult> {
        tokio::select! {
            _ = stopped_signal(self.stop.subscribe()) => Err("SlipStream is shutting down".into()),
            result = crate::grabber::grab_site(&self.rss_client, &request, self.allow_loopback) => result,
        }
    }

    pub async fn add_batch_urls(
        &self,
        raw_text: &str,
        save_dir: Option<String>,
        category_id: Option<String>,
        start_paused: bool,
    ) -> Result<BatchAddSummary> {
        self.validate_transfer_category(category_id.as_deref())?;
        let save_dir = save_dir.filter(|path| !path.trim().is_empty()).or_else(|| {
            category_id.as_deref().map(|id| {
                resolve_category_dir(&self.settings(), id)
                    .to_string_lossy()
                    .into_owned()
            })
        });
        let mut summary = BatchAddSummary::default();
        let mut seen = HashSet::new();
        let mut added = Vec::new();
        let mut reported_cap = false;
        for (line, raw) in batch_url_lines(raw_text) {
            let magnet = is_magnet_source(raw);
            let url = if magnet {
                validate_magnet_uri(raw)
                    .map_err(message)
                    .and_then(|_| Url::parse(raw).map_err(message))
            } else {
                validate_media_url(raw, self.allow_loopback)
            };
            let url = match url {
                Ok(mut url) => {
                    url.set_fragment(None);
                    String::from(url)
                }
                Err(error) => {
                    summary.skip_with_error(line, &error);
                    continue;
                }
            };
            if seen.contains(&url) {
                summary.skipped += 1;
                continue;
            }
            if seen.len() == MAX_IMPORT_URLS {
                if reported_cap {
                    summary.skipped += 1;
                } else {
                    summary.skip_with_error(line, "Batch import is limited to 500 URLs");
                    reported_cap = true;
                }
                continue;
            }
            seen.insert(url.clone());
            let result = if magnet {
                self.add_torrent_with_preview(
                    AddTorrentRequest {
                        source: url,
                        save_dir: save_dir.clone(),
                        selected_file_indices: None,
                        sequential: false,
                        start_paused,
                    },
                    None,
                )
                .await
            } else {
                self.add_http_download(AddHttpDownloadRequest {
                    url,
                    filename: None,
                    save_dir: save_dir.clone(),
                    max_connections: None,
                    queue_id: None,
                    headers: HashMap::new(),
                    referrer: None,
                    expected_sha256: None,
                    expected_size_bytes: None,
                    start_paused,
                    overwrite_existing: false,
                    ..AddHttpDownloadRequest::default()
                })
                .await
            };
            match result {
                Ok(id) => added.push(id),
                Err(error) => summary.skip_with_error(line, &error),
            }
        }
        summary.added = added.len();
        self.log_event(
            "info",
            "batch",
            format!(
                "Batch import: {} added, {} skipped",
                summary.added, summary.skipped
            ),
        );
        if category_id.is_some() && !added.is_empty() {
            self.set_transfers_category(&added, category_id)
                .await
                .map_err(|error| {
                    format!(
                        "Imported {} transfers but could not save their category: {error}",
                        summary.added
                    )
                })?;
        }
        Ok(summary)
    }

    pub async fn export_transfer_urls(&self, ids: &[String], out_path: &str) -> Result<usize> {
        let mut seen = HashSet::new();
        let mut text = String::new();
        for id in unique_ids(ids) {
            let Ok(snapshot) = self.transfer_snapshot(id).await else {
                continue;
            };
            let source = snapshot.source_url.trim();
            if !source.chars().any(char::is_control)
                && Url::parse(source)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "magnet"))
                && seen.insert(source.to_owned())
            {
                writeln!(text, "{source}").unwrap();
            }
        }
        export_text(out_path, text).await?;
        Ok(seen.len())
    }

    async fn enqueue_http(
        &self,
        mut scrubbed: ScrubbedRequest,
        settings: &AppSettings,
        options: ExtendedHttpOptions,
    ) -> Result<String> {
        let request = scrubbed.0.as_mut().expect("HTTP request to enqueue");
        let directory = http_save_directory(request.save_dir.take(), settings).await?;
        request.save_dir = Some(
            local_drive_path(&directory)
                .map_err(message)?
                .into_os_string()
                .into_string()
                .map_err(|_| "non-Unicode save directory".to_owned())?,
        );
        request
            .max_connections
            .get_or_insert(settings.default_connections);
        let start_paused = request.start_paused || options.start_paused;
        if !start_paused
            && (settings.auto_discover_checksums || request.auto_discover_checksum)
            && request.expected_sha256.is_none()
        {
            request.expected_sha256 = tokio::time::timeout(
                Duration::from_secs(3),
                crate::checksum_discovery::discover_sha256(
                    &self.rss_client,
                    &request.url,
                    self.allow_loopback,
                ),
            )
            .await
            .ok()
            .flatten();
        }
        let id = self
            .engine
            .add_http_download_with_options(
                scrubbed.0.take().expect("HTTP request to enqueue"),
                options,
            )
            .await
            .map_err(message)?;
        self.log_event("info", "http", format!("Download added ({id})"));
        if !start_paused {
            self.open_progress_if_enabled(&id, settings);
        }
        Ok(id)
    }

    fn stage_http_download(
        &self,
        request: AddHttpDownloadRequest,
        filename: String,
        settings: &AppSettings,
    ) -> Result<()> {
        let category = match_category_for_filename(settings, &filename);
        let directory = resolve_category_dir(settings, &category.id);
        let duplicate = self
            .engine
            .find_duplicate(&request.url, &directory, &filename)
            .filter(|found| {
                !self
                    .dismissed_http
                    .read()
                    .unwrap()
                    .contains(&found.transfer_id)
            });
        let id = Uuid::new_v4().to_string();
        let preview = PendingDownloadPreview {
            pending_id: id.clone(),
            source_domain: validate_http_url(&request.url)
                .map_err(message)?
                .host_str()
                .unwrap_or_default()
                .to_owned(),
            url: browser_source_url(&request.url),
            filename,
            suggested_category_id: category.id.clone(),
            suggested_save_dir: directory.to_string_lossy().into_owned(),
            expected_size_bytes: request.expected_size_bytes,
            duplicate,
        };
        self.expire_pending_entries();
        self.pending_downloads
            .lock()
            .unwrap()
            .insert(PendingHttpEntry { request, preview })?;
        self.pending_changed.notify_one();
        if let Err(error) = self.ui.open_confirm_dialog(&id) {
            self.discard_pending_download(&id);
            return Err(error);
        }
        Ok(())
    }

    pub fn get_pending_download(&self, id: &str) -> Result<PendingDownloadPreview> {
        self.pending_downloads
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.preview.clone())
            .ok_or_else(|| PENDING_UNAVAILABLE.to_owned())
    }

    fn take_pending_download(&self, id: &str) -> Result<PendingHttpEntry> {
        self.pending_downloads
            .lock()
            .unwrap()
            .take(id)
            .ok_or_else(|| PENDING_UNAVAILABLE.to_owned())
    }

    pub(crate) fn discard_pending_download(&self, id: &str) -> bool {
        let discarded = self.pending_downloads.lock().unwrap().discard(id);
        if discarded {
            self.pending_changed.notify_one();
        }
        discarded
    }

    pub fn cancel_pending_download(&self, id: &str) {
        if self.discard_pending_download(id) {
            self.ui.close_confirm_dialog(id);
        }
    }

    fn expire_pending_entries(&self) {
        let expired = self.pending_downloads.lock().unwrap().expire();
        for id in expired {
            self.ui.close_confirm_dialog(&id);
        }
    }

    pub async fn confirm_pending_download(
        &self,
        confirmation: ConfirmPendingDownloadRequest,
    ) -> Result<String> {
        let pending_id = &confirmation.pending_id;
        let _confirmation =
            PendingConfirmation::begin(&self.pending_downloads, &self.pending_changed, pending_id)?;
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        let filename = sanitize_filename(&confirmation.filename).map_err(message)?;
        let category = settings
            .categories
            .iter()
            .position(|category| category.id == confirmation.category_id)
            .ok_or_else(|| "Download category not found.".to_owned())?;
        let directory = if confirmation.save_dir.trim().is_empty() {
            resolve_category_dir(&settings, &confirmation.category_id)
        } else {
            PathBuf::from(confirmation.save_dir)
        };
        let directory = tokio::task::spawn_blocking(move || {
            let directory = local_drive_path(&ensure_safe_directory(&directory)?)?;
            directory
                .into_os_string()
                .into_string()
                .map_err(|_| crate::paths::invalid("non-Unicode save directory"))
        })
        .await
        .map_err(message)?
        .map_err(message)?;
        let mut scrubbed = ScrubbedRequest(Some(
            self.pending_downloads
                .lock()
                .unwrap()
                .get(pending_id)
                .ok_or(PENDING_UNAVAILABLE)?
                .request
                .clone(),
        ));
        let request = scrubbed.0.as_mut().expect("pending HTTP request");
        request.filename = Some(filename);
        request.save_dir = Some(directory.clone());
        request.start_paused = !confirmation.start_immediately;
        request.overwrite_existing = confirmation.overwrite_existing;
        let id = self
            .enqueue_http(
                scrubbed,
                &settings,
                ExtendedHttpOptions {
                    description: confirmation.description,
                    ..ExtendedHttpOptions::default()
                },
            )
            .await?;
        drop(self.take_pending_download(pending_id)?);
        self.ui.close_confirm_dialog(pending_id);
        if let Err(error) = self.update_transfer_meta(std::slice::from_ref(&id), |meta| {
            meta.category_id = Some(confirmation.category_id.clone());
        }) {
            self.log_event(
                "warn",
                "metadata",
                format!("Could not save the download category: {error}"),
            );
        }
        if confirmation.remember_category_path {
            settings.categories[category].save_dir = Some(directory);
            if let Err(error) = self.persist_categories(&settings) {
                tracing::warn!(%error, "could not remember the download category path");
            }
        }
        Ok(id)
    }

    pub async fn resolve_duplicate_existing(&self, pending_id: &str) -> Result<String> {
        let _confirmation =
            PendingConfirmation::begin(&self.pending_downloads, &self.pending_changed, pending_id)?;
        let _operation = self.operation().await?;
        let duplicate = self
            .get_pending_download(pending_id)?
            .duplicate
            .ok_or_else(|| "No duplicate transfer was found.".to_owned())?;
        let id = duplicate.transfer_id;
        let snapshot = self
            .http_snapshot(&id)
            .filter(|snapshot| {
                snapshot.status != TransferStatus::Canceled && !snapshot.output_superseded
            })
            .ok_or_else(|| "The duplicate transfer is unavailable.".to_owned())?;
        drop(self.take_pending_download(pending_id)?);
        self.pending_changed.notify_one();
        self.ui.close_confirm_dialog(pending_id);
        if snapshot.status == TransferStatus::Completed {
            self.ui.open_complete_dialog(&id)?;
        } else {
            if matches!(
                snapshot.status,
                TransferStatus::Paused | TransferStatus::Failed
            ) {
                self.engine.resume_transfer(&id).await.map_err(message)?;
                self.verified.write().unwrap().remove(&id);
                self.log_event("info", "http", format!("Transfer resumed ({id})"));
            }
            self.ui.open_progress_dialog(&id)?;
        }
        Ok(id)
    }

    fn persist_categories(&self, settings: &AppSettings) -> Result<()> {
        write_json_atomically(&self.data_dir.join(SETTINGS_FILE), settings).map_err(message)?;
        self.settings
            .write()
            .unwrap()
            .categories
            .clone_from(&settings.categories);
        self.ui.emit_settings_changed();
        self.log_event("info", "settings", "Download categories updated");
        Ok(())
    }

    pub async fn add_download_category(
        &self,
        name: String,
        extensions: Vec<String>,
        save_dir: Option<String>,
    ) -> Result<DownloadCategory> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        let name = sanitize_filename(&name).map_err(message)?;
        let mut id = category_id(&name);
        if id.is_empty() {
            id = "category".into();
        }
        let base = id.clone();
        let mut number = 2;
        while settings.categories.iter().any(|category| category.id == id) {
            id = format!("{base}-{number}");
            number += 1;
        }
        settings.categories.push(DownloadCategory {
            id,
            name,
            extensions,
            save_dir,
        });
        normalize_categories(&mut settings.categories).map_err(message)?;
        self.persist_categories(&settings)?;
        Ok(settings.categories.pop().expect("new download category"))
    }

    fn update_transfer_meta(
        &self,
        ids: &[String],
        update: impl Fn(&mut TransferMetadata),
    ) -> Result<()> {
        let mut current = self.transfer_meta.write().unwrap();
        let mut next = current.clone();
        for id in ids {
            let entry = next.entry(id.clone()).or_default();
            update(entry);
            if entry.category_id.is_none() && entry.tags.is_empty() {
                next.remove(id);
            }
        }
        if next != *current {
            write_json_atomically(&self.data_dir.join(TRANSFER_META_FILE), &next)
                .map_err(message)?;
            *current = next;
        }
        Ok(())
    }

    async fn existing_transfer_ids(&self, ids: &[String]) -> Vec<String> {
        let mut existing = Vec::new();
        for id in unique_ids(ids) {
            if self.transfer_snapshot(id).await.is_ok() {
                existing.push(id.clone());
            }
        }
        existing
    }

    fn validate_transfer_category(&self, category_id: Option<&str>) -> Result<()> {
        if category_id.is_some_and(|id| {
            !self
                .settings
                .read()
                .unwrap()
                .categories
                .iter()
                .any(|category| category.id == id)
        }) {
            return Err("Download category not found.".into());
        }
        Ok(())
    }

    pub async fn set_transfers_category(
        &self,
        ids: &[String],
        category_id: Option<String>,
    ) -> Result<usize> {
        let _operation = self.operation().await?;
        self.validate_transfer_category(category_id.as_deref())?;
        let existing = self.existing_transfer_ids(ids).await;
        self.update_transfer_meta(&existing, |meta| meta.category_id.clone_from(&category_id))?;
        Ok(existing.len())
    }

    pub async fn set_transfers_tags(&self, ids: &[String], tags: Vec<String>) -> Result<usize> {
        let _operation = self.operation().await?;
        let tags = sanitize_transfer_tags(tags);
        let existing = self.existing_transfer_ids(ids).await;
        self.update_transfer_meta(&existing, |meta| meta.tags.clone_from(&tags))?;
        Ok(existing.len())
    }

    pub async fn set_transfer_speed_limit(&self, id: &str, limit_bps: Option<u64>) -> Result<()> {
        let operation = self.operation().await?;
        if self.http_snapshot(id).is_some() {
            return self
                .engine
                .set_transfer_speed_limit(id, limit_bps)
                .await
                .map_err(message);
        }
        let manager = self.torrent.read().await.manager.clone();
        drop(operation);
        manager
            .set_torrent_download_limit(id, limit_bps)
            .await
            .map_err(message)
    }

    pub async fn get_transfer_snapshot(&self, id: &str) -> Result<TransferSnapshot> {
        let mut snapshot = self.transfer_snapshot(id).await?;
        apply_transfer_metadata(
            &mut snapshot,
            &self.settings.read().unwrap(),
            &self.transfer_meta.read().unwrap(),
        );
        Ok(snapshot)
    }

    async fn transfer_snapshot(&self, id: &str) -> Result<TransferSnapshot> {
        if let Some(snapshot) = self.http_snapshot(id) {
            return Ok(snapshot);
        }
        if let Some(job) = self.media_job(id) {
            return Ok(job.snapshot());
        }
        self.torrent
            .read()
            .await
            .manager
            .snapshot(id)
            .map_err(message)
    }

    fn completion_options(&self, id: &str) -> TransferCompletionOptions {
        self.completion_options
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| TransferCompletionOptions {
                show_complete_dialog: self.settings.read().unwrap().show_complete_dialog,
                auto_close_progress: false,
                open_folder_on_complete: false,
            })
    }

    pub async fn get_transfer_completion_options(
        &self,
        id: &str,
    ) -> Result<TransferCompletionOptions> {
        self.transfer_snapshot(id).await?;
        Ok(self.completion_options(id))
    }

    pub async fn set_transfer_completion_options(
        &self,
        id: &str,
        options: TransferCompletionOptions,
    ) -> Result<()> {
        let _operation = self.operation().await?;
        self.transfer_snapshot(id).await?;
        self.completion_options
            .lock()
            .unwrap()
            .insert(id.into(), options);
        Ok(())
    }

    pub async fn open_progress_dialog(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        self.transfer_snapshot(id).await?;
        self.ui.open_progress_dialog(id)
    }

    fn open_progress_if_enabled(&self, id: &str, settings: &AppSettings) {
        if settings.show_progress_dialog && self.ui.supports_dialogs() {
            if let Err(error) = self.ui.open_progress_dialog(id) {
                tracing::warn!(transfer_id = id, %error, "could not open progress dialog");
            }
        }
    }

    fn announce_completion(&self, snapshot: &TransferSnapshot) {
        self.log_event(
            "info",
            "transfer",
            format!("Download completed ({})", snapshot.id),
        );
        let _ = self.completion_alerts.send(CompletionAlert {
            transfer_id: snapshot.id.clone(),
            title: snapshot.filename.clone(),
            filename: snapshot.filename.clone(),
            save_dir: snapshot.save_dir.clone(),
            kind: snapshot.kind,
        });
    }

    async fn on_transfer_completed(&self, snapshot: &TransferSnapshot) {
        self.announce_completion(snapshot);
        if snapshot.kind == TransferKind::Http && !snapshot.output_superseded {
            if let Ok(path) = slipstream_protocol::safe_join_under(
                Path::new(&snapshot.save_dir),
                &snapshot.filename,
            ) {
                self.maybe_auto_extract(snapshot.kind, path, &snapshot.id);
            }
        }
        if !self.ui.supports_dialogs() {
            return;
        }
        let id = &snapshot.id;
        let options = self.completion_options(id);
        if options.auto_close_progress {
            self.ui.close_progress_dialog(id);
        }
        if options.open_folder_on_complete {
            if let Err(error) = self.reveal_in_explorer(id).await {
                tracing::warn!(transfer_id = id, %error, "could not reveal completed transfer");
            }
        }
        if options.show_complete_dialog {
            if let Err(error) = self.ui.open_complete_dialog(id) {
                tracing::warn!(transfer_id = id, %error, "could not open completion dialog");
            }
        }
    }

    pub async fn refresh_download_url(&self, request: RefreshDownloadUrlRequest) -> Result<()> {
        let _operation = self.operation().await?;
        if self.http_snapshot(&request.transfer_id).is_none() {
            return Err("HTTP transfer not found".into());
        }
        self.engine
            .refresh_download_address(request)
            .await
            .map_err(message)
    }

    pub async fn probe_media(&self, url: &str, playlist: Option<bool>) -> Result<MediaProbeResult> {
        self.probe_media_with_headers(url, playlist, &HashMap::new())
            .await
    }

    pub async fn probe_media_with_headers(
        &self,
        url: &str,
        playlist: Option<bool>,
        headers: &HashMap<String, String>,
    ) -> Result<MediaProbeResult> {
        let url = validate_media_url(url, self.allow_loopback).inspect_err(|_| {
            self.log_event("warn", "media", "Media probe rejected the URL");
        })?;
        let playlist = playlist.unwrap_or_else(|| is_pure_playlist_url(&url));
        let key = ProbeKey::from_probe_headers(url.as_str(), playlist, headers);
        Ok((*self.probe_media_shared(key).await?).clone())
    }

    fn ytdlp_cache_path(&self) -> PathBuf {
        self.data_dir.join("yt-dlp-cache")
    }

    async fn probe_media_shared(&self, key: ProbeKey) -> ProbeResult {
        let stopped = self.stop.subscribe();
        if *stopped.borrow() {
            return Err("SlipStream is shutting down".into());
        }
        // Keep the URL path and query, and the request headers, in the cache key.
        let mut result = {
            let mut cache = self.probe_cache.lock().unwrap();
            if let Some(probe) = cache.get(&key) {
                return Ok(probe);
            }
            if let Some(running) = cache.in_flight.get(&key) {
                running.subscribe()
            } else {
                let (sender, receiver) = watch::channel(None);
                cache.in_flight.insert(key.clone(), sender.clone());
                let toolchain_cache = self.toolchain_cache.clone();
                let probe_cache = self.probe_cache.clone();
                let probe_permits = self.probe_permits.clone();
                let logs = self.logs.clone();
                let settings = self.settings();
                let resource_dir = self.resource_dir.clone();
                let vault = self.vault.lock().unwrap().clone();
                let cache_dir = self.ytdlp_cache_path();
                let allow_loopback = self.allow_loopback;
                // Own the probe independently of the first caller's lifetime.
                tokio::spawn(async move {
                    push_log(&logs, "info", "media", "Media probe started".into());
                    let result = tokio::select! {
                        biased;
                        _ = stopped_signal(stopped) => Err("SlipStream is shutting down".into()),
                        result = async {
                            let _permit = probe_permits.acquire().await.map_err(message)?;
                            let url = validate_media_url(&key.url, allow_loopback)?;
                            let (proxy, cookies_from_browser) = media_network_options(&settings, &vault, &url)?;
                            run_media_probe(&toolchain_cache, settings, resource_dir, url, allow_loopback, ProbeOptions {
                                playlist: key.playlist,
                                cache_dir: Some(cache_dir),
                                headers: key.headers.iter().cloned().collect(),
                                proxy,
                                cookies_from_browser,
                            }).await
                        } => result.map(Arc::new),
                    };
                    if result.is_err() {
                        push_log(&logs, "warn", "media", "Media probe failed".into());
                    }
                    let mut cache = probe_cache.lock().unwrap();
                    if cache
                        .in_flight
                        .get(&key)
                        .is_some_and(|current| current.same_channel(&sender))
                    {
                        cache.in_flight.remove(&key);
                        if let Ok(probe) = &result {
                            cache.insert(key, probe.clone());
                        }
                    }
                    sender.send_replace(Some(result));
                });
                receiver
            }
        };
        let ready = result.wait_for(Option::is_some).await.map_err(message)?;
        ready.as_ref().expect("probe result is ready").clone()
    }

    pub async fn add_media_download(&self, mut request: AddMediaDownloadRequest) -> Result<String> {
        let url = validate_media_url(&request.url, self.allow_loopback)?;
        if let Some(items) = &mut request.playlist_items {
            *items = items
                .split(',')
                .map(|part| match part.split_once('-') {
                    Some((first, last)) => format!("{}-{}", first.trim(), last.trim()),
                    None => part.trim().to_owned(),
                })
                .collect::<Vec<_>>()
                .join(",");
            validate_playlist_items(items).map_err(message)?;
        }
        let explicit_entries = request.selected_entries.take();
        let playlist = explicit_entries.is_some()
            || request.download_playlist
            || request.playlist_items.is_some()
            || is_pure_playlist_url(&url);
        let key = ProbeKey::new(url.as_str(), playlist, &request.headers)?;
        let selection = if playlist {
            Some(
                self.select_playlist_entries(key, explicit_entries, &request)
                    .await?,
            )
        } else {
            None
        };
        request.url = url.into();
        let _operation = self.operation().await?;
        let settings = self.settings();
        let raw_dir = PathBuf::from(
            request
                .save_dir
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&settings.default_download_dir),
        );
        let directory = tokio::task::spawn_blocking(move || ensure_safe_directory(&raw_dir))
            .await
            .map_err(message)?
            .map_err(message)?;
        if let Some(selection) = selection {
            let id = self.enqueue_playlist(request, directory, selection).await?;
            self.open_progress_if_enabled(&id, &settings);
            return Ok(id);
        }
        let job = Arc::new(MediaJob::new(request, directory, None)?);
        let id = job.snapshot().id;
        self.media.write().unwrap().insert(id.clone(), job.clone());
        if let Err(error) = self.start_media_job(&job, None).await {
            self.media.write().unwrap().remove(&id);
            self.log_event("error", "media", format!("Download failed to start ({id})"));
            return Err(error);
        }
        self.log_event("info", "media", format!("Download added ({id})"));
        self.open_progress_if_enabled(&id, &settings);
        Ok(id)
    }

    async fn select_playlist_entries(
        &self,
        key: ProbeKey,
        explicit_entries: Option<Vec<SelectedPlaylistEntry>>,
        request: &AddMediaDownloadRequest,
    ) -> Result<PlaylistSelection> {
        if let Some(mut selected) = explicit_entries {
            if !(1..=MAX_SELECTED_ENTRIES).contains(&selected.len()) {
                return Err("select between 1 and 512 playlist entries".into());
            }
            let mut indices = HashSet::new();
            for entry in &mut selected {
                if entry.index == 0 {
                    return Err("playlist entry indices must be positive".into());
                }
                if !indices.insert(entry.index) {
                    return Err("playlist entry indices must be unique".into());
                }
                if let Some(url) = &mut entry.url {
                    *url = validate_media_url(url, self.allow_loopback)?.into();
                }
            }
            return Ok(PlaylistSelection {
                entries: selected,
                probe: self.probe_cache.lock().unwrap().get(&key),
            });
        }
        let probe = self.probe_media_shared(key).await?;
        let incomplete = probe.playlist_count.unwrap_or(0) as usize > probe.playlist_items.len();
        let entries = if let Some(items) = &request.playlist_items {
            let mut indices = std::collections::BTreeSet::new();
            for index in playlist_ranges(items)?.into_iter().flatten() {
                indices.insert(index);
                if indices.len() > MAX_SELECTED_ENTRIES {
                    // Keep the full selector on one playlist job when expanding it
                    // would exceed the coordinator's child limit.
                    return Ok(PlaylistSelection {
                        entries: Vec::new(),
                        probe: Some(probe),
                    });
                }
            }
            if indices.is_empty() {
                return Err("the playlist selection contains no video indices".into());
            }
            indices
                .into_iter()
                .map(|index| {
                    let item = probe.playlist_items.iter().find(|item| item.index == index);
                    SelectedPlaylistEntry {
                        index,
                        title: item
                            .map(|item| item.title.clone())
                            .filter(|title| !title.is_empty())
                            .unwrap_or_else(|| format!("Video {index}")),
                        url: item
                            .and_then(|item| item.url.as_deref())
                            .and_then(|url| validate_media_url(url, self.allow_loopback).ok())
                            .map(Into::into),
                    }
                })
                .collect()
        } else if incomplete
            || probe.playlist_count.unwrap_or(0) as usize > MAX_PLAYLIST_PREVIEW_ENTRIES
            || probe.playlist_items.iter().all(|item| item.url.is_none())
        {
            Vec::new()
        } else {
            probe
                .playlist_items
                .iter()
                .map(|item| SelectedPlaylistEntry {
                    index: item.index,
                    title: item.title.clone(),
                    url: item
                        .url
                        .as_deref()
                        .and_then(|url| validate_media_url(url, self.allow_loopback).ok())
                        .map(Into::into),
                })
                .collect()
        };
        Ok(PlaylistSelection {
            entries,
            probe: Some(probe),
        })
    }

    async fn enqueue_playlist(
        &self,
        mut request: AddMediaDownloadRequest,
        directory: PathBuf,
        PlaylistSelection {
            mut entries,
            probe: playlist_probe,
        }: PlaylistSelection,
    ) -> Result<String> {
        let title = request
            .playlist_folder_name
            .take()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                playlist_probe
                    .as_ref()
                    .map(|probe| probe.title.clone())
                    .filter(|title| !title.eq_ignore_ascii_case("download"))
            })
            .unwrap_or_else(|| "Playlist".into());
        let directory = tokio::task::spawn_blocking({
            let title = title.clone();
            move || ensure_playlist_subfolder(&directory, &title)
        })
        .await
        .map_err(message)?
        .map_err(message)?;
        let playlist_id = Uuid::new_v4().to_string();
        let mut playlist = PlaylistCoordinator {
            title: title
                .trim()
                .chars()
                .take(MAX_PLAYLIST_TITLE_CHARS)
                .collect(),
            directory,
            job_ids: Vec::new(),
            paused: false,
        };
        let meta = |index, total| {
            Some(PlaylistJobMeta {
                id: playlist_id.clone(),
                title: playlist.title.clone(),
                index,
                total,
            })
        };
        if request.audio_only_preset.is_some() {
            request.format_id = None;
        }
        if request.audio_only_preset.is_some() || request.format_id.is_some() {
            request.max_height = None;
        }
        let jobs = if entries.is_empty() {
            request.download_playlist = true;
            let total = playlist_probe
                .as_ref()
                .and_then(|probe| probe.playlist_count)
                .filter(|_| request.playlist_items.is_none());
            vec![Arc::new(MediaJob::new(
                request,
                playlist.directory.clone(),
                meta(None, total),
            )?)]
        } else {
            entries.sort_by_key(|entry| entry.index);
            let total = entries.len() as u32;
            let width = entries.last().unwrap().index.to_string().len().max(2);
            let parent_url = Url::parse(&request.url).map_err(message)?;
            entries
                .into_iter()
                .enumerate()
                .map(|(position, entry)| {
                    let title = media_title_stem(&entry.title, "Video");
                    let mut child = AddMediaDownloadRequest {
                        filename: Some(format!("{:0width$} - {title}", entry.index)),
                        ..request.clone()
                    };
                    child.download_playlist = entry.url.is_none();
                    child.playlist_items = child.download_playlist.then(|| entry.index.to_string());
                    if let Some(url) = entry.url {
                        let entry_url = Url::parse(&url).map_err(message)?;
                        if entry_url.origin() != parent_url.origin() {
                            child.headers.retain(|name, _| {
                                !name.eq_ignore_ascii_case("Referer")
                                    && !name.eq_ignore_ascii_case("Origin")
                            });
                        }
                        child.url = url;
                    }
                    MediaJob::new(
                        child,
                        playlist.directory.clone(),
                        meta(Some(position as u32 + 1), Some(total)),
                    )
                    .map(Arc::new)
                })
                .collect::<Result<Vec<_>>>()?
        };
        {
            let mut media = self.media.write().unwrap();
            for job in &jobs {
                let id = job.snapshot().id;
                playlist.job_ids.push(id.clone());
                media.insert(id, job.clone());
            }
        }
        let first_id = playlist.job_ids[0].clone();
        self.playlists
            .lock()
            .unwrap()
            .insert(playlist_id.clone(), playlist);
        let pre_probe = playlist_probe.filter(|_| jobs[0].request().download_playlist);
        if let Err(error) = self.start_media_job(&jobs[0], pre_probe).await {
            let playlist = self.playlists.lock().unwrap().remove(&playlist_id).unwrap();
            let mut media = self.media.write().unwrap();
            for id in playlist.job_ids {
                media.remove(&id);
            }
            self.log_event(
                "error",
                "media",
                format!("Playlist failed to start ({playlist_id})"),
            );
            return Err(error);
        }
        for job in &jobs {
            self.log_event(
                "info",
                "media",
                format!("Download added ({})", job.snapshot().id),
            );
        }
        Ok(first_id)
    }

    fn media_slot_available(&self) -> bool {
        let limit = usize::from(self.settings.read().unwrap().max_concurrent_downloads);
        self.media
            .read()
            .unwrap()
            .values()
            .filter(|job| active(job.status()))
            .count()
            < limit
    }

    fn remove_queued_media_job(&self, job: &Arc<MediaJob>) {
        self.media_queue
            .lock()
            .unwrap()
            .retain(|(queued, _)| !Arc::ptr_eq(queued, job));
    }

    async fn start_media_job(
        &self,
        job: &Arc<MediaJob>,
        pre_probe: Option<Arc<MediaProbeResult>>,
    ) -> Result<()> {
        {
            let mut queue = self.media_queue.lock().unwrap();
            if queue.iter().any(|(queued, _)| Arc::ptr_eq(queued, job)) {
                return Ok(());
            }
            if self.last_decision_paused.load(Ordering::SeqCst)
                || !queue.is_empty()
                || !self.media_slot_available()
            {
                job.queue_for_start()?;
                queue.push_back((job.clone(), pre_probe));
                let _ = self.media_finished.send(MediaJobEvent::QueueChanged(None));
                return Ok(());
            }
        }
        self.launch_media_job(job, pre_probe).await
    }

    async fn drain_media_queue(&self) {
        while !self.last_decision_paused.load(Ordering::SeqCst) && self.media_slot_available() {
            let next = self.media_queue.lock().unwrap().pop_front();
            let Some((job, pre_probe)) = next else { return };
            if let Err(error) = self.launch_media_job(&job, pre_probe).await {
                job.fail_queued_start(error);
                let snapshot = job.snapshot();
                self.log_event(
                    "error",
                    "media",
                    format!("Download failed to start ({})", snapshot.id),
                );
                let _ = self
                    .media_finished
                    .send(MediaJobEvent::QueueChanged(snapshot.playlist_id));
            }
        }
    }

    async fn launch_media_job(
        &self,
        job: &Arc<MediaJob>,
        pre_probe: Option<Arc<MediaProbeResult>>,
    ) -> Result<()> {
        let request = job.request();
        let playlist = request.download_playlist;
        let key = ProbeKey::new(&request.url, playlist, &request.headers)?;
        let pre_probe = pre_probe.or_else(|| self.probe_cache.lock().unwrap().get(&key));
        let url = validate_media_url(&request.url, self.allow_loopback)?;
        ensure_public_media_host(&url, self.allow_loopback).await?;
        let settings = self.settings();
        let (proxy, cookies_from_browser) =
            media_network_options(&settings, &self.vault.lock().unwrap(), &url)?;
        let power = settings.media_power.clone();
        job.start(MediaJobRunOptions {
            settings,
            resource_dir: self.resource_dir.clone(),
            toolchain_cache: self.toolchain_cache.clone(),
            download: DownloadOptions {
                playlist,
                pre_probe: pre_probe.map(|probe| (*probe).clone()),
                cache_dir: Some(self.ytdlp_cache_path()),
                proxy,
                cookies_from_browser,
                sponsorblock_remove: power.sponsorblock_remove,
                embed_chapters: power.embed_chapters,
                split_chapters: power.split_chapters,
                embed_thumbnail: power.embed_thumbnail,
                embed_metadata: power.embed_metadata,
                filename_template: power.filename_template,
                ..DownloadOptions::default()
            },
            scans: self.scans.clone(),
            media_finished: self.media_finished.clone(),
        })
        .await
    }

    pub async fn preview_torrent(
        &self,
        source: &str,
        save_dir: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<TorrentPreviewResult> {
        if source
            .trim()
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("magnet:"))
        {
            self.check_torrent_network().await?;
        }
        self.log_event("info", "torrent", "Torrent preview started");
        let settings = self.settings();
        let source = source.trim();
        let (manager, save_dir) = {
            let torrent = self.torrent.read().await;
            let save_dir = save_dir.unwrap_or(&settings.default_download_dir);
            (
                torrent.manager.clone(),
                torrent
                    .save_dir_override(Path::new(save_dir))
                    .map(Path::to_path_buf),
            )
        };
        let (mut preview, bytes) = manager
            .preview_source(source, save_dir.as_deref(), cancel)
            .await
            .inspect_err(|_| self.log_event("warn", "torrent", "Torrent preview failed"))
            .map_err(message)?;
        if let Some(bytes) = bytes {
            let token = Uuid::new_v4().to_string();
            let mut previews = self.torrent_previews.lock().unwrap();
            if *self.stop.borrow() {
                return Err("SlipStream is shutting down".into());
            }
            let now = Instant::now();
            previews
                .retain(|_, (created, _, _)| now.duration_since(*created) < TORRENT_PREVIEW_TTL);
            if previews.len() >= TORRENT_PREVIEW_CAPACITY {
                if let Some(oldest) = previews.iter().min_by_key(|(_, (created, _, _))| *created) {
                    let oldest = oldest.0.clone();
                    previews.remove(&oldest);
                }
            }
            previews.insert(token.clone(), (now, bytes, torrent_source_key(source)));
            preview.preview_token = Some(token);
        }
        self.log_event("info", "torrent", "Torrent preview ready");
        Ok(preview)
    }

    pub async fn add_torrent(&self, request: AddTorrentRequest) -> Result<String> {
        self.add_torrent_with_preview(request, None).await
    }

    pub async fn add_torrent_with_preview(
        &self,
        mut request: AddTorrentRequest,
        preview_token: Option<String>,
    ) -> Result<String> {
        let _operation = self.operation().await?;
        self.check_torrent_network().await?;
        request.source = request.source.trim().to_owned();
        let bytes = if let Some(token) = preview_token {
            let mut previews = self.torrent_previews.lock().unwrap();
            let now = Instant::now();
            previews
                .retain(|_, (created, _, _)| now.duration_since(*created) < TORRENT_PREVIEW_TTL);
            let source = torrent_source_key(&request.source);
            let Some((_, bytes, _)) = previews
                .get(&token)
                .filter(|(_, _, cached_source)| *cached_source == source)
            else {
                return Err(
                    "Torrent preview expired. Please click Inspect again before downloading."
                        .to_string(),
                );
            };
            Some(bytes.clone())
        } else {
            None
        };
        self.add_torrent_locked(request, bytes).await
    }

    async fn add_torrent_locked(
        &self,
        mut request: AddTorrentRequest,
        bytes: Option<Bytes>,
    ) -> Result<String> {
        let settings = self.settings();
        self.refresh_torrent_session(&settings).await?;
        self.check_torrent_network().await?;
        let torrent = self.torrent.read().await;
        let requested = request
            .save_dir
            .as_deref()
            .unwrap_or(&settings.default_download_dir);
        request.save_dir = torrent
            .save_dir_override(Path::new(requested))
            .map(|path| path.to_string_lossy().into_owned());
        let pause_by_automation = !request.start_paused
            && (self.automation_stops_torrents.load(Ordering::SeqCst)
                || self.last_decision_paused.load(Ordering::SeqCst));
        request.start_paused |= pause_by_automation;
        let start_paused = request.start_paused;
        let id = match bytes {
            Some(bytes) => {
                torrent
                    .manager
                    .add_pre_resolved_torrent(
                        request.source,
                        bytes,
                        TorrentAddOptions {
                            save_dir: request.save_dir.map(PathBuf::from),
                            selected_file_indices: request.selected_file_indices,
                            sequential: Some(request.sequential),
                            start_paused,
                            ..Default::default()
                        },
                    )
                    .await
            }
            None => torrent.manager.add_torrent(request).await,
        };
        let id = id.map_err(message)?;
        if pause_by_automation {
            self.record_torrent_automation_pause(&id);
        }
        self.torrent_completions.lock().unwrap().insert(id.clone());
        self.torrent_changed.notify_one();
        self.log_event("info", "torrent", format!("Download added ({id})"));
        if !start_paused {
            self.open_progress_if_enabled(&id, &settings);
        }
        Ok(id)
    }

    pub async fn get_torrent_peers(&self, id: &str) -> Result<TorrentPeerSummary> {
        self.torrent
            .read()
            .await
            .manager
            .peer_summary(id)
            .map_err(message)
    }

    pub async fn list_torrent_files(&self, id: &str) -> Result<Vec<TorrentFileEntry>> {
        self.torrent
            .read()
            .await
            .manager
            .list_files(id)
            .map_err(message)
    }

    pub async fn set_torrent_files(&self, id: &str, indices: Vec<usize>) -> Result<()> {
        let _operation = self.operation().await?;
        let torrent = self.torrent.write().await;
        torrent
            .manager
            .set_selected_files(id, &indices)
            .await
            .map_err(message)?;
        self.verified.write().unwrap().remove(id);
        self.torrent_completions.lock().unwrap().insert(id.into());
        self.torrent_changed.notify_one();
        Ok(())
    }

    pub async fn force_recheck_torrent(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        self.check_torrent_network().await?;
        self.torrent
            .read()
            .await
            .manager
            .force_recheck(id)
            .await
            .map_err(message)?;
        self.verified.write().unwrap().remove(id);
        self.torrent_completions
            .lock()
            .unwrap()
            .insert(id.to_owned());
        self.torrent_changed.notify_one();
        Ok(())
    }

    pub async fn move_torrent_storage(&self, id: &str, new_dir: &Path) -> Result<()> {
        let _operation = self.operation().await?;
        self.check_torrent_network().await?;
        self.torrent
            .read()
            .await
            .manager
            .move_storage(id, new_dir)
            .await
            .map_err(message)?;
        self.verified.write().unwrap().remove(id);
        self.torrent_completed_files.lock().unwrap().remove(id);
        self.torrent_changed.notify_one();
        Ok(())
    }

    pub async fn add_torrent_trackers(&self, id: &str, urls: Vec<String>) -> Result<()> {
        let _operation = self.operation().await?;
        self.check_torrent_network().await?;
        self.torrent
            .read()
            .await
            .manager
            .add_trackers(id, urls)
            .await
            .map_err(message)
    }

    pub async fn reannounce_torrent(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        self.check_torrent_network().await?;
        self.torrent
            .read()
            .await
            .manager
            .reannounce(id)
            .await
            .map_err(message)
    }

    pub async fn set_torrent_upload_limit(&self, id: &str, up_bps: Option<u64>) -> Result<()> {
        let _operation = self.operation().await?;
        self.torrent
            .read()
            .await
            .manager
            .set_torrent_upload_limit(id, up_bps)
            .await
            .map_err(message)
    }

    pub(crate) async fn torrent_upload_limit(&self, id: &str) -> Result<Option<u64>> {
        self.torrent
            .read()
            .await
            .manager
            .torrent_upload_limit(id)
            .map_err(message)
    }

    pub async fn create_torrent_file(
        &self,
        req: CreateTorrentRequest,
    ) -> Result<CreateTorrentResult> {
        drop(self.operation().await?);
        tokio::task::spawn_blocking(move || slipstream_torrent::create_v1_torrent(&req))
            .await
            .map_err(message)?
            .map_err(message)
    }

    pub async fn list_network_adapters(&self) -> Vec<String> {
        tokio::task::spawn_blocking(crate::win32::list_active_network_adapters)
            .await
            .unwrap_or_default()
    }

    pub async fn poll_rss_feeds_now(&self) -> Result<usize> {
        self.poll_rss_feeds(true).await
    }

    async fn poll_rss_feeds(&self, force: bool) -> Result<usize> {
        let mut seen = self.rss_seen.lock().await;
        let now = Utc::now();
        let rules: Vec<_> = {
            let settings = self.settings();
            let mut last_poll = self.last_rss_poll.lock().unwrap();
            last_poll.retain(|id, _| settings.rss_feeds.iter().any(|rule| &rule.id == id));
            settings
                .rss_feeds
                .into_iter()
                .filter(|rule| {
                    rule.enabled
                        && (force
                            || last_poll
                                .get(&rule.id)
                                .copied()
                                .or(rule.last_checked_at)
                                .is_none_or(|last| {
                                    now < last
                                        || now - last
                                            >= chrono::Duration::minutes(i64::from(
                                                rule.poll_interval_minutes,
                                            ))
                                }))
                })
                .collect()
        };
        if force || !rules.is_empty() {
            self.check_torrent_network().await?;
        }
        let mut added = 0;
        for rule in rules {
            self.check_torrent_network().await?;
            let poll_started = Utc::now();
            let stopped = stopped_signal(self.stop.subscribe());
            let fetched = tokio::select! {
                biased;
                () = stopped => return Ok(added),
                result = rss::fetch_feed_items(&self.rss_client, &rule.feed_url, self.allow_loopback) => result,
            };
            let mut error = None;
            match fetched {
                Ok(items) => {
                    for item in items {
                        if seen.contains(&rule.id, &item.item_key)
                            || !rss::wildcard_matches(&rule.include_pattern, &item.title)
                            || (!rule.exclude_pattern.is_empty()
                                && rss::wildcard_matches(&rule.exclude_pattern, &item.title))
                        {
                            continue;
                        }
                        match self.add_rss_item(&rule, &item.torrent_or_magnet_url).await {
                            Ok(true) => {
                                seen.mark_seen(&rule.id, &item.item_key);
                                self.rss_matches_added.fetch_add(1, Ordering::SeqCst);
                                added += 1;
                            }
                            Ok(false) => break,
                            Err(reason) if reason == VPN_TRAFFIC_BLOCKED => {
                                error = Some(reason);
                                break;
                            }
                            Err(reason) => {
                                error.get_or_insert(reason);
                            }
                        }
                    }
                }
                Err(reason) => error = Some(reason),
            }
            seen.save(&self.data_dir).map_err(message)?;
            self.finish_rss_poll(&rule, poll_started, error.as_deref())
                .await?;
            if error.as_deref() == Some(VPN_TRAFFIC_BLOCKED) {
                return Err(VPN_TRAFFIC_BLOCKED.into());
            }
        }
        Ok(added)
    }

    async fn add_rss_item(&self, rule: &RssFeedRule, source: &str) -> Result<bool> {
        self.check_torrent_network().await?;
        let staged = if source.starts_with("magnet:") {
            None
        } else {
            tokio::select! {
                biased;
                () = stopped_signal(self.stop.subscribe()) => return Ok(false),
                result = rss::stage_torrent(&self.rss_client, source, &self.data_dir, self.allow_loopback) => Some(result?),
            }
        };
        let _operation = self.operation().await?;
        let settings = self.settings();
        if !settings.rss_feeds.iter().any(|current| current == rule) {
            return Ok(false);
        }
        let save_dir = if let Some(raw) = &rule.save_dir {
            let base =
                local_drive_path(Path::new(&settings.default_download_dir)).map_err(message)?;
            let target = local_drive_path(Path::new(raw)).map_err(message)?;
            if !target.starts_with(base) {
                return Err(
                    "RSS save directory must be inside the default download directory".into(),
                );
            }
            validate_existing_ancestors(&target).map_err(message)?;
            Some(target.to_string_lossy().into_owned())
        } else {
            None
        };
        self.add_torrent_locked(
            AddTorrentRequest {
                source: source.to_owned(),
                save_dir,
                selected_file_indices: None,
                sequential: false,
                start_paused: !rule.auto_start,
            },
            staged,
        )
        .await?;
        Ok(true)
    }

    async fn finish_rss_poll(
        &self,
        rule: &RssFeedRule,
        checked_at: DateTime<Utc>,
        error: Option<&str>,
    ) -> Result<()> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        let Some(current) = settings
            .rss_feeds
            .iter_mut()
            .find(|current| *current == rule)
        else {
            return Ok(());
        };
        current.last_checked_at = Some(checked_at);
        current.last_error = error.map(|reason| log_text(reason, 512));
        write_json_atomically(&self.data_dir.join(SETTINGS_FILE), &settings).map_err(message)?;
        *self.settings.write().unwrap() = settings;
        self.last_rss_poll
            .lock()
            .unwrap()
            .insert(rule.id.clone(), checked_at);
        self.ui.emit_settings_changed();
        if let Some(error) = error {
            self.log_event(
                "warn",
                "rss",
                format!("Feed '{}' reported an error: {error}", rule.name),
            );
        }
        Ok(())
    }

    pub async fn pause_transfer(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        self.pause_transfer_inner(id).await?;
        self.clear_automation_pause(id);
        self.log_event("info", "transfer", format!("Transfer paused ({id})"));
        Ok(())
    }

    async fn pause_transfer_inner(&self, id: &str) -> Result<()> {
        if self.http_snapshot(id).is_some() {
            self.engine.pause_transfer(id).await.map_err(message)?;
        } else if let Some(job) = self.media_job(id) {
            self.remove_queued_media_job(&job);
            job.stop(TransferStatus::Paused).await?;
            let _ = self.media_finished.send(MediaJobEvent::QueueChanged(None));
        } else {
            self.torrent
                .read()
                .await
                .manager
                .pause(id)
                .await
                .map_err(message)?;
        }
        Ok(())
    }

    pub async fn resume_transfer(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        let torrent = self.torrent.read().await.manager.snapshot(id);
        if let Ok(snapshot) = torrent {
            self.check_torrent_network().await?;
            if snapshot.status == TransferStatus::Paused && self.record_torrent_automation_pause(id)
            {
                return Ok(());
            }
        }
        self.resume_transfer_inner(id).await?;
        self.clear_automation_pause(id);
        self.log_event("info", "transfer", format!("Transfer resumed ({id})"));
        Ok(())
    }

    async fn resume_transfer_inner(&self, id: &str) -> Result<()> {
        if self.http_snapshot(id).is_some() {
            self.engine.resume_transfer(id).await.map_err(message)?;
        } else if let Some(job) = self.media_job(id) {
            let snapshot = job.snapshot();
            if let Some(playlist_id) = &snapshot.playlist_id {
                let (paused, jobs) = self.playlist_jobs(playlist_id)?;
                let sibling_queued = self.media_queue.lock().unwrap().iter().any(|(queued, _)| {
                    !Arc::ptr_eq(&job, queued)
                        && jobs.iter().any(|member| Arc::ptr_eq(member, queued))
                });
                if paused || sibling_queued || jobs.iter().any(|job| active(job.status())) {
                    return Err("the playlist is paused or already has a running transfer".into());
                }
                if snapshot.status == TransferStatus::Queued {
                    self.advance_playlist(playlist_id, true).await?;
                    return Ok(());
                }
            }
            self.start_media_job(&job, None).await?;
        } else {
            self.torrent
                .read()
                .await
                .manager
                .resume(id)
                .await
                .map_err(message)?;
        }
        self.verified.write().unwrap().remove(id);
        Ok(())
    }

    pub async fn bulk_pause(&self, ids: &[String]) -> usize {
        run_bulk(ids, async |id| self.pause_transfer(id).await).await
    }

    pub async fn bulk_resume(&self, ids: &[String]) -> usize {
        run_bulk(ids, async |id| self.resume_transfer(id).await).await
    }

    pub async fn bulk_stop(&self, ids: &[String]) -> usize {
        run_bulk(ids, async |id| self.stop_transfer(id).await).await
    }

    async fn stop_transfer(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        let snapshot = self.transfer_snapshot(id).await?;
        match snapshot.kind {
            TransferKind::Http => self.engine.cancel_transfer(id).await.map_err(message)?,
            TransferKind::Media => {
                let job = self.media_job(id).ok_or("Media transfer not found")?;
                if snapshot.status == TransferStatus::Completed {
                    return Err("Transfer has already finished".into());
                }
                self.remove_queued_media_job(&job);
                job.stop(TransferStatus::Canceled).await?;
                let _ = self
                    .media_finished
                    .send(MediaJobEvent::QueueChanged(snapshot.playlist_id));
            }
            // Pausing detaches torrent activity while retaining its resumable entry.
            TransferKind::Torrent => self
                .torrent
                .read()
                .await
                .manager
                .pause(id)
                .await
                .map_err(message)?,
        }
        self.clear_automation_pause(id);
        self.log_event("info", "transfer", format!("Transfer stopped ({id})"));
        Ok(())
    }

    pub async fn bulk_remove(&self, ids: &[String], delete_files: bool) -> usize {
        run_bulk(ids, async |id| self.cancel_transfer(id, delete_files).await).await
    }

    pub async fn bulk_set_speed_limit(
        &self,
        ids: &[String],
        limit_bps: Option<u64>,
    ) -> Result<usize> {
        let mut updated = 0;
        for id in unique_ids(ids) {
            if self
                .transfer_snapshot(id)
                .await
                .is_ok_and(|snapshot| snapshot.kind != TransferKind::Media)
            {
                self.set_transfer_speed_limit(id, limit_bps).await?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    pub async fn change_all(&self, pause: bool) {
        let mut handled_playlists = HashSet::new();
        for snapshot in self.snapshot().await.transfers {
            let result = if let Some(playlist_id) = &snapshot.playlist_id {
                if !handled_playlists.insert(playlist_id.clone()) {
                    continue;
                }
                if pause {
                    self.pause_playlist(playlist_id).await
                } else {
                    self.resume_playlist(playlist_id).await
                }
            } else if pause
                && (active(snapshot.status) || snapshot.status == TransferStatus::Queued)
            {
                self.pause_transfer(&snapshot.id).await
            } else if !pause && snapshot.status == TransferStatus::Paused {
                self.resume_transfer(&snapshot.id).await
            } else {
                continue;
            };
            if let Err(error) = result {
                tracing::warn!(transfer_id = %snapshot.id, %error, "could not change transfer state");
            }
        }
    }

    // All scheduling and playlist control methods use the daemon operation lock.
    fn playlist_jobs(&self, id: &str) -> Result<(bool, Vec<Arc<MediaJob>>)> {
        let playlists = self.playlists.lock().unwrap();
        let playlist = playlists.get(id).ok_or("playlist was not found")?;
        let media = self.media.read().unwrap();
        let jobs = playlist
            .job_ids
            .iter()
            .filter_map(|id| media.get(id).cloned())
            .collect();
        Ok((playlist.paused, jobs))
    }

    async fn advance_playlist(&self, id: &str, resume_paused: bool) -> Result<()> {
        let (paused, jobs) = self.playlist_jobs(id)?;
        let has_pending_job = self
            .media_queue
            .lock()
            .unwrap()
            .iter()
            .any(|(queued, _)| jobs.iter().any(|job| Arc::ptr_eq(job, queued)));
        if paused || has_pending_job || jobs.iter().any(|job| active(job.status())) {
            return Ok(());
        }
        let paused_job = jobs
            .iter()
            .find(|job| job.snapshot().status == TransferStatus::Paused);
        if paused_job.is_some() && !resume_paused {
            return Ok(());
        }
        let next = paused_job.or_else(|| {
            jobs.iter()
                .find(|job| job.snapshot().status == TransferStatus::Queued)
        });
        if let Some(job) = next {
            self.start_media_job(job, None).await?;
        }
        Ok(())
    }

    pub async fn pause_playlist(&self, playlist_id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        let (_, jobs) = self.playlist_jobs(playlist_id)?;
        self.playlists
            .lock()
            .unwrap()
            .get_mut(playlist_id)
            .unwrap()
            .paused = true;
        for job in &jobs {
            self.clear_automation_pause(&job.snapshot().id);
            self.remove_queued_media_job(job);
        }
        if let Some(job) = jobs.iter().find(|job| active(job.status())) {
            if let Err(error) = job.stop(TransferStatus::Paused).await {
                if job.snapshot().status != TransferStatus::Completed {
                    return Err(error);
                }
            }
        }
        let _ = self.media_finished.send(MediaJobEvent::QueueChanged(None));
        self.log_event("info", "media", format!("Playlist paused ({playlist_id})"));
        Ok(())
    }

    pub async fn resume_playlist(&self, playlist_id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        self.playlists
            .lock()
            .unwrap()
            .get_mut(playlist_id)
            .ok_or("playlist was not found")?
            .paused = false;
        self.advance_playlist(playlist_id, true).await?;
        self.log_event("info", "media", format!("Playlist resumed ({playlist_id})"));
        Ok(())
    }

    pub fn reveal_playlist_folder(&self, playlist_id: &str) -> Result<()> {
        let directory = self
            .playlists
            .lock()
            .unwrap()
            .get(playlist_id)
            .ok_or("playlist was not found")?
            .directory
            .clone();
        open_reveal_action(reveal_action(&directory, None).map_err(message)?)
    }

    pub async fn cancel_transfer(&self, id: &str, remove_files: bool) -> Result<()> {
        let _operation = self.operation().await?;
        if let Some(mut snapshot) = self.http_snapshot(id) {
            if !matches!(
                snapshot.status,
                TransferStatus::Completed | TransferStatus::Canceled
            ) {
                let stopped = self.engine.cancel_transfer(id).await;
                snapshot = self.engine.snapshot(id).map_err(message)?;
                if !matches!(
                    snapshot.status,
                    TransferStatus::Completed | TransferStatus::Canceled
                ) {
                    stopped.map_err(message)?;
                }
            }
            self.engine
                .scrub_transfer_credentials(id)
                .map_err(message)?;
            if remove_files {
                let outputs = self.engine.lock_outputs().await;
                let snapshot = self.engine.snapshot(id).map_err(message)?;
                let mut paths = vec![format!(".{id}.slipstream-part")];
                // Only delete the final file while this transfer still owns it.
                if snapshot.status == TransferStatus::Completed && !snapshot.output_superseded {
                    paths.push(snapshot.filename);
                }
                tokio::task::spawn_blocking(move || {
                    let _outputs = outputs;
                    delete_files(Path::new(&snapshot.save_dir), &paths)
                })
                .await
                .map_err(message)?
                .map_err(message)?;
            }
            let mut dismissed = self.dismissed_http.read().unwrap().clone();
            dismissed.insert(id.into());
            write_json_atomically(&self.data_dir.join(DISMISSED_FILE), &dismissed)
                .map_err(message)?;
            *self.dismissed_http.write().unwrap() = dismissed;
        } else if let Some(job) = self.media_job(id) {
            self.remove_queued_media_job(&job);
            job.stop(TransferStatus::Canceled).await?;
            let snapshot = job.snapshot();
            let _ = self
                .media_finished
                .send(MediaJobEvent::QueueChanged(snapshot.playlist_id.clone()));
            if remove_files {
                remove_outputs(snapshot.save_dir.into(), job.outputs()).await?;
            }
            self.media.write().unwrap().remove(id);
            if let Some(playlist_id) = snapshot.playlist_id {
                let mut playlists = self.playlists.lock().unwrap();
                if let Some(playlist) = playlists.get_mut(&playlist_id) {
                    playlist.job_ids.retain(|job_id| job_id != id);
                    if playlist.job_ids.is_empty() {
                        playlists.remove(&playlist_id);
                    }
                }
            }
        } else {
            self.torrent
                .read()
                .await
                .manager
                .remove(id, remove_files)
                .await
                .map_err(message)?;
            self.torrent_completions.lock().unwrap().remove(id);
            self.torrent_announced.lock().unwrap().remove(id);
            self.torrent_failures.lock().unwrap().remove(id);
            self.torrent_completed_files.lock().unwrap().remove(id);
        }
        self.clear_automation_pause(id);
        self.verified.write().unwrap().remove(id);
        self.completion_options.lock().unwrap().remove(id);
        let mut metadata = self.transfer_meta.write().unwrap();
        if metadata.remove(id).is_some() {
            if let Err(error) =
                write_json_atomically(&self.data_dir.join(TRANSFER_META_FILE), &*metadata)
            {
                self.log_event(
                    "warn",
                    "metadata",
                    format!("Could not prune transfer metadata: {error}"),
                );
            }
        }
        self.log_event(
            "info",
            "transfer",
            format!("Transfer canceled and removed ({id})"),
        );
        Ok(())
    }

    async fn transfer_paths(&self, id: &str) -> Result<(PathBuf, Vec<String>)> {
        if let Some(snapshot) = self.http_snapshot(id) {
            let mut paths = Vec::new();
            if snapshot.status == TransferStatus::Completed && !snapshot.output_superseded {
                paths.push(snapshot.filename);
            }
            return Ok((snapshot.save_dir.into(), paths));
        }
        if let Some(job) = self.media_job(id) {
            return Ok((job.snapshot().save_dir.into(), job.outputs()));
        }
        let torrent = self.torrent.read().await;
        let snapshot = torrent.manager.snapshot(id).map_err(message)?;
        let paths = torrent
            .manager
            .list_files(id)
            .unwrap_or_default()
            .into_iter()
            .filter(|file| file.selected)
            .map(|file| file.relative_path)
            .collect();
        Ok((snapshot.save_dir.into(), paths))
    }

    pub async fn verify_transfer_checksum(&self, id: &str, expected_sha256: &str) -> Result<bool> {
        let expected = decode_sha256(expected_sha256).map_err(message)?;
        let target = {
            let _operation = self.operation().await?;
            self.transfer_paths(id).await?
        };
        let [relative] = target.1.as_slice() else {
            return Err("checksum verification requires a single transfer file".into());
        };
        let root = target.0.clone();
        let relative = relative.clone();
        let digest = tokio::task::spawn_blocking(move || hash_file(&root, &relative))
            .await
            .map_err(message)?
            .map_err(message)?;
        let _operation = self.operation().await?;
        let _outputs = self.engine.lock_outputs().await;
        if self.transfer_paths(id).await? != target {
            return Err("transfer files changed during checksum verification".into());
        }
        let matches = digest == expected;
        self.verified.write().unwrap().insert(id.into(), matches);
        Ok(matches)
    }

    pub async fn reveal_in_explorer(&self, id: &str) -> Result<()> {
        let _outputs = self.engine.lock_outputs().await;
        let (root, files) = self.transfer_paths(id).await?;
        open_reveal_action(
            reveal_action(&root, files.first().map(String::as_str)).map_err(message)?,
        )
    }

    pub async fn open_completed_file(&self, id: &str) -> Result<()> {
        self.open_completed_with_handler(id, crate::win32::open_file)
            .await
    }

    pub async fn open_completed_file_with(&self, id: &str) -> Result<()> {
        self.open_completed_with_handler(id, crate::win32::open_file_with)
            .await
    }

    async fn open_completed_with_handler<F>(&self, id: &str, handler: F) -> Result<()>
    where
        F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
    {
        let _operation = self.operation().await?;
        let outputs = self.engine.lock_outputs().await;
        let snapshot = self.transfer_snapshot(id).await?;
        if snapshot.status != TransferStatus::Completed {
            return Err("Only completed files can be opened.".into());
        }
        let (root, paths) = self.transfer_paths(id).await?;
        let [relative] = paths.as_slice() else {
            return Err("Opening a file requires a single transfer output.".into());
        };
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            let _outputs = outputs;
            with_completed_file(snapshot.status, &root, &relative, handler)
        })
        .await
        .map_err(message)?
        .map_err(message)
    }

    pub async fn get_toolchain_status(&self) -> ToolchainStatus {
        toolchain_status(
            &self.settings(),
            &self.registry_base_key,
            self.resource_dir(),
        )
        .await
    }

    pub async fn show_main_window(&self) -> Result<()> {
        if *self.stop.borrow() {
            return Err("SlipStream is shutting down".into());
        }
        self.ui.show_main_window().await?;
        self.window_shown();
        Ok(())
    }

    pub(crate) fn window_shown(&self) {
        self.window_changed.notify_one();
    }

    pub async fn open_picker(&self, url: Option<String>) -> Result<()> {
        if let Some(source) = &url {
            if is_torrent_path(source.trim()) && validate_http_url(source).is_err() {
                return self.open_picker_local_source(source.clone()).await;
            }
        }
        self.open_picker_with_headers(url, HashMap::new()).await
    }

    pub async fn open_picker_local_source(&self, source: String) -> Result<()> {
        self.queue_picker(local_picker_event(&source)?).await
    }

    pub async fn open_picker_with_headers(
        &self,
        url: Option<String>,
        headers: HashMap<String, String>,
    ) -> Result<()> {
        if let Some(url) = url.as_deref() {
            if is_magnet_source(url) {
                validate_magnet_uri(url).map_err(message)?;
            } else {
                validate_http_url(url).map_err(message)?;
            }
        }
        self.queue_picker(OpenPickerEvent {
            headers: if url.is_some() {
                filter_media_headers(&headers)
            } else {
                HashMap::new()
            },
            url,
        })
        .await
    }

    async fn queue_picker(&self, event: OpenPickerEvent) -> Result<()> {
        self.show_main_window().await?;
        self.picker.lock().unwrap().pending.push_back(event);
        self.flush_or_check_picker()
    }

    async fn show_queued_picker(&self) -> Result<()> {
        self.show_main_window().await?;
        self.flush_or_check_picker()
    }

    fn flush_or_check_picker(&self) -> Result<()> {
        if !self.picker.lock().unwrap().ready {
            self.ui.trigger_ready_check();
        }
        self.flush_picker()
    }

    pub fn window_loading(&self) -> u64 {
        let mut picker = self.picker.lock().unwrap();
        picker.generation += 1;
        picker.ready = false;
        let generation = picker.generation;
        drop(picker);
        self.window_changed.notify_one();
        generation
    }

    pub fn window_generation(&self) -> u64 {
        self.picker.lock().unwrap().generation
    }

    pub fn window_ready(&self, generation: u64) -> Result<()> {
        let mut picker = self.picker.lock().unwrap();
        if picker.generation != generation {
            return Ok(());
        }
        picker.ready = true;
        self.window_changed.notify_one();
        Self::flush_locked(&*self.ui, &mut picker)
    }

    fn flush_picker(&self) -> Result<()> {
        let mut picker = self.picker.lock().unwrap();
        Self::flush_locked(&*self.ui, &mut picker)
    }

    fn flush_locked(ui: &dyn UiBridge, picker: &mut Picker) -> Result<()> {
        if picker.ready && ui.has_main_window() {
            while let Some(event) = picker.pending.front() {
                ui.emit_open_picker(event)?;
                picker.pending.pop_front();
            }
        }
        Ok(())
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::Acquire)
    }

    pub(crate) fn claim_startup_update_check(&self) -> bool {
        self.settings.read().unwrap().update_mode == UpdateMode::Auto
            && !self.startup_update_checked.swap(true, Ordering::Relaxed)
    }

    pub async fn quit(&self) {
        if let Err(error) = self.shutdown().await {
            tracing::error!(%error, "daemon shutdown failed");
        }
        if !self.quitting.swap(true, Ordering::AcqRel) {
            self.ui.request_quit();
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut runtime = self.tasks.lock().await;
        let Some(tasks) = runtime.as_mut() else {
            return Ok(());
        };
        self.stop.send_replace(true);
        let mut error = None;
        while let Some(task) = tasks.last_mut() {
            if let Err(failure) = task.await {
                error = Some(message(failure));
            }
            tasks.pop();
        }
        let _operation = self.operations.lock().await;
        let pending = self.pending_downloads.lock().unwrap().clear();
        for id in pending {
            self.ui.close_confirm_dialog(&id);
        }
        self.completion_options.lock().unwrap().clear();
        self.torrent_previews.lock().unwrap().clear();
        let jobs: Vec<_> = self.media.read().unwrap().values().cloned().collect();
        for job in jobs {
            if let Err(failure) = job.shutdown().await {
                error = Some(failure);
            }
        }
        self.torrent.read().await.manager.shutdown().await;
        if let Err(failure) = self.engine.shutdown().await {
            error = Some(message(failure));
        }
        *runtime = None;
        error.map_or(Ok(()), Err)
    }

    async fn grab_batch(&self, urls: Vec<String>, referrer: Option<String>) -> Result<()> {
        if !(1..=MAX_BATCH_URLS).contains(&urls.len()) {
            return Err("provide between 1 and 512 batch URLs".into());
        }
        let _operation = self.operation().await?;
        let settings = self.settings();
        let save_dir = http_save_directory(None, &settings).await?;
        let count = urls.len();
        self.engine
            .add_http_batch(urls, referrer, save_dir, settings.default_connections)
            .map_err(message)?;
        self.log_event(
            "info",
            "http",
            format!("Browser batch added {count} downloads"),
        );
        Ok(())
    }

    async fn native_request(&self, request: NativeBridgeRequest) -> Result<NativeBridgeResponse> {
        use NativeBridgeRequest::*;
        match request {
            Authenticate { .. } => {
                return Ok(error_response(
                    "already_authenticated",
                    "Connection is already authenticated",
                ))
            }
            Ping => {}
            GetStatus => {
                let settings = self.settings();
                let mut batch = self.snapshot().await;
                for transfer in &mut batch.transfers {
                    transfer.source_url = browser_source_url(&transfer.source_url);
                    transfer.save_dir = String::new();
                    transfer.description = None;
                }
                return bounded_native_status(NativeBridgeResponse::Status {
                    batch,
                    theme: settings.theme,
                    accent: settings.accent,
                });
            }
            AddHttpDownload { mut request } => {
                request.overwrite_existing = false;
                if request.save_dir.is_some() {
                    return Err("browser requests cannot override save_dir".into());
                }
                let _operation = self.operation().await?;
                let (url, filename) = self
                    .engine
                    .preview_http_request(&request)
                    .map_err(message)?;
                request.url = url;
                let settings = self.settings();
                if settings.show_download_dialog && self.ui.supports_dialogs() {
                    self.stage_http_download(request, filename, &settings)?;
                } else {
                    return self
                        .enqueue_http(
                            ScrubbedRequest(Some(request)),
                            &settings,
                            ExtendedHttpOptions::default(),
                        )
                        .await
                        .map(created);
                }
            }
            ProbeMedia {
                url,
                playlist,
                headers,
            } => {
                return self
                    .probe_media_with_headers(&url, playlist, &headers)
                    .await
                    .map(|probe| NativeBridgeResponse::MediaProbed { probe })
            }
            AddMediaDownload { mut request } => {
                if request.save_dir.is_some() {
                    return Err("browser requests cannot override save_dir".into());
                }
                request.save_dir = None;
                return self.add_media_download(request).await.map(created);
            }
            AddTorrent { mut request } => {
                validate_magnet_uri(&request.source).map_err(message)?;
                if request.save_dir.is_some() {
                    return Err("browser requests cannot override save_dir".into());
                }
                request.save_dir = None;
                return self.add_torrent(request).await.map(created);
            }
            RefreshDownloadUrl { request } => self.refresh_download_url(request).await?,
            PauseTransfer { transfer_id } => self.pause_transfer(&transfer_id).await?,
            ResumeTransfer { transfer_id } => self.resume_transfer(&transfer_id).await?,
            OpenDesktopWindow {
                open_picker_url,
                headers,
            } => {
                self.open_picker_with_headers(open_picker_url, headers)
                    .await?
            }
            GrabBatchUrls { urls, referrer } => self.grab_batch(urls, referrer).await?,
        }
        Ok(NativeBridgeResponse::Ack {
            ok: true,
            message: None,
        })
    }
}

impl TorrentSession {
    async fn new(config: &TorrentManagerConfig, settings: &AppSettings) -> Result<Self> {
        let mut config = Self::configured(config, settings, false)?;
        let network = &settings.torrent_network;
        match Self::start(config.clone(), network).await {
            Err(error) if config.blocklist_url.is_some() => {
                tracing::warn!(%error, "could not start torrent session with local blocklist");
                config.blocklist_url = None;
                Self::start(config, network).await
            }
            result => result,
        }
    }

    fn configured(
        config: &TorrentManagerConfig,
        settings: &AppSettings,
        strict_blocklist: bool,
    ) -> Result<TorrentManagerConfig> {
        let network = &settings.torrent_network;
        // librqbit 8 uses an exclusive u16 range. Its API cannot represent port
        // 65535 as an endpoint that the session can actually bind.
        let listen_port_range = if network.listen_port_start == 0 {
            None
        } else if network.listen_port_start == u16::MAX {
            return Err("The torrent engine cannot listen on port 65535 alone".into());
        } else {
            Some(network.listen_port_start..network.listen_port_end.saturating_add(1))
        };
        let blocklist_url = match blocklist_url(network.blocklist_path.as_deref()) {
            Err(error) if !strict_blocklist => {
                tracing::warn!(%error, "ignoring unavailable torrent blocklist");
                None
            }
            result => result?,
        };
        Ok(TorrentManagerConfig {
            default_save_dir: PathBuf::from(&settings.default_download_dir),
            listen_port_range,
            enable_dht: network.enable_dht,
            enable_upnp: network.enable_upnp,
            global_download_limit_bps: torrent_rate(settings.global_speed_limit_bps),
            global_upload_limit_bps: network.global_upload_limit_bps,
            socks_proxy_url: network.socks5_proxy_url.clone(),
            blocklist_url,
            ..config.clone()
        })
    }

    async fn start(config: TorrentManagerConfig, network: &TorrentNetworkSettings) -> Result<Self> {
        let manager = TorrentManager::new(config.clone()).await.map_err(message)?;
        Ok(Self {
            manager: Arc::new(manager),
            base: config.default_save_dir.clone(),
            config,
            network: network.clone(),
            stopped: false,
        })
    }

    fn needs_reconfigure(&self, settings: &AppSettings) -> bool {
        let current = &self.network;
        let next = &settings.torrent_network;
        self.stopped
            || self.base != Path::new(&settings.default_download_dir)
            || current.listen_port_start != next.listen_port_start
            || current.listen_port_end != next.listen_port_end
            || current.enable_dht != next.enable_dht
            || current.enable_upnp != next.enable_upnp
            || current.socks5_proxy_url != next.socks5_proxy_url
            || current.blocklist_path != next.blocklist_path
    }

    async fn replace(
        &mut self,
        config: TorrentManagerConfig,
        network: &TorrentNetworkSettings,
    ) -> Result<()> {
        if !self.stopped
            && self.manager.listen_port().is_some_and(|port| {
                config
                    .listen_port_range
                    .as_ref()
                    .is_some_and(|range| range.contains(&port))
            })
        {
            self.stopped = true;
            self.manager.shutdown().await;
        }
        match Self::start(config, network).await {
            Ok(replacement) => {
                let previous = std::mem::replace(self, replacement);
                if !previous.stopped {
                    previous.manager.shutdown().await;
                }
                Ok(())
            }
            Err(error) => {
                if self.stopped {
                    *self = Self::start(self.config.clone(), &self.network)
                        .await
                        .map_err(|restore| {
                            format!("{error}. Could not restore torrent session: {restore}")
                        })?;
                }
                Err(error)
            }
        }
    }

    fn canonical_base(&self) -> PathBuf {
        std::fs::canonicalize(&self.base).unwrap_or_else(|_| self.base.clone())
    }

    fn save_dir_override<'a>(&self, requested: &'a Path) -> Option<&'a Path> {
        let matches_base = requested == self.base
            || (requested.is_absolute()
                && std::fs::canonicalize(requested)
                    .is_ok_and(|path| path == self.canonical_base()));
        (!matches_base).then_some(requested)
    }
}

fn blocklist_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let path = local_drive_path(Path::new(raw)).map_err(message)?;
    let drive = path
        .ancestors()
        .last()
        .ok_or("Blocklist requires a local drive")?;
    let drive: Vec<_> = drive.as_os_str().encode_wide().chain([0]).collect();
    // DRIVE_REMOTE from GetDriveTypeW. A mapped share can have a drive letter.
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(drive.as_ptr()) } == 4 {
        return Err("Blocklist must be on a local drive".into());
    }
    let parent = path.parent().ok_or("Blocklist requires a file path")?;
    validate_existing_ancestors(parent).map_err(message)?;
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not read the blocklist: {error}"))?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err("Blocklist must be a regular local file".into());
    }
    Url::from_file_path(path)
        .map(|url| Some(url.into()))
        .map_err(|()| "Could not convert the blocklist path to a file URL".into())
}

struct NativeHandler(Weak<AppState>);

impl PipeRequestHandler for NativeHandler {
    async fn handle(&self, request: NativeBridgeRequest) -> NativeBridgeResponse {
        let Some(state) = self.0.upgrade() else {
            return error_response("daemon_stopped", "SlipStream has stopped");
        };
        state.native_request(request).await.unwrap_or_else(|error| {
            state.log_event("warn", "pipe", "Authenticated native request failed");
            error_response("request_failed", &error)
        })
    }
}

enum LocalPickerTarget {
    Starting(Vec<OpenPickerEvent>),
    Ready(Weak<AppState>),
}

struct LocalPickerHandler(Arc<Mutex<LocalPickerTarget>>);

impl PipeRequestHandler for LocalPickerHandler {
    async fn handle(&self, request: NativeBridgeRequest) -> NativeBridgeResponse {
        let NativeBridgeRequest::OpenDesktopWindow {
            open_picker_url: Some(source),
            headers,
        } = request
        else {
            return error_response("unsupported_request", "Expected a local picker request");
        };
        if !headers.is_empty() {
            return error_response(
                "unsupported_request",
                "Local picker requests cannot include headers",
            );
        }
        let event = match local_picker_event(&source) {
            Ok(event) => event,
            Err(error) => return error_response("request_failed", &error),
        };
        let state = {
            let mut target = self.0.lock().unwrap();
            match &mut *target {
                LocalPickerTarget::Starting(events) => {
                    if events.len() >= LOCAL_PICKER_STARTUP_CAPACITY {
                        return error_response("picker_busy", "The local picker queue is full");
                    }
                    events.push(event);
                    return NativeBridgeResponse::Ack {
                        ok: true,
                        message: None,
                    };
                }
                LocalPickerTarget::Ready(state) => state.upgrade(),
            }
        };
        let Some(state) = state else {
            return error_response("daemon_stopped", "SlipStream has stopped");
        };
        match state.queue_picker(event).await {
            Ok(()) => NativeBridgeResponse::Ack {
                ok: true,
                message: None,
            },
            Err(error) => error_response("request_failed", &error),
        }
    }
}

fn local_picker_event(source: &str) -> Result<OpenPickerEvent> {
    let source = source.trim();
    let url = if is_magnet_source(source) {
        validate_magnet_uri(source).map_err(message)?;
        source.to_owned()
    } else {
        if !is_torrent_path(source) {
            return Err("expected a magnet link or a local .torrent file".into());
        }
        let absolute = std::path::absolute(source).map_err(message)?;
        let path = local_drive_path(&absolute).map_err(message)?;
        let parent = path
            .parent()
            .ok_or("torrent source has no parent directory")?;
        validate_existing_ancestors(parent).map_err(message)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(message)?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("torrent source must be a regular local file".into());
        }
        path.to_string_lossy().into_owned()
    };
    Ok(OpenPickerEvent {
        url: Some(url),
        headers: HashMap::new(),
    })
}

fn is_magnet_source(source: &str) -> bool {
    source
        .trim_start()
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("magnet"))
}

fn is_torrent_path(source: &str) -> bool {
    Path::new(source)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
}

fn torrent_source_key(source: &str) -> String {
    if is_magnet_source(source) {
        let (_, query) = source.split_once(':').expect("magnet URI scheme");
        format!("magnet:{query}")
    } else {
        std::path::absolute(source)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| source.to_owned())
    }
}

fn browser_source_url(raw: &str) -> String {
    if let Ok(mut url) = validate_http_url(raw) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        url.into()
    } else if raw
        .trim()
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("magnet"))
    {
        "magnet:".into()
    } else {
        String::new()
    }
}

async fn http_save_directory(raw: Option<String>, settings: &AppSettings) -> Result<PathBuf> {
    let raw = raw
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| settings.default_download_dir.clone());
    tokio::task::spawn_blocking(move || ensure_safe_directory(Path::new(&raw)))
        .await
        .map_err(message)?
        .map_err(message)
}

fn bounded_native_status(mut response: NativeBridgeResponse) -> Result<NativeBridgeResponse> {
    const BUDGET: usize = MAX_NATIVE_MESSAGE_BYTES - 16 * 1024;
    if serde_json::to_vec(&response).map_err(message)?.len() < BUDGET {
        return Ok(response);
    }
    let NativeBridgeResponse::Status { batch, .. } = &mut response else {
        unreachable!();
    };
    let mut transfers = std::mem::take(&mut batch.transfers);
    let mut size = serde_json::to_vec(&response).map_err(message)?.len();
    let mut sizes = Vec::with_capacity(transfers.len());
    for transfer in &mut transfers {
        transfer.segments.clear();
        for text in [&mut transfer.error_message, &mut transfer.completion_notice]
            .into_iter()
            .flatten()
        {
            if text.len() > 256 {
                text.truncate(text.floor_char_boundary(253));
                text.push_str("...");
            }
        }
        let bytes = serde_json::to_vec(transfer).map_err(message)?.len() + 1;
        size += bytes;
        sizes.push(bytes);
    }
    // Snapshot order is oldest first. Preserve live transfers even when they
    // predate completed ones. If live transfers alone fill a frame, keep the newest.
    for terminal_only in [true, false] {
        for (transfer, bytes) in transfers.iter().zip(&mut sizes) {
            if size < BUDGET {
                break;
            }
            if !terminal_only
                || matches!(
                    transfer.status,
                    TransferStatus::Completed | TransferStatus::Failed | TransferStatus::Canceled
                )
            {
                size -= std::mem::take(bytes);
            }
        }
    }
    let NativeBridgeResponse::Status { batch, .. } = &mut response else {
        unreachable!();
    };
    batch.transfers = transfers
        .into_iter()
        .zip(sizes)
        .filter_map(|(transfer, bytes)| (bytes > 0).then_some(transfer))
        .collect();
    Ok(response)
}

fn check_media_ip(ip: std::net::IpAddr, allow_loopback: bool) -> Result<()> {
    let mapped = matches!(ip, std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some());
    if mapped || (is_private_or_loopback_ip(ip) && !(allow_loopback && ip.is_loopback())) {
        return Err("disallowed private or loopback IP address".into());
    }
    Ok(())
}

pub(crate) fn validate_media_url(raw: &str, allow_loopback: bool) -> Result<Url> {
    let url = validate_http_url(raw).map_err(message)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are unsupported".into());
    }
    if let Some(host) = url.host_str() {
        let host = host.trim_end_matches('.');
        let stripped = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = stripped.parse::<std::net::IpAddr>() {
            check_media_ip(ip, allow_loopback)?;
        } else if !allow_loopback
            && (host.eq_ignore_ascii_case("localhost")
                || host.to_ascii_lowercase().ends_with(".localhost"))
        {
            return Err("disallowed loopback host".into());
        }
    }
    Ok(url)
}

async fn ensure_public_media_host(url: &Url, allow_loopback: bool) -> Result<()> {
    let host = url.host_str().ok_or("missing media URL host")?;
    let port = url
        .port_or_known_default()
        .ok_or("missing media URL port")?;
    let mut addrs = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|error| format!("could not resolve media host: {error}"))?
        .peekable();
    if addrs.peek().is_none() {
        return Err("media host resolved to no addresses".into());
    }
    for addr in addrs {
        check_media_ip(addr.ip(), allow_loopback)?;
    }
    Ok(())
}

fn is_pure_playlist_url(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    let mut path = url.path_segments().into_iter().flatten();
    let first = path.next();
    let targets_video = url.query_pairs().any(|(key, _)| key == "v")
        || (matches!(host, "youtu.be" | "www.youtu.be") && first.is_some_and(|id| !id.is_empty()))
        || ((host == "youtube.com" || host.ends_with(".youtube.com"))
            && matches!(first, Some("shorts" | "live" | "embed"))
            && path.next().is_some_and(|id| !id.is_empty()));
    !targets_video
        && (url.path().trim_end_matches('/').ends_with("/playlist")
            || url
                .query_pairs()
                .any(|(key, value)| key == "list" && !value.is_empty()))
}

fn playlist_ranges(items: &str) -> Result<Vec<std::ops::RangeInclusive<u32>>> {
    validate_playlist_items(items).map_err(message)?;
    items
        .split(',')
        .map(|part| {
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            Ok(first.parse::<u32>().map_err(message)?..=last.parse::<u32>().map_err(message)?)
        })
        .collect()
}

fn sync_secret_flags(settings: &mut AppSettings, vault: &SecretVault) {
    settings.proxy.has_password = vault.has_secret(proxy_secret_key());
    for entry in &mut settings.site_logins {
        entry.has_password = vault.has_secret(&site_login_secret_key(&entry.id));
    }
}

fn read_vault_secret(vault: &SecretVault, key: &str) -> Option<String> {
    vault.get_secret(key).unwrap_or_else(|error| {
        tracing::warn!(%error, "could not decrypt local vault secret");
        None
    })
}

fn site_auth_header(entry: &SiteLoginEntry, secret: &str) -> Option<HeaderValue> {
    let raw = match entry.auth_scheme {
        SiteAuthScheme::Basic => format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{secret}", entry.username))
        ),
        SiteAuthScheme::Bearer => format!("Bearer {secret}"),
    };
    let mut header = HeaderValue::from_str(&raw).ok()?;
    header.set_sensitive(true);
    Some(header)
}

fn http_proxy_config(vault: &SecretVault, settings: &AppSettings) -> Result<EngineProxyConfig> {
    let proxy = &settings.proxy;
    Ok(match proxy.mode {
        ProxyMode::Direct => EngineProxyConfig::Direct,
        ProxyMode::System => EngineProxyConfig::System,
        ProxyMode::Manual => EngineProxyConfig::Manual {
            url: proxy.url.clone().ok_or("manual proxy requires a URL")?,
            username: proxy.username.clone(),
            password: proxy_password(vault, settings),
            bypass_hosts: proxy.bypass_hosts.clone(),
        },
    })
}

fn proxy_password(vault: &SecretVault, settings: &AppSettings) -> Option<String> {
    if settings.proxy.has_password && settings.proxy.username.is_some() {
        read_vault_secret(vault, proxy_secret_key())
    } else {
        None
    }
}

fn media_network_options(
    settings: &AppSettings,
    vault: &SecretVault,
    target: &Url,
) -> Result<(Option<String>, Option<String>)> {
    let host = target.host_str().unwrap_or_default();
    let proxy = &settings.proxy;
    let bypass = proxy
        .bypass_hosts
        .iter()
        .any(|pattern| match pattern.as_str() {
            "*" => true,
            "<local>" => !host.contains(['.', ':']),
            _ => host_pattern_matches(pattern, host),
        });
    let proxy_url = if proxy.apply_to_media && proxy.mode == ProxyMode::Manual && !bypass {
        let mut url = slipstream_protocol::validate_http_proxy_url(
            proxy.url.as_deref().ok_or("manual proxy requires a URL")?,
        )
        .map_err(|_| "invalid HTTP proxy URL")?;
        if let Some(username) = &proxy.username {
            url.set_username(username)
                .map_err(|_| "invalid proxy username")?;
        }
        if let Some(password) = proxy_password(vault, settings) {
            url.set_password(Some(&password))
                .map_err(|_| "invalid proxy credentials")?;
        }
        Some(url.into())
    } else if proxy.apply_to_media && proxy.mode == ProxyMode::System {
        slipstream_engine::system_http_proxy_for(target).map_err(message)?
    } else {
        None
    };
    let cookies = &settings.cookie_passthrough;
    let browser = (cookies.enabled
        && cookies
            .allowed_hosts
            .iter()
            .any(|pattern| host_pattern_matches(pattern, host)))
    .then(|| cookies.browser.clone());
    Ok((proxy_url, browser))
}

async fn run_media_probe(
    toolchain_cache: &ToolchainCache,
    settings: AppSettings,
    resource_dir: Option<PathBuf>,
    url: Url,
    allow_loopback: bool,
    mut options: ProbeOptions,
) -> Result<MediaProbeResult> {
    let binary = discover_ytdlp_for_probe(toolchain_cache, settings, resource_dir).await?;
    if let Some(cache_dir) = options.cache_dir.take() {
        options.cache_dir = Some(
            tokio::task::spawn_blocking(move || ensure_safe_tree(&cache_dir))
                .await
                .map_err(message)?
                .map_err(message)?,
        );
    }
    ensure_public_media_host(&url, allow_loopback).await?;
    probe_url_with_binary(&binary, url.as_str(), options)
        .await
        .map_err(message)
}

fn open_reveal_action(action: RevealAction) -> Result<()> {
    let explorer = explorer_executable().map_err(message)?;
    let mut command = StdCommand::new(explorer);
    match action {
        RevealAction::SelectFile(raw) => command.raw_arg(raw),
        RevealAction::OpenDir(dir) => command.arg(dir),
    };
    command.spawn().map_err(message)?;
    Ok(())
}

fn created(transfer_id: String) -> NativeBridgeResponse {
    NativeBridgeResponse::TransferCreated { transfer_id }
}

async fn remove_outputs(root: PathBuf, paths: Vec<String>) -> Result<()> {
    tokio::task::spawn_blocking(move || delete_files(&root, &paths))
        .await
        .map_err(message)?
        .map_err(message)
}

pub fn expand_wildcard_urls(pattern: &str) -> Result<Vec<String>> {
    slipstream_engine::expand_wildcard_url(pattern).map_err(message)
}

async fn stopped_signal(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

async fn expire_pending_downloads(state: Arc<AppState>, stop: watch::Receiver<bool>) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    loop {
        let next = state.pending_downloads.lock().unwrap().next_expiration();
        let deadline = async {
            if let Some(next) = next {
                tokio::time::sleep_until(next).await;
            } else {
                pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            _ = state.pending_changed.notified() => {},
            _ = deadline => state.expire_pending_entries(),
        }
    }
}

async fn watch_media_jobs(
    state: Weak<AppState>,
    stop: watch::Receiver<bool>,
    mut finished: mpsc::UnboundedReceiver<MediaJobEvent>,
) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            Some(event) = finished.recv() => {
                let Some(state) = state.upgrade() else { return };
                let Ok(_operation) = state.operation().await else { return };
                let id = match event {
                    MediaJobEvent::QueueChanged(id) => id,
                    MediaJobEvent::Finished { transfer_id, playlist_id } => {
                        if let Some(job) = state.media_job(&transfer_id) {
                            let snapshot = job.snapshot();
                            match snapshot.status {
                                TransferStatus::Completed => state.on_transfer_completed(&snapshot).await,
                                TransferStatus::Failed => state.log_event(
                                    "error", "media", format!("Download failed ({transfer_id})"),
                                ),
                                _ => {}
                            }
                        }
                        playlist_id
                    }
                };
                state.drain_media_queue().await;
                if let Some(id) = id.filter(|id| state.playlists.lock().unwrap().contains_key(id)) {
                    if let Err(error) = state.advance_playlist(&id, false).await {
                        tracing::warn!(playlist_id = %id, %error, "could not advance playlist");
                    }
                }
            }
        }
    }
}

async fn emit_telemetry(state: Arc<AppState>, stop: watch::Receiver<bool>) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    let mut next = Instant::now();
    loop {
        if !state.ui.has_telemetry_windows() {
            tokio::select! {
                _ = &mut stopped => return,
                _ = state.window_changed.notified() => {}
            }
            next = Instant::now();
            continue;
        }
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            _ = state.window_changed.notified() => continue,
            _ = tokio::time::sleep_until(next) => {}
        }
        if state.ui.has_telemetry_windows() {
            let batch = state.snapshot().await;
            if state.ui.has_telemetry_windows() {
                if let Err(error) = state.ui.emit_telemetry(&batch) {
                    tracing::debug!(%error, "could not emit telemetry");
                }
            }
        }
        next += TELEMETRY_INTERVAL;
        let now = Instant::now();
        if next <= now {
            next = now + TELEMETRY_INTERVAL;
        }
    }
}

async fn watch_completions(
    state: Arc<AppState>,
    stop: watch::Receiver<bool>,
    mut http: broadcast::Receiver<EngineEvent>,
    mut seen_http: HashSet<String>,
) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    let mut announced_http = seen_http.clone();
    let mut pending_http = HashMap::new();
    let mut next_completion_check = Instant::now();
    loop {
        let check_completions =
            !state.torrent_completions.lock().unwrap().is_empty() || !pending_http.is_empty();
        let timer = async {
            if check_completions {
                tokio::time::sleep_until(next_completion_check).await;
            } else {
                pending::<()>().await;
            }
        };
        let completed = tokio::select! {
            biased;
            _ = &mut stopped => return,
            _ = state.torrent_changed.notified() => continue,
            _ = timer => {
                retry_http_completions(&state, &state.scans, &mut seen_http, &mut pending_http);
                scan_torrent_completions(&state, &state.scans).await;
                next_completion_check = Instant::now() + Duration::from_secs(1);
                continue;
            }
            event = http.recv() => match event {
                Ok(EngineEvent::StatusChanged { transfer_id, status: TransferStatus::Completed }) => {
                    state.http_snapshot(&transfer_id).into_iter().collect::<Vec<_>>()
                }
                Ok(EngineEvent::StatusChanged { transfer_id, status: TransferStatus::Failed }) => {
                    state.log_event("error", "http", format!("Download failed ({transfer_id})"));
                    continue;
                }
                Ok(EngineEvent::QueueDrained { queue_id, .. }) => {
                    state.on_queue_drained(&queue_id).await;
                    continue;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    state.engine.snapshots().into_iter().filter(|s| s.status == TransferStatus::Completed).collect()
                }
                Err(broadcast::error::RecvError::Closed) => return,
                _ => continue,
            },
        };
        for snapshot in completed {
            let id = snapshot.id.clone();
            scan_http_completion(
                &state,
                &state.scans,
                &mut seen_http,
                &mut pending_http,
                snapshot,
            );
            if !announced_http.contains(&id) {
                let Ok(_operation) = state.operation().await else {
                    return;
                };
                if let Some(snapshot) = state.http_snapshot(&id).filter(|snapshot| {
                    snapshot.status == TransferStatus::Completed && !snapshot.output_superseded
                }) {
                    announced_http.insert(id);
                    state.on_transfer_completed(&snapshot).await;
                }
            }
        }
    }
}

fn try_enqueue_http_scan(
    state: &AppState,
    scans: &mpsc::Sender<ScanRequest>,
    seen: &mut HashSet<String>,
    snapshot: &TransferSnapshot,
) -> bool {
    if snapshot.output_superseded
        || seen.contains(&snapshot.id)
        || state.dismissed_http.read().unwrap().contains(&snapshot.id)
    {
        return true;
    }
    if !state.settings.read().unwrap().auto_defender_scan {
        seen.insert(snapshot.id.clone());
        return true;
    }
    match scans.try_send((
        snapshot.save_dir.clone().into(),
        vec![snapshot.filename.clone()],
    )) {
        Ok(()) => {
            seen.insert(snapshot.id.clone());
            true
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!(transfer_id = %snapshot.id, "could not enqueue HTTP Defender scan");
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => true,
    }
}

fn scan_http_completion(
    state: &AppState,
    scans: &mpsc::Sender<ScanRequest>,
    seen: &mut HashSet<String>,
    pending: &mut HashMap<String, TransferSnapshot>,
    snapshot: TransferSnapshot,
) {
    if try_enqueue_http_scan(state, scans, seen, &snapshot) {
        pending.remove(&snapshot.id);
    } else {
        pending.insert(snapshot.id.clone(), snapshot);
    }
}

fn retry_http_completions(
    state: &AppState,
    scans: &mpsc::Sender<ScanRequest>,
    seen: &mut HashSet<String>,
    pending: &mut HashMap<String, TransferSnapshot>,
) {
    pending.retain(|id, snapshot| {
        if let Some(current) = state.http_snapshot(id) {
            *snapshot = current;
            !try_enqueue_http_scan(state, scans, seen, snapshot)
        } else {
            false
        }
    });
}

async fn scan_torrent_completions(state: &AppState, scans: &mpsc::Sender<ScanRequest>) {
    let Ok(_operation) = state.operation().await else {
        return;
    };
    let mut completed = Vec::new();
    {
        let torrent = state.torrent.read().await;
        let mut pending = state.torrent_completions.lock().unwrap();
        let mut completed_files = state.torrent_completed_files.lock().unwrap();
        let mut announced = state.torrent_announced.lock().unwrap();
        let mut failures = state.torrent_failures.lock().unwrap();
        pending.retain(|id| {
            let Ok(report) = torrent.manager.stats(id) else {
                completed_files.remove(id);
                announced.remove(id);
                failures.remove(id);
                return false;
            };
            if report.snapshot.status == TransferStatus::Failed {
                if failures.insert(id.clone()) {
                    state.log_event("error", "torrent", format!("Download failed ({id})"));
                }
                return true;
            }
            failures.remove(id);
            if !report.finished {
                return true;
            }
            if announced.insert(id.clone()) {
                state.announce_completion(&report.snapshot);
            }
            let Ok(files) = torrent.manager.list_files(id) else {
                return false;
            };
            let recorded = completed_files.entry(id.clone()).or_default();
            let mut to_mark = Vec::new();
            let mut to_scan = Vec::new();
            let auto_scan = state.settings.read().unwrap().auto_defender_scan;
            for path in files
                .into_iter()
                .filter(|file| file.selected)
                .map(|file| file.relative_path)
            {
                if !recorded.marked.contains(&path) {
                    to_mark.push(path.clone());
                }
                if recorded.extraction_started.insert(path.clone()) {
                    if let Ok(candidate) = slipstream_protocol::safe_join_under(
                        Path::new(&report.snapshot.save_dir), &path,
                    ) {
                        state.maybe_auto_extract(TransferKind::Torrent, candidate, id);
                    }
                }
                if auto_scan && !recorded.scanned.contains(&path) {
                    to_scan.push(path);
                }
            }
            let mut retry_scan = false;
            let scan = if to_scan.is_empty() {
                None
            } else {
                match scans.clone().try_reserve_owned() {
                    Ok(permit) => {
                        recorded.scanned.extend(to_scan.iter().cloned());
                        Some((permit, to_scan))
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!(transfer_id = %id, "could not enqueue torrent Defender scan");
                        retry_scan = true;
                        None
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => None,
                }
            };
            let marking_pending = !to_mark.is_empty();
            if marking_pending || scan.is_some() {
                completed.push((id.clone(), PathBuf::from(report.snapshot.save_dir), to_mark, scan, retry_scan));
            }
            marking_pending || retry_scan
        });
    }
    if completed.is_empty() {
        return;
    }
    let results = tokio::task::spawn_blocking(move || {
        let mut results = Vec::with_capacity(completed.len());
        for (id, root, to_mark, scan, retry_scan) in completed {
            let mut retry = retry_scan;
            let mut marked = Vec::new();
            for path in to_mark {
                match write_zone_identifier(&root, &path) {
                    Ok(()) => marked.push(path),
                    Err(error) => {
                        let permanent = matches!(
                            error.kind(),
                            std::io::ErrorKind::Unsupported
                                | std::io::ErrorKind::InvalidInput
                                | std::io::ErrorKind::NotFound
                        ) || matches!(error.raw_os_error(), Some(1 | 50 | 87 | 123));
                        if permanent {
                            marked.push(path);
                        } else {
                            retry = true;
                        }
                        tracing::debug!(transfer_id = %id, permanent, %error, "could not write torrent Zone.Identifier");
                    }
                }
            }
            if let Some((permit, to_scan)) = scan {
                permit.send((root, to_scan));
            }
            results.push((id, marked, retry));
        }
        results
    })
    .await;
    match results {
        Ok(results) => {
            let mut pending = state.torrent_completions.lock().unwrap();
            let mut completed = state.torrent_completed_files.lock().unwrap();
            for (id, marked, retry) in results {
                completed
                    .entry(id.clone())
                    .or_default()
                    .marked
                    .extend(marked);
                if !retry {
                    pending.remove(&id);
                }
            }
        }
        Err(error) => tracing::debug!(%error, "torrent completion worker failed"),
    }
}

async fn scan_completed(
    state: Weak<AppState>,
    stop: watch::Receiver<bool>,
    mut requests: mpsc::Receiver<ScanRequest>,
) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    loop {
        let (root, paths) = tokio::select! {
            biased;
            _ = &mut stopped => return,
            Some(request) = requests.recv() => request,
            else => return,
        };
        let enabled = state
            .upgrade()
            .is_some_and(|state| state.settings.read().unwrap().auto_defender_scan);
        if !enabled {
            continue;
        }
        let Some(defender) = defender_executable() else {
            tracing::debug!("Windows Defender command-line tool was not found");
            continue;
        };
        for path in paths {
            let args = match build_defender_scan_args(&root, &path) {
                Ok(args) => args,
                Err(error) => {
                    tracing::debug!(%error, "skipping unavailable scan target");
                    continue;
                }
            };
            let mut command = Command::new(&defender);
            command
                .args(args)
                .creation_flags(CREATE_NO_WINDOW)
                .kill_on_drop(true)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let result = tokio::select! {
                biased;
                _ = &mut stopped => return,
                result = timeout(DEFENDER_SCAN_TIMEOUT, command.status()) => result,
            };
            match result {
                Ok(Ok(status)) if status.success() => {}
                Ok(Ok(status)) => {
                    tracing::warn!(%status, "Windows Defender scan returned an unsuccessful status")
                }
                Ok(Err(error)) => tracing::warn!(%error, "could not run Windows Defender scan"),
                Err(_) => tracing::warn!("Windows Defender scan timed out"),
            }
        }
    }
}

async fn run_automation_loop(state: Weak<AppState>, stop: watch::Receiver<bool>) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    let tracker = Arc::new(Mutex::new(WatchFolderTracker::default()));
    let mut tick: u64 = 0;
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(5) {
            let revision = state.settings_revision.load(Ordering::SeqCst);
            let Ok(_operation) = state.operation().await else {
                return;
            };
            state.evaluate_and_apply_automation(revision).await;
            if let Err(error) = state.refresh_torrent_session(&state.settings()).await {
                tracing::debug!(%error, "could not apply deferred torrent session settings");
            }
        }
        if tick.is_multiple_of(3) {
            let revision = state.settings_revision.load(Ordering::SeqCst);
            let rules = state.settings.read().unwrap().watch_folders.clone();
            if rules.is_empty() || state.vpn_kill_switch_active.load(Ordering::SeqCst) {
                continue;
            }
            let tracker_ref = tracker.clone();
            let candidates = tokio::task::spawn_blocking(move || {
                let mut guard = tracker_ref.lock().unwrap();
                scan_watch_folders(&rules, &mut guard)
            })
            .await
            .unwrap_or_default();
            for candidate in candidates {
                let path = candidate.torrent_path;
                let Some(mut captured) = tracker.lock().unwrap().take_captured(&path) else {
                    continue;
                };
                let request = AddTorrentRequest {
                    source: path.to_string_lossy().into_owned(),
                    save_dir: candidate.target_save_dir.clone(),
                    selected_file_indices: None,
                    sequential: false,
                    start_paused: candidate.start_paused,
                };
                let result = {
                    let Ok(_operation) = state.operation().await else {
                        return;
                    };
                    if state.settings_revision.load(Ordering::SeqCst) != revision {
                        break;
                    }
                    let bytes = Bytes::from(std::mem::take(&mut captured.bytes));
                    state.add_torrent_locked(request, Some(bytes)).await
                };
                match result {
                    Ok(id) => {
                        tracker.lock().unwrap().mark_added(
                            path.clone(),
                            captured.size,
                            captured.modified,
                        );
                        if let Some(category_id) = candidate.category_id {
                            let _ = state
                                .set_transfers_category(
                                    std::slice::from_ref(&id),
                                    Some(category_id),
                                )
                                .await;
                        }
                        if candidate.delete_torrent_after_add {
                            let delete_path = path.clone();
                            match tokio::task::spawn_blocking(move || {
                                captured.delete_after_add(&delete_path)
                            })
                            .await
                            {
                                Ok(Ok(())) => tracker.lock().unwrap().forget(&path),
                                Ok(Err(error)) => {
                                    tracing::debug!(%error, "could not delete the added watch file")
                                }
                                Err(error) => {
                                    tracing::debug!(%error, "watch file cleanup worker failed")
                                }
                            }
                        }
                        state.log_event(
                            "info",
                            "watch",
                            format!("Added torrent from watch folder: {}", path.display()),
                        );
                    }
                    Err(error) => {
                        tracker.lock().unwrap().record_failure(&path);
                        state.log_event(
                            "warn",
                            "watch",
                            format!("Watch folder skipped '{}': {error}", path.display()),
                        );
                    }
                }
            }
        }
    }
}

async fn run_rss_loop(state: Weak<AppState>, stop: watch::Receiver<bool>) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            biased;
            () = &mut stopped => return,
            () = tokio::time::sleep(Duration::from_secs(15)) => {}
        }
        let Some(state) = state.upgrade() else { return };
        if state.vpn_kill_switch_active.load(Ordering::SeqCst) {
            continue;
        }
        if let Err(error) = state.poll_rss_feeds(false).await {
            tracing::debug!(%error, "RSS polling did not finish");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use slipstream_torrent::TorrentAddOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use uuid::Uuid;
    use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    struct TestDir(PathBuf);

    #[tokio::test]
    async fn completed_torrent_archive_extracts_without_interrupting_seeding() {
        use std::io::{Cursor, Write as _};

        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let mut settings = state.settings();
        settings.auto_extract = slipstream_protocol::AutoExtractSettings {
            enabled: true,
            create_subfolder: true,
            delete_archive_after_extract: true,
            overwrite_existing: true,
        };
        let downloads = PathBuf::from(&settings.default_download_dir);
        state.save_settings(settings).await.unwrap();

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"torrent archive").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let manifest =
            slipstream_torrent::build_v1_single_file_torrent("bundle.zip", &bytes, 16 * 1024)
                .unwrap();
        let archive = downloads.join("bundle.zip");
        std::fs::write(&archive, &bytes).unwrap();
        let manager = state.torrent.read().await.manager.clone();
        let id = manager
            .add_torrent_bytes(
                &manifest,
                TorrentAddOptions {
                    overwrite: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        manager
            .wait_until_completed(&id, Duration::from_secs(10))
            .await
            .unwrap();
        state.torrent_completions.lock().unwrap().insert(id.clone());
        scan_torrent_completions(&state, &state.scans).await;
        let destination = downloads.join("bundle").join("readme.txt");
        timeout(Duration::from_secs(5), async {
            while !destination.is_file() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"torrent archive");
        assert_eq!(std::fs::read(archive).unwrap(), bytes);
        assert_eq!(
            manager.snapshot(&id).unwrap().status,
            TransferStatus::Seeding
        );
        state.shutdown().await.unwrap();
    }

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("slipstream-daemon-unit-{}", Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct IsolatedReg(String);

    impl IsolatedReg {
        fn new() -> Self {
            Self(format!(r"Software\SlipStreamDaemonUnit-{}", Uuid::new_v4()))
        }
    }

    impl Drop for IsolatedReg {
        fn drop(&mut self) {
            let wide: Vec<_> = self.0.encode_utf16().chain([0]).collect();
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr()) };
        }
    }

    struct NullUi;

    impl UiBridge for NullUi {
        fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn emit_telemetry(&self, _batch: &TelemetryBatch) -> Result<()> {
            Ok(())
        }

        fn emit_open_picker(&self, _event: &OpenPickerEvent) -> Result<()> {
            Ok(())
        }

        fn has_main_window(&self) -> bool {
            false
        }

        fn request_quit(&self) {}
    }

    async fn start_test_daemon(directory: &Path) -> Arc<AppState> {
        let settings = AppSettings {
            default_download_dir: directory.join("Downloads").to_string_lossy().into_owned(),
            auto_defender_scan: false,
            torrent_network: TorrentNetworkSettings {
                listen_port_start: 0,
                listen_port_end: 0,
                enable_dht: false,
                ..Default::default()
            },
            ..AppSettings::default()
        };
        write_json_atomically(&directory.join(SETTINGS_FILE), &settings).unwrap();
        AppState::start(
            directory.to_path_buf(),
            Arc::new(NullUi),
            DaemonOptions {
                pipe_name: Some(format!(
                    r"\\.\pipe\SlipStream.DaemonUnitTest.{}",
                    Uuid::new_v4()
                )),
                allow_loopback: true,
                torrent: TorrentManagerConfig {
                    enable_dht: false,
                    enable_trackers: false,
                    listen_port_range: None,
                    ..TorrentManagerConfig::default()
                },
                ..DaemonOptions::default()
            },
        )
        .await
        .unwrap()
    }

    fn test_magnet(digit: char, start_paused: bool) -> AddTorrentRequest {
        AddTorrentRequest {
            source: format!("magnet:?xt=urn:btih:{}", digit.to_string().repeat(40)),
            save_dir: None,
            selected_file_indices: None,
            sequential: false,
            start_paused,
        }
    }

    #[tokio::test]
    async fn vpn_blocks_adds_and_resumes_and_releases_only_its_own_pauses() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let active = state.add_torrent(test_magnet('a', false)).await.unwrap();
        let manual = state.add_torrent(test_magnet('b', true)).await.unwrap();
        let mut settings = state.settings();
        let missing = "__impossible_vpn_adapter_99999__";
        settings.torrent_network.vpn_kill_switch_enabled = true;
        settings.torrent_network.vpn_adapter_name = Some(missing.into());
        state.save_settings(settings).await.unwrap();

        let status = state.automation_status();
        assert!(status.vpn_kill_switch_active);
        assert!(!status.vpn_adapter_connected);
        assert_eq!(status.vpn_adapter_name.as_deref(), Some(missing));
        assert_eq!(
            *state.vpn_paused_torrents.lock().unwrap(),
            HashSet::from([active.clone()])
        );
        assert_eq!(
            state.get_transfer_snapshot(&active).await.unwrap().status,
            TransferStatus::Paused
        );

        let result = state.add_torrent(test_magnet('c', true)).await;
        assert_eq!(result.unwrap_err(), VPN_TRAFFIC_BLOCKED);
        assert_eq!(
            state
                .add_torrent_with_preview(test_magnet('d', false), Some("expired".into()))
                .await
                .unwrap_err(),
            VPN_TRAFFIC_BLOCKED,
        );
        assert_eq!(
            state.resume_transfer(&manual).await.unwrap_err(),
            VPN_TRAFFIC_BLOCKED
        );
        assert_eq!(
            state.poll_rss_feeds_now().await.unwrap_err(),
            VPN_TRAFFIC_BLOCKED
        );
        // A newly disconnected adapter must still block traffic before the next tick.
        state.vpn_kill_switch_active.store(false, Ordering::SeqCst);
        assert_eq!(
            state.resume_transfer(&manual).await.unwrap_err(),
            VPN_TRAFFIC_BLOCKED
        );

        let mut settings = state.settings();
        settings.torrent_network.vpn_kill_switch_enabled = false;
        state.save_settings(settings).await.unwrap();
        assert!(!state.automation_status().vpn_kill_switch_active);
        assert_eq!(
            state.get_transfer_snapshot(&active).await.unwrap().status,
            TransferStatus::Probing
        );
        assert_eq!(
            state.get_transfer_snapshot(&manual).await.unwrap().status,
            TransferStatus::Paused
        );
        state.shutdown().await.unwrap();
    }

    #[test]
    fn seeding_limits_are_inclusive_and_either_limit_stops_seeding() {
        let mut settings = TorrentSeedingSettings {
            max_seed_ratio_hundredths: Some(150),
            max_seed_minutes: Some(2),
            ..Default::default()
        };
        assert!(!seed_limit_reached(&settings, 1.49, 119));
        assert!(seed_limit_reached(&settings, 1.5, 0));
        assert!(seed_limit_reached(&settings, 0.0, 120));
        settings.stop_seeding_on_limit = false;
        assert!(!seed_limit_reached(&settings, 2.0, 200));
        assert!(!seed_limit_reached(
            &TorrentSeedingSettings::default(),
            100.0,
            u64::MAX
        ));
    }

    #[tokio::test]
    async fn seeding_ratio_pauses_a_live_seed_after_a_local_peer_downloads() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let mut settings = state.settings();
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = socket.local_addr().unwrap();
        drop(socket);
        settings.torrent_network.listen_port_start = peer.port();
        settings.torrent_network.listen_port_end = peer.port();
        let downloads = PathBuf::from(&settings.default_download_dir);
        state.save_settings(settings).await.unwrap();
        let payload = vec![24; 16 * 1024];
        std::fs::write(downloads.join("payload.bin"), &payload).unwrap();
        let manifest =
            slipstream_torrent::build_v1_single_file_torrent("payload.bin", &payload, 16 * 1024)
                .unwrap();
        let seed = state.torrent.read().await.manager.clone();
        let id = seed
            .add_torrent_bytes(
                &manifest,
                TorrentAddOptions {
                    overwrite: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        seed.wait_until_completed(&id, Duration::from_secs(10))
            .await
            .unwrap();
        let leech = TorrentManager::new(TorrentManagerConfig {
            default_save_dir: dir.0.join("leech"),
            enable_dht: false,
            enable_trackers: false,
            listen_port_range: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let downloaded = leech
            .add_torrent_bytes(
                &manifest,
                TorrentAddOptions {
                    initial_peers: vec![peer],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        leech
            .wait_until_completed(&downloaded, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(seed.stats(&id).unwrap().ratio >= 1.0);

        let mut settings = state.settings();
        settings.torrent_seeding.max_seed_ratio_hundredths = Some(100);
        state.save_settings(settings).await.unwrap();
        assert_eq!(seed.snapshot(&id).unwrap().status, TransferStatus::Paused);
        assert!(!state.automation_owns_pause(&id));
        leech.shutdown().await;
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_torrent_file_round_trips_and_upload_limit_preserves_download_limit() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let source = dir.0.join("source.bin");
        let output = dir.0.join("source.torrent");
        let payload = b"created through the daemon";
        std::fs::write(&source, payload).unwrap();
        let created = state
            .create_torrent_file(CreateTorrentRequest {
                source_path: source.to_string_lossy().into_owned(),
                output_path: output.to_string_lossy().into_owned(),
                trackers: Vec::new(),
                piece_length_kib: 16,
                private: true,
                comment: Some("round trip".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(created.total_bytes, payload.len() as u64);
        assert_eq!((created.file_count, created.piece_count), (1, 1));
        let mut request = test_magnet('a', true);
        request.source = output.to_string_lossy().into_owned();
        let id = state.add_torrent(request).await.unwrap();
        assert_eq!(id, created.info_hash);
        state
            .set_transfer_speed_limit(&id, Some(64 * 1024))
            .await
            .unwrap();
        state
            .set_torrent_upload_limit(&id, Some(32 * 1024))
            .await
            .unwrap();
        let snapshot = state.get_transfer_snapshot(&id).await.unwrap();
        assert_eq!(snapshot.speed_limit_bps, Some(64 * 1024));
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn backup_redacts_proxy_credentials_and_keeps_the_local_blocklist_on_import() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let blocklist = dir.0.join("local blocklist.p2p");
        std::fs::write(&blocklist, "local:192.0.2.1-192.0.2.1\n").unwrap();
        let proxy = "socks5://proxy-user:proxy-password@127.0.0.1:1080";
        let mut settings = state.settings();
        settings.torrent_network.blocklist_path = Some(blocklist.to_string_lossy().into_owned());
        settings.torrent_network.socks5_proxy_url = Some(proxy.into());
        state.save_settings(settings).await.unwrap();
        let path = dir.0.join("backup.json");
        state.export_settings_backup(&path).await.unwrap();
        let backup = std::fs::read_to_string(&path).unwrap();
        assert!(!backup.contains("proxy-user"));
        assert!(!backup.contains("proxy-password"));
        let mut bundle: SettingsBackupBundle = serde_json::from_str(&backup).unwrap();
        assert_eq!(bundle.settings.torrent_network.blocklist_path, None);
        assert_eq!(
            bundle.settings.torrent_network.socks5_proxy_url.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        bundle.settings.torrent_network.blocklist_path =
            Some(r"\\example.org\remote\blocklist.txt".into());
        write_json_atomically(&path, &bundle).unwrap();
        let imported = state.import_settings_backup(&path).await.unwrap();
        assert_eq!(
            imported.torrent_network.blocklist_path,
            Some(blocklist.to_string_lossy().into_owned())
        );
        assert!(state
            .torrent
            .read()
            .await
            .config
            .blocklist_url
            .as_deref()
            .unwrap()
            .starts_with("file:///"));
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn network_transport_changes_wait_for_an_empty_session_and_upload_limit_is_live() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let original = state.torrent.read().await.manager.clone();
        let id = state.add_torrent(test_magnet('a', true)).await.unwrap();
        let mut settings = state.settings();
        settings.torrent_network.global_upload_limit_bps = Some(128 * 1024);
        state.save_settings(settings.clone()).await.unwrap();
        {
            let session = state.torrent.read().await;
            assert!(Arc::ptr_eq(&session.manager, &original));
            assert_eq!(session.config.global_upload_limit_bps, Some(128 * 1024));
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        settings.torrent_network.listen_port_start = port;
        settings.torrent_network.listen_port_end = port;
        state.save_settings(settings).await.unwrap();
        assert!(Arc::ptr_eq(&state.torrent.read().await.manager, &original));
        state.cancel_transfer(&id, false).await.unwrap();
        let operation = state.operation().await.unwrap();
        state
            .refresh_torrent_session(&state.settings())
            .await
            .unwrap();
        {
            let session = state.torrent.read().await;
            assert!(!Arc::ptr_eq(&session.manager, &original));
            assert_eq!(session.manager.listen_port(), Some(port));
            assert_eq!(session.config.global_upload_limit_bps, Some(128 * 1024));
        }
        drop(operation);
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rss_poll_enqueues_matching_magnets_and_staged_torrents_once() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let save_dir = Path::new(&state.settings().default_download_dir).join("rss");
        let manifest =
            slipstream_torrent::build_v1_single_file_torrent("feed.bin", b"payload", 16 * 1024)
                .unwrap();
        let metadata = slipstream_torrent::parse_torrent_bytes(&manifest, &save_dir).unwrap();
        let file_hash = hex::encode(metadata.info_hash_v1.unwrap());
        let magnet = test_magnet('e', false);
        let xml = format!(
            r#"<rss><channel>
            <item><title>Stable ISO</title><guid>magnet</guid><link>{}</link></item>
            <item><title>Stable ISO file</title><guid>manifest</guid><enclosure type="application/x-bittorrent" url="/download.torrent"/></item>
            <item><title>Beta ISO</title><guid>skip</guid><link>{}</link></item>
        </channel></rss>"#,
            magnet.source,
            test_magnet('b', false).source
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let feed_url = format!("http://{}/feed", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for expected in ["/feed", "/download.torrent", "/feed"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                assert!(request.starts_with(format!("GET {expected} ").as_bytes()));
                let bytes = if expected == "/feed" {
                    xml.as_bytes()
                } else {
                    &manifest
                };
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(bytes).await.unwrap();
            }
        });
        // Production settings reject loopback feeds. Install the fixture only
        // in the isolated daemon that was started with its test loopback option.
        state.settings.write().unwrap().rss_feeds.push(RssFeedRule {
            id: "local-feed".into(),
            name: "Test feed".into(),
            feed_url,
            include_pattern: "*iso*".into(),
            exclude_pattern: "beta".into(),
            save_dir: Some(save_dir.to_string_lossy().into_owned()),
            poll_interval_minutes: 5,
            enabled: true,
            auto_start: false,
            last_checked_at: None,
            last_error: None,
        });
        assert_eq!(state.poll_rss_feeds_now().await.unwrap(), 2);
        assert_eq!(state.automation_status().rss_matches_added, 2);
        assert_eq!(state.poll_rss_feeds(false).await.unwrap(), 0);
        assert_eq!(state.poll_rss_feeds_now().await.unwrap(), 0);
        let snapshots = state.torrent.read().await.manager.list_snapshots();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots
            .iter()
            .all(|snapshot| snapshot.status == TransferStatus::Paused
                && Path::new(&snapshot.save_dir) == save_dir));
        assert!(snapshots.iter().any(|snapshot| snapshot.id == file_hash));
        let stored: AppSettings = read_json(&dir.0.join(SETTINGS_FILE)).unwrap();
        assert!(stored.rss_feeds[0].last_checked_at.is_some());
        assert_eq!(stored.rss_feeds[0].last_error, None);
        let seen = RssSeenStore::load(&dir.0).unwrap();
        assert!(seen.contains(
            "local-feed",
            &hex::encode(sha2::Sha256::digest(b"manifest"))
        ));
        server.await.unwrap();
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn batch_import_caps_distinct_urls_and_bounds_error_samples() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let mut lines = "file:///C:/private.txt\n".repeat(12);
        for index in 0..503 {
            writeln!(lines, "http://127.0.0.1/file-{index}.txt").unwrap();
        }
        let result = state
            .add_batch_urls(&lines, None, None, true)
            .await
            .unwrap();
        assert_eq!(result.added, 500);
        assert_eq!(result.skipped, 15);
        assert_eq!(result.errors.len(), 10);
        let transfers = state.snapshot().await.transfers;
        assert_eq!(transfers.len(), 500);
        assert!(transfers
            .iter()
            .all(|row| row.status == TransferStatus::Paused));
        assert!(!transfers.iter().any(|row| row.filename == "file-500.txt"));
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn logs_keep_the_latest_500_sanitized_entries() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        for i in 0..502 {
            state.log_event("info\n", "test\t", format!("{i}\r\n\0{}", "雪".repeat(520)));
        }
        let logs = state.get_logs();
        assert_eq!(logs.len(), 500);
        assert!(logs[0].message.starts_with("2   "));
        let last = logs.last().unwrap();
        assert_eq!(last.level, "info ");
        assert_eq!(last.category, "test ");
        assert!(last.message.starts_with("501   "));
        assert_eq!(last.message.chars().count(), 512);
        assert!(last.message.chars().all(|ch| !ch.is_control()));
        let path = temp.0.join("diagnostics.log");
        assert_eq!(
            state.export_logs(path.to_str().unwrap()).await.unwrap(),
            500
        );
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 500);
        state.clear_logs();
        assert!(state.get_logs().is_empty());
        state.shutdown().await.unwrap();
    }

    fn media_request() -> AddMediaDownloadRequest {
        serde_json::from_value(serde_json::json!({
            "url": "https://example.com/video",
            "embed_subtitles_langs": [],
            "lossless_remux": false,
            "normalize_audio": false
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn grab_batch_rejects_bad_sizes_urls_and_referrers_without_enqueuing() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let url = "https://example.com/item.bin".to_owned();
        for (urls, referrer) in [
            (vec![], None),
            (vec![url.clone(); MAX_BATCH_URLS + 1], None),
            (vec![url.clone(), "http://169.254.169.254/".into()], None),
            (vec![url], Some("file:///invalid-referrer".into())),
        ] {
            assert!(state.grab_batch(urls, referrer).await.is_err());
            assert!(state.engine.snapshots().is_empty());
        }
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn selected_playlist_entries_reject_duplicate_indices() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let mut request = media_request();
        let entry = SelectedPlaylistEntry {
            index: 1,
            title: "Video".into(),
            url: None,
        };
        request.selected_entries = Some(vec![entry.clone(), entry]);
        assert_eq!(
            state.add_media_download(request).await.unwrap_err(),
            "playlist entry indices must be unique"
        );
        assert!(state.media.read().unwrap().is_empty());
        assert!(state.playlists.lock().unwrap().is_empty());
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn playlist_start_error_rolls_back_children_and_coordinator() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let mut request = media_request();
        request.headers.insert("Cookie".into(), "private".into());
        let directory = ensure_safe_directory(&temp.0.join("Downloads")).unwrap();
        let result = state
            .enqueue_playlist(
                request,
                directory,
                PlaylistSelection {
                    entries: Vec::new(),
                    probe: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert!(state.media.read().unwrap().is_empty());
        assert!(state.playlists.lock().unwrap().is_empty());
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_media_can_be_canceled_and_capacity_restarts_the_queue() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        state.settings.write().unwrap().max_concurrent_downloads = 0;
        let mut request = media_request();
        request.url = "http://127.0.0.1:1/video".into();
        let canceled = state.add_media_download(request.clone()).await.unwrap();
        let next = state.add_media_download(request).await.unwrap();
        assert_eq!(
            state.media_job(&next).unwrap().status(),
            TransferStatus::Queued
        );
        state.cancel_transfer(&canceled, false).await.unwrap();
        assert!(state.media_job(&canceled).is_none());
        assert_eq!(state.media_queue.lock().unwrap().len(), 1);

        let mut settings = state.settings();
        settings.max_concurrent_downloads = 1;
        state.save_settings(settings).await.unwrap();
        timeout(Duration::from_secs(5), async {
            while state.media_job(&next).unwrap().status() == TransferStatus::Queued {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("media queue should advance when a slot is available");
        assert!(state.media_queue.lock().unwrap().is_empty());
        state.shutdown().await.unwrap();
    }

    fn sqlite_fixture(path: &Path, sql: &str) -> String {
        use std::ffi::{c_char, c_int, c_void, CStr, CString};
        use std::ptr;

        // These test calls use the same bundled SQLite library linked by the engine.
        extern "C" {
            fn sqlite3_open(path: *const c_char, database: *mut *mut c_void) -> c_int;
            fn sqlite3_exec(
                database: *mut c_void,
                sql: *const c_char,
                callback: unsafe extern "C" fn(
                    *mut c_void,
                    c_int,
                    *mut *mut c_char,
                    *mut *mut c_char,
                ) -> c_int,
                output: *mut c_void,
                error: *mut *mut c_char,
            ) -> c_int;
            fn sqlite3_close(database: *mut c_void) -> c_int;
        }
        unsafe extern "C" fn collect(
            output: *mut c_void,
            columns: c_int,
            values: *mut *mut c_char,
            _: *mut *mut c_char,
        ) -> c_int {
            if columns != 1 || (*values).is_null() {
                return 1;
            }
            let value = CStr::from_ptr(*values).to_string_lossy();
            (*output.cast::<String>()).push_str(&value);
            0
        }
        let path = CString::new(path.to_str().unwrap()).unwrap();
        let sql = CString::new(sql).unwrap();
        let mut database = ptr::null_mut();
        let mut output = String::new();
        let opened = unsafe { sqlite3_open(path.as_ptr(), &mut database) };
        assert_eq!(opened, 0);
        let result = unsafe {
            sqlite3_exec(
                database,
                sql.as_ptr(),
                collect,
                ptr::from_mut(&mut output).cast(),
                ptr::null_mut(),
            )
        };
        assert_eq!(unsafe { sqlite3_close(database) }, 0);
        assert_eq!(result, 0, "SQLite test fixture statement failed");
        output
    }

    #[tokio::test]
    async fn dismissing_completed_http_scrubs_legacy_sqlite_credentials() {
        let temp = TestDir::new();
        let state = start_test_daemon(&temp.0).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let id = state
            .add_http_download(AddHttpDownloadRequest {
                url: format!("http://{}/file.bin", listener.local_addr().unwrap()),
                filename: None,
                save_dir: None,
                max_connections: None,
                queue_id: None,
                headers: HashMap::new(),
                referrer: None,
                expected_sha256: None,
                expected_size_bytes: None,
                start_paused: false,
                overwrite_existing: false,
                ..AddHttpDownloadRequest::default()
            })
            .await
            .unwrap();
        state.shutdown().await.unwrap();
        let db = temp.0.join("slipstream.sqlite");
        sqlite_fixture(
            &db,
            "UPDATE transfers SET record = json_set(record,
            '$.status', 'completed',
            '$.headers', json_object('Authorization', 'private'),
            '$.referrer', 'https://app.example.com/referrer',
            '$.source_url', 'https://user:pass@example.com/file.bin?token=private#fragment')",
        );

        let state = start_test_daemon(&temp.0).await;
        assert_eq!(
            state.http_snapshot(&id).unwrap().status,
            TransferStatus::Completed
        );
        state.cancel_transfer(&id, false).await.unwrap();
        assert!(state.http_snapshot(&id).is_none());
        state.shutdown().await.unwrap();

        let record: serde_json::Value =
            serde_json::from_str(&sqlite_fixture(&db, "SELECT record FROM transfers")).unwrap();
        assert_eq!(record["status"], "completed");
        assert_eq!(record["headers"], serde_json::json!({}));
        assert!(record["referrer"].is_null());
        assert_eq!(record["source_url"], "https://example.com/file.bin");
    }

    #[test]
    fn probe_cache_expires_entries_before_evicting_the_oldest() {
        let url = "https://example.com/video";
        let probe = Arc::new(
            slipstream_media::parse_ytdlp_probe_json(r#"{"title":"Video"}"#, url).unwrap(),
        );
        let past = Instant::now();
        let key = |url: &str, playlist| ProbeKey::new(url, playlist, &HashMap::new()).unwrap();
        let expired = key(url, true);
        let oldest = key(url, false);
        let mut cache = ProbeCache::default();
        cache
            .entries
            .insert(expired.clone(), (past - PROBE_CACHE_TTL, probe.clone()));
        cache.entries.insert(
            oldest.clone(),
            (past - Duration::from_secs(1), probe.clone()),
        );
        for i in 0..PROBE_CACHE_CAPACITY - 2 {
            cache.insert(key(&format!("{url}/{i}"), false), probe.clone());
        }
        assert!(cache.get(&expired).is_none());
        assert!(cache.get(&oldest).is_some());
        cache.insert(key(&format!("{url}/new"), false), probe.clone());
        assert_eq!(cache.entries.len(), PROBE_CACHE_CAPACITY);
        assert!(cache.get(&oldest).is_some());
        cache.insert(expired.clone(), probe);
        assert_eq!(cache.entries.len(), PROBE_CACHE_CAPACITY);
        assert!(cache.get(&oldest).is_none());
        assert!(cache.get(&expired).is_some());
    }

    #[test]
    fn browser_status_source_url_strips_private_details() {
        assert_eq!(
            browser_source_url("https://user:pass@example.com:8443/a%20b?token=private#fragment"),
            "https://example.com:8443/a%20b"
        );
        assert_eq!(
            browser_source_url("magnet:?tr=https://example.com"),
            "magnet:"
        );
        assert!(browser_source_url(r"C:\private\example.torrent").is_empty());
    }

    #[test]
    fn native_status_budget_compacts_large_batches_and_keeps_live_transfers() {
        let job = MediaJob::new(media_request(), PathBuf::new(), None).unwrap();
        let mut snapshot = job.snapshot();
        snapshot.id = "oldest-live".into();
        snapshot.status = TransferStatus::Active;
        snapshot.error_message = Some("é".repeat(2048));
        snapshot.completion_notice = Some("界".repeat(2048));
        snapshot
            .segments
            .push(slipstream_protocol::SegmentSnapshot {
                index: 0,
                start_byte: 0,
                end_byte: 99,
                downloaded_bytes: 0,
                speed_bps: 0,
                state: slipstream_protocol::SegmentState::Active,
            });
        let mut transfers = vec![snapshot.clone()];
        snapshot.status = TransferStatus::Completed;
        for id in 0..2000 {
            snapshot.id = format!("terminal-{id}");
            transfers.push(snapshot.clone());
        }
        let response = bounded_native_status(NativeBridgeResponse::Status {
            batch: TelemetryBatch {
                aggregate_down_bps: 50,
                aggregate_up_bps: 0,
                active_count: 1,
                active_threads: 1,
                speed_limit_bps: None,
                transfers,
                timestamp: chrono::Utc::now(),
            },
            theme: slipstream_protocol::ThemeMode::System,
            accent: slipstream_protocol::AccentMode::Blue,
        })
        .unwrap();
        assert!(
            serde_json::to_vec(&response).unwrap().len() < MAX_NATIVE_MESSAGE_BYTES - 16 * 1024
        );
        let NativeBridgeResponse::Status { batch, .. } = response else {
            panic!("expected status")
        };
        assert_eq!(batch.aggregate_down_bps, 50);
        assert_eq!(batch.transfers.first().unwrap().id, "oldest-live");
        assert_eq!(batch.transfers.last().unwrap().id, "terminal-1999");
        assert!(batch.transfers.len() < 2001);
        let error = format!("{}...", "é".repeat(126));
        let notice = format!("{}...", "界".repeat(84));
        assert!(batch.transfers.iter().all(|item| item.segments.is_empty()
            && item.error_message.as_deref() == Some(error.as_str())
            && item.completion_notice.as_deref() == Some(notice.as_str())));
    }

    #[test]
    fn probe_cache_keys_match_header_names_values_and_playlist_mode() {
        let url = "https://example.com/video";
        let key = |mode, name: &str, value: &str| {
            ProbeKey::new(url, mode, &HashMap::from([(name.into(), value.into())])).unwrap()
        };
        let a = key(false, "user-agent", "fixture");
        assert!(a == key(false, "User-Agent", "fixture"));
        assert!(a != key(false, "User-Agent", "other"));
        assert!(a != key(true, "User-Agent", "fixture"));
        assert!(a != ProbeKey::new(url, false, &HashMap::new()).unwrap());
    }

    #[test]
    fn native_probe_headers_keep_valid_values_and_drop_conflicts() {
        let url = "https://example.com/video";
        let headers = [
            ("referer", "https://example.com/watch"),
            ("Referer", "https://example.com/watch"),
            ("Origin", "https://example.com/invalid-path"),
            ("Accept-Language", "en-US\r\nX-Invalid: true"),
            ("User-Agent", "first"),
            ("user-agent", "conflicting"),
            ("Cookie", "session=private"),
        ]
        .map(|(name, value)| (name.into(), value.into()))
        .into();
        let filtered = ProbeKey::from_probe_headers(url, false, &headers);
        assert_eq!(
            filtered.headers,
            vec![("Referer".into(), "https://example.com/watch".into())]
        );
        let picker_headers = filter_media_headers(&headers);
        let download_key = ProbeKey::new(url, false, &picker_headers).unwrap();
        assert!(filtered == download_key);
    }

    #[test]
    fn validate_media_url_blocks_private_and_respects_allow_loopback() {
        for loopback in [
            "http://127.0.0.1:8080/video",
            "http://[::1]:8080/video",
            "http://2130706433/video",
            "http://localhost/video",
            "http://localhost./video",
            "http://sub.localhost/video",
            "http://api.localhost./video",
        ] {
            assert!(validate_media_url(loopback, false).is_err(), "{loopback}");
            assert!(validate_media_url(loopback, true).is_ok(), "{loopback}");
        }
        for blocked_always in [
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/video",
            "http://192.168.1.1/video",
            "http://10.0.0.1/video",
            "http://0.0.0.0/video",
            "http://[::ffff:127.0.0.1]/video",
            "http://[fe80::1]/video",
            "http://[fc00::1]/video",
        ] {
            assert!(
                validate_media_url(blocked_always, false).is_err(),
                "{blocked_always}"
            );
            assert!(
                validate_media_url(blocked_always, true).is_err(),
                "{blocked_always}"
            );
        }
    }

    #[test]
    fn validate_media_url_rejects_usernames_and_passwords() {
        for url in [
            "https://alice@example.com/video",
            "https://alice:secret@example.com/video",
            "https://:secret@example.com/video",
        ] {
            assert!(validate_media_url(url, false).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn media_host_lookup_enforces_address_policy() {
        let loopback = Url::parse("http://localhost/video").unwrap();
        assert_eq!(
            ensure_public_media_host(&loopback, false)
                .await
                .unwrap_err(),
            "disallowed private or loopback IP address"
        );
        ensure_public_media_host(&loopback, true).await.unwrap();
        let private = Url::parse("https://192.168.1.1/video").unwrap();
        assert!(ensure_public_media_host(&private, false).await.is_err());
    }

    #[test]
    fn classifies_playlist_urls_without_treating_watch_urls_as_playlists() {
        for (path, expected) in [
            ("/watch?v=1&list=PL1", false),
            ("/watch?list=PL1", true),
            ("/playlist", true),
            ("/channel/playlist/?list=PL1", true),
            ("/playlist-export", false),
            ("/watch?list=", false),
            ("/video#list=PL1", false),
        ] {
            let url = validate_media_url(&format!("https://example.com{path}"), false).unwrap();
            assert_eq!(is_pure_playlist_url(&url), expected, "{path}");
        }
    }

    #[test]
    fn youtube_video_paths_with_list_can_be_probed_as_single_video() {
        for (url, pure) in [
            ("https://youtu.be/jNQXAC9IVRw?list=PL123", false),
            ("https://youtube.com/shorts/jNQXAC9IVRw?list=PL123", false),
            ("https://www.youtube.com/live/jNQXAC9IVRw?list=PL123", false),
            ("https://m.youtube.com/embed/jNQXAC9IVRw?list=PL123", false),
            ("https://youtu.be/?list=PL123", true),
            ("https://youtube.com/shorts/?list=PL123", true),
            ("https://youtube.com/playlist?v=video&list=PL123", false),
            ("https://notyoutube.com/shorts/video?list=PL123", true),
        ] {
            let url = validate_media_url(url, false).unwrap();
            assert_eq!(is_pure_playlist_url(&url), pure, "{url}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scan_queues_preserve_pending_transfers_when_defender_channel_is_full() {
        let temp = TestDir::new();
        let downloads = temp.0.join("Downloads");
        std::fs::create_dir(&downloads).unwrap();
        let settings = AppSettings {
            default_download_dir: downloads.to_string_lossy().into_owned(),
            auto_defender_scan: true,
            torrent_network: TorrentNetworkSettings {
                listen_port_start: 0,
                listen_port_end: 0,
                enable_dht: false,
                ..Default::default()
            },
            ..AppSettings::default()
        };
        std::fs::write(
            temp.0.join(SETTINGS_FILE),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/item.bin", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    while !buf.ends_with(b"\r\n\r\n") {
                        let Ok(byte) = stream.read_u8().await else {
                            return;
                        };
                        buf.push(byte);
                    }
                    let req = String::from_utf8_lossy(&buf);
                    if req.starts_with("HEAD ") {
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                            .await;
                    } else if let Some(range) = req.lines().find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.eq_ignore_ascii_case("range").then_some(v.trim())
                    }) {
                        let (a, b) = range
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        let a: usize = a.parse().unwrap();
                        let b: usize = b.parse().unwrap();
                        let hdr = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b - a + 1);
                        let _ = stream.write_all(hdr.as_bytes()).await;
                        let _ = stream.write_all(&b"test"[a..=b]).await;
                    } else {
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest")
                            .await;
                    }
                });
            }
        });

        let reg = IsolatedReg::new();
        let pipe = format!(r"\\.\pipe\SlipStream.DaemonScanTest.{}", Uuid::new_v4());
        let state = AppState::start(
            temp.0.clone(),
            Arc::new(NullUi),
            DaemonOptions {
                pipe_name: Some(pipe),
                allow_loopback: true,
                registry_base_key: Some(reg.0.clone()),
                resource_dir: None,
                allow_power_actions: false,
                torrent: TorrentManagerConfig {
                    enable_dht: false,
                    enable_trackers: false,
                    listen_port_range: None,
                    ..TorrentManagerConfig::default()
                },
            },
        )
        .await
        .unwrap();

        let mut alerts = state.subscribe_completion_alerts();
        let http_id = state
            .add_http_download(AddHttpDownloadRequest {
                url,
                filename: Some("http.bin".into()),
                save_dir: None,
                max_connections: Some(1),
                queue_id: None,
                headers: HashMap::new(),
                referrer: None,
                expected_sha256: None,
                expected_size_bytes: None,
                start_paused: false,
                overwrite_existing: false,
                ..AddHttpDownloadRequest::default()
            })
            .await
            .unwrap();
        let http_snap = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(snap) = state.http_snapshot(&http_id) {
                    if snap.status == TransferStatus::Completed {
                        return snap;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let alert = timeout(Duration::from_secs(10), alerts.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alert.transfer_id, http_id);
        assert_eq!(alert.kind, TransferKind::Http);

        std::fs::write(downloads.join("torrent.bin"), b"test").unwrap();
        let mut manifest =
            b"d4:infod6:lengthi4e4:name11:torrent.bin12:piece lengthi16384e6:pieces20:".to_vec();
        manifest.extend([
            0xa9, 0x4a, 0x8f, 0xe5, 0xcc, 0xb1, 0x9b, 0xa6, 0x1c, 0x4c, 0x08, 0x73, 0xd3, 0x91,
            0xe9, 0x87, 0x98, 0x2f, 0xbb, 0xd3,
        ]);
        manifest.extend(b"ee");
        let torrent_id = {
            let torrent = state.torrent.read().await;
            let id = torrent
                .manager
                .add_torrent_bytes(
                    &manifest,
                    TorrentAddOptions {
                        overwrite: true,
                        ..TorrentAddOptions::default()
                    },
                )
                .await
                .unwrap();
            torrent
                .manager
                .wait_until_completed(&id, Duration::from_secs(10))
                .await
                .unwrap();
            id
        };
        let zone_stream = downloads.join("torrent.bin:Zone.Identifier");
        let locked_stream = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&zone_stream)
            .unwrap();
        state
            .torrent_completions
            .lock()
            .unwrap()
            .insert(torrent_id.clone());

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send((downloads.clone(), vec!["blocker".into()]))
            .unwrap();

        let mut seen_http = HashSet::new();
        let mut pending_http = HashMap::new();
        scan_http_completion(&state, &tx, &mut seen_http, &mut pending_http, http_snap);
        assert!(!seen_http.contains(&http_id));
        assert!(pending_http.contains_key(&http_id));

        scan_torrent_completions(&state, &tx).await;
        let alert = alerts.try_recv().unwrap();
        assert_eq!(alert.transfer_id, torrent_id);
        assert_eq!(alert.kind, TransferKind::Torrent);
        assert!(state
            .torrent_completions
            .lock()
            .unwrap()
            .contains(&torrent_id));
        assert!(state
            .torrent_completed_files
            .lock()
            .unwrap()
            .get(&torrent_id)
            .is_some_and(|files| files.marked.is_empty()));
        drop(locked_stream);

        scan_torrent_completions(&state, &tx).await;
        assert!(alerts.try_recv().is_err());
        assert!(state
            .torrent_completions
            .lock()
            .unwrap()
            .contains(&torrent_id));
        assert!(state
            .torrent_completed_files
            .lock()
            .unwrap()
            .get(&torrent_id)
            .is_some_and(|files| files.scanned.is_empty()));
        assert_eq!(
            std::fs::read(zone_stream).unwrap(),
            b"[ZoneTransfer]\r\nZoneId=3\r\n"
        );

        assert_eq!(rx.recv().await.unwrap().1, vec!["blocker".to_owned()]);

        retry_http_completions(&state, &tx, &mut seen_http, &mut pending_http);
        assert_eq!(rx.try_recv().unwrap().1, vec!["http.bin".to_owned()]);
        assert!(seen_http.contains(&http_id));
        assert!(pending_http.is_empty());

        scan_torrent_completions(&state, &tx).await;
        assert_eq!(rx.try_recv().unwrap().1, vec!["torrent.bin".to_owned()]);
        assert!(!state
            .torrent_completions
            .lock()
            .unwrap()
            .contains(&torrent_id));
        assert!(state
            .torrent_completed_files
            .lock()
            .unwrap()
            .get(&torrent_id)
            .is_some_and(|files| files.scanned.contains("torrent.bin")));

        server.abort();
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn phase4_vault_logins_cookie_scope_and_backup_redaction() {
        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;

        let mut settings = state.settings();
        settings.proxy = slipstream_protocol::ProxySettings {
            mode: slipstream_protocol::ProxyMode::Manual,
            url: Some("http://127.0.0.1:8888".into()),
            username: Some("proxy-user".into()),
            has_password: false,
            bypass_hosts: vec!["bypass.local".into()],
            apply_to_media: true,
        };
        settings.cookie_passthrough = slipstream_protocol::CookiePassThroughSettings {
            enabled: true,
            browser: "chrome".into(),
            allowed_hosts: vec!["video.example.com".into()],
        };
        state.save_settings(settings).await.unwrap();

        state
            .set_proxy_password(Some("top-secret-proxy".into()))
            .await
            .unwrap();
        assert!(state.settings().proxy.has_password);

        state
            .upsert_site_login(
                slipstream_protocol::SiteLoginEntry {
                    id: "cdn-login".into(),
                    host_pattern: "cdn.example.com".into(),
                    username: "alice".into(),
                    has_password: false,
                    auth_scheme: slipstream_protocol::SiteAuthScheme::Basic,
                    enabled: true,
                },
                Some("site-secret".into()),
            )
            .await
            .unwrap();
        assert!(state
            .settings()
            .site_logins
            .iter()
            .any(|item| item.id == "cdn-login" && item.has_password));

        let current_settings = state.settings();
        let empty_vault = SecretVault::default();

        let (proxy_arg, cookies_arg) = media_network_options(
            &current_settings,
            &empty_vault,
            &url::Url::parse("https://video.example.com/watch").unwrap(),
        )
        .unwrap();
        assert!(proxy_arg
            .as_deref()
            .is_some_and(|value| value.contains("proxy-user")));
        assert_eq!(cookies_arg.as_deref(), Some("chrome"));

        let (other_proxy, other_cookies) = media_network_options(
            &current_settings,
            &empty_vault,
            &url::Url::parse("https://other.example.net/watch").unwrap(),
        )
        .unwrap();
        assert!(other_proxy.is_some());
        assert_eq!(other_cookies, None);

        let (bypass_proxy, _) = media_network_options(
            &current_settings,
            &empty_vault,
            &url::Url::parse("http://bypass.local/clip").unwrap(),
        )
        .unwrap();
        assert_eq!(bypass_proxy, None);

        let backup_path = dir.0.join("phase4-backup.json");
        state.export_settings_backup(&backup_path).await.unwrap();
        let backup_json = std::fs::read_to_string(&backup_path).unwrap();
        assert!(!backup_json.contains("top-secret-proxy"));
        assert!(!backup_json.contains("site-secret"));
        let bundle: SettingsBackupBundle = serde_json::from_str(&backup_json).unwrap();
        assert!(!bundle.settings.proxy.has_password);
        assert!(!bundle.settings.cookie_passthrough.enabled);
        assert!(bundle
            .settings
            .site_logins
            .iter()
            .all(|item| !item.has_password));

        let fresh_dir = TestDir::new();
        let fresh_state = start_test_daemon(&fresh_dir.0).await;
        let imported = fresh_state
            .import_settings_backup(&backup_path)
            .await
            .unwrap();
        assert!(!imported.proxy.has_password);
        assert!(!imported.cookie_passthrough.enabled);
        assert!(imported.site_logins.iter().all(|item| !item.has_password));
        fresh_state.shutdown().await.unwrap();

        state.set_proxy_password(None).await.unwrap();
        assert!(!state.settings().proxy.has_password);

        state.delete_site_login("cdn-login").await.unwrap();
        assert!(state
            .settings()
            .site_logins
            .iter()
            .all(|item| item.id != "cdn-login"));

        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn phase4_grabber_blocks_loopback_and_metalink_round_trips() {
        let payload = b"metalink-payload";
        let digest = sha2::Sha256::digest(payload);
        let sha_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(payload).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let blocked_dir = TestDir::new();
        let blocked_state = AppState::start(
            blocked_dir.0.clone(),
            Arc::new(NullUi),
            DaemonOptions {
                pipe_name: Some(format!(
                    r"\\.\pipe\SlipStream.DaemonUnitTest.{}",
                    Uuid::new_v4()
                )),
                allow_loopback: false,
                torrent: TorrentManagerConfig {
                    enable_dht: false,
                    enable_trackers: false,
                    listen_port_range: None,
                    ..TorrentManagerConfig::default()
                },
                ..DaemonOptions::default()
            },
        )
        .await
        .unwrap();

        let grab_err = blocked_state
            .grab_site_urls(slipstream_protocol::SiteGrabRequest {
                url: format!("http://127.0.0.1:{}/index.html", addr.port()),
                max_depth: 1,
                same_origin_only: true,
                extensions: Vec::new(),
                include_pattern: String::new(),
                exclude_pattern: String::new(),
            })
            .await;
        assert!(grab_err.is_err());
        blocked_state.shutdown().await.unwrap();

        let dir = TestDir::new();
        let state = start_test_daemon(&dir.0).await;
        let meta_path = dir.0.join("sample.meta4");
        let meta_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="payload.bin">
    <size>{}</size>
    <hash type="sha-256">{sha_hex}</hash>
    <url priority="1">http://127.0.0.1:{}/payload.bin</url>
  </file>
</metalink>"#,
            payload.len(),
            addr.port()
        );
        std::fs::write(&meta_path, meta_xml).unwrap();

        let entries = state.parse_metalink_file(&meta_path).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "payload.bin");
        assert_eq!(entries[0].sha256.as_deref(), Some(sha_hex.as_str()));
        assert_eq!(entries[0].mirrors.len(), 1);

        let ids = state
            .enqueue_metalink_entries(entries, None, None)
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        assert!(state
            .snapshot()
            .await
            .transfers
            .iter()
            .any(|item| item.id == ids[0] && item.filename == "payload.bin"));

        server.abort();
        state.shutdown().await.unwrap();
    }
}
