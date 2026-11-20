mod server;

use crate::report::{ChainDimensions, EvalRecord, Item, Outcome, CHAIN_CATEGORY};
use server::{Action, ChaosServer, CutoffKind};
use sha2::{Digest, Sha256};
use slipstream_engine::{DownloadEngine, EngineConfig};
use slipstream_protocol::{
    AddHttpDownloadRequest, RefreshDownloadUrlRequest, TransferSnapshot, TransferStatus,
};
use std::{
    collections::HashSet,
    fmt::Display,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinSet, time};

pub const CHAIN_COUNT: usize = 108;
const PREFIX_BYTES: usize = 2048;
const SOCKET_DROP_BYTES: usize = 1024;
const OUTPUT_NAME: &str = "download.bin";
const V1: &str = "\"v1\"";
const V2: &str = "\"v2\"";
const TOKEN: &str = "Bearer safety-evaluation-token";
const RAW_REFERRER: &str = "https://fixture:secret@referrer.example.test/watch?clip=one";
const CLEAN_REFERRER: &str = "https://referrer.example.test/watch?clip=one";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Direct,
    PauseReopen,
    ExpireRefresh,
}

impl Lifecycle {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct_complete",
            Self::PauseReopen => "pause_reopen_wal",
            Self::ExpireRefresh => "expire_403_refresh",
        }
    }

    fn initial_path(self) -> &'static str {
        match self {
            Self::ExpireRefresh => "/expired",
            _ => "/download",
        }
    }

    fn final_path(self) -> &'static str {
        match self {
            Self::ExpireRefresh => "/fresh",
            _ => "/download",
        }
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    kib: usize,
    segments: u8,
    drop_socket: bool,
    lifecycle: Lifecycle,
    rotate: bool,
}

impl Scenario {
    fn dimensions(self) -> ChainDimensions {
        ChainDimensions {
            size_tier: format!("{}_kib", self.kib),
            segment_count: self.segments,
            socket_drop_mode: if self.drop_socket {
                "mid_flight_drop"
            } else {
                "none"
            }
            .into(),
            lifecycle_mode: self.lifecycle.label().into(),
            if_range_mode: if self.rotate {
                "rotated_etag"
            } else {
                "matching_etag"
            }
            .into(),
        }
    }

    fn turns(self) -> usize {
        let lifecycle_steps = match self.lifecycle {
            Lifecycle::Direct => 3,
            Lifecycle::PauseReopen => 5,
            Lifecycle::ExpireRefresh => 6,
        };
        lifecycle_steps + usize::from(self.drop_socket)
    }

    fn final_etag(self) -> &'static str {
        if self.rotate {
            V2
        } else {
            V1
        }
    }

    fn drop_start(self) -> usize {
        if !self.rotate && self.lifecycle != Lifecycle::Direct {
            PREFIX_BYTES
        } else {
            0
        }
    }
}

fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for kib in [64, 256, 640] {
        for segments in [4, 8, 16] {
            for drop_socket in [false, true] {
                for lifecycle in [
                    Lifecycle::Direct,
                    Lifecycle::PauseReopen,
                    Lifecycle::ExpireRefresh,
                ] {
                    for rotate in [false, true] {
                        scenarios.push(Scenario {
                            kib,
                            segments,
                            drop_socket,
                            lifecycle,
                            rotate,
                        });
                    }
                }
            }
        }
    }
    scenarios
}

