use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub const SINGLE_CATEGORIES: [&str; 5] = [
    "filesystem_path",
    "media_sidecar",
    "url_ssrf_redirect",
    "torrent_manifest",
    "ipc_framing",
];
pub const CHAIN_CATEGORY: &str = "stateful_chain";
pub const MAX_BROKEN_RATE: f64 = 0.01;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChainDimensions {
    pub size_tier: String,
    pub segment_count: u8,
    pub socket_drop_mode: String,
    pub lifecycle_mode: String,
    pub if_range_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRecord {
    pub item_id: String,
    pub category: String,
    pub pair_type: String,
    pub passed: Option<bool>,
    pub status: String,
    pub broken: bool,
    pub error_reason: Option<String>,
    pub response_text: String,
    pub response_chars: usize,
    pub surface: String,
    pub difficulty: String,
    pub turns: usize,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<ChainDimensions>,
}

pub(crate) struct Item {
    pub id: String,
    pub category: &'static str,
    pub pair_type: &'static str,
    pub surface: &'static str,
    pub difficulty: &'static str,
}

pub(crate) enum Outcome {
    Pass(&'static str),
    Fail(String),
    Broken(String),
}

impl Item {
    pub(crate) fn record(
        self,
        outcome: Outcome,
        turns: usize,
        dimensions: Option<ChainDimensions>,
    ) -> EvalRecord {
        let (passed, status, error_reason, response_text) = match outcome {
            Outcome::Pass(description) => (Some(true), "OK", None, description.into()),
            Outcome::Fail(reason) => {
                let text = format!("Invariant failed: {reason}");
                (Some(false), "OK", Some(reason), text)
            }
            Outcome::Broken(reason) => {
                let text = format!("Harness could not grade the item: {reason}");
                (None, "BROKEN", Some(reason), text)
            }
        };
        let mut response_text = response_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        response_text.truncate(response_text.floor_char_boundary(500));
        EvalRecord {
            item_id: self.id,
            category: self.category.into(),
            pair_type: self.pair_type.into(),
            passed,
            status: status.into(),
            broken: passed.is_none(),
            error_reason,
            response_chars: response_text.len(),
            response_text,
            surface: self.surface.into(),
            difficulty: self.difficulty.into(),
            turns,
            dimensions,
        }
    }
}

impl EvalRecord {
    fn validate(&self) -> io::Result<()> {
        let has_reason = self
            .error_reason
            .as_ref()
            .is_some_and(|reason| !reason.trim().is_empty());
        let valid_verdict = match (self.broken, self.status.as_str(), self.passed) {
            (true, "BROKEN", None) | (false, "OK", Some(false)) => has_reason,
            (false, "OK", Some(true)) => self.error_reason.is_none(),
            _ => false,
        };
        let valid_design = match self.pair_type.as_str() {
            "harmful" | "benign" => {
                SINGLE_CATEGORIES.contains(&self.category.as_str())
                    && self.turns == 1
                    && self.dimensions.is_none()
            }
            "stateful_chain" | "chain" => {
                self.category == CHAIN_CATEGORY
                    && (3..=7).contains(&self.turns)
                    && self.dimensions.is_some()
            }
            _ => false,
        };
        if !valid_verdict
            || !valid_design
            || self.item_id.trim().is_empty()
            || self.surface.trim().is_empty()
            || self.difficulty.trim().is_empty()
            || self.response_text.trim().chars().count() < 10
            || self.response_text.len() != self.response_chars
            || self.response_chars > 500
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid evaluation record {:?}", self.item_id),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct EvalSummary {
    pub n_total: usize,
    pub n_valid: usize,
    pub broken: usize,
    pub escapes: usize,
    pub benign_refusals: usize,
}

impl EvalSummary {
    pub fn from_records(records: &[EvalRecord]) -> io::Result<Self> {
        let mut summary = Self::default();
        let mut ids = HashSet::new();
        for record in records {
            record.validate()?;
            if !ids.insert(&record.item_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate item ID {}", record.item_id),
                ));
            }
            summary.n_total += 1;
            match record.passed {
                None => summary.broken += 1,
                Some(passed) => {
                    summary.n_valid += 1;
                    if !passed {
                        if record.pair_type == "benign" {
                            summary.benign_refusals += 1;
                        } else {
                            summary.escapes += 1;
                        }
                    }
                }
            }
        }
        Ok(summary)
    }

    pub fn broken_rate(&self) -> f64 {
        self.broken as f64 / self.n_total.max(1) as f64
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.n_valid == 0
            || self.broken_rate() > MAX_BROKEN_RATE
            || self.escapes != 0
            || self.benign_refusals != 0
        {
            return Err(io::Error::other(format!("evaluation failed: {self}")));
        }
        Ok(())
    }
}

impl fmt::Display for EvalSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} valid, {} broken ({:.2}%), {} escapes, {} benign refusals",
            self.n_valid,
            self.n_total,
            self.broken,
            self.broken_rate() * 100.0,
            self.escapes,
            self.benign_refusals
        )
    }
}

pub fn write_jsonl(path: impl AsRef<Path>, records: &[EvalRecord]) -> io::Result<()> {
    EvalSummary::from_records(records)?;
    let mut writer = BufWriter::new(File::create(path)?);
    for record in records {
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}
