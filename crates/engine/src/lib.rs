//! HTTP downloads with resumable ranges, persistent queues, and batched telemetry.

mod download;
mod http;
mod limiter;
mod output;
mod storage;
#[cfg(test)]
mod tests;
mod wildcard;

pub use http::validate_redirect_target;
pub use slipstream_protocol::{
    AddHttpDownloadRequest, PostQueueAction, RefreshDownloadUrlRequest, SegmentSnapshot,
    SegmentState, TelemetryBatch, TransferSnapshot, TransferStatus,
};
pub use wildcard::{expand_wildcard_url, MAX_WILDCARD_EXPANSION};

use chrono::Utc;
use http::{filename_from_url, sanitize_http_url_credentials, Http, Source};
use limiter::Limiter;
use serde::{Deserialize, Serialize};
use slipstream_protocol::{
    safe_join_under, sanitize_filename, validate_http_url, TransferKind, UI_SEGMENT_COUNT,
};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use storage::{Database, Record, Segments};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::{Id, JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use uuid::Uuid;

const DEFAULT_QUEUE: &str = "default";
const TELEMETRY_PERIOD: Duration = Duration::from_millis(50);

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Url(#[from] slipstream_protocol::UrlSecurityError),
    #[error(transparent)]
    Path(#[from] slipstream_protocol::PathSecurityError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Http(reqwest::Error),
    #[error("HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("invalid HTTP response: {0}")]
    InvalidResponse(&'static str),
    #[error("blocked connection to {0}")]
    BlockedAddress(IpAddr),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("transfer or queue '{0}' was not found")]
    NotFound(String),
    #[error("{0}")]
    InvalidState(&'static str),
    #[error("{0} checksum mismatch")]
    ChecksumMismatch(&'static str),
    #[error("download stopped")]
    Interrupted,
    #[error("download representation changed or server refused its range")]
    RestartRequired,
    #[error("download engine is closed")]
    Closed,
}

impl EngineError {
    fn is_transient(&self) -> bool {
        match self {
            Self::Http(error) => {
                error.is_connect()
                    || error.is_timeout()
                    || error.is_body()
                    || error.is_decode()
                    || error.is_request()
            }
            Self::Status(status) => status.is_server_error() || status.as_u16() == 408,
            Self::Io(error) => error.kind() == std::io::ErrorKind::UnexpectedEof,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub db_path: PathBuf,
    pub default_download_dir: PathBuf,
    pub default_connections: u8,
    pub max_concurrent_downloads: usize,
    pub global_speed_limit_bps: Option<u64>,
    /// Allows loopback connections for isolated tests. Other blocked IPs stay blocked.
    pub allow_loopback: bool,
    pub request_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            db_path: "slipstream.sqlite".into(),
            default_download_dir: ".".into(),
            default_connections: 16,
            max_concurrent_downloads: 4,
            global_speed_limit_bps: None,
            allow_loopback: false,
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    pub id: String,
    pub max_concurrent_downloads: usize,
    pub post_action: PostQueueAction,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            id: DEFAULT_QUEUE.into(),
            max_concurrent_downloads: 4,
            post_action: PostQueueAction::None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtendedHttpOptions {
    pub expected_md5: Option<String>,
    pub speed_limit_bps: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    StatusChanged {
        transfer_id: String,
        status: TransferStatus,
    },
    SegmentSplit {
        transfer_id: String,
        lower_index: u8,
        upper_index: u8,
        split_point: u64,
    },
    QueueDrained {
        queue_id: String,
        action: PostQueueAction,
    },
}

#[derive(Clone)]
pub struct DownloadEngine {
    inner: Arc<Inner>,
    runtime: Arc<Runtime>,
}

struct Runtime {
    inner: Arc<Inner>,
    supervisor: tokio::sync::Mutex<Option<JoinHandle<Result<()>>>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.inner.request_shutdown();
    }
}

pub(crate) struct Inner {
    config: EngineConfig,
    http: Http,
    global_limiter: Limiter,
    events: broadcast::Sender<EngineEvent>,
    telemetry: broadcast::Sender<TelemetryBatch>,
    wake: Notify,
    state: Mutex<State>,
}

struct State {
    database: Option<Database>,
    jobs: HashMap<String, Job>,
    queues: HashMap<String, QueueConfig>,
    armed_queues: HashSet<String>,
    running: HashMap<String, RunControl>,
    concurrency: usize,
    closing: bool,
}

struct RunControl {
    task: Id,
    stop: watch::Sender<bool>,
    done: watch::Sender<Option<std::result::Result<TransferStatus, String>>>,
    desired: Option<TransferStatus>,
}

pub(crate) struct Job {
    record: Record,
    segments: Segments,
    limiter: Arc<Limiter>,
}

impl State {
    fn open(&self) -> Result<()> {
        if self.closing {
            return Err(EngineError::Closed);
        }
        Ok(())
    }

    fn db(&self) -> Result<&Database> {
        self.database.as_ref().ok_or(EngineError::Closed)
    }

    fn job(&self, id: &str) -> Result<&Job> {
        self.jobs
            .get(id)
            .ok_or_else(|| EngineError::NotFound(id.into()))
    }

    fn job_mut(&mut self, id: &str) -> Result<(&mut Database, &mut Job)> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| EngineError::NotFound(id.into()))?;
        Ok((self.database.as_mut().ok_or(EngineError::Closed)?, job))
    }
}

impl DownloadEngine {
    pub async fn new(config: EngineConfig) -> Result<Self> {
        let http = Http::new(config.allow_loopback, config.request_timeout)?;
        let path = config.db_path.clone();
        let (database, queues, jobs) = tokio::task::spawn_blocking(move || {
            let mut db = Database::open(&path)?;
            let mut queues = db.queues()?;
            if !queues.contains_key(DEFAULT_QUEUE) {
                let queue = QueueConfig::default();
                db.save_queue(&queue)?;
                queues.insert(queue.id.clone(), queue);
            }
            let mut jobs = HashMap::new();
            for (mut record, mut segments) in db.transfers()? {
                if matches!(
                    record.status,
                    TransferStatus::Active | TransferStatus::Probing
                ) {
                    if output::recovered_commit(&record)? {
                        record.set_status(TransferStatus::Completed);
                    } else {
                        record.set_status(TransferStatus::Paused);
                        for segment in segments.values_mut() {
                            segment.settled(SegmentState::Paused);
                        }
                    }
                    record.commit_digest = None;
                    db.replace_segments(&record, &segments)?;
                }
                jobs.insert(
                    record.id.clone(),
                    Job {
                        limiter: Arc::new(Limiter::new(record.speed_limit_bps)),
                        segments,
                        record,
                    },
                );
            }
            Ok::<_, EngineError>((db, queues, jobs))
        })
        .await??;

        let armed_queues = jobs
            .values()
            .filter(|job| is_queued_work(job.record.status))
            .map(|job| job.record.queue_id.clone())
            .collect();
        let inner = Arc::new(Inner {
            http,
            global_limiter: Limiter::new(config.global_speed_limit_bps),
            events: broadcast::channel(256).0,
            telemetry: broadcast::channel(32).0,
            wake: Notify::new(),
            state: Mutex::new(State {
                database: Some(database),
                jobs,
                queues,
                armed_queues,
                running: HashMap::new(),
                concurrency: config.max_concurrent_downloads.max(1),
                closing: false,
            }),
            config,
        });
        let supervisor = tokio::spawn(Arc::clone(&inner).supervise());
        Ok(Self {
            runtime: Arc::new(Runtime {
                inner: Arc::clone(&inner),
                supervisor: tokio::sync::Mutex::new(Some(supervisor)),
            }),
            inner,
        })
    }

    pub async fn add_http_download(&self, request: AddHttpDownloadRequest) -> Result<String> {
        self.add_http_download_with_options(request, ExtendedHttpOptions::default())
            .await
    }

    pub async fn add_http_download_with_options(
        &self,
        request: AddHttpDownloadRequest,
        options: ExtendedHttpOptions,
    ) -> Result<String> {
        let source = Source::new(
            &request.url,
            &request.headers,
            request.referrer.as_deref(),
            self.inner.config.allow_loopback,
        )?;
        let filename = match request.filename.as_deref() {
            Some(filename) => sanitize_filename(filename)?,
            None => filename_from_url(&source.url),
        };
        let expected_sha256 = checksum(request.expected_sha256, 64, "SHA-256")?;
        let expected_md5 = checksum(options.expected_md5, 32, "MD5")?;
        let queue_id = request.queue_id.unwrap_or_else(|| DEFAULT_QUEUE.into());
        {
            let state = self.inner.lock();
            state.open()?;
            if !state.queues.contains_key(&queue_id) {
                return Err(EngineError::NotFound(queue_id));
            }
        }
        let save_dir = request
            .save_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.inner.config.default_download_dir.clone());
        tokio::fs::create_dir_all(&save_dir).await?;
        let save_dir = tokio::fs::canonicalize(save_dir).await?;
        safe_join_under(&save_dir, &filename)?;
        let now = Utc::now();
        let record = Record {
            id: Uuid::new_v4().to_string(),
            filename,
            use_server_filename: request.filename.is_none(),
            source_domain: source.url.host_str().unwrap_or_default().to_owned(),
            source_url: source.url.to_string(),
            save_dir,
            headers: request.headers,
            referrer: request
                .referrer
                .as_deref()
                .and_then(sanitize_http_url_credentials),
            expected_sha256,
            expected_md5,
            status: TransferStatus::Queued,
            total: None,
            ranges: false,
            validators: Default::default(),
            max_connections: request
                .max_connections
                .unwrap_or(self.inner.config.default_connections)
                .clamp(1, 32),
            protocol: String::new(),
            queue_id: queue_id.clone(),
            error: None,
            sha256_verified: None,
            commit_digest: None,
            speed_limit_bps: options.speed_limit_bps.filter(|rate| *rate > 0),
            created_at: now,
            updated_at: now,
        };
        let id = record.id.clone();
        let mut state = self.inner.lock();
        state.open()?;
        state.db()?.save_record(&record)?;
        state.armed_queues.insert(queue_id);
        state.jobs.insert(
            id.clone(),
            Job {
                limiter: Arc::new(Limiter::new(record.speed_limit_bps)),
                record,
                segments: Segments::new(),
            },
        );
        self.inner.status(&id, TransferStatus::Queued);
        self.inner.wake.notify_one();
        Ok(id)
    }

    pub fn snapshot(&self, id: &str) -> Result<TransferSnapshot> {
        Ok(self.inner.lock().job(id)?.snapshot())
    }

    pub fn snapshots(&self) -> Vec<TransferSnapshot> {
        self.inner.lock().snapshots()
    }

    pub fn telemetry_batch(&self) -> TelemetryBatch {
        self.inner
            .lock()
            .telemetry_batch(self.inner.global_limiter.rate())
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    pub fn subscribe_telemetry(&self) -> broadcast::Receiver<TelemetryBatch> {
        self.inner.telemetry.subscribe()
    }

    pub fn queue_configs(&self) -> Vec<QueueConfig> {
        let mut configs: Vec<_> = self.inner.lock().queues.values().cloned().collect();
        configs.sort_by(|a, b| a.id.cmp(&b.id));
        configs
    }

    pub async fn set_queue_config(&self, mut config: QueueConfig) -> Result<()> {
        if config.id.trim().is_empty() {
            return Err(EngineError::InvalidRequest("queue ID is empty".into()));
        }
        config.max_concurrent_downloads = config.max_concurrent_downloads.max(1);
        let mut state = self.inner.lock();
        state.open()?;
        state.db()?.save_queue(&config)?;
        state.queues.insert(config.id.clone(), config);
        self.inner.wake.notify_one();
        Ok(())
    }

    pub async fn assign_queue(&self, id: &str, queue_id: &str) -> Result<()> {
        let mut state = self.inner.lock();
        state.open()?;
        if !state.queues.contains_key(queue_id) {
            return Err(EngineError::NotFound(queue_id.into()));
        }
        if state.running.contains_key(id) {
            return Err(EngineError::InvalidState(
                "pause the transfer before reassigning its queue",
            ));
        }
        let (db, job) = state.job_mut(id)?;
        let mut record = job.record.clone();
        record.queue_id = queue_id.into();
        record.updated_at = Utc::now();
        db.save_record(&record)?;
        let armed = is_queued_work(record.status);
        job.record = record;
        if armed {
            state.armed_queues.insert(queue_id.into());
        }
        self.inner.wake.notify_one();
        Ok(())
    }

    pub async fn set_max_concurrent_downloads(&self, maximum: usize) -> Result<()> {
        let mut state = self.inner.lock();
        state.open()?;
        state.concurrency = maximum.max(1);
        self.inner.wake.notify_one();
        Ok(())
    }

    pub async fn set_global_speed_limit(&self, bps: Option<u64>) -> Result<()> {
        let state = self.inner.lock();
        state.open()?;
        self.inner.global_limiter.set(bps);
        Ok(())
    }

    pub async fn set_transfer_speed_limit(&self, id: &str, bps: Option<u64>) -> Result<()> {
        let mut state = self.inner.lock();
        state.open()?;
        let (db, job) = state.job_mut(id)?;
        let mut record = job.record.clone();
        record.speed_limit_bps = bps.filter(|rate| *rate > 0);
        db.save_record(&record)?;
        job.limiter.set(record.speed_limit_bps);
        job.record = record;
        Ok(())
    }

    pub async fn refresh_download_address(&self, request: RefreshDownloadUrlRequest) -> Result<()> {
        let source = Source::new(
            &request.new_url,
            &request.headers,
            request.referrer.as_deref(),
            self.inner.config.allow_loopback,
        )?;
        let id = &request.transfer_id;
        let mut state = self.inner.lock();
        state.open()?;
        let (db, job) = state.job_mut(id)?;
        if !matches!(
            job.record.status,
            TransferStatus::Paused | TransferStatus::Failed
        ) {
            return Err(EngineError::InvalidState(
                "transfer must be paused or failed to refresh its URL",
            ));
        }
        let mut record = job.record.clone();
        record.source_domain = source.url.host_str().unwrap_or_default().to_owned();
        record.source_url = source.url.to_string();
        record.headers = request.headers;
        record.referrer = request
            .referrer
            .as_deref()
            .and_then(sanitize_http_url_credentials);
        let changed = record.status == TransferStatus::Failed;
        record.set_status(TransferStatus::Paused);
        if changed {
            let mut segments = job.segments.clone();
            for segment in segments.values_mut() {
                segment.settled(SegmentState::Paused);
            }
            db.replace_segments(&record, &segments)?;
            job.segments = segments;
        } else {
            db.save_record(&record)?;
        }
        job.record = record;
        if changed {
            self.inner.status(id, TransferStatus::Paused);
        }
        Ok(())
    }

    pub async fn pause_transfer(&self, id: &str) -> Result<()> {
        self.stop_transfer(id, TransferStatus::Paused).await
    }

    pub async fn cancel_transfer(&self, id: &str) -> Result<()> {
        self.stop_transfer(id, TransferStatus::Canceled).await
    }

    async fn stop_transfer(&self, id: &str, target: TransferStatus) -> Result<()> {
        let mut done = {
            let mut state = self.inner.lock();
            state.open()?;
            let current = state.job(id)?.record.status;
            if current == target {
                return Ok(());
            }
            if matches!(
                current,
                TransferStatus::Completed | TransferStatus::Canceled
            ) {
                return Err(EngineError::InvalidState("transfer has already finished"));
            }
            if let Some(control) = state.running.get_mut(id) {
                if control.desired != Some(TransferStatus::Canceled) {
                    control.desired = Some(target);
                }
                control.stop.send_replace(true);
                control.done.subscribe()
            } else {
                let (db, job) = state.job_mut(id)?;
                save_status(db, job, target, None)?;
                self.inner.status(id, target);
                self.inner.wake.notify_one();
                return Ok(());
            }
        };
        let outcome = done.wait_for(Option::is_some).await;
        match outcome.ok().and_then(|value| value.clone()) {
            Some(Ok(final_status)) if final_status == target => Ok(()),
            Some(Ok(TransferStatus::Completed | TransferStatus::Canceled)) => {
                Err(EngineError::InvalidState("transfer has already finished"))
            }
            Some(Ok(_)) => Ok(()),
            Some(Err(message)) => Err(EngineError::InvalidRequest(message)),
            None => Err(EngineError::Closed),
        }
    }

    pub async fn resume_transfer(&self, id: &str) -> Result<()> {
        let mut state = self.inner.lock();
        state.open()?;
        let (db, job) = state.job_mut(id)?;
        if !matches!(
            job.record.status,
            TransferStatus::Paused | TransferStatus::Failed
        ) {
            return Err(EngineError::InvalidState(
                "transfer must be paused or failed to resume",
            ));
        }
        let mut record = job.record.clone();
        record.set_status(TransferStatus::Queued);
        db.save_record(&record)?;
        let queue_id = record.queue_id.clone();
        job.record = record;
        state.armed_queues.insert(queue_id);
        self.inner.status(id, TransferStatus::Queued);
        self.inner.wake.notify_one();
        Ok(())
    }

    /// Pause active transfers, wait for their writes to settle, and close SQLite.
    /// Queued transfers retain their queue positions and can start on reopen.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.request_shutdown();
        let mut supervisor = self.runtime.supervisor.lock().await;
        if let Some(task) = supervisor.as_mut() {
            let outcome = task.await;
            *supervisor = None;
            outcome??;
        }
        Ok(())
    }
}

fn checksum(value: Option<String>, len: usize, label: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            let value = value.trim();
            if value.len() != len || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(EngineError::InvalidRequest(format!(
                    "invalid {label} digest"
                )));
            }
            Ok(value.to_ascii_lowercase())
        })
        .transpose()
}