pub async fn run_stateful_chains() -> io::Result<Vec<EvalRecord>> {
    let root = TempRoot::new()?;
    let permits = Arc::new(Semaphore::new(8));
    let mut running = JoinSet::new();
    let mut grid = HashSet::new();
    for (index, scenario) in scenarios().into_iter().enumerate() {
        let dimensions = scenario.dimensions();
        assert!(
            grid.insert(dimensions.clone()),
            "duplicate stateful chain dimensions"
        );
        let item = Item {
            id: format!("stateful-chain-{:03}", index + 1),
            category: CHAIN_CATEGORY,
            pair_type: CHAIN_CATEGORY,
            surface: "http_download_engine",
            difficulty: "lifecycle",
        };
        let directory = root.0.join(&item.id);
        let permits = Arc::clone(&permits);
        running.spawn(async move {
            let _permit = permits.acquire_owned().await.expect("concurrency semaphore closed");
            let mut task = tokio::spawn(run_chain(scenario, directory));
            let result = match time::timeout(Duration::from_secs(12), &mut task).await {
                Ok(Ok(Ok(()))) => Outcome::Pass("The completed download matched the independent integrity and lifecycle checks."),
                Ok(Ok(Err(ChainError::Failed(reason)))) => Outcome::Fail(reason),
                Ok(Ok(Err(ChainError::Broken(reason)))) => Outcome::Broken(reason),
                Ok(Err(error)) => Outcome::Broken(format!("chain task failed: {error}")),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Outcome::Broken("chain exceeded its 12-second timeout".into())
                }
            };
            item.record(result, scenario.turns(), Some(dimensions))
        });
    }
    assert_eq!(grid.len(), CHAIN_COUNT);
    let mut records = Vec::with_capacity(CHAIN_COUNT);
    while let Some(result) = running.join_next().await {
        records.push(result.map_err(io::Error::other)?);
    }
    records.sort_unstable_by(|a, b| a.item_id.cmp(&b.item_id));
    Ok(records)
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("slipstream-eval-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "Could not remove evaluation directory {} ({error})",
                self.0.display()
            );
        }
    }
}

enum ChainError {
    Failed(String),
    Broken(String),
}

type ChainResult<T> = Result<T, ChainError>;

fn target<T>(operation: &str, result: Result<T, impl Display>) -> ChainResult<T> {
    result.map_err(|error| ChainError::Failed(format!("{operation}: {error}")))
}

fn fixture<T>(operation: &str, result: Result<T, impl Display>) -> ChainResult<T> {
    result.map_err(|error| ChainError::Broken(format!("{operation}: {error}")))
}

fn check(condition: bool, reason: impl Into<String>) -> ChainResult<()> {
    if condition {
        Ok(())
    } else {
        Err(ChainError::Failed(reason.into()))
    }
}

async fn run_chain(scenario: Scenario, directory: PathBuf) -> ChainResult<()> {
    let save_dir = directory.join("downloads");
    fixture(
        "create isolated download directory",
        fs::create_dir_all(&save_dir),
    )?;
    let mut server = fixture("start loopback fixture", ChaosServer::start(scenario).await)?;
    let config = EngineConfig {
        db_path: directory.join("transfer.sqlite"),
        default_download_dir: save_dir.clone(),
        allow_loopback: true,
        request_timeout: Duration::from_secs(5),
        ..EngineConfig::default()
    };
    let server_state = Arc::clone(&server.state);
    let exercise = async {
        let expected = server_state.payload(scenario.final_etag());
        let digest = format!("{:x}", Sha256::digest(expected));
        exercise_engine(scenario, &server_state, config, &digest).await?;
        verify_requests(scenario, &server_state)?;
        verify_output(
            &save_dir,
            expected,
            &digest,
            &server_state.url(scenario.lifecycle.final_path()),
            scenario.lifecycle == Lifecycle::ExpireRefresh,
        )
    };
    tokio::select! {
        result = exercise => result,
        result = &mut server.task => Err(ChainError::Broken(match result {
            Ok(Ok(())) => "loopback server stopped early".into(),
            Ok(Err(error)) => format!("loopback fixture failed: {error}"),
            Err(error) => format!("loopback server task failed: {error}"),
        })),
    }
}

