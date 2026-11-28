use crate::http::{
    connect_rtt_ms, protocol, ContentCoding, FetchProfile, GetOutcome, Http, Metadata, ProbeMode,
    RestartReason, Source, Validators,
};
use crate::limiter::Limiter;
use crate::output;
use crate::storage::{Record, Segment, Segments};
use crate::{EngineError, EngineEvent, Inner, Result, SegmentState, TransferStatus};
use chrono::Utc;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::{header::AUTHORIZATION, Response, StatusCode};
use std::future::Future;
use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{watch, OwnedMutexGuard};
use tokio::time::Instant;

const SPLIT_THRESHOLD: u64 = 512 * 1024;
const WRITE_SIZE: usize = 16 * 1024;
const _: () = assert!(WRITE_SIZE as u64 <= SPLIT_THRESHOLD / 2);
const RETRIES: u32 = 6;
const RESTARTS: usize = 3;

struct Run<'a> {
    engine: &'a Inner,
    id: &'a str,
    source_domain: String,
    source: Source,
    source_index: usize,
    expected_mirror_size: Option<u64>,
    more_sources: bool,
    http: Arc<Http>,
    limiter: Arc<Limiter>,
    queue_limiter: Arc<Limiter>,
    profile: Mutex<FetchProfile>,
    concurrency: Mutex<ConcurrencyState>,
    concurrency_changed: watch::Sender<()>,
    has_written: AtomicBool,
    range_fallback_tried: AtomicBool,
}

struct ConcurrencyState {
    effective_max_workers: usize,
    active_workers: usize,
    retiring_workers: usize,
    retry_at: Option<Instant>,
    retired_on_congestion: bool,
}

struct Plan {
    part: PathBuf,
    total: Option<u64>,
    ranges: bool,
    conditional: bool,
    indices: Vec<u8>,
    response: Option<Response>,
}

enum SegmentOutcome {
    Complete,
    Retired,
    Rebalance,
}

#[derive(Clone)]
struct Stop {
    transfer: watch::Receiver<bool>,
    group: watch::Receiver<bool>,
}

impl Stop {
    fn check(&self) -> Result<()> {
        if *self.transfer.borrow() || *self.group.borrow() {
            return Err(EngineError::Interrupted);
        }
        Ok(())
    }

    async fn during<T>(&mut self, future: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.transfer.wait_for(|requested| *requested) => Err(EngineError::Interrupted),
            _ = self.group.wait_for(|requested| *requested) => Err(EngineError::Interrupted),
            result = future => result,
        }
    }

    async fn wait(&mut self, delay: Duration) -> Result<()> {
        self.during(async {
            tokio::time::sleep(delay).await;
            Ok(())
        })
        .await
    }
}

impl Inner {
    pub(crate) async fn download(
        &self,
        id: &str,
        signal: watch::Receiver<bool>,
    ) -> Result<OwnedMutexGuard<()>> {
        let (_probe_stop, probe_signal) = watch::channel(false);
        let mut stop = Stop {
            transfer: signal,
            group: probe_signal,
        };
        stop.check()?;
        let (record, segments, limiter, queue_limiter) = {
            let state = self.lock();
            let job = state.job(id)?;
            (
                job.record.clone(),
                job.segments.clone(),
                Arc::clone(&job.limiter),
                Arc::clone(&state.queue_limiters[&job.record.queue_id]),
            )
        };
        let primary = Source::new(
            &record.source_url,
            &record.headers,
            record.referrer.as_deref(),
            self.config.allow_loopback,
        )?;
        let http = Arc::clone(&self.http.read().unwrap());
        let authorization = if primary.headers.contains_key(AUTHORIZATION) {
            None
        } else {
            self.site_authorization.read().unwrap().clone()
        };
        let mut result = Err(EngineError::InvalidState("download has no source"));
        for (index, mirror) in std::iter::once(None)
            .chain(record.mirror_urls.iter().map(Some))
            .enumerate()
        {
            stop.check()?;
            let mut source = match mirror {
                Some(url) => primary.for_mirror(url, self.config.allow_loopback)?,
                None => primary.clone(),
            };
            if source.url.scheme() == "https" {
                if let Some(header) = authorization
                    .as_ref()
                    .and_then(|lookup| lookup(&source.url))
                {
                    source.headers.insert(AUTHORIZATION, header);
                }
            }
            let source_domain = source.url.host_str().unwrap_or_default().to_owned();
            let initial_workers = self.learned_host_cap(&source_domain).map_or_else(
                || usize::from(record.max_connections),
                |cap| cap.min(usize::from(record.max_connections)).max(1),
            );
            let run = Run {
                engine: self,
                id,
                source_domain,
                source,
                source_index: index,
                expected_mirror_size: mirror.and(record.expected_size_bytes),
                more_sources: index < record.mirror_urls.len(),
                http: Arc::clone(&http),
                limiter: Arc::clone(&limiter),
                queue_limiter: Arc::clone(&queue_limiter),
                profile: Mutex::new(FetchProfile::default()),
                concurrency: Mutex::new(ConcurrencyState {
                    effective_max_workers: initial_workers,
                    active_workers: 0,
                    retiring_workers: 0,
                    retry_at: None,
                    retired_on_congestion: false,
                }),
                concurrency_changed: watch::channel(()).0,
                has_written: AtomicBool::new(false),
                range_fallback_tried: AtomicBool::new(false),
            };
            let resume = if index == 0 {
                run.resume_plan(&record, &segments).await?
            } else {
                None
            };
            let resumed = resume.is_some();
            result = run.download(&mut stop, resume).await;
            if resumed
                || run.has_written.load(Ordering::Relaxed)
                || !result.as_ref().is_err_and(|error| {
                    error.is_transient()
                        || matches!(
                            error,
                            EngineError::Status {
                                status: StatusCode::NOT_FOUND | StatusCode::FORBIDDEN,
                                ..
                            } | EngineError::InvalidResponse(
                                "mirror size differs from expected size"
                            )
                        )
                })
            {
                break;
            }
        }
        result
    }
}

