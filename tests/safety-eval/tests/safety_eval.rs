use serde_json::{json, Value};
use slipstream_safety_eval::{
    report::{
        write_jsonl, EvalRecord, EvalSummary, PreregSpec, CHAIN_CATEGORY, MAX_BROKEN_RATE,
        SINGLE_CATEGORIES,
    },
    single_input::{run_single_inputs, CASES_PER_SIDE},
    stateful_chain::{run_stateful_chains, CHAIN_COUNT},
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn preregistered_safety_evaluation() {
    let started = Instant::now();
    let prereg: Value = serde_json::from_str(include_str!("../prereg.json")).unwrap();
    assert_eq!(prereg["eval_id"], "slipstream-safety-v1");
    assert_eq!(prereg["eval_purpose"], "regression_baseline");
    assert_eq!(prereg["runs_per_item"], 1);
    assert_eq!(prereg["paired_design"], true);
    assert_eq!(prereg["held_constant_settings"]["surface"], "agent_harness");
    assert_eq!(
        prereg["slices"],
        json!(SINGLE_CATEGORIES
            .into_iter()
            .chain([
                CHAIN_CATEGORY,
                "category",
                "pair_type",
                "difficulty",
                "conversation_length",
                "surface",
            ])
            .collect::<Vec<_>>()),
    );
    let prereg_spec =
        PreregSpec::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("prereg.json")).unwrap();
    assert_eq!(
        prereg["thresholds"],
        json!({
            "appropriate_response_min_rate": 1.0,
            "harmful_harmless_min_rate": 1.0,
            "benign_max_refusal_rate": 0.0,
            "max_grader_error_rate": MAX_BROKEN_RATE,
        })
    );
    assert_eq!(
        prereg["sample_floors"],
        json!({"multi_turn_min": 100, "single_turn_min": 1000})
    );

    let out = TestDir::new();
    let single = run_single_inputs().await;
    let chains = run_stateful_chains().await.unwrap();
    let single = round_trip(&out.0.join("single_input_results.jsonl"), &single);
    let chains = round_trip(&out.0.join("multi_chain_results.jsonl"), &chains);
    let mut coverage = HashMap::new();
    for record in &single {
        *coverage
            .entry((record.category.as_str(), record.pair_type.as_str()))
            .or_insert(0) += 1;
    }
    assert_eq!(single.len(), 1200);
    assert_eq!(coverage.len(), 10);
    for category in SINGLE_CATEGORIES {
        for side in ["benign", "harmful"] {
            assert_eq!(coverage[&(category, side)], CASES_PER_SIDE);
        }
    }
    assert_eq!(chains.len(), CHAIN_COUNT);
    let actual: HashSet<_> = chains
        .iter()
        .map(|record| {
            let d = record.dimensions.as_ref().unwrap();
            (
                d.size_tier.as_str(),
                d.segment_count,
                d.socket_drop_mode.as_str(),
                d.lifecycle_mode.as_str(),
                d.if_range_mode.as_str(),
            )
        })
        .collect();
    assert_eq!(actual.len(), CHAIN_COUNT);
    for size in ["64_kib", "256_kib", "640_kib"] {
        for segments in [4, 8, 16] {
            for drop in ["none", "mid_flight_drop"] {
                for lifecycle in ["direct_complete", "pause_reopen_wal", "expire_403_refresh"] {
                    for validator in ["matching_etag", "rotated_etag"] {
                        assert!(actual.contains(&(size, segments, drop, lifecycle, validator)));
                    }
                }
            }
        }
    }
    assert_clean(&single);
    assert_clean(&chains);
    prereg_spec.validate_suite(&single, &chains).unwrap();
    let elapsed = started.elapsed();
    println!("Evaluated 1,308 cases and verified JSONL in {elapsed:.2?}");
    assert!(
        elapsed < Duration::from_secs(120),
        "evaluation took {elapsed:.2?}"
    );
}