async fn exercise_engine(
    scenario: Scenario,
    server: &server::State,
    config: EngineConfig,
    digest: &str,
) -> ChainResult<()> {
    let engine = target("open engine", DownloadEngine::new(config.clone()).await)?;
    let initial = initial_stage(scenario, server, &engine, digest).await;
    let closed = engine.shutdown().await;
    drop(engine);
    let (id, paused) = initial?;
    target("close initial engine and WAL", closed)?;
    let Some(paused) = paused else {
        return Ok(());
    };

    let engine = target(
        "reopen engine from SQLite",
        DownloadEngine::new(config).await,
    )?;
    let resumed = async {
        let reopened = target("read reopened snapshot", engine.snapshot(&id))?;
        check(
            reopened.status == TransferStatus::Paused
                && reopened.resumable
                && reopened.downloaded_bytes == paused.downloaded_bytes
                && reopened.source_url == paused.source_url
                && reopened.segments.len() == paused.segments.len()
                && reopened
                    .segments
                    .iter()
                    .zip(&paused.segments)
                    .all(|(left, right)| {
                        (left.start_byte, left.end_byte, left.downloaded_bytes)
                            == (right.start_byte, right.end_byte, right.downloaded_bytes)
                    }),
            format!("WAL reopen lost paused status, source, or persisted cursors: {reopened:?}"),
        )?;
        server.release.send_replace(true);
        target(
            "resume persisted download",
            engine.resume_transfer(&id).await,
        )?;
        let completed = wait_for(&engine, &id, server, terminal).await?;
        verify_completed(&completed, scenario)
    }
    .await;
    let closed = engine.shutdown().await;
    resumed?;
    target("close resumed engine", closed)?;
    Ok(())
}

async fn initial_stage(
    scenario: Scenario,
    server: &server::State,
    engine: &DownloadEngine,
    digest: &str,
) -> ChainResult<(String, Option<TransferSnapshot>)> {
    let id = target(
        "queue isolated download",
        engine
            .add_http_download(AddHttpDownloadRequest {
                url: server.url(scenario.lifecycle.initial_path()),
                filename: Some(OUTPUT_NAME.into()),
                save_dir: None,
                max_connections: Some(scenario.segments),
                queue_id: None,
                headers: Default::default(),
                referrer: None,
                expected_sha256: Some(digest.into()),
            })
            .await,
    )?;

    match scenario.lifecycle {
        Lifecycle::Direct => {
            let completed = wait_for(engine, &id, server, terminal).await?;
            verify_completed(&completed, scenario)?;
            return Ok((id, None));
        }
        Lifecycle::PauseReopen => {
            let active = wait_for(engine, &id, server, |snapshot| {
                snapshot.status == TransferStatus::Active
                    && snapshot.downloaded_bytes >= PREFIX_BYTES as u64
            })
            .await?;
            check(
                active.status == TransferStatus::Active
                    && active.segments.len() == usize::from(scenario.segments),
                format!(
                    "expected active partial download on the requested segmentation, got {:?}",
                    active.status
                ),
            )?;
            target("pause active transfer", engine.pause_transfer(&id).await)?;
        }
        Lifecycle::ExpireRefresh => {
            let failed = wait_for(engine, &id, server, terminal).await?;
            check(failed.status == TransferStatus::Failed
                    && failed.error_message.as_ref().is_some_and(|why| why.contains("403"))
                    && failed.downloaded_bytes == PREFIX_BYTES as u64
                    && failed.segments.len() == usize::from(scenario.segments),
                format!("expected 403 after exactly {PREFIX_BYTES} persisted bytes, got {:?} at {} bytes ({:?})",
                    failed.status, failed.downloaded_bytes, failed.error_message))?;
            target(
                "refresh expired URL",
                engine
                    .refresh_download_address(RefreshDownloadUrlRequest {
                        transfer_id: id.clone(),
                        new_url: server.url("/fresh"),
                        headers: [("Authorization".into(), TOKEN.into())].into(),
                        referrer: Some(RAW_REFERRER.into()),
                    })
                    .await,
            )?;
        }
    }

    let paused = target("read paused snapshot", engine.snapshot(&id))?;
    check(
        paused.status == TransferStatus::Paused
            && paused.resumable
            && paused.downloaded_bytes == PREFIX_BYTES as u64,
        format!(
            "expected exactly {PREFIX_BYTES} resumable paused bytes, got {:?} at {} bytes",
            paused.status, paused.downloaded_bytes
        ),
    )?;
    Ok((id, Some(paused)))
}

fn terminal(snapshot: &TransferSnapshot) -> bool {
    matches!(
        snapshot.status,
        TransferStatus::Completed | TransferStatus::Failed | TransferStatus::Canceled
    )
}

