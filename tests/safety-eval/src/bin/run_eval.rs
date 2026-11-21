use slipstream_safety_eval::{
    report::{write_jsonl, EvalRecord, EvalSummary, PreregSpec},
    single_input::run_single_inputs,
    stateful_chain::run_stateful_chains,
};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let out_dir = match args.next() {
        None => PathBuf::from("target/safety-eval-results"),
        Some(arg) if arg == "--out-dir" && args.len() == 1 => {
            let dir = args.next().expect("checked option argument");
            if dir.is_empty() {
                return Err(usage_error());
            }
            PathBuf::from(dir)
        }
        Some(arg) if (arg == "--help" || arg == "-h") && args.len() == 0 => {
            println!("Usage: run_eval [--out-dir <path>]");
            println!("Default output directory: target/safety-eval-results");
            return Ok(());
        }
        _ => return Err(usage_error()),
    };
    let prereg_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("prereg.json");
    let prereg = PreregSpec::load(&prereg_path)?;
    fs::create_dir_all(&out_dir)?;
    let chain_path = out_dir.join("multi_chain_results.jsonl");
    match fs::remove_file(&chain_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Persist and validate single-input results before launching stateful chains so
    // partial outputs are preserved and preregistration violations abort early.
    let single = run_single_inputs().await;
    let single_path = out_dir.join("single_input_results.jsonl");
    write_jsonl(&single_path, &single)?;
    report("Single inputs", &single)?;
    prereg.validate_single(&single)?;

    let chains = run_stateful_chains().await?;
    write_jsonl(&chain_path, &chains)?;
    report("Stateful chains", &chains)?;
    prereg.validate_chains(&chains)?;

    println!("Wrote JSONL to {}", out_dir.display());
    Ok(())
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: run_eval [--out-dir <path>]",
    )
}

fn report(label: &str, records: &[EvalRecord]) -> io::Result<()> {
    let summary = EvalSummary::from_records(records)?;
    println!("{label}: {summary}");
    for record in records
        .iter()
        .filter(|record| record.passed != Some(true))
        .take(20)
    {
        eprintln!(
            "{} [{}] {}",
            record.item_id, record.surface, record.response_text
        );
    }
    summary.validate()
}