#[test]
fn prereg_and_fail_loud_guards_reject_broken_and_undersized_runs() {
    let spec = PreregSpec::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("prereg.json")).unwrap();
    let sample_single = |idx: usize, pair_type: &str, broken: bool| EvalRecord {
        item_id: format!("single-{idx}"),
        category: "filesystem_path".into(),
        pair_type: pair_type.into(),
        passed: (!broken).then_some(true),
        status: if broken { "BROKEN" } else { "OK" }.into(),
        broken,
        error_reason: broken.then(|| "simulated grader timeout".into()),
        response_text: "Deterministic guard evaluation outcome string.".into(),
        response_chars: 46,
        surface: "test".into(),
        difficulty: "direct".into(),
        turns: 1,
        dimensions: None,
    };
    let sample_chain = |i: usize, pair_type: &str, broken: bool| EvalRecord {
        item_id: format!("chain-{i}"),
        category: CHAIN_CATEGORY.into(),
        pair_type: pair_type.into(),
        passed: (!broken).then_some(true),
        status: if broken { "BROKEN" } else { "OK" }.into(),
        broken,
        error_reason: broken.then(|| "simulated harness failure".into()),
        response_text: "Deterministic stateful chain outcome summary.".into(),
        response_chars: 45,
        surface: "http_download_engine".into(),
        difficulty: "lifecycle".into(),
        turns: 3,
        dimensions: Some(slipstream_safety_eval::report::ChainDimensions {
            size_tier: "64_kib".into(),
            segment_count: 4,
            socket_drop_mode: "none".into(),
            lifecycle_mode: "direct_complete".into(),
            if_range_mode: "matching_etag".into(),
        }),
    };
    let pair_for = |i: usize| {
        if i.is_multiple_of(2) {
            "harmful"
        } else {
            "benign"
        }
    };

    // Positive baseline: paired 1,000 single + 100 valid chains satisfies all prereg gates.
    let single_ok: Vec<_> = (0..1000)
        .map(|i| sample_single(i, pair_for(i), false))
        .collect();
    let chains_ok: Vec<_> = (0..100)
        .map(|i| sample_chain(i, CHAIN_CATEGORY, false))
        .collect();
    spec.validate_suite(&single_ok, &chains_ok).unwrap();

    // Undersized and valid-count floor violations (1 broken out of 1000/100 stays <= 1% error rate,
    // so only the post-broken valid sample floor check catches it).
    let single_under_floor: Vec<_> = (0..10)
        .map(|i| sample_single(i, pair_for(i), false))
        .collect();
    assert!(spec.validate_single(&single_under_floor).is_err());

    let single_valid_floor_breach: Vec<_> = (0..1000)
        .map(|i| sample_single(i, pair_for(i), i == 0))
        .collect();
    assert!(spec.validate_single(&single_valid_floor_breach).is_err());

    let chains_valid_floor_breach: Vec<_> = (0..100)
        .map(|i| sample_chain(i, CHAIN_CATEGORY, i == 0))
        .collect();
    assert!(spec.validate_chains(&chains_valid_floor_breach).is_err());

    // Paired-control & category/pair_type separation guards.
    let single_harmful_only: Vec<_> = (0..1000)
        .map(|i| sample_single(i, "harmful", false))
        .collect();
    assert!(spec.validate_single(&single_harmful_only).is_err());

    let mut single_wrong_cat = single_ok.clone();
    single_wrong_cat[0].category = CHAIN_CATEGORY.into();
    assert!(spec.validate_single(&single_wrong_cat).is_err());

    let chains_wrong_pair: Vec<_> = (0..100)
        .map(|i| sample_chain(i, "harmful", false))
        .collect();
    assert!(spec.validate_chains(&chains_wrong_pair).is_err());

    // Broken rate > 1% guard.
    let chains_over_broken: Vec<_> = (0..200)
        .map(|i| sample_chain(i, CHAIN_CATEGORY, i < 3)) // 1.5% > 1% even though n_valid=197 >= 100
        .collect();
    assert!(spec.validate_chains(&chains_over_broken).is_err());
    assert!(EvalSummary::from_records(&chains_over_broken)
        .unwrap()
        .validate()
        .is_err());
}

fn round_trip(path: &Path, expected: &[EvalRecord]) -> Vec<EvalRecord> {
    write_jsonl(path, expected).unwrap();
    let data = fs::read_to_string(path).unwrap();
    assert!(data.ends_with('\n'));
    let actual: Vec<EvalRecord> = data
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(actual, expected);
    actual
}

fn assert_clean(records: &[EvalRecord]) {
    let summary = EvalSummary::from_records(records).unwrap();
    let unexpected: Vec<_> = records
        .iter()
        .filter(|record| record.passed != Some(true))
        .map(|record| {
            format!(
                "{} [{}] {}",
                record.item_id, record.surface, record.response_text
            )
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "{summary}\n{}",
        unexpected.join("\n")
    );
    assert_eq!(summary.broken, 0);
    assert_eq!(summary.escapes, 0);
    assert_eq!(summary.benign_refusals, 0);
    assert_eq!(summary.n_valid, records.len());
    summary.validate().unwrap();
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("slipstream-eval-jsonl-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if !std::thread::panicking() {
                panic!("failed to remove {}: {error}", self.0.display());
            }
            eprintln!("failed to remove {}: {error}", self.0.display());
        }
    }
}