fn is_queued_work(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Queued
            | TransferStatus::Probing
            | TransferStatus::Active
            | TransferStatus::Paused
    )
}

fn save_status(
    db: &mut Database,
    job: &mut Job,
    status: TransferStatus,
    error: Option<String>,
) -> Result<()> {
    let mut record = job.record.clone();
    let mut segments = job.segments.clone();
    record.set_status(status);
    record.error = error;
    // Keep the intent in SQLite if publishing Completed fails after a move.
    if status == TransferStatus::Completed {
        record.commit_digest = None;
    } else if status == TransferStatus::Canceled {
        record.headers.clear();
        record.referrer = None;
        record.source_url = validate_http_url(&record.source_url).map_or_else(
            |_| String::new(),
            |mut url| {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                url.into()
            },
        );
    }
    let segment_state = if status == TransferStatus::Failed {
        SegmentState::Error
    } else {
        SegmentState::Paused
    };
    for segment in segments.values_mut() {
        segment.settled(segment_state);
    }
    db.replace_segments(&record, &segments)?;
    job.record = record;
    job.segments = segments;
    Ok(())
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn status(&self, id: &str, status: TransferStatus) {
        let _ = self.events.send(EngineEvent::StatusChanged {
            transfer_id: id.to_owned(),
            status,
        });
    }

    fn request_shutdown(&self) {
        let mut state = self.lock();
        state.closing = true;
        for run in state.running.values_mut() {
            if run.desired.is_none() {
                run.desired = Some(TransferStatus::Paused);
            }
            run.stop.send_replace(true);
        }
        self.wake.notify_one();
    }

    async fn supervise(self: Arc<Self>) -> Result<()> {
        let mut runs = JoinSet::new();
        let mut timer = tokio::time::interval(TELEMETRY_PERIOD);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_sample = Instant::now();
        let mut failure = None;
        loop {
            if let Err(error) = self.schedule(&mut runs) {
                failure = Some(error);
                self.request_shutdown();
            }
            if self.lock().closing && runs.is_empty() {
                break;
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = timer.tick() => {
                    let now = Instant::now();
                    self.sample(now.duration_since(last_sample));
                    last_sample = now;
                }
                Some(finished) = runs.join_next_with_id(), if !runs.is_empty() => {
                    if let Err(error) = self.finish_run(finished) {
                        failure = Some(error);
                        self.request_shutdown();
                    }
                }
            }
        }
        let db = self.lock().database.take();
        if let Some(db) = db {
            tokio::task::spawn_blocking(move || db.checkpoint(true)).await??;
        }
        failure.map_or(Ok(()), Err)
    }

    fn schedule(self: &Arc<Self>, runs: &mut JoinSet<Result<()>>) -> Result<()> {
        let mut state = self.lock();
        if state.closing {
            return Ok(());
        }
        let mut occupancy: HashMap<_, usize> = HashMap::new();
        for id in state.running.keys() {
            *occupancy
                .entry(state.jobs[id].record.queue_id.clone())
                .or_default() += 1;
        }
        let mut candidates: Vec<_> = state
            .jobs
            .values()
            .filter(|job| job.record.status == TransferStatus::Queued)
            .map(|job| (job.record.created_at, job.record.id.clone()))
            .collect();
        candidates.sort();
        for (_, id) in candidates {
            if runs.len() >= state.concurrency {
                break;
            }
            let queue = state.jobs[&id].record.queue_id.clone();
            let occupied = occupancy.entry(queue.clone()).or_default();
            if *occupied >= state.queues[&queue].max_concurrent_downloads {
                continue;
            }
            let (db, job) = state.job_mut(&id)?;
            let mut record = job.record.clone();
            record.set_status(TransferStatus::Probing);
            db.save_record(&record)?;
            job.record = record;
            let (stop, signal) = watch::channel(false);
            let (done, _) = watch::channel(None);
            let engine = Arc::clone(self);
            let transfer_id = id.clone();
            let task = runs.spawn(async move { engine.download(&transfer_id, signal).await });
            state.running.insert(
                id.clone(),
                RunControl {
                    task: task.id(),
                    stop,
                    done,
                    desired: None,
                },
            );
            *occupied += 1;
            self.status(&id, TransferStatus::Probing);
        }
        let pending: HashSet<_> = state
            .jobs
            .values()
            .filter(|job| is_queued_work(job.record.status))
            .map(|job| job.record.queue_id.clone())
            .collect();
        let drained: Vec<_> = state.armed_queues.difference(&pending).cloned().collect();
        for queue_id in drained {
            state.armed_queues.remove(&queue_id);
            let action = state.queues[&queue_id].post_action;
            let _ = self
                .events
                .send(EngineEvent::QueueDrained { queue_id, action });
        }
        Ok(())
    }

    fn finish_run(
        &self,
        outcome: std::result::Result<(Id, Result<()>), tokio::task::JoinError>,
    ) -> Result<()> {
        let (task, result) = match outcome {
            Ok(outcome) => outcome,
            Err(error) => (error.id(), Err(error.into())),
        };
        let mut state = self.lock();
        let id = state
            .running
            .iter()
            .find(|(_, run)| run.task == task)
            .map(|(id, _)| id.clone())
            .ok_or(EngineError::InvalidState(
                "finished transfer has no run control",
            ))?;
        let run = state.running.remove(&id).unwrap();
        let (status, error) = match result {
            Ok(()) => (TransferStatus::Completed, None),
            Err(_) if run.desired.is_some() => (run.desired.unwrap(), None),
            Err(EngineError::Interrupted) => (TransferStatus::Paused, None),
            Err(error) => (TransferStatus::Failed, Some(error.to_string())),
        };
        let (db, job) = state.job_mut(&id)?;
        if let Err(err) = save_status(db, job, status, error) {
            let _ = run.done.send_replace(Some(Err(err.to_string())));
            return Err(err);
        }
        self.status(&id, status);
        if state.running.is_empty() && self.telemetry.receiver_count() > 0 {
            let _ = self
                .telemetry
                .send(state.telemetry_batch(self.global_limiter.rate()));
        }
        let _ = run.done.send_replace(Some(Ok(status)));
        Ok(())
    }

    fn sample(&self, elapsed: Duration) {
        let mut state = self.lock();
        if state.running.is_empty() {
            return;
        }
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        for job in state.jobs.values_mut() {
            for segment in job.segments.values_mut() {
                let moved = segment.cursor.saturating_sub(segment.sample_cursor);
                segment.sample_cursor = segment.cursor;
                if segment.state == SegmentState::Active {
                    let rate = moved as f64 / seconds;
                    segment.speed = (segment.speed as f64 * 0.7 + rate * 0.3) as u64;
                }
            }
        }
        if self.telemetry.receiver_count() > 0 {
            let _ = self
                .telemetry
                .send(state.telemetry_batch(self.global_limiter.rate()));
        }
    }
}

