use crate::{
    media_jobs::{MediaJob, ScanRequest},
    paths::{
        build_defender_scan_args, decode_sha256, delete_files, ensure_safe_directory, hash_file,
        local_drive_path, reveal_action, RevealAction,
    },
    settings::{normalize, read_json, torrent_rate, write_json_atomically, SETTINGS_FILE},
    tools::{defender_executable, explorer_executable, toolchain_status, ToolchainStatus},
};
use serde::{Deserialize, Serialize};
use slipstream_engine::{DownloadEngine, EngineConfig, EngineEvent};
use slipstream_media::{probe_url, MediaToolchain, ProbeOptions};
use slipstream_native_host::{
    pipe::{error_response, PipeRequestHandler, PipeServer},
    registry::DEFAULT_BASE_KEY,
    security::default_pipe_name,
};
use slipstream_protocol::{
    validate_http_url, validate_magnet_uri, AddHttpDownloadRequest, AddMediaDownloadRequest,
    AddTorrentRequest, AppSettings, MediaProbeResult, NativeBridgeRequest, NativeBridgeResponse,
    RefreshDownloadUrlRequest, TelemetryBatch, TorrentFileEntry, TransferSnapshot, TransferStatus,
};
use slipstream_torrent::{TorrentManager, TorrentManagerConfig};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    future::{pending, Future},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command as StdCommand, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{
        broadcast, mpsc, watch, Mutex as AsyncMutex, MutexGuard, Notify, RwLock as AsyncRwLock,
    },
    task::JoinHandle,
    time::{timeout, Instant},
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

type Result<T> = std::result::Result<T, String>;
const DISMISSED_FILE: &str = "dismissed-http.json";
const TELEMETRY_INTERVAL: Duration = Duration::from_millis(50);
const DEFENDER_SCAN_QUEUE_CAPACITY: usize = 128;
const DEFENDER_SCAN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenPickerEvent {
    pub url: Option<String>,
}

pub trait UiBridge: Send + Sync + 'static {
    fn show_main_window(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn emit_telemetry(&self, batch: &TelemetryBatch) -> Result<()>;
    fn emit_open_picker(&self, event: &OpenPickerEvent) -> Result<()>;
    fn has_main_window(&self) -> bool;
    fn trigger_ready_check(&self) {}
    fn request_quit(&self);
}

#[derive(Default)]
pub struct DaemonOptions {
    pub pipe_name: Option<String>,
    pub allow_loopback: bool,
    pub registry_base_key: Option<String>,
    pub torrent: TorrentManagerConfig,
}

struct TorrentSession {
    manager: TorrentManager,
    base: PathBuf,
}

#[derive(Default)]
struct Picker {
    ready: bool,
    pending: VecDeque<OpenPickerEvent>,
}

