//! Rust operator CLI for the strict RFC-0016 live-holdout harness.
//!
//! The CLI performs no network or process execution. The shell runner invokes
//! it for path validation, post-run oracle inspection, scoring, and comparison.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

/// Just enough of a report to decide whether this build can score it.
#[derive(serde::Deserialize)]
struct SchemaVersionPeek {
    schema_version: u32,
}

use alloy_eval::{
    check_live_holdout_report_version, compare_live_holdout, inspect_live_holdout,
    live_holdout_corpus_digest, live_holdout_target_path_text, live_holdout_telemetry,
    load_live_holdout_observations, score_live_holdout, LiveHoldoutArmIdentity, LiveHoldoutDriver,
    LiveHoldoutEndpoint, LiveHoldoutOracleEvidence, LiveHoldoutReport, LiveHoldoutTreatmentBuild,
    LiveHoldoutTreatmentIdentity,
};

const USAGE: &str = "\
alloy-eval-live-holdout — strict live-BYOM telemetry (not an offline gate)

USAGE:
  alloy-eval-live-holdout target-path --manifest <path>
  alloy-eval-live-holdout corpus-digest --fixtures <dir>
  alloy-eval-live-holdout oracle --fixture-dir <dir> --workspace <dir>
      --run-log <path> --exit-code <n> --compile-clean <bool>
      --cargo-check-exit <n|null> --tests-pass <bool>
      --cargo-test-exit <n|null>
  alloy-eval-live-holdout telemetry --driver <naive|alloy>
      --input <naive-result.json|events.jsonl>
  alloy-eval-live-holdout score --fixtures <dir> --observations <path>
      --model <model> --temperature <n> --driver <naive|alloy>
      --profile <none|default|autonomous> --base-url <url>
      --source-revision <40-hex-sha> --binary-bundle-sha256 <64-hex-sha>
      [--evaluator-revision <40-hex-sha>] --reps <n> --out <path>
  alloy-eval-live-holdout compare --arm <id=report> --arm <id=report> --out <path>

IDENTITY:
  --source-revision and --binary-bundle-sha256 identify the TREATMENT: the
  product build whose repairs are being scored. Together with --driver and
  --profile they may differ between arms; that difference is the measurement.

  --evaluator-revision identifies the PROTOCOL: the checkout whose evaluator
  and corpus performed the scoring. It must be identical across arms, or they
  are incomparable. It defaults to --source-revision, which is correct only
  while one bundle produces every arm; pass it explicitly when re-scoring
  observations produced by an older build.";

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
        "corpus-digest" => {
            let fixtures = required(&options, "fixtures")?;
            Ok(format!(
                "{}\n",
                live_holdout_corpus_digest(&PathBuf::from(fixtures))?
            ))
        }
        "oracle" => oracle(&options),
        "telemetry" => telemetry(&options),
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
        LiveHoldoutOracleEvidence {
            exit_code: required(options, "exit-code")?
                .parse()
                .map_err(|_| "--exit-code must be an integer".to_owned())?,
            compile_clean: parse_bool(options, "compile-clean")?,
            cargo_check_exit: parse_optional_exit(options, "cargo-check-exit")?,
            tests_pass: parse_bool(options, "tests-pass")?,
            cargo_test_exit: parse_optional_exit(options, "cargo-test-exit")?,
        },
    )?;
    // Eleven-field TSV consumed by eval/live-holdout/run.sh, in order:
    // process_pass, compile_clean, tests_pass, safety_clean, semantic_pass,
    // reference_match, oracle_pass, failure_class, cargo_check_exit,
    // cargo_test_exit, repair_generations.
    Ok(format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        fields.process_pass,
        fields.compile_clean,
        fields.tests_pass,
        fields.safety_clean,
        fields.semantic_pass,
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

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |count| count.to_string())
}

fn telemetry(options: &BTreeMap<String, Vec<String>>) -> Result<String, String> {
    let extracted = live_holdout_telemetry(
        parse_driver(options)?,
        &PathBuf::from(required(options, "input")?),
    )?;
    // Three-field TSV consumed by eval/live-holdout/run.sh, in order:
    // model_calls, tokens_in, tokens_out. Unreported usage stays `null`.
    Ok(format!(
        "{}\t{}\t{}\n",
        extracted.model_calls,
        optional_count(extracted.tokens_in),
        optional_count(extracted.tokens_out),
    ))
}