impl State {
    fn snapshots(&self) -> Vec<TransferSnapshot> {
        let mut snapshots: Vec<_> = self.jobs.values().map(Job::snapshot).collect();
        snapshots.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        snapshots
    }

    fn telemetry_batch(&self, speed_limit_bps: Option<u64>) -> TelemetryBatch {
        let transfers = self.snapshots();
        TelemetryBatch {
            aggregate_down_bps: transfers
                .iter()
                .map(|snapshot| snapshot.download_speed_bps)
                .sum(),
            aggregate_up_bps: 0,
            active_count: self.running.len() as u32,
            active_threads: transfers
                .iter()
                .map(|snapshot| u32::from(snapshot.active_connections))
                .sum(),
            speed_limit_bps,
            transfers,
            timestamp: Utc::now(),
        }
    }
}

impl Job {
    fn snapshot(&self) -> TransferSnapshot {
        let record = &self.record;
        let downloaded_bytes = self.segments.values().map(|s| s.cursor - s.start).sum();
        let download_speed_bps = self.segments.values().map(|s| s.speed).sum();
        let eta_seconds = record.total.and_then(|total| {
            (download_speed_bps != 0).then(|| {
                total
                    .saturating_sub(downloaded_bytes)
                    .div_ceil(download_speed_bps)
            })
        });
        let mut segments: Vec<_> = self
            .segments
            .values()
            .map(|s| SegmentSnapshot {
                index: s.index,
                start_byte: s.start,
                end_byte: s.end.unwrap_or(s.cursor),
                downloaded_bytes: s.cursor - s.start,
                speed_bps: s.speed,
                state: s.state,
            })
            .collect();
        segments.sort_by_key(|segment| segment.start_byte);
        TransferSnapshot {
            id: record.id.clone(),
            kind: TransferKind::Http,
            status: record.status,
            filename: record.filename.clone(),
            source_url: record.source_url.clone(),
            source_domain: record.source_domain.clone(),
            save_dir: record.save_dir.to_string_lossy().into_owned(),
            total_bytes: record.total,
            downloaded_bytes,
            uploaded_bytes: 0,
            download_speed_bps,
            upload_speed_bps: 0,
            eta_seconds,
            active_connections: segments
                .iter()
                .filter(|s| s.state == SegmentState::Active)
                .count() as u16,
            active_peers: 0,
            protocol_label: record.protocol.clone(),
            format_tags: Vec::new(),
            ui_segments: project(&self.segments, record.total, record.status),
            segments,
            sha256_verified: record.sha256_verified,
            resumable: record.ranges
                && record.total.is_some()
                && record.validators.if_range().is_some(),
            queue_id: record.queue_id.clone(),
            error_message: record.error.clone(),
            completion_notice: None,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

fn project(
    segments: &Segments,
    total: Option<u64>,
    status: TransferStatus,
) -> [f32; UI_SEGMENT_COUNT] {
    // Scale byte offsets so every UI bin has the same width even for tiny files.
    let fill = if status == TransferStatus::Completed {
        1.0
    } else {
        0.0
    };
    let Some(total) = total.filter(|total| *total > 0).map(u128::from) else {
        return [fill; UI_SEGMENT_COUNT];
    };
    std::array::from_fn(|index| {
        let left = total * index as u128;
        let right = left + total;
        let overlap: u128 = segments
            .values()
            .map(|segment| {
                let start = u128::from(segment.start) * UI_SEGMENT_COUNT as u128;
                let cursor = u128::from(segment.cursor) * UI_SEGMENT_COUNT as u128;
                cursor.min(right).saturating_sub(start.max(left))
            })
            .sum();
        (overlap as f64 / total as f64).min(1.0) as f32
    })
}

pub fn engine_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
