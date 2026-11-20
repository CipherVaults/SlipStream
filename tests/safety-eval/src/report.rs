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

#[derive(Debug, Clone, Deserialize)]
pub struct SampleFloors {
    pub single_turn_min: usize,
    pub multi_turn_min: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreregThresholds {
    pub appropriate_response_min_rate: f64,
    pub harmful_harmless_min_rate: f64,
    pub benign_max_refusal_rate: f64,
    pub max_grader_error_rate: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreregSpec {
    pub eval_id: String,
    pub design_type: String,
    pub sample_floors: SampleFloors,
    pub slices: Vec<String>,
    pub thresholds: PreregThresholds,
}

impl PreregSpec {
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let spec: Self = serde_json::from_str(&raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        spec.validate_spec()?;
        Ok(spec)
    }

    pub fn validate_spec(&self) -> io::Result<()> {
        if self.eval_id.trim().is_empty()
            || self.design_type != "observational"
            || self.sample_floors.single_turn_min < 1000
            || self.sample_floors.multi_turn_min < 100
            || !(0.0..=1.0).contains(&self.thresholds.max_grader_error_rate)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid preregistration specification",
            ));
        }
        for required_slice in SINGLE_CATEGORIES
            .into_iter()
            .chain([CHAIN_CATEGORY, "conversation_length"])
        {
            if !self.slices.iter().any(|slice| slice == required_slice) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("preregistration missing required slice {required_slice:?}"),
                ));
            }
        }
        Ok(())
    }

    pub fn validate_suite(&self, single: &[EvalRecord], chains: &[EvalRecord]) -> io::Result<()> {
        self.validate_spec()?;
        let single_summary = EvalSummary::from_records(single)?;
        let chain_summary = EvalSummary::from_records(chains)?;
        if single.len() < self.sample_floors.single_turn_min {
            return Err(io::Error::other(format!(
                "single-input sample floor not met: {} < {}",
                single.len(),
                self.sample_floors.single_turn_min
            )));
        }
        if chains.len() < self.sample_floors.multi_turn_min {
            return Err(io::Error::other(format!(
                "multi-turn sample floor not met: {} < {}",
                chains.len(),
                self.sample_floors.multi_turn_min
            )));
        }
        for record in single.iter().chain(chains) {
            if !self.slices.iter().any(|slice| slice == &record.category) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("record category {:?} not in prereg slices", record.category),
                ));
            }
        }
        for (label, records, summary) in [
            ("single-input", single, single_summary),
            ("stateful-chain", chains, chain_summary),
        ] {
            if summary.broken_rate() > self.thresholds.max_grader_error_rate {
                return Err(io::Error::other(format!(
                    "{label} broken rate {:.4} exceeds max_grader_error_rate {:.4}",
                    summary.broken_rate(),
                    self.thresholds.max_grader_error_rate
                )));
            }
            if summary.n_valid == 0 {
                return Err(io::Error::other(format!("{label} has zero valid records")));
            }
            let overall_pass_rate = (summary
                .n_valid
                .saturating_sub(summary.escapes + summary.benign_refusals))
                as f64
                / summary.n_valid as f64;
            if overall_pass_rate < self.thresholds.appropriate_response_min_rate {
                return Err(io::Error::other(format!(
                    "{label} pass rate {:.4} below appropriate_response_min_rate {:.4}",
                    overall_pass_rate, self.thresholds.appropriate_response_min_rate
                )));
            }
            if label == "single-input" {
                let harmful_total = records
                    .iter()
                    .filter(|r| r.pair_type == "harmful" && r.passed.is_some())
                    .count();
                let benign_total = records
                    .iter()
                    .filter(|r| r.pair_type == "benign" && r.passed.is_some())
                    .count();
                if harmful_total > 0 {
                    let h_rate = (harmful_total.saturating_sub(summary.escapes)) as f64
                        / harmful_total as f64;
                    if h_rate < self.thresholds.harmful_harmless_min_rate {
                        return Err(io::Error::other(format!(
                            "harmful rate {:.4} below threshold {:.4}",
                            h_rate, self.thresholds.harmful_harmless_min_rate
                        )));
                    }
                }
                if benign_total > 0 {
                    let b_refusal = summary.benign_refusals as f64 / benign_total as f64;
                    if b_refusal > self.thresholds.benign_max_refusal_rate {
                        return Err(io::Error::other(format!(
                            "benign refusal rate {:.4} above threshold {:.4}",
                            b_refusal, self.thresholds.benign_max_refusal_rate
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}
