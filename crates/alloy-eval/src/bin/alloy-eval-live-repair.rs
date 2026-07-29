//! Operator CLI for the live-repair benchmark.
//!
//! **This is not an RFC-0016 holdout gate.** It plans, renders, and scores a
//! live-endpoint benchmark run; its results are operator telemetry and must
//! never gate a milestone. The offline harness (`EvalHarness`, `evaluate_gate`,
//! `crates/alloy-eval/fixtures/{train,holdout}`) is untouched by this binary
//! and is not reachable from it.
//!
//! Per RFC-0016 §10.2 this binary is pure: it performs no network I/O and
//! spawns no processes. Executing the real `alloy` binary is the job of the
//! thin wrapper at `eval/live-repair/run.sh`.
//!
//! ```text
//! alloy-eval-live-repair plan --fixtures <dir>
//! alloy-eval-live-repair render-router --model <m> --temperature <t> --base-url <u>
//! alloy-eval-live-repair score --fixtures <dir> --observations <jsonl>
//!                              --model <m> --temperature <t> --base-url <u>
//!                              [--reps <n>] [--out <report.json>]
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use alloy_eval::{
    parse_observations_jsonl, render_router_toml, LiveRepairCorpus, LiveRepairEndpoint,
    LiveRepairReport,
};

const USAGE: &str = "\
alloy-eval-live-repair — live-endpoint repair benchmark (operator tooling, NOT a holdout gate)

USAGE:
  alloy-eval-live-repair plan --fixtures <dir>
  alloy-eval-live-repair render-router --model <m> --temperature <t> --base-url <u>
  alloy-eval-live-repair score --fixtures <dir> --observations <jsonl>
                               --model <m> --temperature <t> --base-url <u>
                               [--reps <n>] [--out <json>]

EXIT: 0 scored, 2 bad invocation or invalid observations, 3 the sweep could not
execute the alloy binary (broken harness, not a result)";

/// Exit code for a run whose repetitions could not execute `alloy` at all.
/// Distinct from `2` (bad invocation) and from `0` (a scored result, however
/// bad the pass rate): the sweep itself is broken and must not be published.
const EXIT_HARNESS_BROKEN: u8 = 3;

fn main() -> ExitCode {
    match run() {
        Ok(Output { text, exit }) => {
            print!("{text}");
            exit
        }
        Err(message) => {
            eprintln!("alloy-eval-live-repair: {message}");
            ExitCode::from(2)
        }
    }
}

/// Command output plus the process exit code it implies.
struct Output {
    text: String,
    exit: ExitCode,
}

impl From<String> for Output {
    fn from(text: String) -> Self {
        Self {
            text,
            exit: ExitCode::SUCCESS,
        }
    }
}

fn run() -> Result<Output, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = args.split_first().ok_or_else(|| USAGE.to_owned())?;
    let options = parse_options(rest)?;

    match command.as_str() {
        "plan" => plan(&options).map(Output::from),
        "render-router" => render_router(&options).map(Output::from),
        "score" => score(&options),
        "-h" | "--help" | "help" => Ok(Output::from(format!("{USAGE}\n"))),
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

/// Parse `--key value` pairs; every option this CLI accepts takes a value.
fn parse_options(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut options = BTreeMap::new();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("expected an option, got {flag}"))?;
        let value = iter
            .next()
            .ok_or_else(|| format!("option --{key} requires a value"))?;
        if options.insert(key.to_owned(), value.clone()).is_some() {
            return Err(format!("option --{key} given twice"));
        }
    }
    Ok(options)
}

fn required<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Result<&'a String, String> {
    options
        .get(key)
        .ok_or_else(|| format!("missing required option --{key}"))
}

fn corpus(options: &BTreeMap<String, String>) -> Result<LiveRepairCorpus, String> {
    let root = PathBuf::from(required(options, "fixtures")?);
    LiveRepairCorpus::load(&root).map_err(|err| err.to_string())
}

fn endpoint(options: &BTreeMap<String, String>) -> Result<LiveRepairEndpoint, String> {
    let temperature: f64 = required(options, "temperature")?
        .parse()
        .map_err(|_| "--temperature must be a number".to_owned())?;
    Ok(LiveRepairEndpoint {
        model: required(options, "model")?.clone(),
        temperature,
        base_url: required(options, "base-url")?.clone(),
    })
}

/// Emit one tab-delimited `id<TAB>workspace<TAB>goal` line per fixture.
///
/// The manifest is the single source of the goal text and of the workspace
/// directory the wrapper copies, so the shell never hard-codes either.
fn plan(options: &BTreeMap<String, String>) -> Result<String, String> {
    let corpus = corpus(options)?;
    let mut out = String::new();
    for fixture in corpus.fixtures() {
        let manifest = fixture.manifest();
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            manifest.id,
            fixture.workspace_dir().display(),
            manifest.goal
        ));
    }
    Ok(out)
}

fn render_router(options: &BTreeMap<String, String>) -> Result<String, String> {
    render_router_toml(&endpoint(options)?).map_err(|err| err.to_string())
}

fn score(options: &BTreeMap<String, String>) -> Result<Output, String> {
    let corpus = corpus(options)?;
    let endpoint = endpoint(options)?;
    let expected_reps = match options.get("reps") {
        Some(raw) => Some(
            raw.parse::<u32>()
                .ok()
                .filter(|reps| *reps >= 1)
                .ok_or_else(|| "--reps must be a positive integer".to_owned())?,
        ),
        None => None,
    };
    let observations_path = PathBuf::from(required(options, "observations")?);
    let raw = std::fs::read_to_string(&observations_path)
        .map_err(|err| format!("read {}: {err}", observations_path.display()))?;
    let observations = parse_observations_jsonl(&raw).map_err(|err| err.to_string())?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let report = LiveRepairReport::assemble(run_id, endpoint, &corpus, observations, expected_reps)
        .map_err(|err| err.to_string())?;

    if let Some(out) = options.get("out") {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|err| format!("serialize report: {err}"))?;
        std::fs::write(out, format!("{json}\n")).map_err(|err| format!("write {out}: {err}"))?;
    }

    // The report is still written and printed — an operator needs to see what
    // happened — but a run that could not execute `alloy` is not a result, so
    // the process exits non-zero rather than blessing the pass rate.
    let exit = if report.has_harness_errors() {
        eprintln!(
            "alloy-eval-live-repair: {} repetition(s) could not execute the alloy binary; \
             this sweep is broken and its pass rate must not be published",
            report.overall.harness_errors
        );
        ExitCode::from(EXIT_HARNESS_BROKEN)
    } else {
        ExitCode::SUCCESS
    };

    Ok(Output {
        text: format!(
            "{}\n{}\n",
            report.render_fixture_lines(),
            report.render_summary()
        ),
        exit,
    })
}