fn parse_driver(options: &BTreeMap<String, Vec<String>>) -> Result<LiveHoldoutDriver, String> {
    match required(options, "driver")?.as_str() {
        "naive" => Ok(LiveHoldoutDriver::Naive),
        "alloy" => Ok(LiveHoldoutDriver::Alloy),
        other => Err(format!("--driver must be naive or alloy, got {other}")),
    }
}

fn parse_profile(options: &BTreeMap<String, Vec<String>>) -> Result<Option<String>, String> {
    let value = required(options, "profile")?;
    match value.as_str() {
        "none" => Ok(None),
        "default" | "autonomous" => Ok(Some(value.clone())),
        other => Err(format!(
            "--profile must be none, default, or autonomous, got {other}"
        )),
    }
}

fn parse_hex_sha(
    options: &BTreeMap<String, Vec<String>>,
    key: &str,
    expected_len: usize,
) -> Result<String, String> {
    let value = required(options, key)?.clone();
    if value.len() != expected_len
        || !value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
    {
        return Err(format!(
            "--{key} must be a {expected_len}-character lower-case hex string, got {value}"
        ));
    }
    Ok(value)
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
        base_url: required(options, "base-url")?.clone(),
    })
}

fn treatment(
    options: &BTreeMap<String, Vec<String>>,
) -> Result<LiveHoldoutTreatmentIdentity, String> {
    Ok(LiveHoldoutTreatmentIdentity {
        build: LiveHoldoutTreatmentBuild {
            source_revision: parse_hex_sha(options, "source-revision", 40)?,
            binary_bundle_sha256: parse_hex_sha(options, "binary-bundle-sha256", 64)?,
        },
        driver: parse_driver(options)?,
        profile: parse_profile(options)?,
    })
}

/// The checkout that scored this evidence. It defaults to the treatment's
/// source revision — right while one bundle produces every arm — and is
/// passed explicitly when one evaluator re-scores observations from several
/// builds, which is the only way those runs become comparable.
fn evaluator_revision(options: &BTreeMap<String, Vec<String>>) -> Result<String, String> {
    if options.contains_key("evaluator-revision") {
        parse_hex_sha(options, "evaluator-revision", 40)
    } else {
        parse_hex_sha(options, "source-revision", 40)
    }
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
        LiveHoldoutArmIdentity {
            evaluator_revision: evaluator_revision(options)?,
            endpoint: endpoint(options)?,
            treatment: treatment(options)?,
        },
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
        // Read the version before the body: an older report lacks fields this
        // build requires, and a bare serde error would read as a corrupt file
        // rather than as legacy evidence.
        let version = serde_json::from_str::<SchemaVersionPeek>(&raw)
            .map_err(|error| format!("parse {report_path}: {error}"))?
            .schema_version;
        check_live_holdout_report_version(version, report_path)?;
        let report: LiveHoldoutReport =
            serde_json::from_str(&raw).map_err(|error| format!("parse {report_path}: {error}"))?;
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

fn driver_label(driver: LiveHoldoutDriver) -> &'static str {
    match driver {
        LiveHoldoutDriver::Naive => "naive",
        LiveHoldoutDriver::Alloy => "alloy",
    }
}

fn profile_label(profile: Option<&str>) -> &str {
    profile.unwrap_or("none")
}

fn render_report(report: &LiveHoldoutReport) -> String {
    let overall = &report.overall;
    let interval = overall
        .oracle
        .wilson95
        .map(|value| value.render())
        .unwrap_or_else(|| "unmeasured".to_owned());
    // Protocol and treatment are printed apart, so an operator reading the
    // terminal can see which one a later run changed.
    format!(
        "driver={} profile={} treatment_build={} protocol_evaluator={} protocol_corpus={}@{} overall oracle={}/{} rate={} wilson95={} process={}/{} compile={}/{} tests={}/{} compile_clean_reference_mismatch={}/{} compile_clean_tests_failed={}/{} tests_pass_reference_mismatch={}/{} reference={}/{} model_calls={} tokens_in={} tokens_out={}\n",
        driver_label(report.treatment.driver),
        profile_label(report.treatment.profile.as_deref()),
        report.treatment.build.binary_bundle_sha256,
        report.protocol.evaluator_revision,
        report.protocol.corpus,
        report.protocol.corpus_digest,
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
        overall.model_calls_total,
        overall.tokens_in_total,
        overall.tokens_out_total,
    )
}