async fn wait_for(
    engine: &DownloadEngine,
    id: &str,
    server: &server::State,
    ready: impl Fn(&TransferSnapshot) -> bool,
) -> ChainResult<TransferSnapshot> {
    let mut events = engine.subscribe_events();
    let mut poll = time::interval(Duration::from_millis(2));
    loop {
        // Read the pending cutoff before the snapshot so a new representation's
        // cutoff cannot be released by a cursor from the previous representation.
        let pending = *server.pending_cutoff.borrow();
        let snapshot = target("inspect download snapshot", engine.snapshot(id))?;
        if let Some(cutoff) = pending {
            if snapshot.status == TransferStatus::Active
                && snapshot.segments.iter().any(|segment| {
                    segment.start_byte == 0 && segment.downloaded_bytes >= cutoff.cursor() as u64
                })
            {
                server.persisted.send_if_modified(|current| {
                    let changed = *current != Some(cutoff.kind);
                    *current = Some(cutoff.kind);
                    changed
                });
            }
        }
        if ready(&snapshot) || terminal(&snapshot) {
            return Ok(snapshot);
        }
        tokio::select! {
            _ = poll.tick() => {},
            _ = events.recv() => {},
        }
    }
}

fn verify_completed(snapshot: &TransferSnapshot, scenario: Scenario) -> ChainResult<()> {
    check(
        snapshot.status == TransferStatus::Completed
            && snapshot.sha256_verified == Some(true)
            && snapshot.downloaded_bytes == (scenario.kib * 1024) as u64
            && snapshot.filename == OUTPUT_NAME,
        format!("expected a verified complete download, got {:?} at {} bytes (SHA-256 {:?}, error {:?})",
            snapshot.status, snapshot.downloaded_bytes, snapshot.sha256_verified, snapshot.error_message),
    )
}

fn verify_requests(scenario: Scenario, server: &server::State) -> ChainResult<()> {
    let logs = server
        .observations
        .lock()
        .expect("loopback evidence lock poisoned");
    let path = scenario.lifecycle.final_path();
    check(
        logs.first().is_some_and(|obs| {
            obs.action == Action::Head
                && obs.etag == V1
                && obs.request.path == scenario.lifecycle.initial_path()
        }),
        "transfer did not start with a HEAD probe for the original representation",
    )?;
    let completed_ranges = logs.iter().filter(|obs| {
        obs.request.path == path
            && obs.etag == scenario.final_etag()
            && obs.request.range.is_some()
            && matches!(
                obs.action,
                Action::ServeRange | Action::Cutoff(CutoffKind::SocketDrop)
            )
    });
    let segment_size = scenario.kib * 1024 / usize::from(scenario.segments);
    for start in (0..scenario.kib * 1024).step_by(segment_size) {
        let effective_start =
            if start == 0 && !scenario.rotate && scenario.lifecycle != Lifecycle::Direct {
                PREFIX_BYTES
            } else {
                start
            };
        check(
            completed_ranges
                .clone()
                .any(|obs| obs.request.range == Some((effective_start, start + segment_size - 1))),
            format!(
                "missing final representation request for configured segment at {effective_start}"
            ),
        )?;
    }
    if scenario.lifecycle != Lifecycle::Direct {
        check(
            logs.iter().any(|obs| {
                obs.request.path == path
                    && obs.request.if_range.as_deref() == Some(V1)
                    && obs.request.range == Some((PREFIX_BYTES, segment_size - 1))
            }),
            "reopened transfer did not send the persisted cursor and original If-Range",
        )?;
    }
    if scenario.lifecycle == Lifecycle::ExpireRefresh {
        check(logs.iter().any(|obs| obs.action == Action::Cutoff(CutoffKind::Expiration)
                && obs.request.path == "/expired" && obs.request.range == Some((0, segment_size - 1)))
            && logs.iter().any(|obs| obs.action == Action::Forbidden
                && obs.request.path == "/expired" && obs.request.range == Some((PREFIX_BYTES, segment_size - 1))
                && obs.request.if_range.as_deref() == Some(V1)),
            "expired endpoint did not cut the first worker and return 403 on its persisted-cursor retry")?;
        check(
            logs.iter()
                .filter(|obs| obs.request.path == "/fresh")
                .all(|obs| {
                    obs.request.authorization.as_deref() == Some(TOKEN)
                        && obs.request.referer.as_deref() == Some(CLEAN_REFERRER)
                        && obs.request.range.is_some()
                }),
            "refreshed requests lost Authorization, sanitized Referer, or Range after reopen",
        )?;
    }
    if scenario.rotate {
        let reprobe = logs
            .iter()
            .position(|obs| obs.action == Action::Probe && obs.etag == V2);
        check(
            reprobe.is_some_and(|index| {
                logs[..index].iter().any(|obs| {
                    if scenario.lifecycle == Lifecycle::Direct {
                        obs.action == Action::ServeRange
                            && obs.etag == V2
                            && obs.request.if_range.is_none()
                    } else {
                        obs.action == Action::ValidatorMismatch
                            && obs.request.if_range.as_deref() == Some(V1)
                    }
                })
            }),
            "rotated validator did not cause a fresh probe after the representation changed",
        )?;
    } else {
        check(
            logs.iter().all(|obs| obs.action != Action::Probe),
            "matching validator unexpectedly caused a fresh range probe",
        )?;
    }
    let drops: Vec<_> = logs
        .iter()
        .enumerate()
        .filter(|(_, obs)| obs.action == Action::Cutoff(CutoffKind::SocketDrop))
        .collect();
    check(
        drops.len() == usize::from(scenario.drop_socket),
        format!(
            "expected {} injected socket drop(s), observed {}",
            usize::from(scenario.drop_socket),
            drops.len()
        ),
    )?;
    if let Some(&(index, drop)) = drops.first() {
        let (start, end) = drop.request.range.expect("socket cutoff must be ranged");
        check(
            logs[index + 1..].iter().any(|obs| {
                obs.request.path == path
                    && obs.etag == scenario.final_etag()
                    && obs.request.range == Some((start + SOCKET_DROP_BYTES, end))
                    && obs.request.if_range.as_deref() == Some(scenario.final_etag())
            }),
            "socket drop recovery did not retry from the persisted cursor with If-Range",
        )?;
    }
    Ok(())
}

