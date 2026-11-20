use crate::paths::{canonical_directory, confined_target, invalid};
use chrono::Utc;
use slipstream_media::{
    DownloadOptions, MediaPipeline, MediaProgressEvent, MediaToolchain, ToolchainConfig,
};
use slipstream_protocol::{
    safe_join_under, sanitize_filename, validate_http_url, AddMediaDownloadRequest, TransferKind,
    TransferSnapshot, TransferStatus, UI_SEGMENT_COUNT,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinHandle,
};
use uuid::Uuid;

pub(crate) type ScanRequest = (PathBuf, Vec<String>);

pub(crate) struct MediaJob {
    request: AddMediaDownloadRequest,
    directory: PathBuf,
    data: Mutex<MediaData>,
    run: AsyncMutex<Option<MediaRun>>,
}

struct MediaData {
    snapshot: TransferSnapshot,
    outputs: Vec<String>,
}

struct MediaRun {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl MediaJob {
    pub(crate) fn new(
        mut request: AddMediaDownloadRequest,
        directory: PathBuf,
    ) -> Result<Self, String> {
        let mut url = validate_http_url(&request.url).map_err(|error| error.to_string())?;
        let filename = request
            .filename
            .as_deref()
            .map(sanitize_filename)
            .transpose()
            .map_err(|error| error.to_string())?;
        request.filename.clone_from(&filename);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        let mut format_tags = Vec::new();
        if let Some(format_id) = request
            .format_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            format_tags.push(format_id.to_owned());
        }
        if let Some(preset) = request.audio_only_preset {
            format_tags.push(format!("{preset:?}"));
        }
        if let Some(container) = request.target_container {
            format_tags.push(format!("{container:?}").to_uppercase());
        }
        if request.lossless_remux {
            format_tags.push("Lossless".into());
        }
        if request.normalize_audio {
            format_tags.push("Normalized".into());
        }
        let now = Utc::now();
        let snapshot = TransferSnapshot {
            id: Uuid::new_v4().to_string(),
            kind: TransferKind::Media,
            status: TransferStatus::Queued,
            filename: filename.unwrap_or_else(|| "Media download".into()),
            source_url: url.to_string(),
            source_domain: url.host_str().unwrap_or_default().to_owned(),
            save_dir: directory.to_string_lossy().into_owned(),
            total_bytes: None,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            active_connections: 0,
            active_peers: 0,
            protocol_label: "yt-dlp".into(),
            format_tags,
            ui_segments: [0.0; UI_SEGMENT_COUNT],
            segments: Vec::new(),
            sha256_verified: None,
            resumable: false,
            queue_id: "default".into(),
            error_message: None,
            created_at: now,
            updated_at: now,
        };
        Ok(Self {
            request,
            directory,
            data: Mutex::new(MediaData {
                snapshot,
                outputs: Vec::new(),
            }),
            run: AsyncMutex::new(None),
        })
    }

    pub(crate) fn snapshot(&self) -> TransferSnapshot {
        self.data.lock().unwrap().snapshot.clone()
    }

    pub(crate) fn outputs(&self) -> Vec<String> {
        self.data.lock().unwrap().outputs.clone()
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        config: ToolchainConfig,
        scans: mpsc::Sender<ScanRequest>,
    ) -> Result<(), String> {
        let mut run = self.run.lock().await;
        {
            let mut data = self.data.lock().unwrap();
            let snapshot = &mut data.snapshot;
            if !matches!(
                snapshot.status,
                TransferStatus::Queued | TransferStatus::Paused | TransferStatus::Failed
            ) {
                return Err("media transfer must be paused or failed to resume".into());
            }
            snapshot.status = TransferStatus::Probing;
            snapshot.downloaded_bytes = 0;
            snapshot.total_bytes = None;
            snapshot.ui_segments.fill(0.0);
            snapshot.error_message = None;
            snapshot.updated_at = Utc::now();
        }
        if let Some(previous) = run.as_mut() {
            let _ = (&mut previous.task).await;
        }
        let (stop, stopped) = watch::channel(false);
        let job = self.clone();
        let task = tokio::spawn(async move { job.download(config, scans, stopped).await });
        *run = Some(MediaRun { stop, task });
        Ok(())
    }

    pub(crate) async fn stop(&self, status: TransferStatus) -> Result<(), String> {
        let mut run = self.run.lock().await;
        if let Some(current) = run.as_mut() {
            current.stop.send_replace(true);
            let outcome = (&mut current.task).await;
            *run = None;
            outcome.map_err(|error| error.to_string())?;
        }
        let mut data = self.data.lock().unwrap();
        let snapshot = &mut data.snapshot;
        if matches!(
            snapshot.status,
            TransferStatus::Completed | TransferStatus::Canceled
        ) {
            if status == TransferStatus::Paused {
                return Err("transfer has already finished".into());
            }
        } else {
            snapshot.status = status;
            snapshot.download_speed_bps = 0;
            snapshot.eta_seconds = None;
            snapshot.active_connections = 0;
            snapshot.updated_at = Utc::now();
        }
        Ok(())
    }