impl Run<'_> {
    async fn download(&self, stop: &mut Stop, resume: Option<Plan>) -> Result<OwnedMutexGuard<()>> {
        let mut plan = match resume {
            Some(plan) => plan,
            None => self.fresh_plan(stop, ProbeMode::HeadThenRange).await?,
        };
        for restarts in 0..=RESTARTS {
            match self.execute_plan(&mut plan, stop).await {
                Ok(None) => return self.finalize(stop).await,
                Ok(Some(reason)) if restarts < RESTARTS => {
                    let mode = if reason == RestartReason::RangeUnsupported || !plan.ranges {
                        ProbeMode::Full
                    } else {
                        ProbeMode::Range
                    };
                    plan = self.fresh_plan(stop, mode).await?;
                }
                Ok(Some(_)) => return Err(EngineError::RestartRequired),
                Err(error) => {
                    if matches!(error, EngineError::Interrupted) {
                        output::sync_part(&plan.part).await?;
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("download loop returns after its final attempt")
    }

    async fn resume_plan(&self, record: &Record, segments: &Segments) -> Result<Option<Plan>> {
        let has_progress = segments
            .values()
            .any(|segment| segment.cursor > segment.start);
        if record.representation_source != self.source_index
            || !record.ranges
            || !has_progress
            || record.total.is_none()
            || record.validators.if_range().is_none()
        {
            return Ok(None);
        }
        let (part, _) = record.paths()?;
        let path = part.clone();
        let length =
            tokio::task::spawn_blocking(move || output::regular_file_length(&path)).await??;
        if length != record.total {
            return Ok(None);
        }
        Ok(Some(Plan {
            part,
            total: record.total,
            ranges: true,
            conditional: true,
            indices: segments
                .values()
                .filter(|segment| segment.end != Some(segment.cursor))
                .map(|segment| segment.index)
                .collect(),
            response: None,
        }))
    }

    async fn fetch<T, F: Future<Output = Result<T>>>(
        &self,
        probing: bool,
        mut request: impl FnMut(FetchProfile) -> F,
    ) -> Result<T> {
        let initial = *self.profile.lock().unwrap();
        let mut profile = initial;
        let mut result = request(profile).await;
        let mut ipv4_exhausted = false;
        if result
            .as_ref()
            .is_err_and(|error| profile.ipv4_unavailable(error))
        {
            ipv4_exhausted = true;
            profile.ipv4_only = false;
            result = request(profile).await;
        }
        if let Err(error) = &result {
            if profile.should_fallback_http1(error)
                && (probing
                    || (!self.has_written.load(Ordering::Relaxed) && self.active_workers() <= 1))
            {
                let mut h1_profile = profile;
                h1_profile.http1_only = true;
                let attempted = request(h1_profile).await;
                if attempted.is_ok() || matches!(attempted, Err(EngineError::Status { .. })) {
                    profile = h1_profile;
                }
                result = attempted;
            }
        }
        while let Err(EngineError::Status {
            status: StatusCode::FORBIDDEN,
            sent_cookie,
            ..
        }) = &result
        {
            // A parallel rejection first reduces concurrency. Only a single worker
            // opening the representation can change its request profile.
            if !probing && (self.has_written.load(Ordering::Relaxed) || self.active_workers() > 1) {
                break;
            }
            let Some(next) = profile.fallback(&self.source, *sent_cookie, ipv4_exhausted) else {
                break;
            };
            let attempted = request(next).await;
            if attempted
                .as_ref()
                .is_err_and(|error| next.ipv4_unavailable(error))
            {
                // Keep the 403 that triggered fallback and try header changes on any family.
                ipv4_exhausted = true;
                profile.ipv4_only = false;
                continue;
            }
            profile = next;
            result = attempted;
        }
        if result.is_ok() && profile != initial {
            let mut winning = self.profile.lock().unwrap();
            if *winning == initial {
                *winning = profile;
            }
        }
        result
    }

    fn active_workers(&self) -> usize {
        self.concurrency.lock().unwrap().active_workers
    }

    async fn wait_for_server(&self, stop: &mut Stop) -> Result<()> {
        loop {
            let deadline = self.concurrency.lock().unwrap().retry_at;
            let Some(delay) = deadline.and_then(|time| time.checked_duration_since(Instant::now()))
            else {
                return Ok(());
            };
            stop.wait(delay).await?;
        }
    }

    async fn probe(&self, stop: &mut Stop, mode: ProbeMode) -> Result<Metadata> {
        self.wait_for_server(stop).await?;
        for retry in 0..=RETRIES {
            let response = stop
                .during(self.fetch(true, |profile| self.http.probe(&self.source, profile, mode)))
                .await;
            match response {
                Err(error) if error.is_transient() && retry < RETRIES && !self.more_sources => {
                    stop.wait(retry_delay(retry, true, &error)).await?
                }
                response => return response,
            }
        }
        unreachable!("probe loop returns after its final attempt")
    }

    async fn fresh_plan(&self, stop: &mut Stop, mode: ProbeMode) -> Result<Plan> {
        let meta = self.probe(stop, mode).await?;
        if self
            .expected_mirror_size
            .is_some_and(|size| meta.total != Some(size))
        {
            return Err(EngineError::InvalidResponse(
                "mirror size differs from expected size",
            ));
        }
        if mode == ProbeMode::Full
            && !meta
                .response
                .as_ref()
                .is_some_and(|response| response.status().is_success())
        {
            return Err(EngineError::InvalidResponse(
                "full probe did not return a successful GET",
            ));
        }
        stop.check()?;
        self.has_written.store(false, Ordering::Relaxed);
        let plan = {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let mut record = job.record.clone();
            if let Some(filename) = meta.filename.filter(|_| record.use_server_filename) {
                record.filename = filename;
                record.use_server_filename = false;
            }
            let reprobe = record.status != TransferStatus::Probing;
            record.set_status(TransferStatus::Probing);
            record.representation_source = self.source_index;
            record.total = meta.total;
            record.ranges = meta.ranges && meta.total.is_some();
            record.validators = meta.validators;
            record.protocol = meta.protocol.into();
            record.sha256_verified = None;
            record.commit_digest = None;
            let (part, _) = record.paths()?;
            let segments = partition(record.total, record.ranges, record.max_connections);
            let indices = segments
                .values()
                .filter(|s| s.state != SegmentState::Completed)
                .map(|s| s.index)
                .collect();
            let destroys_progress = job
                .segments
                .values()
                .any(|segment| segment.cursor > segment.start);
            // Clear progress in SQLite before performing the destructive reset.
            db.replace_segments(&record, &segments)?;
            let plan = Plan {
                part,
                total: record.total,
                ranges: record.ranges,
                conditional: false,
                indices,
                response: meta.response,
            };
            job.record = record;
            job.segments = segments;
            job.connect_rtt_ms = job.connect_rtt_ms.or(meta.connect_rtt_ms);
            if destroys_progress {
                db.checkpoint(false)?;
            }
            if reprobe {
                self.engine.status(self.id, TransferStatus::Probing);
            }
            plan
        };
        let part = plan.part.clone();
        let total = plan.total;
        tokio::task::spawn_blocking(move || output::reset_part(&part, total)).await??;
        Ok(plan)
    }

    async fn execute_plan(
        &self,
        plan: &mut Plan,
        stop: &mut Stop,
    ) -> Result<Option<RestartReason>> {
        stop.check()?;
        let worker_count = {
            let mut concurrency = self.concurrency.lock().unwrap();
            let worker_count = plan.indices.len().min(concurrency.effective_max_workers);
            concurrency.active_workers = worker_count;
            concurrency.retiring_workers = 0;
            concurrency.retired_on_congestion = false;
            worker_count
        };
        let indices = &plan.indices[..worker_count];
        self.activate(indices)?;
        if plan.indices.is_empty() {
            if plan.conditional && plan.total.is_some_and(|total| total > 0) {
                // A paused transfer can already have all its bytes on disk.
                // Verify its old representation before publishing the part file.
                let validators = self.validators()?;
                for retry in 0..=RETRIES {
                    match stop
                        .during(self.fetch(false, |profile| {
                            self.http.get_or_restart(
                                &self.source,
                                profile,
                                Some((0, 0)),
                                plan.total,
                                &validators,
                                true,
                            )
                        }))
                        .await
                    {
                        Err(error) if error.is_transient() && retry < RETRIES => {
                            stop.wait(retry_delay(retry, true, &error)).await?;
                        }
                        Ok(GetOutcome::Response(_)) => break,
                        Ok(GetOutcome::Restart(reason)) => return Ok(Some(reason)),
                        Err(error) => return Err(error),
                    }
                }
            }
            return Ok(None);
        }
        let mut response = plan.response.take();
        let restart = Mutex::new(None);
        let (group_stop, group) = watch::channel(false);
        let mut workers = FuturesUnordered::new();
        for (worker_index, &index) in indices.iter().enumerate() {
            let stagger = if self.engine.config.allow_loopback {
                Duration::ZERO
            } else {
                Duration::from_millis(25 * worker_index as u64)
            };
            workers.push(self.worker(
                plan,
                index,
                &restart,
                Stop {
                    transfer: stop.transfer.clone(),
                    group: group.clone(),
                },
                response.take(),
                stagger,
            ));
        }
        let mut failure = None;
        while let Some(result) = workers.next().await {
            let retired = matches!(result, Ok(true));
            {
                let mut concurrency = self.concurrency.lock().unwrap();
                concurrency.active_workers = concurrency.active_workers.saturating_sub(1);
                if retired {
                    concurrency.retiring_workers = concurrency.retiring_workers.saturating_sub(1);
                }
            }
            if let Err(error) = result {
                if failure.is_none()
                    || (matches!(failure, Some(EngineError::Interrupted))
                        && !matches!(error, EngineError::Interrupted))
                {
                    failure = Some(error);
                }
                group_stop.send_replace(true);
            }
        }
        if matches!(failure, Some(EngineError::RestartRequired)) {
            let reason = (*restart.lock().unwrap()).unwrap_or(RestartReason::RepresentationChanged);
            return Ok(Some(reason));
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let all_complete = self
            .engine
            .lock()
            .job(self.id)?
            .segments
            .values()
            .all(|segment| {
                segment.state == SegmentState::Completed && segment.end == Some(segment.cursor)
            });
        if !all_complete {
            return Err(EngineError::InvalidState(
                "transfer stopped before all segments completed",
            ));
        }
        Ok(None)
    }

    fn activate(&self, indices: &[u8]) -> Result<()> {
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let mut record = job.record.clone();
        let mut segments = job.segments.clone();
        record.set_status(TransferStatus::Active);
        for segment in segments.values_mut() {
            segment.state = if segment.end == Some(segment.cursor) {
                SegmentState::Completed
            } else {
                SegmentState::Pending
            };
            segment.speed = 0;
        }
        for index in indices {
            let segment = segments.get_mut(index).unwrap();
            segment.state = SegmentState::Active;
        }
        db.replace_segments(&record, &segments)?;
        job.record = record;
        job.segments = segments;
        self.engine.status(self.id, TransferStatus::Active);
        Ok(())
    }

    async fn worker(
        &self,
        plan: &Plan,
        mut index: u8,
        restart: &Mutex<Option<RestartReason>>,
        mut stop: Stop,
        mut response: Option<Response>,
        stagger: Duration,
    ) -> Result<bool> {
        if !stagger.is_zero() {
            stop.wait(stagger).await?;
        }
        let mut file = OpenOptions::new().write(true).open(&plan.part).await?;
        loop {
            stop.check()?;
            let outcome = self
                .download_segment(plan, index, &mut file, restart, &mut stop, response.take())
                .await?;
            let (next_index, retired) = match outcome {
                SegmentOutcome::Complete => {
                    stop.check()?;
                    self.finish_segment(index, plan.ranges)?
                }
                SegmentOutcome::Rebalance => {
                    stop.check()?;
                    self.reassign_segment(index)?
                }
                SegmentOutcome::Retired => return Ok(true),
            };
            if retired {
                return Ok(true);
            }
            let Some(next_index) = next_index else {
                return Ok(false);
            };
            index = next_index;
        }
    }

    async fn download_segment(
        &self,
        plan: &Plan,
        index: u8,
        file: &mut File,
        restart: &Mutex<Option<RestartReason>>,
        stop: &mut Stop,
        mut probe_response: Option<Response>,
    ) -> Result<SegmentOutcome> {
        for retry in 0..=RETRIES {
            stop.check()?;
            let (range, total, validators, conditional, untouched) = {
                let state = self.engine.lock();
                let job = state.job(self.id)?;
                let segment = &job.segments[&index];
                if plan.ranges && segment.end == Some(segment.cursor) {
                    return Ok(SegmentOutcome::Complete);
                }
                let range = plan
                    .ranges
                    .then(|| (segment.cursor, segment.end.unwrap() - 1));
                let whole_transfer_untouched = !self.has_written.load(Ordering::Relaxed)
                    && job.segments.values().all(|item| item.cursor == item.start);
                (
                    range,
                    job.record.total,
                    job.record.validators.clone(),
                    plan.conditional || segment.cursor > segment.start,
                    whole_transfer_untouched,
                )
            };
            let result = async {
                let outcome = if let Some(response) = probe_response
                    .take()
                    .filter(|_| index == 0 && range.is_none() && untouched)
                {
                    GetOutcome::Response(response)
                } else {
                    self.wait_for_server(stop).await?;
                    let mut concurrency_changed = self.concurrency_changed.subscribe();
                    let outcome = stop.during(async {
                        tokio::select! {
                            biased;
                            result = self.fetch(false, |profile| {
                                self.http.get_or_restart(
                                    &self.source, profile, range, total, &validators, conditional,
                                )
                            }) => result.map(Some),
                            _ = concurrency_changed.changed() => Ok(None),
                        }
                    }).await?;
                    let Some(outcome) = outcome else {
                        return Ok(SegmentOutcome::Rebalance);
                    };
                    outcome
                };
                let response = match outcome {
                    GetOutcome::Response(response) => response,
                    GetOutcome::Restart(reason) => {
                        record_restart(restart, reason);
                        return Err(EngineError::RestartRequired);
                    }
                };
                if let Err(error) = self.note_response(&response, conditional) {
                    if matches!(error, EngineError::RestartRequired) {
                        record_restart(restart, RestartReason::RepresentationChanged);
                    }
                    return Err(error);
                }
                let coding = if plan.ranges {
                    ContentCoding::Identity
                } else {
                    ContentCoding::from_response(&response)?
                };
                if coding != ContentCoding::Identity && total.is_some() {
                    self.clear_stream_length(index, file).await?;
                }
                file.seek(SeekFrom::Start(range.map_or(0, |(start, _)| start)))
                    .await?;
                self.write_response(coding.reader(response), index, plan.ranges, file, stop)
                    .await?;
                Ok(SegmentOutcome::Complete)
            }
            .await;
            let Err(error) = result else {
                return result;
            };
            if self.retire_on_congestion(index, &error)? {
                return Ok(SegmentOutcome::Retired);
            }
            let (single_worker, can_replace_ranges, post_backdown_grace) = {
                let concurrency = self.concurrency.lock().unwrap();
                // Keep partial ranges resumable when congestion retired a worker.
                (
                    concurrency.active_workers <= concurrency.retiring_workers + 1,
                    untouched || !concurrency.retired_on_congestion,
                    concurrency.retired_on_congestion,
                )
            };
            if single_worker
                && plan.ranges
                && can_replace_ranges
                && matches!(
                    error,
                    EngineError::Status { status, .. }
                        if status.as_u16() >= 400
                            && !matches!(
                                status,
                                StatusCode::UNAUTHORIZED
                                    | StatusCode::NOT_FOUND
                                    | StatusCode::GONE
                                    | StatusCode::PRECONDITION_FAILED
                            )
                )
                && !self.range_fallback_tried.swap(true, Ordering::Relaxed)
            {
                stop.wait(retry_delay(retry, true, &error)).await?;
                record_restart(restart, RestartReason::RangeUnsupported);
                return Err(EngineError::RestartRequired);
            }
            let grace_retry = single_worker
                && post_backdown_grace
                && retry < 2
                && matches!(
                    error,
                    EngineError::Status { status, .. }
                        if matches!(
                            status,
                            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                        ) || matches!(status.as_u16(), 460..=499 | 509)
                );
            if (!error.is_transient() && !grace_retry) || retry == RETRIES {
                return Err(error);
            }
            if !plan.ranges {
                self.reset_stream(index, file).await?;
            }
            stop.wait(retry_delay(retry, single_worker, &error)).await?;
        }
        unreachable!("segment loop returns after its final attempt")
    }

    fn validators(&self) -> Result<Validators> {
        Ok(self.engine.lock().job(self.id)?.record.validators.clone())
    }

    fn note_response(&self, response: &Response, conditional: bool) -> Result<()> {
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        job.connect_rtt_ms = job.connect_rtt_ms.or_else(|| connect_rtt_ms(response));
        let mut record = job.record.clone();
        let known = &mut record.validators;
        if known.changed(response, conditional) {
            return Err(EngineError::RestartRequired);
        }
        let found = Validators::from_response(response);
        let protocol = protocol(response);
        if record.use_server_filename
            || (!known.etag_present && found.etag_present)
            || (known.etag.is_none() && found.etag.is_some())
            || (known.last_modified.is_none() && found.last_modified.is_some())
            || protocol != record.protocol
        {
            if !known.etag_present && found.etag_present {
                known.etag_present = true;
                known.etag = found.etag;
                known.etag_raw = found.etag_raw;
                if conditional && known.if_range().is_none() {
                    return Err(EngineError::RestartRequired);
                }
            } else {
                known.etag = known.etag.take().or(found.etag);
                known.etag_raw = known.etag_raw.take().or(found.etag_raw);
            }
            known.last_modified = known.last_modified.take().or(found.last_modified);
            record.protocol = protocol.into();
            if record.use_server_filename {
                if let Some(filename) = crate::http::filename(response)? {
                    record.filename = filename;
                }
                record.use_server_filename = false;
            }
            db.save_record(&record)?;
            job.record = record;
        }
        Ok(())
    }

    fn retire_on_congestion(&self, index: u8, error: &EngineError) -> Result<bool> {
        if !is_parallel_backdown_error(error) {
            return Ok(false);
        }
        let new_cap = {
            let mut concurrency = self.concurrency.lock().unwrap();
            if concurrency.active_workers <= concurrency.retiring_workers + 1 {
                return Ok(false);
            }
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let segment = pending_segment(&job.segments[&index]);
            let has_progress = segment.cursor > segment.start;
            db.edit_segments(self.id, &[&segment], None)?;
            job.segments.insert(index, segment);
            concurrency.retiring_workers += 1;
            concurrency.effective_max_workers = (concurrency.effective_max_workers / 2).max(1);
            concurrency.retired_on_congestion = true;
            let delay = error.retry_after();
            if !delay.is_zero() {
                concurrency.retry_at = concurrency.retry_at.max(Some(Instant::now() + delay));
            }
            if has_progress {
                // An interrupted range may need its original validator or refreshed URL.
                // Let workers waiting for response headers pick it up without dropping live bodies.
                self.concurrency_changed.send_replace(());
            }
            concurrency.effective_max_workers
        };
        if should_learn_host_cap(error) {
            self.engine.record_host_cap(&self.source_domain, new_cap);
        }
        Ok(true)
    }

    fn try_retire_for_cap(&self) -> bool {
        let mut concurrency = self.concurrency.lock().unwrap();
        let live = concurrency
            .active_workers
            .saturating_sub(concurrency.retiring_workers);
        if live > 1 && live > concurrency.effective_max_workers {
            concurrency.retiring_workers += 1;
            true
        } else {
            false
        }
    }

    fn reassign_segment(&self, index: u8) -> Result<(Option<u8>, bool)> {
        let retire = self.try_retire_for_cap();
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let current = pending_segment(&job.segments[&index]);
        if retire {
            db.edit_segments(self.id, &[&current], None)?;
            job.segments.insert(index, current);
            return Ok((None, true));
        }
        let Some(pending) = job.segments.values().find(|segment| {
            segment.state == SegmentState::Pending && segment.cursor > segment.start
        }) else {
            return Ok((Some(index), false));
        };
        let mut next = pending.clone();
        next.state = SegmentState::Active;
        let next_index = next.index;
        db.edit_segments(self.id, &[&current, &next], None)?;
        job.segments.insert(index, current);
        job.segments.insert(next_index, next);
        Ok((Some(next_index), false))
    }

    async fn reset_stream(&self, index: u8, file: &File) -> Result<()> {
        let total = {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let segment = job.segments.get_mut(&index).unwrap();
            db.save_cursor(self.id, index, 0)?;
            segment.cursor = 0;
            segment.reserved_end = 0;
            segment.sample_cursor = 0;
            segment.speed = 0;
            job.record.total
        };
        file.set_len(0).await?;
        file.set_len(total.unwrap_or(0)).await?;
        Ok(())
    }

    async fn clear_stream_length(&self, index: u8, file: &File) -> Result<()> {
        {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let mut record = job.record.clone();
            record.total = None;
            let mut segments = job.segments.clone();
            segments.get_mut(&index).unwrap().end = None;
            db.replace_segments(&record, &segments)?;
            job.record = record;
            job.segments = segments;
        }
        file.set_len(0).await?;
        Ok(())
    }

    async fn write_response(
        &self,
        mut reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        index: u8,
        ranges: bool,
        file: &mut File,
        stop: &mut Stop,
    ) -> Result<()> {
        let mut buffer = [0; WRITE_SIZE];
        loop {
            if ranges && self.remaining(index, 1)? == 0 {
                return Ok(());
            }
            let count = stop
                .during(async { Ok(reader.read(&mut buffer).await?) })
                .await?;
            if count == 0 {
                let state = self.engine.lock();
                let segment = &state.job(self.id)?.segments[&index];
                if segment.end.is_some_and(|end| segment.cursor < end) {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete HTTP body",
                    )
                    .into());
                }
                return Ok(());
            }
            let chunk = &buffer[..count];
            let mut offset = 0;
            while offset < chunk.len() {
                let available = self.remaining(index, (chunk.len() - offset).min(WRITE_SIZE))?;
                if available == 0 {
                    return if ranges {
                        Ok(())
                    } else {
                        Err(EngineError::RestartRequired)
                    };
                }
                stop.during(async {
                    self.engine
                        .global_limiter
                        .acquire(&self.queue_limiter, &self.limiter, available)
                        .await;
                    Ok(())
                })
                .await?;
                let (start, count) = self.reserve(index, available)?;
                if count == 0 {
                    return if ranges {
                        Ok(())
                    } else {
                        Err(EngineError::RestartRequired)
                    };
                }
                // Each worker has a separate, independently positioned file handle.
                // The reservation prevents a split from moving this worker's end inward
                // over an ongoing write. Wait for the write before advancing the SQLite cursor.
                file.write_all(&chunk[offset..offset + count]).await?;
                file.flush().await?;
                self.persist_write(index, start + count as u64)?;
                offset += count;
            }
        }
    }

    fn remaining(&self, index: u8, maximum: usize) -> Result<usize> {
        let state = self.engine.lock();
        Ok(available(&state.job(self.id)?.segments[&index], maximum))
    }

    fn reserve(&self, index: u8, maximum: usize) -> Result<(u64, usize)> {
        let mut state = self.engine.lock();
        let (_, job) = state.job_mut(self.id)?;
        let segment = job.segments.get_mut(&index).unwrap();
        let count = available(segment, maximum);
        if segment.cursor > i64::MAX as u64 - count as u64 {
            return Err(EngineError::InvalidResponse(
                "download exceeds supported size",
            ));
        }
        segment.reserved_end = segment.cursor + count as u64;
        Ok((segment.cursor, count))
    }

    fn persist_write(&self, index: u8, cursor: u64) -> Result<()> {
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let segment = job.segments.get_mut(&index).unwrap();
        debug_assert_eq!(cursor, segment.reserved_end);
        debug_assert!(segment.end.is_none_or(|end| cursor <= end));
        db.save_cursor(self.id, index, cursor)?;
        segment.cursor = cursor;
        job.record.updated_at = Utc::now();
        self.has_written.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn finish_segment(&self, index: u8, ranges: bool) -> Result<(Option<u8>, bool)> {
        let retire = self.try_retire_for_cap();
        let can_continue = !retire;
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let mut complete = job.segments[&index].clone();
        debug_assert!(complete.end.is_none_or(|end| end == complete.cursor));
        complete.end = Some(complete.cursor);
        complete.state = SegmentState::Completed;
        complete.speed = 0;
        let mut record = job.record.clone();
        if record.total.is_none() {
            record.total = complete.end;
            let segments = [(index, complete)].into();
            db.replace_segments(&record, &segments)?;
            job.record = record;
            job.segments = segments;
            return Ok((None, retire));
        }

        if let Some(pending) = job
            .segments
            .values()
            .find(|segment| can_continue && segment.state == SegmentState::Pending)
        {
            let mut pending = pending.clone();
            pending.state = SegmentState::Active;
            let next_index = pending.index;
            db.edit_segments(self.id, &[&complete, &pending], None)?;
            job.segments.insert(index, complete);
            job.segments.insert(next_index, pending);
            return Ok((Some(next_index), false));
        }

        let donor = (ranges && can_continue)
            .then(|| {
                job.segments
                    .values()
                    .filter(|segment| {
                        segment.index != index && segment.state == SegmentState::Active
                    })
                    .filter_map(|segment| {
                        let remaining = segment.end? - segment.cursor;
                        (remaining >= SPLIT_THRESHOLD).then_some((remaining, segment))
                    })
                    .max_by_key(|(remaining, segment)| {
                        (*remaining, std::cmp::Reverse(segment.index))
                    })
            })
            .flatten();
        let Some((remaining, donor)) = donor else {
            db.edit_segments(self.id, &[&complete], None)?;
            job.segments.insert(index, complete);
            return Ok((None, retire));
        };

        let mut lower = donor.clone();
        let split_point = lower.cursor + remaining / 2;
        // A donor may have an in-flight write. Even at the minimum split size,
        // half the remaining bytes exceed the largest write reservation.
        debug_assert!(lower.reserved_end <= split_point);
        let old_end = lower.end.replace(split_point);
        let (next_index, merge) = free_index(&job.segments)?;
        let upper = Segment::new(next_index, split_point, old_end, SegmentState::Active);
        let mut changed = vec![&complete, &lower, &upper];
        let removed = merge.as_ref().map(|(merged, removed)| {
            changed.push(merged);
            *removed
        });
        db.edit_segments(self.id, &changed, removed)?;
        if let Some((merged, removed)) = merge {
            job.segments.remove(&removed);
            job.segments.insert(merged.index, merged);
        }
        let lower_index = lower.index;
        for segment in [complete, lower, upper] {
            job.segments.insert(segment.index, segment);
        }
        let _ = self.engine.events.send(EngineEvent::SegmentSplit {
            transfer_id: self.id.into(),
            lower_index,
            upper_index: next_index,
            split_point,
        });
        Ok((Some(next_index), false))
    }

    async fn finalize(&self, stop: &Stop) -> Result<OwnedMutexGuard<()>> {
        stop.check()?;
        let record = self.engine.lock().job(self.id)?.record.clone();
        let for_check = record.clone();
        let cancel = stop.clone();
        let verification = tokio::task::spawn_blocking(move || {
            output::prepare_commit(&for_check, || cancel.check())
        })
        .await??;
        {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let mut record = job.record.clone();
            record.sha256_verified = verification.sha256_verified;
            if let Some(mismatch) = verification.mismatch {
                let empty = Segments::new();
                db.replace_segments(&record, &empty)?;
                db.checkpoint(false)?;
                job.record = record;
                job.segments = empty;
                return Err(EngineError::ChecksumMismatch(mismatch));
            }
            db.save_record(&record)?;
            job.record = record.clone();
            stop.check()?;
        }
        let output_guard = Arc::clone(&self.engine.outputs).lock_owned().await;
        let filename = record.filename;
        for number in 0.. {
            stop.check()?;
            let candidate = output::numbered_filename(&filename, number);
            let target = slipstream_protocol::safe_join_under(&record.save_dir, &candidate)?;
            if !record.overwrite_existing {
                match tokio::fs::symlink_metadata(&target).await {
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let record = {
                let mut state = self.engine.lock();
                let (db, job) = state.job_mut(self.id)?;
                let mut record = job.record.clone();
                record.filename = candidate;
                // Persist this candidate before moving it so recovery can find the output.
                record.commit_digest = Some(verification.digest.clone());
                db.save_record(&record)?;
                db.checkpoint(false)?;
                job.record = record.clone();
                record
            };
            match output::commit(&record).await {
                // The supervisor releases the guard after it publishes ownership in SQLite.
                Ok(()) => return Ok(output_guard),
                Err(EngineError::Io(error))
                    if !record.overwrite_existing
                        && error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!("filename suffixes are unbounded")
    }
}

fn retry_delay(retry: u32, single_worker: bool, error: &EngineError) -> Duration {
    let millis = if single_worker {
        match retry {
            0 => 500,
            1 => 1500,
            _ => 3000,
        }
    } else {
        100 * 2_u64.pow(retry)
    };
    Duration::from_millis(millis).max(error.retry_after())
}

fn pending_segment(segment: &Segment) -> Segment {
    let mut pending = segment.clone();
    pending.state = SegmentState::Pending;
    pending.speed = 0;
    pending
}

fn is_parallel_backdown_error(error: &EngineError) -> bool {
    match error {
        EngineError::Status { status, .. } => {
            status.as_u16() >= 400 && *status != StatusCode::PRECONDITION_FAILED
        }
        EngineError::Http(_) => true,
        EngineError::Io(io_err) => io_err.kind() == io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

fn should_learn_host_cap(error: &EngineError) -> bool {
    match error {
        EngineError::Status { status, .. } => {
            matches!(
                *status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
            ) || matches!(status.as_u16(), 421 | 460..=499 | 509 | 520..=530)
        }
        EngineError::Http(_) => true,
        _ => false,
    }
}

fn record_restart(slot: &Mutex<Option<RestartReason>>, reason: RestartReason) {
    let mut guard = slot.lock().unwrap();
    if *guard != Some(RestartReason::RepresentationChanged) {
        *guard = Some(reason);
    }
}

fn available(segment: &Segment, maximum: usize) -> usize {
    segment.end.map_or(maximum, |end| {
        (end - segment.cursor).min(maximum as u64) as usize
    })
}

fn partition(total: Option<u64>, ranges: bool, max_connections: u8) -> Segments {
    match total {
        Some(0) => [(0, Segment::new(0, 0, total, SegmentState::Completed))].into(),
        Some(total) if ranges => {
            let count = total.min(u64::from(max_connections));
            let width = total / count;
            let extras = total % count;
            let boundary = |index: u64| index * width + index.min(extras);
            (0..count)
                .map(|index| {
                    let start = boundary(index);
                    let end = boundary(index + 1);
                    let index = index as u8;
                    (
                        index,
                        Segment::new(index, start, Some(end), SegmentState::Pending),
                    )
                })
                .collect()
        }
        _ => [(0, Segment::new(0, 0, total, SegmentState::Pending))].into(),
    }
}

fn free_index(segments: &Segments) -> Result<(u8, Option<(Segment, u8)>)> {
    if let Some(index) = (0..=u8::MAX).find(|index| !segments.contains_key(index)) {
        return Ok((index, None));
    }
    // Recycle a protocol index only after coalescing adjacent completed spans.
    // At most 32 indices are in use by workers, so there is a completed pair.
    let mut ordered: Vec<_> = segments.values().collect();
    ordered.sort_by_key(|segment| segment.start);
    for pair in ordered.windows(2) {
        let (first, second) = (pair[0], pair[1]);
        if first.state == SegmentState::Completed
            && second.state == SegmentState::Completed
            && first.end == Some(second.start)
        {
            let mut merged = first.clone();
            merged.end = second.end;
            merged.cursor = second.cursor;
            merged.reserved_end = second.cursor;
            merged.sample_cursor = second.cursor;
            return Ok((second.index, Some((merged, second.index))));
        }
    }
    Err(EngineError::InvalidState(
        "no reusable completed segment index",
    ))
}
