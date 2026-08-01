//! Strict live-holdout oracle, scoring, and arm comparison.
//!
//! This module is pure: the shell runner owns process execution and cargo
//! post-checks, while Rust validates and aggregates their evidence.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::live_repair::{wilson_interval, WilsonInterval, WILSON_Z_95};

const CORPUS: &str = "rfc0016-holdout-live";
const TIMEOUT_EXIT_CODE: i32 = 124;
pub const REPORT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Deserialize)]
struct HoldoutManifest {
    naive_target_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrictObservation {
    pub fixture_id: String,
    pub repetition: u32,
    pub exit_code: i32,
    pub process_pass: bool,
    pub compile_clean: bool,
    pub tests_pass: bool,
    pub reference_match: bool,
    pub oracle_pass: bool,
    pub failure_class: String,
    pub cargo_check_exit: Option<i32>,
    pub cargo_test_exit: Option<i32>,
    pub repair_generations: u32,
    pub wall_ms: u64,
    pub evidence_relpath: String,
    pub model: String,
    pub temperature: f64,
    #[serde(default = "default_profile")]
    pub profile: String,
    pub base_url: String,
    pub corpus: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rate {
    pub passes: u32,
    pub attempts: u32,
    pub rate: Option<f64>,
    pub wilson95: Option<WilsonInterval>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FixtureSummary {
    pub fixture_id: String,
    pub process: Rate,
    pub compile_clean: Rate,
    pub tests_pass: Rate,
    pub compile_clean_reference_mismatch: Rate,
    pub compile_clean_tests_failed: Rate,
    pub tests_pass_reference_mismatch: Rate,
    pub reference_match: Rate,
    pub oracle: Rate,
    pub failure_classes: BTreeMap<String, u32>,
    pub repair_generations_total: u32,
    pub wall_ms_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Endpoint {
    pub model: String,
    pub temperature: f64,
    #[serde(default = "default_profile")]
    pub profile: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrictReport {
    pub schema_version: u32,
    pub corpus: String,
    pub endpoint: Endpoint,
    pub repetitions: u32,
    pub fixtures: Vec<FixtureSummary>,
    pub overall: FixtureSummary,
    pub observations: Vec<StrictObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Assessment {
    pub result: String,
    pub why_not: Option<String>,
    pub basis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricDelta {
    pub baseline_rate: Option<f64>,
    pub arm_rate: Option<f64>,
    pub delta: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmComparison {
    pub baseline: String,
    pub arm: String,
    pub oracle: MetricDelta,
    pub process: MetricDelta,
    pub compile_clean: MetricDelta,
    pub tests_pass: MetricDelta,
    pub compile_clean_reference_mismatch: MetricDelta,
    pub compile_clean_tests_failed: MetricDelta,
    pub tests_pass_reference_mismatch: MetricDelta,
    pub reference_match: MetricDelta,
    pub assessment: Assessment,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatrixComparison {
    pub schema_version: u32,
    pub corpus: String,
    pub repetitions: u32,
    pub baseline: String,
    pub arms: BTreeMap<String, StrictReport>,
    pub comparisons: Vec<ArmComparison>,
    pub notes: Vec<String>,
}

fn default_profile() -> String {
    "default".to_owned()
}

fn target_path(manifest: &Path) -> Result<PathBuf, String> {
    let raw = fs::read_to_string(manifest)
        .map_err(|error| format!("read {}: {error}", manifest.display()))?;
    let parsed: HoldoutManifest =
        toml::from_str(&raw).map_err(|error| format!("parse {}: {error}", manifest.display()))?;
    let path = PathBuf::from(&parsed.naive_target_path);
    if parsed.naive_target_path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "{}: naive_target_path must remain inside the fixture workspace",
            manifest.display()
        ));
    }
    Ok(path)
}

pub fn target_path_text(manifest: &Path) -> Result<String, String> {
    Ok(target_path(manifest)?.to_string_lossy().into_owned())
}

pub fn oracle(
    fixture_dir: &Path,
    workspace: &Path,
    run_log: &Path,
    exit_code: i32,
    compile_clean: bool,
    cargo_check_exit: Option<i32>,
    tests_pass: bool,
    cargo_test_exit: Option<i32>,
) -> Result<StrictObservationFields, String> {
    let relative_target = target_path(&fixture_dir.join("manifest.toml"))?;
    let actual = workspace.join(&relative_target);
    let expected = fixture_dir
        .join("workspace")
        .join(format!("{}.post", relative_target.to_string_lossy()));
    let log = fs::read_to_string(run_log)
        .map_err(|error| format!("unreadable run log {}: {error}", run_log.display()))?;
    let reference_match = if actual.is_file() && expected.is_file() {
        let actual_bytes = fs::read(&actual)
            .map_err(|error| format!("read actual {}: {error}", actual.display()))?;
        let expected_bytes = fs::read(&expected)
            .map_err(|error| format!("read expected {}: {error}", expected.display()))?;
        actual_bytes == expected_bytes
    } else {
        false
    };
    let postcheck_clean = compile_clean && cargo_check_exit == Some(0);
    let posttest_clean = tests_pass && cargo_test_exit == Some(0);
    let failure_class = classify(
        exit_code,
        &log,
        postcheck_clean,
        posttest_clean,
        reference_match,
    );
    Ok(StrictObservationFields {
        process_pass: exit_code == 0,
        compile_clean,
        tests_pass,
        reference_match,
        oracle_pass: exit_code == 0 && postcheck_clean && reference_match,
        failure_class,
        cargo_check_exit,
        cargo_test_exit,
        repair_generations: log.matches("repair generation replanned").count() as u32,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrictObservationFields {
    pub process_pass: bool,
    pub compile_clean: bool,
    pub tests_pass: bool,
    pub reference_match: bool,
    pub oracle_pass: bool,
    pub failure_class: String,
    pub cargo_check_exit: Option<i32>,
    pub cargo_test_exit: Option<i32>,
    pub repair_generations: u32,
}

fn classify(
    exit_code: i32,
    log: &str,
    postcheck_clean: bool,
    posttest_clean: bool,
    reference_match: bool,
) -> String {
    if exit_code == 0 && postcheck_clean && reference_match {
        return if posttest_clean {
            "pass"
        } else {
            "strict_pass_tests_failed"
        }
        .to_owned();
    }
    if exit_code == 0 && !postcheck_clean {
        return "process_claimed_success_but_compile_failed".to_owned();
    }
    if exit_code == 0 && !reference_match {
        return if posttest_clean {
            "reference_mismatch_tests_passed"
        } else {
            "reference_mismatch_tests_failed"
        }
        .to_owned();
    }
    if exit_code == TIMEOUT_EXIT_CODE {
        return "timeout".to_owned();
    }
    if matches!(exit_code, 126 | 127) {
        return "harness_error".to_owned();
    }
    for (needle, class) in [
        ("reason=\"kind\"", "replan_declined_kind"),
        ("reason=\"exhausted\"", "repair_budget_exhausted"),
        ("reason=\"deadline\"", "repair_deadline"),
        ("repair generation replanned", "process_failed_after_replan"),
    ] {
        if log.contains(needle) {
            return class.to_owned();
        }
    }
    "process_failed".to_owned()
}

pub fn load_observations(path: &Path) -> Result<Vec<StrictObservation>, String> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| format!("observation line {}: {error}", index + 1))
        })
        .collect()
}

fn fixture_ids(root: &Path) -> Result<Vec<String>, String> {
    let mut ids = fs::read_dir(root)
        .map_err(|error| format!("read fixtures {}: {error}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.is_dir() && path.join("manifest.toml").is_file())
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    ids.sort();
    if ids.is_empty() {
        return Err(format!("no fixture manifests under {}", root.display()));
    }
    Ok(ids)
}

fn rate(rows: &[StrictObservation], predicate: impl Fn(&StrictObservation) -> bool) -> Rate {
    let attempts = rows.len() as u32;
    let passes = rows.iter().filter(|row| predicate(row)).count() as u32;
    Rate {
        passes,
        attempts,
        rate: (attempts > 0).then_some(f64::from(passes) / f64::from(attempts)),
        wilson95: (attempts > 0).then(|| wilson_interval(passes, attempts, WILSON_Z_95)),
    }
}

fn summarize_fixture(id: &str, rows: &[StrictObservation]) -> FixtureSummary {
    let mut failure_classes = BTreeMap::new();
    for row in rows {
        *failure_classes
            .entry(row.failure_class.clone())
            .or_insert(0) += 1;
    }
    FixtureSummary {
        fixture_id: id.to_owned(),
        process: rate(rows, |row| row.process_pass),
        compile_clean: rate(rows, |row| row.compile_clean),
        tests_pass: rate(rows, |row| row.tests_pass),
        compile_clean_reference_mismatch: rate(rows, |row| {
            row.compile_clean && !row.reference_match
        }),
        compile_clean_tests_failed: rate(rows, |row| row.compile_clean && !row.tests_pass),
        tests_pass_reference_mismatch: rate(rows, |row| row.tests_pass && !row.reference_match),
        reference_match: rate(rows, |row| row.reference_match),
        oracle: rate(rows, |row| row.oracle_pass),
        failure_classes,
        repair_generations_total: rows.iter().map(|row| row.repair_generations).sum(),
        wall_ms_total: rows.iter().map(|row| row.wall_ms).sum(),
    }
}

pub fn score(
    fixtures: &Path,
    mut rows: Vec<StrictObservation>,
    endpoint: Endpoint,
    repetitions: u32,
) -> Result<StrictReport, String> {
    if repetitions == 0 {
        return Err("repetitions must be at least one".to_owned());
    }
    let ids = fixture_ids(fixtures)?;
    let expected: BTreeSet<_> = ids.iter().cloned().collect();
    let mut grouped: BTreeMap<String, Vec<StrictObservation>> =
        ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for row in &rows {
        if !expected.contains(&row.fixture_id) {
            return Err(format!("unknown fixture id {}", row.fixture_id));
        }
        if row.corpus != CORPUS
            || row.model != endpoint.model
            || row.base_url != endpoint.base_url
            || row.profile != endpoint.profile
            || row.temperature != endpoint.temperature
        {
            return Err(format!("endpoint identity mismatch for {}", row.fixture_id));
        }
        if row.process_pass != (row.exit_code == 0)
            || (row.compile_clean && row.cargo_check_exit != Some(0))
            || (!row.compile_clean && row.cargo_check_exit == Some(0))
            || (row.tests_pass && row.cargo_test_exit != Some(0))
            || (!row.tests_pass && row.cargo_test_exit == Some(0))
        {
            return Err(format!(
                "process/compile/test evidence inconsistency for {}",
                row.fixture_id
            ));
        }
        let expected_evidence = format!("{}/rep-{}", row.fixture_id, row.repetition);
        if row.evidence_relpath != expected_evidence {
            return Err(format!(
                "evidence path inconsistency for {}: expected {expected_evidence}",
                row.fixture_id
            ));
        }
        if row.oracle_pass
            != (row.process_pass
                && row.compile_clean
                && row.cargo_check_exit == Some(0)
                && row.reference_match)
        {
            return Err(format!(
                "oracle derivation inconsistency for {}",
                row.fixture_id
            ));
        }
        if row.process_pass {
            // Process-success classes are fully determined by compile/reference
            // evidence; reject spoofed labels such as timeout/reference swaps.
            let postcheck_clean = row.compile_clean && row.cargo_check_exit == Some(0);
            let posttest_clean = row.tests_pass && row.cargo_test_exit == Some(0);
            let expected_class = classify(
                row.exit_code,
                "",
                postcheck_clean,
                posttest_clean,
                row.reference_match,
            );
            if row.failure_class != expected_class {
                return Err(format!(
                    "failure-class consistency violation for {}: expected {expected_class}, got {}",
                    row.fixture_id, row.failure_class
                ));
            }
        } else if row.oracle_pass || row.failure_class == "pass" {
            return Err(format!(
                "failure-class consistency violation for {}",
                row.fixture_id
            ));
        }
        grouped
            .get_mut(&row.fixture_id)
            .expect("fixture id checked")
            .push(row.clone());
    }
    for (id, fixture_rows) in &mut grouped {
        fixture_rows.sort_by_key(|row| row.repetition);
        let expected_reps: Vec<_> = (1..=repetitions).collect();
        let actual: Vec<_> = fixture_rows.iter().map(|row| row.repetition).collect();
        if actual != expected_reps {
            return Err(format!(
                "fixture {id} repetitions {actual:?}, expected {expected_reps:?}"
            ));
        }
    }
    rows.sort_by(|left, right| {
        left.fixture_id
            .cmp(&right.fixture_id)
            .then(left.repetition.cmp(&right.repetition))
    });
    let fixtures = ids
        .iter()
        .map(|id| summarize_fixture(id, grouped.get(id).expect("fixture id exists")))
        .collect::<Vec<_>>();
    let overall = summarize_fixture("overall", &rows);
    Ok(StrictReport {
        schema_version: REPORT_SCHEMA_VERSION,
        corpus: CORPUS.to_owned(),
        endpoint,
        repetitions,
        fixtures,
        overall,
        observations: rows,
    })
}

fn delta(left: &Rate, right: &Rate) -> MetricDelta {
    MetricDelta {
        baseline_rate: left.rate,
        arm_rate: right.rate,
        delta: left.rate.zip(right.rate).map(|(base, arm)| arm - base),
    }
}

pub fn compare(named_reports: Vec<(String, StrictReport)>) -> Result<MatrixComparison, String> {
    if named_reports.len() < 2 {
        return Err("at least two reports are required".to_owned());
    }
    let (baseline_name, baseline) = &named_reports[0];
    let baseline_ids: BTreeSet<_> = baseline
        .fixtures
        .iter()
        .map(|fixture| fixture.fixture_id.clone())
        .collect();
    let mut arms = BTreeMap::new();
    for (name, report) in &named_reports {
        let ids: BTreeSet<_> = report
            .fixtures
            .iter()
            .map(|fixture| fixture.fixture_id.clone())
            .collect();
        if ids != baseline_ids || report.corpus != baseline.corpus {
            return Err(format!("arm {name} is incompatible with {baseline_name}"));
        }
        if report.repetitions != baseline.repetitions {
            return Err(format!(
                "arm {name} does not use repetitions={}",
                baseline.repetitions
            ));
        }
        if arms.insert(name.clone(), report.clone()).is_some() {
            return Err(format!("duplicate arm identity {name}"));
        }
    }
    let comparisons = named_reports[1..]
        .iter()
        .map(|(name, report)| {
            let oracle = delta(&baseline.overall.oracle, &report.overall.oracle);
            let (result, why_not, basis) = match oracle.delta {
                Some(value) if value > 0.0 => (
                    "improved".to_owned(),
                    None,
                    "strict_oracle_rate_delta_positive".to_owned(),
                ),
                Some(value) if value < 0.0 => (
                    "why_not".to_owned(),
                    Some("strict_oracle_rate_decreased".to_owned()),
                    "strict_oracle_rate_delta_negative".to_owned(),
                ),
                _ => (
                    "why_not".to_owned(),
                    Some("no_strict_oracle_rate_change".to_owned()),
                    "strict_oracle_rate_delta_zero".to_owned(),
                ),
            };
            ArmComparison {
                baseline: baseline_name.clone(),
                arm: name.clone(),
                oracle,
                process: delta(&baseline.overall.process, &report.overall.process),
                compile_clean: delta(
                    &baseline.overall.compile_clean,
                    &report.overall.compile_clean,
                ),
                tests_pass: delta(&baseline.overall.tests_pass, &report.overall.tests_pass),
                compile_clean_reference_mismatch: delta(
                    &baseline.overall.compile_clean_reference_mismatch,
                    &report.overall.compile_clean_reference_mismatch,
                ),
                compile_clean_tests_failed: delta(
                    &baseline.overall.compile_clean_tests_failed,
                    &report.overall.compile_clean_tests_failed,
                ),
                tests_pass_reference_mismatch: delta(
                    &baseline.overall.tests_pass_reference_mismatch,
                    &report.overall.tests_pass_reference_mismatch,
                ),
                reference_match: delta(
                    &baseline.overall.reference_match,
                    &report.overall.reference_match,
                ),
                assessment: Assessment {
                    result,
                    why_not,
                    basis,
                },
            }
        })
        .collect();
    Ok(MatrixComparison {
        schema_version: REPORT_SCHEMA_VERSION,
        corpus: baseline.corpus.clone(),
        repetitions: baseline.repetitions,
        baseline: baseline_name.clone(),
        arms,
        comparisons,
        notes: vec![
            "Each arm retains its own denominator and Wilson interval.".to_owned(),
            "Reports are compared only when fixture sets, corpus, and repetitions match."
                .to_owned(),
            "Deltas are descriptive; overlapping Wilson intervals are not a significance test."
                .to_owned(),
            "Strict-oracle results are live-BYOM telemetry, not an offline release gate."
                .to_owned(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            model: "stub-model".to_owned(),
            temperature: 0.6,
            profile: "default".to_owned(),
            base_url: "http://127.0.0.1:8089/v1/".to_owned(),
        }
    }

    fn observation(fixture_id: &str, repetition: u32) -> StrictObservation {
        let endpoint = endpoint();
        StrictObservation {
            fixture_id: fixture_id.to_owned(),
            repetition,
            exit_code: 0,
            process_pass: true,
            compile_clean: true,
            tests_pass: true,
            reference_match: true,
            oracle_pass: true,
            failure_class: "pass".to_owned(),
            cargo_check_exit: Some(0),
            cargo_test_exit: Some(0),
            repair_generations: 0,
            wall_ms: 10,
            evidence_relpath: format!("{fixture_id}/rep-{repetition}"),
            model: endpoint.model,
            temperature: endpoint.temperature,
            profile: endpoint.profile,
            base_url: endpoint.base_url,
            corpus: CORPUS.to_owned(),
        }
    }

    fn fixtures_with(ids: &[&str]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for id in ids {
            let fixture = directory.path().join(id);
            fs::create_dir_all(&fixture).unwrap();
            fs::write(
                fixture.join("manifest.toml"),
                "naive_target_path = \"src/lib.rs\"\n",
            )
            .unwrap();
        }
        directory
    }

    fn report_for(oracle_pass: bool) -> StrictReport {
        let fixtures = fixtures_with(&["shared"]);
        let mut row = observation("shared", 1);
        if !oracle_pass {
            row.reference_match = false;
            row.oracle_pass = false;
            row.failure_class = "reference_mismatch_tests_passed".to_owned();
        }
        score(fixtures.path(), vec![row], endpoint(), 1).unwrap()
    }

    #[test]
    fn classifies_strict_success_and_false_green() {
        assert_eq!(classify(0, "", true, true, true), "pass");
        assert_eq!(
            classify(0, "", true, false, true),
            "strict_pass_tests_failed"
        );
        assert_eq!(
            classify(0, "", true, false, false),
            "reference_mismatch_tests_failed"
        );
        assert_eq!(
            classify(0, "", true, true, false),
            "reference_mismatch_tests_passed"
        );
        assert_eq!(
            classify(0, "", false, false, true),
            "process_claimed_success_but_compile_failed"
        );
        assert_eq!(
            classify(5, "reason=\"kind\"", false, false, false),
            "replan_declined_kind"
        );
    }

    #[test]
    fn rejects_unsafe_target_paths() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("manifest.toml");
        fs::write(&manifest, "naive_target_path = '../escape.rs'\n").unwrap();
        assert!(target_path(&manifest).is_err());
    }

    #[test]
    fn score_accepts_dense_consistent_observations() {
        let fixtures = fixtures_with(&["a", "b"]);
        let rows = vec![
            observation("a", 1),
            observation("a", 2),
            observation("b", 1),
            observation("b", 2),
        ];
        let report = score(fixtures.path(), rows, endpoint(), 2).unwrap();
        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(report.overall.oracle.passes, 4);
        assert_eq!(report.overall.oracle.attempts, 4);
        assert_eq!(report.overall.compile_clean_reference_mismatch.passes, 0);
        assert_eq!(report.fixtures.len(), 2);
    }

    #[test]
    fn score_rejects_endpoint_identity_mismatch() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.temperature = 0.7;
        let error = score(fixtures.path(), vec![row], endpoint(), 1).unwrap_err();
        assert!(error.contains("endpoint identity mismatch"), "{error}");
    }

    #[test]
    fn score_rejects_process_compile_inconsistency() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.compile_clean = true;
        row.cargo_check_exit = Some(101);
        row.oracle_pass = false;
        row.failure_class = "process_claimed_success_but_compile_failed".to_owned();
        let error = score(fixtures.path(), vec![row], endpoint(), 1).unwrap_err();
        assert!(
            error.contains("process/compile/test evidence inconsistency"),
            "{error}"
        );
    }

    #[test]
    fn score_rejects_test_and_evidence_inconsistency() {
        let fixtures = fixtures_with(&["a"]);
        let mut test_row = observation("a", 1);
        test_row.cargo_test_exit = Some(101);
        let error = score(fixtures.path(), vec![test_row], endpoint(), 1).unwrap_err();
        assert!(
            error.contains("process/compile/test evidence inconsistency"),
            "{error}"
        );

        let mut path_row = observation("a", 1);
        path_row.evidence_relpath = "../escape".to_owned();
        let error = score(fixtures.path(), vec![path_row], endpoint(), 1).unwrap_err();
        assert!(error.contains("evidence path inconsistency"), "{error}");
    }

    #[test]
    fn score_rejects_spoofed_failure_class_on_process_success() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.reference_match = false;
        row.oracle_pass = false;
        row.failure_class = "timeout".to_owned();
        let error = score(fixtures.path(), vec![row], endpoint(), 1).unwrap_err();
        assert!(
            error.contains("failure-class consistency violation")
                && error.contains("reference_mismatch_tests_passed"),
            "{error}"
        );
    }

    #[test]
    fn score_rejects_duplicate_and_missing_repetitions() {
        let fixtures = fixtures_with(&["a"]);
        let duplicate = score(
            fixtures.path(),
            vec![observation("a", 1), observation("a", 1)],
            endpoint(),
            2,
        )
        .unwrap_err();
        assert!(duplicate.contains("repetitions"), "{duplicate}");

        let missing = score(fixtures.path(), vec![observation("a", 1)], endpoint(), 2).unwrap_err();
        assert!(missing.contains("repetitions"), "{missing}");
    }

    #[test]
    fn compare_rejects_duplicate_arm_identity() {
        let baseline = report_for(true);
        let arm = report_for(false);
        let error = compare(vec![
            ("baseline".to_owned(), baseline),
            ("baseline".to_owned(), arm),
        ])
        .unwrap_err();
        assert!(error.contains("duplicate arm identity"), "{error}");
    }

    #[test]
    fn compare_rejects_incompatible_inputs() {
        let fixtures_a = fixtures_with(&["a"]);
        let fixtures_b = fixtures_with(&["b"]);
        let left = score(fixtures_a.path(), vec![observation("a", 1)], endpoint(), 1).unwrap();
        let right = score(fixtures_b.path(), vec![observation("b", 1)], endpoint(), 1).unwrap();
        let error = compare(vec![
            ("baseline".to_owned(), left),
            ("arm".to_owned(), right),
        ])
        .unwrap_err();
        assert!(error.contains("incompatible"), "{error}");
    }

    #[test]
    fn compare_accepts_matching_arms() {
        let baseline = report_for(true);
        let arm = report_for(false);
        let matrix = compare(vec![
            ("baseline".to_owned(), baseline),
            ("candidate".to_owned(), arm),
        ])
        .unwrap();
        assert_eq!(matrix.comparisons.len(), 1);
        assert_eq!(matrix.comparisons[0].assessment.result, "why_not");
        assert_eq!(
            matrix.comparisons[0].compile_clean_reference_mismatch.delta,
            Some(1.0)
        );
        assert_eq!(
            matrix.comparisons[0].tests_pass_reference_mismatch.delta,
            Some(1.0)
        );
    }
}
