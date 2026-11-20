use crate::http::{protocol, request_error, Metadata, Source, Validators};
use crate::limiter::Limiter;
use crate::output;
use crate::storage::{Record, Segment, Segments};
use crate::{EngineError, EngineEvent, Inner, Result, SegmentState, TransferStatus};
use chrono::Utc;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Response;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;

const SPLIT_THRESHOLD: u64 = 512 * 1024;
const WRITE_SIZE: usize = 16 * 1024;
const _: () = assert!(WRITE_SIZE as u64 <= SPLIT_THRESHOLD / 2);
const RETRIES: u32 = 6;
const RESTARTS: usize = 3;

struct Run<'a> {
    engine: &'a Inner,
    id: &'a str,
    source: Source,
    limiter: Arc<Limiter>,
}

struct Plan {
    part: PathBuf,
    total: Option<u64>,
    ranges: bool,
    conditional: bool,
    indices: Vec<u8>,
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

    async fn backoff(&mut self, retry: u32) -> Result<()> {
        self.during(async {
            tokio::time::sleep(Duration::from_millis(100 * 2_u64.pow(retry))).await;
            Ok(())
        })
        .await
    }
}

impl Inner {
    pub(crate) async fn download(&self, id: &str, signal: watch::Receiver<bool>) -> Result<()> {
        let (_probe_stop, probe_signal) = watch::channel(false);
        let mut stop = Stop {
            transfer: signal,
            group: probe_signal,
        };
        stop.check()?;
        let (record, segments, limiter) = {
            let state = self.lock();
            let job = state.job(id)?;
            (
                job.record.clone(),
                job.segments.clone(),
                Arc::clone(&job.limiter),
            )
        };
        let source = Source::new(
            &record.source_url,
            &record.headers,
            record.referrer.as_deref(),
            self.config.allow_loopback,
        )?;
        let run = Run {
            engine: self,
            id,
            source,
            limiter,
        };
        let mut plan = match run.resume_plan(&record, &segments).await? {
            Some(plan) => plan,
            None => run.fresh_plan(&mut stop, false, true).await?,
        };
        for restarts in 0..=RESTARTS {
            match run.execute_plan(&plan, &mut stop).await {
                Err(EngineError::RestartRequired) if restarts < RESTARTS => {
                    let allow_ranges = plan.conditional;
                    plan = run.fresh_plan(&mut stop, true, allow_ranges).await?;
                }
                Err(error) => {
                    if matches!(error, EngineError::Interrupted) {
                        output::sync_part(&plan.part).await?;
                    }
                    return Err(error);
                }
                Ok(()) => {
                    return run.finalize(&stop).await;
                }
            }
        }
        unreachable!("download loop returns after its final attempt")
    }
}

