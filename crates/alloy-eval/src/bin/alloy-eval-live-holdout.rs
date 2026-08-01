//! Rust operator CLI for the strict RFC-0016 live-holdout harness.
//!
//! The CLI performs no network or process execution. The shell runner invokes
//! it for path validation, post-run oracle inspection, scoring, and comparison.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use alloy_eval::{
    compare_live_holdout, inspect_live_holdout, live_holdout_target_path_text,
    load_live_holdout_observations, score_live_holdout, LiveHoldoutEndpoint, LiveHoldoutReport,
    LIVE_HOLDOUT_REPORT_VERSION,
};

const USAGE: &str = "\
alloy-eval-live-holdout — strict live-BYOM telemetry (not an offline gate)

USAGE:
  alloy-eval-live-holdout target-path --manifest <path>
  alloy-eval-live-holdout oracle --fixture-dir <dir> --workspace <dir>
      --run-log <path> --exit-code <n> --compile-clean <bool>
      --cargo-check-exit <n|null> --tests-pass <bool>
      --cargo-test-exit <n|null>
  alloy-eval-live-holdout score --fixtures <dir> --observations <path>
      --model <model> --temperature <n> --profile <profile>
      --base-url <url> --reps <n> --out <path>
  alloy-eval-live-holdout compare --arm <id=report> --arm <id=report> --out <path>";

fn main() -> ExitCode {
    match run() {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("alloy-eval-live-holdout: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<String, String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or_else(|| USAGE.to_owned())?;
    let options = parse_options(args.collect())?;
    match command.as_str() {
        "target-path" => {
            let manifest = required(&options, "manifest")?;
            Ok(format!(
                "{}\n",
                live_holdout_target_path_text(&PathBuf::from(manifest))?
            ))
        }
        "oracle" => oracle(&options),
        "score" => score(&options),
        "compare" => compare(&options),
        "-h" | "--help" | "help" => Ok(format!("{USAGE}\n")),
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

fn parse_options(args: Vec<String>) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut options = BTreeMap::new();
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("expected an option, got {flag}"))?
            .to_owned();
        let value = iter
            .next()
            .ok_or_else(|| format!("option --{key} requires a value"))?;
        options.entry(key).or_insert_with(Vec::new).push(value);
    }
    Ok(options)
}

fn required<'a>(
    options: &'a BTreeMap<String, Vec<String>>,
    key: &str,
) -> Result<&'a String, String> {
    match options.get(key).map(Vec::as_slice) {
        None | Some([]) => Err(format!("missing required option --{key}")),
        Some([value]) => Ok(value),
        Some(_) => Err(format!("option --{key} may only be provided once")),
    }
}

fn parse_bool(options: &BTreeMap<String, Vec<String>>, key: &str) -> Result<bool, String> {
    match required(options, key)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(format!("--{key} must be true or false, got {value}")),
    }
}

fn parse_optional_exit(
    options: &BTreeMap<String, Vec<String>>,
    key: &str,
) -> Result<Option<i32>, String> {
    let value = required(options, key)?;
    if value == "null" {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .map_err(|_| format!("--{key} must be an integer or null"))
    }
}

fn oracle(options: &BTreeMap<String, Vec<String>>) -> Result<String, String> {
    let fields = inspect_live_holdout(
        &PathBuf::from(required(options, "fixture-dir")?),
        &PathBuf::from(required(options, "workspace")?),
        &PathBuf::from(required(options, "run-log")?),
        required(options, "exit-code")?
            .parse()
            .map_err(|_| "--exit-code must be an integer".to_owned())?,
        parse_bool(options, "compile-clean")?,
        parse_optional_exit(options, "cargo-check-exit")?,
        parse_bool(options, "tests-pass")?,
        parse_optional_exit(options, "cargo-test-exit")?,
    )?;
    // Nine-field TSV consumed by eval/live-holdout/run.sh, in order:
    // process_pass, compile_clean, tests_pass, reference_match, oracle_pass,
    // failure_class, cargo_check_exit, cargo_test_exit, repair_generations.
    Ok(format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        fields.process_pass,
        fields.compile_clean,
        fields.tests_pass,
        fields.reference_match,
        fields.oracle_pass,
        fields.failure_class,
        fields
            .cargo_check_exit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_owned()),
        fields
            .cargo_test_exit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_owned()),
        fields.repair_generations,
    ))
}

fn endpoint(options: &BTreeMap<String, Vec<String>>) -> Result<LiveHoldoutEndpoint, String> {
    let temperature: f64 = required(options, "temperature")?
        .parse()
        .map_err(|_| "--temperature must be a number".to_owned())?;
    if !temperature.is_finite() {
        return Err("--temperature must be a finite number".to_owned());
    }
    Ok(LiveHoldoutEndpoint {
        model: required(options, "model")?.clone(),
        temperature,
        profile: required(options, "profile")?.clone(),
        base_url: required(options, "base-url")?.clone(),
    })
}

