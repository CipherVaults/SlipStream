use crate::http::Validators;
use crate::{EngineError, QueueConfig, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use slipstream_protocol::{safe_join_under, SegmentState, TransferStatus};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub filename: String,
    pub use_server_filename: bool,
    pub source_url: String,
    // The transfer row stores JSON. Old SQLite rows default to the primary URL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirror_urls: Vec<String>,
    #[serde(default)]
    pub representation_source: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_url_digest: Option<String>,
    pub source_domain: String,
    pub save_dir: PathBuf,
    pub headers: HashMap<String, String>,
    pub referrer: Option<String>,
    pub expected_sha256: Option<String>,
    pub expected_md5: Option<String>,
    pub status: TransferStatus,
    pub total: Option<u64>,
    pub ranges: bool,
    pub validators: Validators,
    pub max_connections: u8,
    pub protocol: String,
    pub queue_id: String,
    #[serde(default)]
    pub queue_paused: bool,
    pub error: Option<String>,
    pub sha256_verified: Option<bool>,
    pub commit_digest: Option<String>,
    pub speed_limit_bps: Option<u64>,
    #[serde(default)]
    pub overwrite_existing: bool,
    #[serde(default)]
    pub output_superseded: bool,
    #[serde(default)]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Record {
    pub(crate) fn paths(&self) -> Result<(PathBuf, PathBuf)> {
        let part = format!(".{}.slipstream-part", self.id);
        Ok((
            safe_join_under(&self.save_dir, &part)?,
            safe_join_under(&self.save_dir, &self.filename)?,
        ))
    }

    pub(crate) fn set_status(&mut self, status: TransferStatus) {
        self.status = status;
        if status != TransferStatus::Paused {
            self.queue_paused = false;
        }
        self.updated_at = Utc::now();
        self.error = None;
    }
}

pub(crate) type Segments = BTreeMap<u8, Segment>;

#[derive(Clone)]
pub(crate) struct Segment {
    pub index: u8,
    pub start: u64,
    pub end: Option<u64>,
    pub cursor: u64,
    // A worker reserves up to one write while its file operation is in flight.
    pub reserved_end: u64,
    pub state: SegmentState,
    pub sample_cursor: u64,
    pub speed: u64,
}

impl Segment {
    pub(crate) fn new(index: u8, start: u64, end: Option<u64>, state: SegmentState) -> Self {
        Self {
            index,
            start,
            end,
            cursor: start,
            reserved_end: start,
            state,
            sample_cursor: start,
            speed: 0,
        }
    }

    pub(crate) fn settled(&mut self, state: SegmentState) {
        if self.state != SegmentState::Completed {
            self.state = state;
        }
        self.speed = 0;
        self.reserved_end = self.cursor;
    }
}

pub(crate) struct Database(Connection);

impl Database {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS queues (
                 id TEXT PRIMARY KEY, config TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS transfers (
                 id TEXT PRIMARY KEY, queue_id TEXT NOT NULL REFERENCES queues(id),
                 record TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS segments (
                 transfer_id TEXT NOT NULL REFERENCES transfers(id) ON DELETE CASCADE,
                 segment_index INTEGER NOT NULL, start_byte INTEGER NOT NULL,
                 end_byte INTEGER, cursor_byte INTEGER NOT NULL, state TEXT NOT NULL,
                 PRIMARY KEY (transfer_id, segment_index)
             );",
        )?;
        Ok(Self(conn))
    }

    pub(crate) fn queues(&self) -> Result<HashMap<String, QueueConfig>> {
        let mut query = self.0.prepare("SELECT id, config FROM queues")?;
        let mut rows = query.query([])?;
        let mut queues = HashMap::new();
        while let Some(row) = rows.next()? {
            let config: String = row.get(1)?;
            queues.insert(row.get(0)?, serde_json::from_str(&config)?);
        }
        Ok(queues)
    }

    pub(crate) fn transfers(&self) -> Result<Vec<(Record, Segments)>> {
        let mut transfers = self.0.prepare("SELECT id, record FROM transfers")?;
        let mut segments = self.0.prepare(
            "SELECT segment_index, start_byte, end_byte, cursor_byte, state
             FROM segments WHERE transfer_id = ?1",
        )?;
        let mut records = transfers.query([])?;
        let mut output = Vec::new();
        while let Some(row) = records.next()? {
            let record = row
                .get::<_, String>(1)
                .map_err(EngineError::from)
                .and_then(|json| serde_json::from_str::<Record>(&json).map_err(EngineError::from));
            let record = match record {
                Ok(record) => record,
                Err(_) => {
                    let id: String = row.get(0)?;
                    tracing::warn!(transfer_id = %id, "skipping unreadable transfer record");
                    continue;
                }
            };
            let mut rows = segments.query([&record.id])?;
            let mut ranges = Segments::new();
            while let Some(row) = rows.next()? {
                let mut segment = Segment::new(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    serde_json::from_str(&row.get::<_, String>(4)?)?,
                );
                segment.cursor = row.get(3)?;
                segment.reserved_end = segment.cursor;
                segment.sample_cursor = segment.cursor;
                ranges.insert(segment.index, segment);
            }
            output.push((record, ranges));
        }
        Ok(output)
    }