fn verify_output(
    save_dir: &Path,
    expected: &[u8],
    digest: &str,
    source: &str,
    expect_referrer: bool,
) -> ChainResult<()> {
    let final_path = save_dir.join(OUTPUT_NAME);
    let actual = target("read committed download", fs::read(&final_path))?;
    let actual_hash = format!("{:x}", Sha256::digest(&actual));
    check(actual_hash == digest && actual == expected,
        format!("committed bytes or SHA-256 differed from the oracle (expected {digest}, got {actual_hash})"))?;
    verify_motw(&final_path, source, expect_referrer)?;
    let entries = target("list output directory", fs::read_dir(save_dir))?;
    let names = target(
        "read output entries",
        entries
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>(),
    )?;
    check(
        names.len() == 1 && names[0] == OUTPUT_NAME,
        format!("unexpected output or orphan part files: {names:?}"),
    )
}

#[cfg(windows)]
fn verify_motw(path: &Path, source: &str, expect_referrer: bool) -> ChainResult<()> {
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    let zone = target("read Mark-of-the-Web ADS", fs::read_to_string(ads))?;
    let host = format!("HostUrl={source}");
    let expected_referrer = format!("ReferrerUrl={CLEAN_REFERRER}");
    let referrers: Vec<_> = zone
        .lines()
        .filter(|line| line.starts_with("ReferrerUrl="))
        .collect();
    check(
        zone.lines().next() == Some("[ZoneTransfer]")
            && zone.lines().any(|line| line == "ZoneId=3")
            && zone.lines().any(|line| line == host)
            && if expect_referrer {
                referrers == [expected_referrer.as_str()]
            } else {
                referrers.is_empty()
            },
        format!("unexpected Mark-of-the-Web ADS: {zone:?}"),
    )
}

#[cfg(not(windows))]
fn verify_motw(_path: &Path, _source: &str, _expect_referrer: bool) -> ChainResult<()> {
    Ok(())
}