impl Run<'_> {
    async fn resume_plan(&self, record: &Record, segments: &Segments) -> Result<Option<Plan>> {
        let has_progress = segments
            .values()
            .any(|segment| segment.cursor > segment.start);
        if !record.ranges
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
        }))
    }

    async fn probe(&self, stop: &mut Stop, force_get: bool) -> Result<Metadata> {
        for retry in 0..=RETRIES {
            let response = stop
                .during(self.engine.http.probe(&self.source, force_get))
                .await;
            match response {
                Err(error) if error.is_transient() && retry < RETRIES => {
                    stop.backoff(retry).await?
                }
                response => return response,
            }
        }
        unreachable!("probe loop returns after its final attempt")
    }

    async fn fresh_plan(
        &self,
        stop: &mut Stop,
        force_get: bool,
        allow_ranges: bool,
    ) -> Result<Plan> {
        let meta = self.probe(stop, force_get).await?;
        stop.check()?;
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
            record.total = meta.total;
            record.ranges = allow_ranges && meta.ranges && meta.total.is_some();
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
            };
            job.record = record;
            job.segments = segments;
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

    async fn execute_plan(&self, plan: &Plan, stop: &mut Stop) -> Result<()> {
        stop.check()?;
        self.activate(plan)?;
        if plan.indices.is_empty() {
            if plan.conditional && plan.total.is_some_and(|total| total > 0) {
                // A paused transfer can already have all its bytes on disk.
                // Verify its old representation before publishing the part file.
                let validators = self.validators()?;
                for retry in 0..=RETRIES {
                    match stop
                        .during(self.engine.http.get(
                            &self.source,
                            Some((0, 0)),
                            plan.total,
                            &validators,
                            true,
                        ))
                        .await
                    {
                        Err(error) if error.is_transient() && retry < RETRIES => {
                            stop.backoff(retry).await?;
                        }
                        result => {
                            result?;
                            break;
                        }
                    }
                }
            }
            return Ok(());
        }
        let (group_stop, group) = watch::channel(false);
        let mut workers = FuturesUnordered::new();
        for &index in &plan.indices {
            workers.push(self.worker(
                plan,
                index,
                Stop {
                    transfer: stop.transfer.clone(),
                    group: group.clone(),
                },
            ));
        }
        let mut failure = None;
        while let Some(result) = workers.next().await {
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
        failure.map_or(Ok(()), Err)
    }

    fn activate(&self, plan: &Plan) -> Result<()> {
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let mut record = job.record.clone();
        let mut segments = job.segments.clone();
        record.set_status(TransferStatus::Active);
        for segment in segments.values_mut() {
            if segment.end == Some(segment.cursor) {
                segment.state = SegmentState::Completed;
            }
        }
        for index in &plan.indices {
            let segment = segments.get_mut(index).unwrap();
            segment.state = SegmentState::Active;
        }
        db.replace_segments(&record, &segments)?;
        job.record = record;
        job.segments = segments;
        self.engine.status(self.id, TransferStatus::Active);
        Ok(())
    }

    async fn worker(&self, plan: &Plan, mut index: u8, mut stop: Stop) -> Result<()> {
        let mut file = OpenOptions::new().write(true).open(&plan.part).await?;
        loop {
            stop.check()?;
            self.download_segment(plan, index, &mut file, &mut stop)
                .await?;
            stop.check()?;
            let Some(next_index) = self.finish_segment(index, plan.ranges)? else {
                return Ok(());
            };
            index = next_index;
        }
    }

    async fn download_segment(
        &self,
        plan: &Plan,
        index: u8,
        file: &mut File,
        stop: &mut Stop,
    ) -> Result<()> {
        for retry in 0..=RETRIES {
            stop.check()?;
            let (range, validators, conditional) = {
                let state = self.engine.lock();
                let job = state.job(self.id)?;
                let segment = &job.segments[&index];
                if plan.ranges && segment.end == Some(segment.cursor) {
                    return Ok(());
                }
                let range = plan
                    .ranges
                    .then(|| (segment.cursor, segment.end.unwrap() - 1));
                (
                    range,
                    job.record.validators.clone(),
                    plan.conditional || (retry != 0 && segment.cursor > segment.start),
                )
            };
            let result = async {
                let response = stop
                    .during(self.engine.http.get(
                        &self.source,
                        range,
                        plan.total,
                        &validators,
                        conditional,
                    ))
                    .await?;
                self.note_response(&response)?;
                file.seek(SeekFrom::Start(range.map_or(0, |(start, _)| start)))
                    .await?;
                self.write_response(response, index, plan.ranges, file, stop)
                    .await
            }
            .await;
            match result {
                Err(error) if error.is_transient() && retry < RETRIES => {
                    if !plan.ranges {
                        self.reset_stream(index, file, plan.total).await?;
                    }
                    stop.backoff(retry).await?;
                }
                result => return result,
            }
        }
        unreachable!("segment loop returns after its final attempt")
    }

    fn validators(&self) -> Result<Validators> {
        Ok(self.engine.lock().job(self.id)?.record.validators.clone())
    }

    fn note_response(&self, response: &Response) -> Result<()> {
        let mut state = self.engine.lock();
        let (db, job) = state.job_mut(self.id)?;
        let mut record = job.record.clone();
        let known = &mut record.validators;
        if known.changed(response, false) {
            return Err(EngineError::RestartRequired);
        }
        let found = Validators::from_response(response);
        let protocol = protocol(response);
        if record.use_server_filename
            || (known.etag.is_none() && found.etag.is_some())
            || (known.last_modified.is_none() && found.last_modified.is_some())
            || protocol != record.protocol
        {
            known.etag = known.etag.take().or(found.etag);
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

    async fn reset_stream(&self, index: u8, file: &File, total: Option<u64>) -> Result<()> {
        {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let segment = job.segments.get_mut(&index).unwrap();
            db.save_cursor(self.id, index, 0)?;
            segment.cursor = 0;
            segment.reserved_end = 0;
            segment.sample_cursor = 0;
            segment.speed = 0;
        }
        file.set_len(0).await?;
        file.set_len(total.unwrap_or(0)).await?;
        Ok(())
    }

    async fn write_response(
        &self,
        mut response: Response,
        index: u8,
        ranges: bool,
        file: &mut File,
        stop: &mut Stop,
    ) -> Result<()> {
        loop {
            if ranges && self.remaining(index, 1)? == 0 {
                return Ok(());
            }
            let chunk = stop
                .during(async { response.chunk().await.map_err(request_error) })
                .await?;
            let Some(chunk) = chunk else {
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
            };
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
                        .acquire(&self.limiter, available)
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
        Ok(())
    }

    fn finish_segment(&self, index: u8, ranges: bool) -> Result<Option<u8>> {
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
            return Ok(None);
        }

        let donor = ranges
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
            return Ok(None);
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
        Ok(Some(next_index))
    }

    async fn finalize(&self, stop: &Stop) -> Result<()> {
        stop.check()?;
        let record = self.engine.lock().job(self.id)?.record.clone();
        let for_check = record.clone();
        let cancel = stop.clone();
        let verification = tokio::task::spawn_blocking(move || {
            output::prepare_commit(&for_check, || cancel.check())
        })
        .await??;
        let record = {
            let mut state = self.engine.lock();
            let (db, job) = state.job_mut(self.id)?;
            let mut record = job.record.clone();
            record.sha256_verified = verification.sha256_verified;
            if let Some(mismatch) = verification.mismatch {
                let empty = Segments::new();
                db.replace_segments(&record, &empty)?;
                job.record = record;
                job.segments = empty;
                return Err(EngineError::ChecksumMismatch(mismatch));
            }
            db.save_record(&record)?;
            job.record = record.clone();
            stop.check()?;
            // The verified file and its ADS are durable before the rename.
            // A crash after rename can be reconciled on the next open.
            record.commit_digest = Some(verification.digest);
            db.save_record(&record)?;
            db.checkpoint(false)?;
            job.record = record.clone();
            record
        };
        output::commit(&record).await
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
