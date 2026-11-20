mod telemetry;

use crate::{
    error::session_error,
    parse_magnet_uri, parse_torrent_bytes,
    storage::{is_link, reject_existing_links, validate_output, SharedBitmap},
    ConfinedStorageFactory, ParsedMagnet, PieceBitmap, TorrentError, TorrentMetadata,
    MAX_TORRENT_BYTES,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use librqbit::{
    api::TorrentIdOrHash, limits::LimitsConfig, AddTorrent, AddTorrentOptions, AddTorrentResponse,
    ManagedTorrent, Session, SessionOptions,
};
use slipstream_protocol::{
    AddTorrentRequest, PathSecurityError, TorrentFileEntry, TransferSnapshot,
};
use std::{
    collections::{BTreeMap, HashSet},
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

pub use telemetry::TorrentStatsReport;

#[derive(Debug, Clone)]
pub struct TorrentManagerConfig {
    pub default_save_dir: PathBuf,
    pub enable_dht: bool,
    /// `None` disables incoming TCP. Use `Some(0..1)` to choose an available port.
    pub listen_port_range: Option<Range<u16>>,
    pub enable_upnp: bool,
    pub global_download_limit_bps: Option<u64>,
    pub global_upload_limit_bps: Option<u64>,
    pub default_sequential: bool,
    pub enable_trackers: bool,
    pub magnet_metadata_timeout: Duration,
}

impl Default for TorrentManagerConfig {
    fn default() -> Self {
        Self {
            default_save_dir: PathBuf::from("downloads"),
            enable_dht: true,
            listen_port_range: Some(6881..6891),
            enable_upnp: false,
            global_download_limit_bps: None,
            global_upload_limit_bps: None,
            default_sequential: false,
            enable_trackers: true,
            magnet_metadata_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TorrentAddOptions {
    /// Alternate output folder, relative to or strictly within `default_save_dir`.
    pub save_dir: Option<PathBuf>,
    pub selected_file_indices: Option<Vec<usize>>,
    pub sequential: Option<bool>,
    pub start_paused: bool,
    /// Allow opening and verifying files already present in the save directory.
    pub overwrite: bool,
    pub initial_peers: Vec<SocketAddr>,
    pub download_limit_bps: Option<u64>,
    pub upload_limit_bps: Option<u64>,
}

pub struct TorrentManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    config: TorrentManagerConfig,
    session: Arc<Session>,
    lifecycle: AsyncRwLock<()>,
    shutting_down: AtomicBool,
    stop: CancellationToken,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
}

struct Entry {
    id: String,
    source: String,
    save_dir: PathBuf,
    magnet: Option<ParsedMagnet>,
    created_at: DateTime<Utc>,
    operation: AsyncMutex<()>,
    bitmap: SharedBitmap,
    state: Mutex<EntryState>,
}

struct EntryState {
    options: TorrentAddOptions,
    metadata: Option<Arc<TorrentMetadata>>,
    bytes: Option<Bytes>,
    handle: Option<Arc<ManagedTorrent>>,
    resolution: Option<(CancellationToken, JoinHandle<()>)>,
    error: Option<String>,
    bitmap_synced: bool,
    previous_uploads: u64,
    seen_peers: u32,
    updated_at: DateTime<Utc>,
}

impl TorrentManager {
    pub async fn new(mut config: TorrentManagerConfig) -> Result<Self, TorrentError> {
        config.default_save_dir = std::path::absolute(&config.default_save_dir)?;
        validate_output(&config.default_save_dir, &config.default_save_dir)?;
        let ratelimits = limits(
            config.global_download_limit_bps,
            config.global_upload_limit_bps,
        )?;
        let ephemeral = config.listen_port_range.as_ref() == Some(&(0..1));
        if !ephemeral
            && config
                .listen_port_range
                .as_ref()
                .is_some_and(|range| range.is_empty() || range.start == 0)
        {
            return Err(TorrentError::InvalidState(
                "invalid TCP listen range".into(),
            ));
        }
        let stop = CancellationToken::new();
        let session = {
            let mut attempts = if ephemeral { 5 } else { 1 };
            loop {
                let listen_port_range = if ephemeral {
                    // v8 accepts a range, and reports 0 for a direct ephemeral bind.
                    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
                    let port = listener.local_addr()?.port();
                    Some(port..port.saturating_add(1))
                } else {
                    config.listen_port_range.clone()
                };
                let result = Session::new_with_opts(
                    config.default_save_dir.clone(),
                    SessionOptions {
                        disable_dht: !config.enable_dht,
                        disable_dht_persistence: true,
                        listen_port_range,
                        enable_upnp_port_forwarding: config.enable_upnp,
                        ratelimits,
                        cancellation_token: Some(stop.child_token()),
                        ..Default::default()
                    },
                )
                .await;
                attempts -= 1;
                match result {
                    Ok(session) => break session,
                    Err(error) if attempts == 0 => return Err(session_error(error)),
                    Err(_) => continue,
                }
            }
        };
        Ok(Self {
            inner: Arc::new(ManagerInner {
                config,
                session,
                lifecycle: AsyncRwLock::new(()),
                shutting_down: AtomicBool::new(false),
                stop,
                entries: RwLock::new(BTreeMap::new()),
            }),
        })
    }

    pub fn listen_port(&self) -> Option<u16> {
        self.inner.session.tcp_listen_port()
    }

    pub fn inspect_torrent_bytes(
        &self,
        bytes: &[u8],
        save_dir: Option<&Path>,
    ) -> Result<TorrentMetadata, TorrentError> {
        parse_torrent_bytes(bytes, &self.inner.save_dir(save_dir)?)
    }

    pub fn inspect_magnet(&self, uri: &str) -> Result<ParsedMagnet, TorrentError> {
        parse_magnet_uri(uri)
    }

    pub async fn add_torrent(&self, req: AddTorrentRequest) -> Result<String, TorrentError> {
        let options = TorrentAddOptions {
            save_dir: req.save_dir.map(PathBuf::from),
            selected_file_indices: req.selected_file_indices,
            sequential: Some(req.sequential),
            ..Default::default()
        };
        if req
            .source
            .trim_start()
            .split_once(':')
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("magnet"))
        {
            return self.add_magnet(&req.source, options).await;
        }
        self.inner.check_running()?;
        let bytes = read_torrent_file(Path::new(&req.source)).await?;
        self.add_bytes(&bytes, options, req.source).await
    }

    pub async fn add_torrent_bytes(
        &self,
        bytes: &[u8],
        options: TorrentAddOptions,
    ) -> Result<String, TorrentError> {
        self.add_bytes(bytes, options, String::new()).await
    }

    async fn add_bytes(
        &self,
        bytes: &[u8],
        options: TorrentAddOptions,
        source: String,
    ) -> Result<String, TorrentError> {
        let _running = self.inner.running().await?;
        let save_dir = self.inner.save_dir(options.save_dir.as_deref())?;
        let metadata = Arc::new(parse_torrent_bytes(bytes, &save_dir)?);
        let id = metadata
            .info_hash_v1
            .map(hex::encode)
            .ok_or_else(unsupported_v2)?;
        let entry = Arc::new(Entry::new(id.clone(), source, save_dir, None, options));
        let _operation = entry.operation.lock().await;
        self.inner.insert(&entry)?;
        if let Err(error) = self
            .inner
            .attach(&entry, Bytes::copy_from_slice(bytes), metadata, false)
            .await
        {
            self.inner.entries.write().unwrap().remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    pub async fn add_magnet(
        &self,
        uri: &str,
        mut options: TorrentAddOptions,
    ) -> Result<String, TorrentError> {
        let _running = self.inner.running().await?;
        let magnet = parse_magnet_uri(uri)?;
        let id = magnet.info_hash_v1_hex.clone().ok_or_else(unsupported_v2)?;
        let save_dir = self.inner.save_dir(options.save_dir.as_deref())?;
        limits(options.download_limit_bps, options.upload_limit_bps)?;
        if options.selected_file_indices.is_none() {
            options
                .selected_file_indices
                .clone_from(&magnet.select_only);
        }
        let entry = Arc::new(Entry::new(
            id.clone(),
            uri.trim().to_owned(),
            save_dir,
            Some(magnet),
            options,
        ));
        let _operation = entry.operation.lock().await;
        self.inner.insert(&entry)?;
        self.inner.start_resolution(&entry);
        Ok(id)
    }

    pub fn list_files(&self, id: &str) -> Result<Vec<TorrentFileEntry>, TorrentError> {
        self.inner.entry(id)?.list_files()
    }

    pub async fn set_file_selected(
        &self,
        id: &str,
        file_index: usize,
        selected: bool,
    ) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        let files = entry.list_files()?;
        let mut indices: HashSet<_> = files
            .into_iter()
            .filter(|file| file.selected)
            .map(|file| file.index)
            .collect();
        let metadata = entry.metadata()?;
        validate_selection(&metadata, &[file_index])?;
        if selected {
            indices.insert(file_index);
        } else {
            indices.remove(&file_index);
        }
        self.inner.update_selection(&entry, indices).await
    }

    pub async fn set_selected_files(
        &self,
        id: &str,
        indices: &[usize],
    ) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        let metadata = entry.metadata()?;
        validate_selection(&metadata, indices)?;
        self.inner
            .update_selection(&entry, indices.iter().copied().collect())
            .await
    }

    pub async fn pause(&self, id: &str) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        self.inner.pause_entry(&entry).await
    }

    pub async fn resume(&self, id: &str) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        let handle = entry.state.lock().unwrap().handle.clone();
        if let Some(handle) = handle {
            let failed =
                handle.with_state(|state| matches!(state, librqbit::ManagedTorrentState::Error(_)));
            if failed {
                let mut state = entry.state.lock().unwrap();
                state.bitmap_synced = false;
                entry.bitmap.write().unwrap().set_bits(&[]);
            } else if handle.is_paused() {
                handle
                    .wait_until_initialized()
                    .await
                    .map_err(session_error)?;
            }
            if failed || handle.is_paused() {
                self.inner
                    .session
                    .unpause(&handle)
                    .await
                    .map_err(session_error)?;
            }
            entry.state.lock().unwrap().options.start_paused = false;
        } else {
            {
                let mut state = entry.state.lock().unwrap();
                state.options.start_paused = false;
                if state
                    .resolution
                    .as_ref()
                    .is_some_and(|(_, task)| !task.is_finished())
                    && state.error.is_none()
                {
                    return Ok(());
                }
            }
            let retry = {
                let state = entry.state.lock().unwrap();
                state.bytes.clone().zip(state.metadata.clone())
            };
            if let Some((bytes, metadata)) = retry {
                self.inner.attach(&entry, bytes, metadata, true).await?;
            } else {
                self.inner.start_resolution(&entry);
            }
        }
        entry.state.lock().unwrap().updated_at = Utc::now();
        Ok(())
    }

    pub async fn remove(&self, id: &str, delete_files: bool) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        entry.cancel_resolution().await;
        let handle = entry.state.lock().unwrap().handle.clone();
        if let Some(handle) = handle {
            // Wait for rqbit's checksum task before detaching its open files.
            let _ = handle.wait_until_initialized().await;
            let delete_result = self
                .inner
                .session
                .delete(TorrentIdOrHash::Id(handle.id()), delete_files)
                .await;
            if delete_result.is_ok()
                || self
                    .inner
                    .session
                    .get(TorrentIdOrHash::Id(handle.id()))
                    .is_none()
            {
                let mut state = entry.state.lock().unwrap();
                state.handle = None;
                state.updated_at = Utc::now();
            }
            if let Err(error) = delete_result {
                let error = session_error(error);
                entry.set_error(&error);
                return Err(error);
            }
        }
        if delete_files {
            let metadata = entry.state.lock().unwrap().metadata.clone();
            if let Some(metadata) = metadata {
                if let Err(error) = cleanup_deleted_files(&entry.save_dir, &metadata) {
                    entry.set_error(&error);
                    return Err(error);
                }
            }
        }
        self.inner.entries.write().unwrap().remove(id);
        Ok(())
    }

    pub fn set_global_rate_limits(
        &self,
        down_bps: Option<u64>,
        up_bps: Option<u64>,
    ) -> Result<(), TorrentError> {
        self.inner.check_running()?;
        let rates = limits(down_bps, up_bps)?;
        self.inner
            .session
            .ratelimits
            .set_download_bps(rates.download_bps);
        self.inner
            .session
            .ratelimits
            .set_upload_bps(rates.upload_bps);
        Ok(())
    }

    /// Re-registers an initialized torrent so its new per-torrent limits apply to peer traffic.
    /// librqbit v8 takes per-torrent limits at registration. The recheck reuses existing files.
    pub async fn set_torrent_rate_limits(
        &self,
        id: &str,
        down_bps: Option<u64>,
        up_bps: Option<u64>,
    ) -> Result<(), TorrentError> {
        let rates = limits(down_bps, up_bps)?;
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        let (handle, manifest, previous) = {
            let state = entry.state.lock().unwrap();
            let previous = limits(
                state.options.download_limit_bps,
                state.options.upload_limit_bps,
            )?;
            (
                state.handle.clone(),
                state.bytes.clone().zip(state.metadata.clone()),
                previous,
            )
        };
        if rates == previous {
            return Ok(());
        }
        if let Some(handle) = &handle {
            let _ = handle.wait_until_initialized().await;
            let live = handle.live();
            self.inner
                .session
                .delete(TorrentIdOrHash::Id(handle.id()), false)
                .await
                .map_err(session_error)?;
            let mut state = entry.state.lock().unwrap();
            state.handle = None;
            state.previous_uploads = state
                .previous_uploads
                .saturating_add(live.map_or(0, |live| live.get_uploaded_bytes()));
        }
        {
            let mut state = entry.state.lock().unwrap();
            state.options.download_limit_bps = down_bps;
            state.options.upload_limit_bps = up_bps;
            state.updated_at = Utc::now();
        }
        if let Some((bytes, metadata)) = manifest {
            if let Err(error) = self.inner.attach(&entry, bytes, metadata, true).await {
                entry.set_error(&error);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Records the preference. librqbit v8 always prioritizes the first and last
    /// pieces of each file, followed by the remaining pieces in file order.
    pub async fn set_sequential(&self, id: &str, sequential: bool) -> Result<(), TorrentError> {
        let _running = self.inner.running().await?;
        let entry = self.inner.entry(id)?;
        let _operation = entry.operation.lock().await;
        self.inner.check_entry(&entry)?;
        let mut state = entry.state.lock().unwrap();
        state.options.sequential = Some(sequential);
        state.updated_at = Utc::now();
        Ok(())
    }

    pub fn stats(&self, id: &str) -> Result<TorrentStatsReport, TorrentError> {
        Ok(self.inner.entry(id)?.report(&self.inner))
    }

    pub fn snapshot(&self, id: &str) -> Result<TransferSnapshot, TorrentError> {
        self.stats(id).map(|stats| stats.snapshot)
    }

    pub fn list_snapshots(&self) -> Vec<TransferSnapshot> {
        self.inner
            .list_entries()
            .iter()
            .map(|entry| entry.report(&self.inner).snapshot)
            .collect()
    }

    pub async fn wait_until_initialized(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Result<(), TorrentError> {
        self.wait_for(id, timeout, false).await
    }

    pub async fn wait_until_completed(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Result<(), TorrentError> {
        self.wait_for(id, timeout, true).await
    }

    async fn wait_for(
        &self,
        id: &str,
        timeout: Duration,
        completion: bool,
    ) -> Result<(), TorrentError> {
        time::timeout(timeout, async {
            loop {
                self.inner.check_running()?;
                let report = self.stats(id)?;
                if let Some(error) = report.snapshot.error_message {
                    return Err(TorrentError::Session(error));
                }
                if (completion && report.finished) || (!completion && report.initialized) {
                    return Ok(());
                }
                time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| TorrentError::InvalidState(format!("timed out waiting for torrent {id}")))?
    }

    pub async fn shutdown(&self) {
        let _exclusive = self.inner.lifecycle.write().await;
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) || self.inner.stop.is_cancelled() {
            return;
        }
        struct CancelOnDrop<'a>(&'a CancellationToken);
        impl Drop for CancelOnDrop<'_> {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel_guard = CancelOnDrop(&self.inner.stop);
        for entry in self.inner.list_entries() {
            let _operation = entry.operation.lock().await;
            entry.cancel_resolution().await;
            let handle = {
                let mut state = entry.state.lock().unwrap();
                state.options.start_paused = true;
                state.updated_at = Utc::now();
                state.handle.clone()
            };
            if let Some(handle) = handle {
                let _ = self.inner.pause_handle_and_record(&entry, &handle).await;
            }
        }
        self.inner.stop.cancel();
        self.inner.session.stop().await;
    }
}

impl Drop for TorrentManager {
    fn drop(&mut self) {
        self.inner.stop.cancel();
    }
}

impl ManagerInner {
    async fn running(&self) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, TorrentError> {
        let guard = self.lifecycle.read().await;
        self.check_running()?;
        Ok(guard)
    }

    fn check_running(&self) -> Result<(), TorrentError> {
        if self.shutting_down.load(Ordering::Acquire) || self.stop.is_cancelled() {
            return Err(TorrentError::InvalidState(
                "torrent manager is stopped".into(),
            ));
        }
        Ok(())
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>, TorrentError> {
        self.entries
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| TorrentError::NotFound(id.to_owned()))
    }

    fn check_entry(&self, entry: &Arc<Entry>) -> Result<(), TorrentError> {
        if Arc::ptr_eq(entry, &self.entry(&entry.id)?) {
            Ok(())
        } else {
            Err(TorrentError::NotFound(entry.id.clone()))
        }
    }

    fn list_entries(&self) -> Vec<Arc<Entry>> {
        self.entries.read().unwrap().values().cloned().collect()
    }

    fn insert(&self, entry: &Arc<Entry>) -> Result<(), TorrentError> {
        let mut entries = self.entries.write().unwrap();
        if entries.contains_key(&entry.id) {
            return Err(TorrentError::InvalidState(format!(
                "torrent {} is already registered",
                entry.id
            )));
        }
        entries.insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    fn save_dir(&self, path: Option<&Path>) -> Result<PathBuf, TorrentError> {
        let base = &self.config.default_save_dir;
        let output = path.map_or_else(|| base.clone(), |path| base.join(path));
        validate_output(base, &output)?;
        Ok(output)
    }

    async fn attach(
        &self,
        entry: &Entry,
        bytes: Bytes,
        metadata: Arc<TorrentMetadata>,
        reuse_files: bool,
    ) -> Result<(), TorrentError> {
        let options = entry.state.lock().unwrap().options.clone();
        if metadata.info_hash_v1.map(hex::encode).as_deref() != Some(&entry.id) {
            return Err(TorrentError::Metadata(
                "resolved metadata does not match the BTv1 info hash".into(),
            ));
        }
        validate_engine_layout(&metadata)?;
        if let Some(indices) = &options.selected_file_indices {
            validate_selection(&metadata, indices)?;
        }
        // Disabling trackers for a raw torrent must preserve the exact info bytes.
        let engine_bytes = if self.config.enable_trackers {
            bytes.clone()
        } else {
            info_only(&bytes)?
        };
        let factory = ConfinedStorageFactory::new(
            &self.config.default_save_dir,
            &entry.save_dir,
            entry.bitmap.clone(),
            entry.magnet.is_some(),
        );
        let rqbit_options = factory.with_output(AddTorrentOptions {
            paused: options.start_paused,
            only_files: options.selected_file_indices,
            overwrite: reuse_files || options.overwrite,
            initial_peers: Some(options.initial_peers),
            ratelimits: limits(options.download_limit_bps, options.upload_limit_bps)?,
            ..Default::default()
        })?;
        *entry.bitmap.write().unwrap() = PieceBitmap::new(
            metadata.total_pieces,
            metadata.piece_length,
            metadata.total_bytes,
        );
        let response = self
            .session
            .add_torrent(AddTorrent::from_bytes(engine_bytes), Some(rqbit_options))
            .await
            .map_err(session_error)?;
        let AddTorrentResponse::Added(_, handle) = response else {
            return Err(TorrentError::InvalidState(
                "librqbit already manages this torrent".into(),
            ));
        };
        let mut state = entry.state.lock().unwrap();
        state.bytes = Some(bytes);
        state.metadata = Some(metadata);
        state.handle = Some(handle);
        state.error = None;
        state.bitmap_synced = false;
        state.updated_at = Utc::now();
        Ok(())
    }

    fn start_resolution(self: &Arc<Self>, entry: &Arc<Entry>) {
        let mut state = entry.state.lock().unwrap();
        state.error = None;
        let Some(magnet) = &entry.magnet else {
            return;
        };
        let use_trackers = self.config.enable_trackers && !magnet.trackers.is_empty();
        if state.options.start_paused
            || (!self.config.enable_dht && !use_trackers && state.options.initial_peers.is_empty())
        {
            return;
        }
        let Some(uri) = magnet.v1_uri(use_trackers) else {
            return;
        };
        let initial_peers = state.options.initial_peers.clone();
        let token = self.stop.child_token();
        let cancel = token.clone();
        let manager = self.clone();
        let entry = entry.clone();
        let task = tokio::spawn(async move {
            let resolve = manager.session.add_torrent(
                AddTorrent::from_url(uri),
                Some(AddTorrentOptions {
                    list_only: true,
                    initial_peers: Some(initial_peers),
                    output_folder: Some(entry.save_dir.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            );
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = time::timeout(manager.config.magnet_metadata_timeout, resolve) => result,
            };
            let _operation = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                guard = entry.operation.lock() => guard,
            };
            if let Err(error) = manager.finish_resolution(&entry, result).await {
                tracing::debug!(id = %entry.id, %error, "magnet resolution failed");
                entry.set_error(&error);
            }
        });
        state.resolution = Some((token, task));
    }

    async fn finish_resolution(
        &self,
        entry: &Entry,
        result: Result<anyhow::Result<AddTorrentResponse>, time::error::Elapsed>,
    ) -> Result<(), TorrentError> {
        let response = result
            .map_err(|_| TorrentError::Session("magnet metadata resolution timed out".into()))?
            .map_err(session_error)?;
        let AddTorrentResponse::ListOnly(resolved) = response else {
            return Err(TorrentError::InvalidState(
                "expected resolved magnet metadata".into(),
            ));
        };
        let metadata = Arc::new(crate::metadata::parse_wire_metadata(
            &resolved.torrent_bytes,
            &entry.save_dir,
        )?);
        if let Some(hash) = entry
            .magnet
            .as_ref()
            .and_then(|m| m.info_hash_v2_hex.as_deref())
        {
            if metadata.info_hash_v2.map(hex::encode).as_deref() != Some(hash) {
                return Err(TorrentError::Metadata(
                    "resolved metadata does not match the BTv2 info hash".into(),
                ));
            }
        }
        {
            let mut state = entry.state.lock().unwrap();
            let peers = &mut state.options.initial_peers;
            peers.extend(resolved.seen_peers);
            peers.sort_unstable();
            peers.dedup();
        }
        self.attach(entry, resolved.torrent_bytes, metadata, false)
            .await
    }

    async fn pause_handle_and_record(
        &self,
        entry: &Entry,
        handle: &Arc<ManagedTorrent>,
    ) -> Result<(), TorrentError> {
        let mut live = handle.live();
        if live.is_none() && !handle.is_paused() {
            let _ = handle.wait_until_initialized().await;
            live = handle.live();
        }
        if let Some(live) = live {
            self.session.pause(handle).await.map_err(session_error)?;
            let uploaded = live.get_uploaded_bytes();
            let mut state = entry.state.lock().unwrap();
            state.previous_uploads = state.previous_uploads.saturating_add(uploaded);
            state.updated_at = Utc::now();
        }
        Ok(())
    }

    async fn pause_entry(&self, entry: &Entry) -> Result<(), TorrentError> {
        entry.cancel_resolution().await;
        let handle = entry.state.lock().unwrap().handle.clone();
        if let Some(handle) = handle {
            handle
                .wait_until_initialized()
                .await
                .map_err(session_error)?;
            if !handle.is_paused() {
                self.pause_handle_and_record(entry, &handle).await?;
            }
        }
        let mut state = entry.state.lock().unwrap();
        state.options.start_paused = true;
        state.updated_at = Utc::now();
        Ok(())
    }

    async fn update_selection(
        &self,
        entry: &Entry,
        indices: HashSet<usize>,
    ) -> Result<(), TorrentError> {
        let handle = entry
            .state
            .lock()
            .unwrap()
            .handle
            .clone()
            .ok_or_else(unresolved)?;
        handle
            .wait_until_initialized()
            .await
            .map_err(session_error)?;
        self.session
            .update_only_files(&handle, &indices)
            .await
            .map_err(session_error)?;
        let mut indices: Vec<_> = indices.into_iter().collect();
        indices.sort_unstable();
        let mut state = entry.state.lock().unwrap();
        state.options.selected_file_indices = Some(indices);
        state.updated_at = Utc::now();
        Ok(())
    }
}

impl Entry {
    fn new(
        id: String,
        source: String,
        save_dir: PathBuf,
        magnet: Option<ParsedMagnet>,
        mut options: TorrentAddOptions,
    ) -> Self {
        if let Some(indices) = &mut options.selected_file_indices {
            indices.sort_unstable();
            indices.dedup();
        }
        let created_at = Utc::now();
        Self {
            id,
            source,
            save_dir,
            magnet,
            created_at,
            operation: AsyncMutex::new(()),
            bitmap: Arc::new(RwLock::new(PieceBitmap::new(0, 0, 0))),
            state: Mutex::new(EntryState {
                options,
                metadata: None,
                bytes: None,
                handle: None,
                resolution: None,
                error: None,
                bitmap_synced: false,
                previous_uploads: 0,
                seen_peers: 0,
                updated_at: created_at,
            }),
        }
    }

    fn metadata(&self) -> Result<Arc<TorrentMetadata>, TorrentError> {
        self.state
            .lock()
            .unwrap()
            .metadata
            .clone()
            .ok_or_else(unresolved)
    }

    fn set_error(&self, error: &TorrentError) {
        let mut state = self.state.lock().unwrap();
        state.error = Some(error.to_string());
        state.updated_at = Utc::now();
    }

    async fn cancel_resolution(&self) {
        let resolution = self.state.lock().unwrap().resolution.take();
        if let Some((token, task)) = resolution {
            token.cancel();
            let _ = task.await;
        }
    }
}

fn info_only(bytes: &[u8]) -> Result<Bytes, TorrentError> {
    // Retain v2 piece layers for storage validation of a hybrid torrent.
    let (root, info) = crate::bencode::decode_with_info(bytes)?;
    let mut output = b"d4:info".to_vec();
    output.extend_from_slice(
        info.ok_or_else(|| TorrentError::Metadata("missing info dictionary".into()))?,
    );
    if let crate::bencode::BencodeValue::Dict(root) = root {
        if let Some(layers) = root.get(b"piece layers".as_slice()) {
            output.extend_from_slice(b"12:piece layers");
            output.extend(crate::bencode::encode(layers));
        }
    }
    output.push(b'e');
    Ok(output.into())
}

fn limits(down: Option<u64>, up: Option<u64>) -> Result<LimitsConfig, TorrentError> {
    fn limit(rate: Option<u64>) -> Result<Option<NonZeroU32>, TorrentError> {
        let rate = rate.unwrap_or(0);
        if rate != 0 && rate < 16 * 1024 {
            return Err(TorrentError::Unsupported(
                "rate limit must fit one 16 KiB BitTorrent request per second".into(),
            ));
        }
        let rate = u32::try_from(rate).map_err(|_| {
            TorrentError::Unsupported("rate limit exceeds librqbit's u32 range".into())
        })?;
        Ok(NonZeroU32::new(rate))
    }
    Ok(LimitsConfig {
        download_bps: limit(down)?,
        upload_bps: limit(up)?,
    })
}

fn validate_selection(metadata: &TorrentMetadata, indices: &[usize]) -> Result<(), TorrentError> {
    for &index in indices {
        let file = metadata
            .file_specs
            .get(index)
            .ok_or_else(|| TorrentError::NotFound(format!("file index {index}")))?;
        if file.is_padding {
            return Err(TorrentError::InvalidState(format!(
                "padding file {index} is not selectable"
            )));
        }
    }
    Ok(())
}

fn validate_engine_layout(metadata: &TorrentMetadata) -> Result<(), TorrentError> {
    const BLOCK: u64 = 16 * 1024;
    let unsupported = || {
        TorrentError::Unsupported("file or piece lengths exceed librqbit's supported layout".into())
    };
    let pieces = u64::from(u32::try_from(metadata.total_pieces).map_err(|_| unsupported())?);
    let piece_length = u64::from(u32::try_from(metadata.piece_length).map_err(|_| unsupported())?);
    let full_pieces = pieces.checked_sub(1).ok_or_else(unsupported)?;
    let last_piece = metadata.total_bytes - full_pieces * piece_length;
    if u32::try_from(full_pieces * piece_length.div_ceil(BLOCK) + last_piece.div_ceil(BLOCK))
        .is_err()
    {
        return Err(unsupported());
    }
    Ok(())
}

async fn read_torrent_file(path: &Path) -> Result<Vec<u8>, TorrentError> {
    if path.as_os_str().is_empty() {
        return Err(PathSecurityError::Empty.into());
    }
    let mut saw_prefix = false;
    let mut saw_root = false;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) => saw_prefix = true,
            std::path::Component::RootDir => saw_root = true,
            std::path::Component::Normal(part) => {
                let text = part.to_str().ok_or(PathSecurityError::InvalidCharacter)?;
                if text.contains('\0') {
                    return Err(PathSecurityError::NulByte.into());
                }
                if text.contains(':') {
                    return Err(PathSecurityError::NtfsAlternateDataStream.into());
                }
            }
            std::path::Component::CurDir | std::path::Component::ParentDir => {}
        }
    }
    if saw_prefix && !saw_root {
        return Err(PathSecurityError::NtfsAlternateDataStream.into());
    }
    let abs_path = std::path::absolute(path)?;
    reject_existing_links(&abs_path)?;
    let meta = tokio::fs::symlink_metadata(&abs_path).await?;
    if is_link(&meta) {
        return Err(PathSecurityError::EscapesBaseDir.into());
    }
    if !meta.file_type().is_file() {
        return Err(TorrentError::Metadata(
            "torrent source must be a regular file".into(),
        ));
    }
    if meta.len() > MAX_TORRENT_BYTES as u64 {
        return Err(TorrentError::Metadata(format!(
            "torrent manifest exceeds maximum size of {MAX_TORRENT_BYTES} bytes"
        )));
    }
    let std_file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        opts.open(&abs_path)?
    };
    let file = tokio::fs::File::from_std(std_file);
    let actual_meta = file.metadata().await?;
    if is_link(&actual_meta) {
        return Err(PathSecurityError::EscapesBaseDir.into());
    }
    if !actual_meta.file_type().is_file() || actual_meta.len() > MAX_TORRENT_BYTES as u64 {
        return Err(TorrentError::Metadata(format!(
            "torrent manifest exceeds maximum size of {MAX_TORRENT_BYTES} bytes"
        )));
    }
    let mut buf = Vec::with_capacity(usize::try_from(actual_meta.len()).unwrap_or(0));
    let mut limited = file.take((MAX_TORRENT_BYTES + 1) as u64);
    limited.read_to_end(&mut buf).await?;
    if buf.len() > MAX_TORRENT_BYTES {
        return Err(TorrentError::Metadata(format!(
            "torrent manifest exceeds maximum size of {MAX_TORRENT_BYTES} bytes"
        )));
    }
    Ok(buf)
}

fn cleanup_deleted_files(save_dir: &Path, metadata: &TorrentMetadata) -> Result<(), TorrentError> {
    let mut dirs = HashSet::new();
    for (file, path) in metadata.file_specs.iter().zip(&metadata.resolved_paths) {
        if file.is_padding {
            continue;
        }
        validate_output(save_dir, path)?;
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(meta) if is_link(&meta) => return Err(PathSecurityError::EscapesBaseDir.into()),
            Ok(meta) if meta.file_type().is_file() => {
                std::fs::remove_file(path)?;
            }
            Ok(_) => {
                return Err(TorrentError::Session(format!(
                    "could not delete non-regular path {}",
                    path.display()
                )));
            }
        }
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir == save_dir || !dir.starts_with(save_dir) {
                break;
            }
            dirs.insert(dir.to_path_buf());
            parent = dir.parent();
        }
    }
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.as_os_str().len()));
    for dir in dirs {
        validate_output(save_dir, &dir)?;
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if !is_link(&meta) && meta.is_dir() => {
                let _ = std::fs::remove_dir(&dir);
            }
            _ => {}
        }
    }
    Ok(())
}

fn unresolved() -> TorrentError {
    TorrentError::InvalidState("torrent metadata is not resolved".into())
}

fn unsupported_v2() -> TorrentError {
    TorrentError::Unsupported("librqbit v8.1.1 requires a BTv1 info hash for transfers".into())
}