    async fn download(
        &self,
        config: ToolchainConfig,
        scans: mpsc::Sender<ScanRequest>,
        mut stopped: watch::Receiver<bool>,
    ) {
        let (tx, mut rx) = mpsc::channel(16);
        let result = {
            let operation = self.stage_download(config, tx);
            tokio::pin!(operation);
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return,
                    result = &mut operation => break result,
                    Some(event) = rx.recv() => self.progress(event),
                }
            }
        };
        while let Ok(event) = rx.try_recv() {
            self.progress(event);
        }
        // Publishing is a separate phase that completes before a pause/cancel
        // can return, so every output is accounted for even if stopping races it.
        let result = match result {
            Ok((stage, paths)) => {
                let directory = self.directory.clone();
                tokio::task::spawn_blocking(move || publish(&directory, stage, &paths))
                    .await
                    .unwrap_or_else(|error| (Vec::new(), Err(io::Error::other(error))))
            }
            Err(message) => (Vec::new(), Err(io::Error::other(message))),
        };
        let (paths, outcome) = result;
        let mut data = self.data.lock().unwrap();
        data.outputs.extend(paths.iter().cloned());
        let snapshot = &mut data.snapshot;
        snapshot.download_speed_bps = 0;
        snapshot.eta_seconds = None;
        snapshot.active_connections = 0;
        snapshot.updated_at = Utc::now();
        match outcome {
            Ok(bytes) => {
                snapshot.status = TransferStatus::Completed;
                if let Some(first) = paths.first() {
                    snapshot.filename.clone_from(first);
                }
                snapshot.downloaded_bytes = bytes;
                snapshot.total_bytes = Some(bytes);
                snapshot.ui_segments.fill(1.0);
                if let Err(error) = scans.try_send((self.directory.clone(), paths)) {
                    tracing::debug!(%error, "could not enqueue media Defender scan");
                }
            }
            Err(error) => {
                snapshot.status = TransferStatus::Failed;
                snapshot.error_message = Some(error.to_string());
            }
        }
    }

    async fn stage_download(
        &self,
        config: ToolchainConfig,
        progress: mpsc::Sender<MediaProgressEvent>,
    ) -> Result<(StagingDir, Vec<PathBuf>), String> {
        let tools = MediaToolchain::discover(config)
            .await
            .map_err(|error| error.to_string())?;
        let path = self
            .directory
            .join(format!(".slipstream-media-{}", Uuid::new_v4()));
        fs::create_dir(&path).map_err(|error| error.to_string())?;
        let stage = StagingDir(path);
        let mut request = self.request.clone();
        request.save_dir = Some(stage.0.to_string_lossy().into_owned());
        let pipeline = MediaPipeline::new(tools, &stage.0).create_save_dir(false);
        let files = pipeline
            .download(&request, &DownloadOptions::default(), Some(&progress))
            .await
            .map_err(|error| error.to_string())?;
        Ok((stage, files))
    }

    fn progress(&self, event: MediaProgressEvent) {
        let mut data = self.data.lock().unwrap();
        let snapshot = &mut data.snapshot;
        match event {
            MediaProgressEvent::Probing => snapshot.status = TransferStatus::Probing,
            MediaProgressEvent::Downloading(progress) => {
                snapshot.status = TransferStatus::Active;
                snapshot.downloaded_bytes = progress.downloaded_bytes;
                snapshot.total_bytes = progress.total_bytes;
                snapshot.download_speed_bps = progress.speed_bps.unwrap_or(0);
                snapshot.eta_seconds = progress.eta_seconds;
                snapshot.active_connections = 1;
                if let Some(percent) = progress.percent() {
                    let filled = percent / 100.0 * UI_SEGMENT_COUNT as f32;
                    for (index, part) in snapshot.ui_segments.iter_mut().enumerate() {
                        *part = (filled - index as f32).clamp(0.0, 1.0);
                    }
                }
            }
            MediaProgressEvent::Remuxing(_) => {
                snapshot.status = TransferStatus::Remuxing;
                snapshot.download_speed_bps = 0;
                snapshot.eta_seconds = None;
                snapshot.active_connections = 0;
            }
            MediaProgressEvent::Completed { .. } | MediaProgressEvent::Failed { .. } => return,
        }
        snapshot.updated_at = Utc::now();
    }
}

struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        for attempt in 0..5 {
            match fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) if attempt < 4 && error.kind() == io::ErrorKind::PermissionDenied => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    tracing::warn!(%error, "could not clean the media staging directory");
                    return;
                }
            }
        }
    }
}

fn publish(root: &Path, stage: StagingDir, paths: &[PathBuf]) -> (Vec<String>, io::Result<u64>) {
    let mut outputs = Vec::with_capacity(paths.len());
    let result = (|| {
        if canonical_directory(root)? != root {
            return Err(invalid("media directory changed during the download"));
        }
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let name = path
                .strip_prefix(&stage.0)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| invalid("media output is outside its staging directory"))?;
            let name = slipstream_protocol::validate_safe_filename_component(name)
                .map_err(|error| invalid(error.to_string()))?;
            let source = confined_target(&stage.0, name)?
                .ok_or_else(|| invalid("media output is missing"))?;
            let metadata = fs::metadata(&source)?;
            if !metadata.is_file() {
                return Err(invalid("media output is not a file"));
            }
            let target = safe_join_under(root, name).map_err(|error| invalid(error.to_string()))?;
            if confined_target(root, name)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{name} already exists"),
                ));
            }
            files.push((source, target, name.to_owned(), metadata.len()));
        }
        let mut size = 0u64;
        for (source, target, name, bytes) in files {
            fs::hard_link(source, target)?;
            outputs.push(name);
            size = size.saturating_add(bytes);
        }
        Ok(size)
    })();
    (outputs, result)
}