fn score(options: &BTreeMap<String, Vec<String>>) -> Result<String, String> {
    let observations =
        load_live_holdout_observations(&PathBuf::from(required(options, "observations")?))?;
    let repetitions: u32 = required(options, "reps")?
        .parse()
        .map_err(|_| "--reps must be a positive integer".to_owned())?;
    let report = score_live_holdout(
        &PathBuf::from(required(options, "fixtures")?),
        observations,
        endpoint(options)?,
        repetitions,
    )?;
    let output = required(options, "out")?;
    let json = serde_json::to_string_pretty(&report)
        .map_err(|error| format!("serialize report: {error}"))?;
    std::fs::write(output, format!("{json}\n"))
        .map_err(|error| format!("write {output}: {error}"))?;
    Ok(render_report(&report))
}

fn compare(options: &BTreeMap<String, Vec<String>>) -> Result<String, String> {
    let paths = options
        .get("arm")
        .ok_or_else(|| "at least two --arm id=report options are required".to_owned())?;
    let mut reports = Vec::new();
    for path in paths {
        let (name, report_path) = path
            .split_once('=')
            .ok_or_else(|| format!("--arm must be id=report, got {path}"))?;
        let raw = std::fs::read_to_string(report_path)
            .map_err(|error| format!("read {report_path}: {error}"))?;
        let report: LiveHoldoutReport =
            serde_json::from_str(&raw).map_err(|error| format!("parse {report_path}: {error}"))?;
        if report.schema_version != LIVE_HOLDOUT_REPORT_VERSION {
            return Err(format!(
                "unsupported schema_version {} in {report_path}; expected {LIVE_HOLDOUT_REPORT_VERSION}",
                report.schema_version,
            ));
        }
        reports.push((name.to_owned(), report));
    }
    let comparison = compare_live_holdout(reports)?;
    let output = required(options, "out")?;
    let json = serde_json::to_string_pretty(&comparison)
        .map_err(|error| format!("serialize comparison: {error}"))?;
    std::fs::write(output, format!("{json}\n"))
        .map_err(|error| format!("write {output}: {error}"))?;
    Ok(render_comparison(&comparison))
}

fn render_report(report: &LiveHoldoutReport) -> String {
    let overall = &report.overall;
    let interval = overall
        .oracle
        .wilson95
        .map(|value| value.render())
        .unwrap_or_else(|| "unmeasured".to_owned());
    format!(
        "overall oracle={}/{} rate={} wilson95={} process={}/{} compile={}/{} tests={}/{} compile_clean_reference_mismatch={}/{} compile_clean_tests_failed={}/{} tests_pass_reference_mismatch={}/{} reference={}/{}\n",
        overall.oracle.passes,
        overall.oracle.attempts,
        overall
            .oracle
            .rate
            .map(|value| format!("{value:.6}"))
            .unwrap_or_else(|| "unmeasured".to_owned()),
        interval,
        overall.process.passes,
        overall.process.attempts,
        overall.compile_clean.passes,
        overall.compile_clean.attempts,
        overall.tests_pass.passes,
        overall.tests_pass.attempts,
        overall.compile_clean_reference_mismatch.passes,
        overall.compile_clean_reference_mismatch.attempts,
        overall.compile_clean_tests_failed.passes,
        overall.compile_clean_tests_failed.attempts,
        overall.tests_pass_reference_mismatch.passes,
        overall.tests_pass_reference_mismatch.attempts,
        overall.reference_match.passes,
        overall.reference_match.attempts,
    )
}

fn render_comparison(comparison: &alloy_eval::LiveHoldoutMatrixComparison) -> String {
    let mut output = format!(
        "baseline={} repetitions={}\narm\toracle\ttests\tcompile_clean_reference_mismatch\tcompile_clean_tests_failed\ttests_pass_reference_mismatch\toracle_wilson95\n",
        comparison.baseline, comparison.repetitions
    );
    for (name, report) in &comparison.arms {
        let interval = report
            .overall
            .oracle
            .wilson95
            .map(|value| value.render())
            .unwrap_or_else(|| "unmeasured".to_owned());
        output.push_str(&format!(
            "{name}\t{}/{}\t{}/{}\t{}/{}\t{}/{}\t{}/{}\t{interval}\n",
            report.overall.oracle.passes,
            report.overall.oracle.attempts,
            report.overall.tests_pass.passes,
            report.overall.tests_pass.attempts,
            report.overall.compile_clean_reference_mismatch.passes,
            report.overall.compile_clean_reference_mismatch.attempts,
            report.overall.compile_clean_tests_failed.passes,
            report.overall.compile_clean_tests_failed.attempts,
            report.overall.tests_pass_reference_mismatch.passes,
            report.overall.tests_pass_reference_mismatch.attempts,
        ));
    }
    for item in &comparison.comparisons {
        output.push_str(&format!(
            "assessment\t{}\t{}\t{}\n",
            item.arm,
            item.assessment.result,
            item.assessment
                .why_not
                .as_deref()
                .unwrap_or(&item.assessment.basis)
        ));
    }
    output
}