    pub(crate) fn save_queue(&self, config: &QueueConfig) -> Result<()> {
        save_queue(&self.0, config)
    }

    pub(crate) fn sync_queues(
        &mut self,
        queues: &HashMap<String, QueueConfig>,
        removed: &[String],
        updated: &[Record],
    ) -> Result<()> {
        let transaction = self.0.transaction()?;
        for config in queues.values() {
            save_queue(&transaction, config)?;
        }
        for record in updated {
            save_record(&transaction, record)?;
        }
        for id in removed {
            transaction.execute("DELETE FROM queues WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn remove_queue(
        &mut self,
        queue_id: &str,
        updated_records: &[Record],
    ) -> Result<()> {
        let transaction = self.0.transaction()?;
        for record in updated_records {
            save_record(&transaction, record)?;
        }
        transaction.execute("DELETE FROM queues WHERE id = ?1", params![queue_id])?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn save_record(&self, record: &Record) -> Result<()> {
        save_record(&self.0, record)
    }

    pub(crate) fn save_records(&mut self, records: &[Record]) -> Result<()> {
        let transaction = self.0.transaction()?;
        for record in records {
            save_record(&transaction, record)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn replace_segments(&mut self, record: &Record, segments: &Segments) -> Result<()> {
        self.replace_segments_with_records(record, segments, &[])
    }

    pub(crate) fn replace_segments_with_records(
        &mut self,
        record: &Record,
        segments: &Segments,
        other_records: &[Record],
    ) -> Result<()> {
        let transaction = self.0.transaction()?;
        save_record(&transaction, record)?;
        for record in other_records {
            save_record(&transaction, record)?;
        }
        transaction.execute("DELETE FROM segments WHERE transfer_id = ?1", [&record.id])?;
        for segment in segments.values() {
            save_segment(&transaction, &record.id, segment)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn edit_segments(
        &mut self,
        id: &str,
        changed: &[&Segment],
        removed: Option<u8>,
    ) -> Result<()> {
        let transaction = self.0.transaction()?;
        if let Some(index) = removed {
            transaction.execute(
                "DELETE FROM segments WHERE transfer_id = ?1 AND segment_index = ?2",
                params![id, index],
            )?;
        }
        for segment in changed {
            save_segment(&transaction, id, segment)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn save_cursor(&self, id: &str, index: u8, cursor: u64) -> Result<()> {
        let count = self.0.execute(
            "UPDATE segments SET cursor_byte = ?3 WHERE transfer_id = ?1 AND segment_index = ?2",
            params![id, index, cursor],
        )?;
        if count != 1 {
            return Err(EngineError::InvalidState("segment missing from database"));
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&self, truncate: bool) -> Result<()> {
        let pragma = if truncate {
            "PRAGMA wal_checkpoint(TRUNCATE)"
        } else {
            "PRAGMA wal_checkpoint(FULL)"
        };
        let busy: u32 = self.0.query_row(pragma, [], |row| row.get(0))?;
        if busy != 0 {
            return Err(EngineError::InvalidState("SQLite checkpoint is busy"));
        }
        Ok(())
    }
}

fn save_queue(connection: &Connection, config: &QueueConfig) -> Result<()> {
    connection.execute(
        "INSERT INTO queues(id, config) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET config = excluded.config",
        params![config.id, serde_json::to_string(config)?],
    )?;
    Ok(())
}

fn save_record(conn: &Connection, record: &Record) -> Result<()> {
    conn.execute(
        "INSERT INTO transfers(id, queue_id, record) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET queue_id = excluded.queue_id, record = excluded.record",
        params![record.id, record.queue_id, serde_json::to_string(record)?],
    )?;
    Ok(())
}

fn save_segment(conn: &Connection, id: &str, segment: &Segment) -> Result<()> {
    conn.execute(
        "INSERT INTO segments(transfer_id, segment_index, start_byte, end_byte, cursor_byte, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(transfer_id, segment_index) DO UPDATE SET
            start_byte = excluded.start_byte, end_byte = excluded.end_byte,
            cursor_byte = excluded.cursor_byte, state = excluded.state",
        params![
            id,
            segment.index,
            segment.start,
            segment.end,
            segment.cursor,
            serde_json::to_string(&segment.state)?,
        ],
    )?;
    Ok(())
}