pub struct AppState {
    data_dir: PathBuf,
    registry_base_key: String,
    ui: Arc<dyn UiBridge>,
    engine: DownloadEngine,
    torrent: AsyncRwLock<TorrentSession>,
    torrent_config: TorrentManagerConfig,
    media: RwLock<HashMap<String, Arc<MediaJob>>>,
    settings: RwLock<AppSettings>,
    dismissed_http: RwLock<HashSet<String>>,
    verified: RwLock<HashMap<String, bool>>,
    operations: AsyncMutex<()>,
    picker: Mutex<Picker>,
    window_changed: Notify,
    torrent_changed: Notify,
    torrent_completions: Mutex<HashSet<String>>,
    torrent_scanned_files: Mutex<HashMap<String, HashSet<String>>>,
    scans: mpsc::Sender<ScanRequest>,
    stop: watch::Sender<bool>,
    tasks: AsyncMutex<Option<Vec<JoinHandle<()>>>>,
    quitting: AtomicBool,
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

impl AppState {
    pub async fn start(
        data_dir: PathBuf,
        ui: Arc<dyn UiBridge>,
        options: DaemonOptions,
    ) -> Result<Arc<Self>> {
        let data_dir = std::path::absolute(data_dir).map_err(message)?;
        std::fs::create_dir_all(&data_dir).map_err(message)?;
        let mut settings: AppSettings =
            read_json(&data_dir.join(SETTINGS_FILE)).map_err(message)?;
        normalize(&mut settings, &data_dir).map_err(message)?;
        let dismissed_http = read_json(&data_dir.join(DISMISSED_FILE)).map_err(message)?;
        let pipe_name = options
            .pipe_name
            .map_or_else(default_pipe_name, Ok)
            .map_err(message)?;
        let server =
            PipeServer::bind(&pipe_name, &data_dir.join("session.token")).map_err(message)?;
        let torrent = TorrentSession::new(&options.torrent, &settings).await?;
        let engine = match DownloadEngine::new(EngineConfig {
            db_path: data_dir.join("slipstream.sqlite"),
            default_download_dir: PathBuf::from(&settings.default_download_dir),
            default_connections: settings.default_connections,
            max_concurrent_downloads: usize::from(settings.max_concurrent_downloads),
            global_speed_limit_bps: settings.global_speed_limit_bps,
            allow_loopback: options.allow_loopback,
            ..EngineConfig::default()
        })
        .await
        {
            Ok(engine) => engine,
            Err(error) => {
                torrent.manager.shutdown().await;
                return Err(message(error));
            }
        };
        let http_events = engine.subscribe_events();
        let scanned_http = engine
            .snapshots()
            .into_iter()
            .filter(|snapshot| snapshot.status == TransferStatus::Completed)
            .map(|snapshot| snapshot.id)
            .collect();
        let (scans, scan_requests) = mpsc::channel(DEFENDER_SCAN_QUEUE_CAPACITY);
        let (stop, _) = watch::channel(false);
        let ready = ui.has_main_window();
        let registry_base_key = options
            .registry_base_key
            .unwrap_or_else(|| DEFAULT_BASE_KEY.to_owned());
        let state = Arc::new(Self {
            data_dir,
            registry_base_key,
            ui,
            engine,
            torrent: AsyncRwLock::new(torrent),
            torrent_config: options.torrent,
            media: RwLock::default(),
            settings: RwLock::new(settings),
            dismissed_http: RwLock::new(dismissed_http),
            verified: RwLock::default(),
            operations: AsyncMutex::new(()),
            picker: Mutex::new(Picker {
                ready,
                ..Picker::default()
            }),
            window_changed: Notify::new(),
            torrent_changed: Notify::new(),
            torrent_completions: Mutex::default(),
            torrent_scanned_files: Mutex::default(),
            scans,
            stop,
            tasks: AsyncMutex::new(Some(Vec::new())),
            quitting: AtomicBool::new(false),
        });
        let runtime = Arc::downgrade(&state);
        let stopped = state.stop.subscribe();
        let tasks = vec![
            tokio::spawn(async move {
                if let Err(error) = server
                    .run(NativeHandler(runtime), stopped_signal(stopped))
                    .await
                {
                    tracing::error!(%error, "named pipe server stopped");
                }
            }),
            tokio::spawn(emit_telemetry(state.clone(), state.stop.subscribe())),
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
        ];
        *state.tasks.lock().await = Some(tasks);
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

    pub fn settings(&self) -> AppSettings {
        self.settings.read().unwrap().clone()
    }

    pub async fn save_settings(&self, mut settings: AppSettings) -> Result<()> {
        let _operation = self.operation().await?;
        normalize(&mut settings, &self.data_dir).map_err(message)?;
        self.apply_settings(settings).await
    }

    pub async fn set_speed_limit(&self, limit_bps: Option<u64>) -> Result<()> {
        let _operation = self.operation().await?;
        let mut settings = self.settings();
        settings.global_speed_limit_bps = limit_bps.filter(|&rate| rate != 0);
        self.apply_settings(settings).await
    }

    async fn apply_settings(&self, settings: AppSettings) -> Result<()> {
        let mut torrent = self.torrent.write().await;
        let new_base = Path::new(&settings.default_download_dir);
        let replacement = if torrent.base != new_base && torrent.manager.list_snapshots().is_empty()
        {
            Some(TorrentSession::new(&self.torrent_config, &settings).await?)
        } else {
            None
        };
        if let Err(error) = write_json_atomically(&self.data_dir.join(SETTINGS_FILE), &settings) {
            if let Some(session) = replacement {
                session.manager.shutdown().await;
            }
            return Err(message(error));
        }
        self.engine
            .set_global_speed_limit(settings.global_speed_limit_bps)
            .await
            .map_err(message)?;
        self.engine
            .set_max_concurrent_downloads(usize::from(settings.max_concurrent_downloads))
            .await
            .map_err(message)?;
        if let Some(replacement) = replacement {
            let previous = std::mem::replace(&mut *torrent, replacement);
            previous.manager.shutdown().await;
        } else {
            torrent
                .manager
                .set_global_rate_limits(
                    torrent_rate(settings.global_speed_limit_bps),
                    self.torrent_config.global_upload_limit_bps,
                )
                .map_err(message)?;
        }
        *self.settings.write().unwrap() = settings;
        Ok(())
    }

    pub async fn snapshot(&self) -> TelemetryBatch {
        let torrent = self.torrent.read().await;
        let mut batch = self.engine.telemetry_batch();
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
        for transfer in &mut batch.transfers {
            if let Some(result) = verified.get(&transfer.id) {
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
        self.enqueue_http(request, &self.settings()).await
    }

    async fn enqueue_http(
        &self,
        mut request: AddHttpDownloadRequest,
        settings: &AppSettings,
    ) -> Result<String> {
        let dir_raw = request
            .save_dir
            .take()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| settings.default_download_dir.clone());
        let validated_dir = tokio::task::spawn_blocking(move || {
            let canonical = ensure_safe_directory(Path::new(&dir_raw))?;
            local_drive_path(&canonical)
        })
        .await
        .map_err(message)?
        .map_err(message)?;
        request.save_dir = Some(
            validated_dir
                .into_os_string()
                .into_string()
                .map_err(|_| "non-Unicode save directory".to_owned())?,
        );
        request
            .max_connections
            .get_or_insert(settings.default_connections);
        self.engine
            .add_http_download(request)
            .await
            .map_err(message)
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

    pub async fn probe_media(&self, url: &str) -> Result<MediaProbeResult> {
        let url = validate_http_url(url).map_err(message)?;
        let tools = MediaToolchain::discover(&self.settings())
            .await
            .map_err(message)?;
        probe_url(
            &tools,
            url.as_str(),
            ProbeOptions {
                playlist: url.query_pairs().any(|(key, _)| key == "list"),
            },
        )
        .await
        .map_err(message)
    }

    pub async fn add_media_download(&self, request: AddMediaDownloadRequest) -> Result<String> {
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
        let job = Arc::new(MediaJob::new(request, directory)?);
        let id = job.snapshot().id;
        job.start((&settings).into(), self.scans.clone()).await?;
        self.media.write().unwrap().insert(id.clone(), job);
        Ok(id)
    }

    pub async fn add_torrent(&self, mut request: AddTorrentRequest) -> Result<String> {
        let _operation = self.operation().await?;
        let settings = self.settings();
        {
            let mut torrent = self.torrent.write().await;
            if torrent.base != Path::new(&settings.default_download_dir)
                && torrent.manager.list_snapshots().is_empty()
            {
                let replacement = TorrentSession::new(&self.torrent_config, &settings).await?;
                let previous = std::mem::replace(&mut *torrent, replacement);
                previous.manager.shutdown().await;
            }
        }
        let torrent = self.torrent.read().await;
        let requested = request
            .save_dir
            .as_deref()
            .unwrap_or(&settings.default_download_dir);
        let requested = Path::new(requested);
        let matches_base = requested == torrent.base
            || (requested.is_absolute()
                && std::fs::canonicalize(requested)
                    .is_ok_and(|path| path == torrent.canonical_base()));
        request.save_dir = if matches_base {
            None
        } else {
            Some(requested.to_string_lossy().into_owned())
        };
        let id = torrent
            .manager
            .add_torrent(request)
            .await
            .map_err(message)?;
        self.torrent_completions.lock().unwrap().insert(id.clone());
        self.torrent_changed.notify_one();
        Ok(id)
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
        self.torrent
            .read()
            .await
            .manager
            .set_selected_files(id, &indices)
            .await
            .map_err(message)?;
        self.verified.write().unwrap().remove(id);
        self.torrent_completions.lock().unwrap().insert(id.into());
        self.torrent_changed.notify_one();
        Ok(())
    }

    pub async fn pause_transfer(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        if self.http_snapshot(id).is_some() {
            self.engine.pause_transfer(id).await.map_err(message)
        } else if let Some(job) = self.media_job(id) {
            job.stop(TransferStatus::Paused).await
        } else {
            self.torrent
                .read()
                .await
                .manager
                .pause(id)
                .await
                .map_err(message)
        }
    }

    pub async fn resume_transfer(&self, id: &str) -> Result<()> {
        let _operation = self.operation().await?;
        if self.http_snapshot(id).is_some() {
            self.engine.resume_transfer(id).await.map_err(message)?;
        } else if let Some(job) = self.media_job(id) {
            job.start((&self.settings()).into(), self.scans.clone())
                .await?;
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

    pub async fn change_all(&self, pause: bool) {
        for snapshot in self.snapshot().await.transfers {
            let result = if pause
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
            if remove_files {
                let mut paths = vec![format!(".{id}.slipstream-part")];
                // A failed download can have collided with someone else's file.
                if snapshot.status == TransferStatus::Completed {
                    paths.push(snapshot.filename);
                }
                remove_outputs(snapshot.save_dir.into(), paths).await?;
            }
            let mut dismissed = self.dismissed_http.read().unwrap().clone();
            dismissed.insert(id.into());
            write_json_atomically(&self.data_dir.join(DISMISSED_FILE), &dismissed)
                .map_err(message)?;
            *self.dismissed_http.write().unwrap() = dismissed;
        } else if let Some(job) = self.media_job(id) {
            job.stop(TransferStatus::Canceled).await?;
            if remove_files {
                remove_outputs(job.snapshot().save_dir.into(), job.outputs()).await?;
            }
            self.media.write().unwrap().remove(id);
        } else {
            self.torrent
                .read()
                .await
                .manager
                .remove(id, remove_files)
                .await
                .map_err(message)?;
            self.torrent_completions.lock().unwrap().remove(id);
            self.torrent_scanned_files.lock().unwrap().remove(id);
        }
        self.verified.write().unwrap().remove(id);
        Ok(())
    }

    async fn transfer_paths(&self, id: &str) -> Result<(PathBuf, Vec<String>)> {
        if let Some(snapshot) = self.http_snapshot(id) {
            let mut paths = Vec::new();
            if snapshot.status == TransferStatus::Completed {
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
        if self.transfer_paths(id).await? != target {
            return Err("transfer files changed during checksum verification".into());
        }
        let matches = digest == expected;
        self.verified.write().unwrap().insert(id.into(), matches);
        Ok(matches)
    }

    pub async fn reveal_in_explorer(&self, id: &str) -> Result<()> {
        let (root, files) = self.transfer_paths(id).await?;
        let action = reveal_action(&root, files.first().map(String::as_str)).map_err(message)?;
        let explorer = explorer_executable().map_err(message)?;
        let mut command = StdCommand::new(explorer);
        match action {
            RevealAction::SelectFile(raw) => {
                command.raw_arg(raw);
            }
            RevealAction::OpenDir(dir) => {
                command.arg(dir);
            }
        }
        command.spawn().map_err(message)?;
        Ok(())
    }

    pub async fn get_toolchain_status(&self) -> ToolchainStatus {
        toolchain_status(&self.settings(), &self.registry_base_key).await
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
        if let Some(url) = url.as_deref() {
            if url
                .trim()
                .split_once(':')
                .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("magnet"))
            {
                validate_magnet_uri(url).map_err(message)?;
            } else {
                validate_http_url(url).map_err(message)?;
            }
        }
        self.show_main_window().await?;
        let needs_ready_check = {
            let mut picker = self.picker.lock().unwrap();
            picker.pending.push_back(OpenPickerEvent { url });
            !picker.ready
        };
        if needs_ready_check {
            self.ui.trigger_ready_check();
        }
        self.flush_picker()
    }

    pub fn window_loading(&self) {
        self.picker.lock().unwrap().ready = false;
        self.window_changed.notify_one();
    }

    pub fn window_ready(&self) -> Result<()> {
        self.picker.lock().unwrap().ready = true;
        self.window_changed.notify_one();
        self.flush_picker()
    }

    fn flush_picker(&self) -> Result<()> {
        let mut picker = self.picker.lock().unwrap();
        if picker.ready && self.ui.has_main_window() {
            while let Some(event) = picker.pending.front() {
                self.ui.emit_open_picker(event)?;
                picker.pending.pop_front();
            }
        }
        Ok(())
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::Acquire)
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
        let jobs: Vec<_> = self.media.read().unwrap().values().cloned().collect();
        for job in jobs {
            if let Err(failure) = job.stop(TransferStatus::Paused).await {
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
        for url in &urls {
            validate_http_url(url).map_err(message)?;
        }
        let _operation = self.operation().await?;
        let settings = self.settings();
        for url in urls {
            self.enqueue_http(
                AddHttpDownloadRequest {
                    url,
                    filename: None,
                    save_dir: None,
                    max_connections: None,
                    queue_id: None,
                    headers: HashMap::new(),
                    referrer: referrer.clone(),
                    expected_sha256: None,
                },
                &settings,
            )
            .await?;
        }
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
                return Ok(NativeBridgeResponse::Status {
                    batch: self.snapshot().await,
                    theme: settings.theme,
                    accent: settings.accent,
                });
            }
            AddHttpDownload { mut request } => {
                if request.save_dir.is_some() {
                    return Err("browser requests cannot override save_dir".into());
                }
                request.save_dir = None;
                return self.add_http_download(request).await.map(created);
            }
            ProbeMedia { url } => {
                return self
                    .probe_media(&url)
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
            OpenDesktopWindow { open_picker_url } => self.open_picker(open_picker_url).await?,
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
        let base = PathBuf::from(&settings.default_download_dir);
        let manager = TorrentManager::new(TorrentManagerConfig {
            default_save_dir: base.clone(),
            global_download_limit_bps: torrent_rate(settings.global_speed_limit_bps),
            ..config.clone()
        })
        .await
        .map_err(message)?;
        Ok(Self { manager, base })
    }

    fn canonical_base(&self) -> PathBuf {
        std::fs::canonicalize(&self.base).unwrap_or_else(|_| self.base.clone())
    }
}

struct NativeHandler(Weak<AppState>);

impl PipeRequestHandler for NativeHandler {
    async fn handle(&self, request: NativeBridgeRequest) -> NativeBridgeResponse {
        let Some(state) = self.0.upgrade() else {
            return error_response("daemon_stopped", "SlipStream has stopped");
        };
        state
            .native_request(request)
            .await
            .unwrap_or_else(|error| error_response("request_failed", &error))
    }
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

async fn emit_telemetry(state: Arc<AppState>, stop: watch::Receiver<bool>) {
    let stopped = stopped_signal(stop);
    tokio::pin!(stopped);
    let mut next = Instant::now();
    loop {
        if !state.ui.has_main_window() {
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
        if state.ui.has_main_window() {
            let batch = state.snapshot().await;
            if state.ui.has_main_window() {
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
    let mut next_torrent_check = Instant::now();
    loop {
        let check_torrents = !state.torrent_completions.lock().unwrap().is_empty();
        let timer = async {
            if check_torrents {
                tokio::time::sleep_until(next_torrent_check).await;
            } else {
                pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            _ = state.torrent_changed.notified() => {}
            _ = timer => {
                scan_torrent_completions(&state).await;
                next_torrent_check = Instant::now() + Duration::from_secs(1);
            }
            event = http.recv() => match event {
                Ok(EngineEvent::StatusChanged { transfer_id, status: TransferStatus::Completed }) => {
                    if let Some(snapshot) = state.http_snapshot(&transfer_id) {
                        scan_http_completion(&state, &mut seen_http, snapshot);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for snapshot in state.engine.snapshots().into_iter().filter(|s| s.status == TransferStatus::Completed) {
                        scan_http_completion(&state, &mut seen_http, snapshot);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                _ => {}
            },
        }
    }
}

fn scan_http_completion(state: &AppState, seen: &mut HashSet<String>, snapshot: TransferSnapshot) {
    if seen.insert(snapshot.id.clone())
        && !state.dismissed_http.read().unwrap().contains(&snapshot.id)
        && state.settings.read().unwrap().auto_defender_scan
    {
        if let Err(error) = state
            .scans
            .try_send((snapshot.save_dir.into(), vec![snapshot.filename]))
        {
            tracing::debug!(%error, "could not enqueue HTTP Defender scan");
        }
    }
}

async fn scan_torrent_completions(state: &AppState) {
    let torrent = state.torrent.read().await;
    let mut pending = state.torrent_completions.lock().unwrap();
    let mut scanned = state.torrent_scanned_files.lock().unwrap();
    pending.retain(|id| {
        let Ok(report) = torrent.manager.stats(id) else {
            scanned.remove(id);
            return false;
        };
        if !report.finished {
            return true;
        }
        if state.settings.read().unwrap().auto_defender_scan {
            if let Ok(files) = torrent.manager.list_files(id) {
                let already = scanned.entry(id.clone()).or_default();
                let paths: Vec<String> = files
                    .into_iter()
                    .filter(|file| file.selected && already.insert(file.relative_path.clone()))
                    .map(|file| file.relative_path)
                    .collect();
                if !paths.is_empty() {
                    if let Err(error) = state
                        .scans
                        .try_send((report.snapshot.save_dir.into(), paths))
                    {
                        tracing::debug!(%error, "could not enqueue torrent Defender scan");
                    }
                }
            }
        }
        false
    });
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