fn render_comparison(comparison: &alloy_eval::LiveHoldoutMatrixComparison) -> String {
    // The protocol is stated once because every arm shares it; the treatment
    // is stated per arm because that is what differs.
    let mut output = format!(
        "baseline={} repetitions={} protocol_corpus={}@{} protocol_evaluator={} schema_version={}\narm\tdriver\tprofile\ttreatment_build\toracle\ttests\tcompile_clean_reference_mismatch\tcompile_clean_tests_failed\ttests_pass_reference_mismatch\toracle_wilson95\tmodel_calls\n",
        comparison.baseline,
        comparison.repetitions,
        comparison.protocol.corpus,
        comparison.protocol.corpus_digest,
        comparison.protocol.evaluator_revision,
        comparison.protocol.schema_version,
    );
    for (name, report) in &comparison.arms {
        let interval = report
            .overall
            .oracle
            .wilson95
            .map(|value| value.render())
            .unwrap_or_else(|| "unmeasured".to_owned());
        output.push_str(&format!(
            "{name}\t{}\t{}\t{}\t{}/{}\t{}/{}\t{}/{}\t{}/{}\t{}/{}\t{interval}\t{}\n",
            driver_label(report.treatment.driver),
            profile_label(report.treatment.profile.as_deref()),
            report.treatment.build.binary_bundle_sha256,
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
            report.overall.model_calls_total,
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
    output.push_str(&render_autonomous_gate(comparison));
    output
}

/// States the default-versus-autonomous verdict outright. Absence is printed
/// as absence: no such pair of arms ran, so no autonomous claim is available.
fn render_autonomous_gate(comparison: &alloy_eval::LiveHoldoutMatrixComparison) -> String {
    let Some(contrast) = &comparison.autonomous_vs_default else {
        return "autonomous_gate\tabsent\tno_single_default_and_autonomous_arm_pair\n".to_owned();
    };
    let clustered = &contrast.comparison.semantic_clustered;
    let bound = |value: Option<f64>| {
        value
            .map(|value| format!("{value:.6}"))
            .unwrap_or_else(|| "unbounded".to_owned())
    };
    format!(
        "autonomous_gate\t{}\tvs\t{}\t{}\t{}\tfixtures={}\tmean={}\tlower95={}\tupper95={}\n",
        contrast.comparison.arm,
        contrast.comparison.baseline,
        if contrast.clears_gate {
            "clears"
        } else {
            "blocked"
        },
        contrast.gate_basis,
        clustered.fixtures,
        bound(clustered.mean_delta),
        bound(clustered.lower95),
        bound(clustered.upper95),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_sha_rejects_uppercase_hex() {
        for (key, len) in [("source-revision", 40), ("binary-bundle-sha256", 64)] {
            let mut options = BTreeMap::new();
            options.insert(key.to_owned(), vec!["A".repeat(len)]);

            let error = parse_hex_sha(&options, key, len).unwrap_err();

            assert!(error.contains("lower-case"), "{error}");
        }
    }

    /// Protocol identity falls back to the treatment revision, so a runner
    /// that knows nothing of the split still scores; passing it explicitly is
    /// what lets one evaluator re-score several builds into one protocol.
    #[test]
    fn evaluator_revision_defaults_to_the_treatment_revision() {
        let treatment_revision = "a".repeat(40);
        let evaluator = "c".repeat(40);
        let mut options = BTreeMap::new();
        options.insert(
            "source-revision".to_owned(),
            vec![treatment_revision.clone()],
        );

        assert_eq!(evaluator_revision(&options).unwrap(), treatment_revision);

        options.insert("evaluator-revision".to_owned(), vec![evaluator.clone()]);
        assert_eq!(evaluator_revision(&options).unwrap(), evaluator);

        // A malformed override is refused rather than silently ignored.
        options.insert("evaluator-revision".to_owned(), vec!["nope".to_owned()]);
        assert!(evaluator_revision(&options).is_err());
    }
}
