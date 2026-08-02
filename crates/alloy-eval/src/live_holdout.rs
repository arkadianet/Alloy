//! Strict live-holdout oracle, scoring, and arm comparison.
//!
//! This module is pure: the shell runner owns process execution and cargo
//! post-checks, while Rust validates and aggregates their evidence.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use alloy_runtime::DigestHasher;
use serde::{Deserialize, Serialize};

use crate::live_repair::{wilson_interval, WilsonInterval, WILSON_Z_95};

const CORPUS: &str = "rfc0016-holdout-live";
const TIMEOUT_EXIT_CODE: i32 = 124;
/// Page size `eval/live-holdout/run.sh` requests from `alloy events`, which
/// is also the runtime's maximum page. An export this long is treated as
/// truncated rather than complete.
const EVENT_EXPORT_PAGE_LIMIT: usize = 1_000;
/// v6 splits the single conflated harness identity into a [`ProtocolIdentity`]
/// (which corpus, which evaluator, which schema scored this) and a
/// [`TreatmentIdentity`] (which product build, driver, and profile produced
/// it). Arms are comparable when the protocol is identical and the treatment
/// differs — the treatment difference is the measurement.
///
/// v5 made `semantic_pass` the primary outcome; v4 carries no semantic
/// measurement at all. Both are readable as archives but are refused for
/// scoring and comparison with an explicit legacy message rather than being
/// silently reinterpreted under the v6 contract. Their raw observations, which
/// carry no schema version, re-score cleanly under v6.
pub const REPORT_SCHEMA_VERSION: u32 = 6;
/// Domain separator for [`corpus_digest`], so a digest can never be confused
/// with a hash of some other structure.
const CORPUS_DIGEST_DOMAIN: &[u8] = b"alloy-live-holdout-corpus-v1\n";

#[derive(Debug, Deserialize)]
struct HoldoutManifest {
    naive_target_path: String,
}

/// Which driver produced an observation: the naive single-shot baseline or
/// the Alloy orchestrated agent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveHoldoutDriver {
    Naive,
    Alloy,
}

/// Build provenance of the product under test: the commit its binaries were
/// built from, and the digest of the exact bundle that ran.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreatmentBuild {
    pub source_revision: String,
    pub binary_bundle_sha256: String,
}

/// What produced the repairs: which build, driven how, under which profile.
///
/// This is the thing an experiment is allowed to change. Two arms that differ
/// here and nowhere else are exactly what a comparison measures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreatmentIdentity {
    pub build: TreatmentBuild,
    pub driver: LiveHoldoutDriver,
    pub profile: Option<String>,
}

/// What scored an observation set: which corpus (by name and by content),
/// which evaluator, and under which schema.
///
/// This is the thing an experiment must NOT change between arms. It is a
/// property of the scoring act rather than of the run, so re-scoring old
/// observations with one fixed evaluator puts every arm under one protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolIdentity {
    pub corpus: String,
    /// Digest over the corpus's oracle inputs, so a silently edited fixture
    /// scores under a visibly different protocol instead of passing as the
    /// same one.
    pub corpus_digest: String,
    /// Revision of the evaluator and corpus checkout that did the scoring —
    /// not of the product binaries, which live in [`TreatmentBuild`].
    pub evaluator_revision: String,
    pub schema_version: u32,
}

/// Everything the scorer must be told about one arm: which evaluator is
/// scoring it, which endpoint it talked to, and which treatment produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmIdentity {
    pub evaluator_revision: String,
    pub endpoint: Endpoint,
    pub treatment: TreatmentIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrictObservation {
    pub fixture_id: String,
    pub repetition: u32,
    pub exit_code: i32,
    pub process_pass: bool,
    pub compile_clean: bool,
    pub tests_pass: bool,
    /// Manifest safety and diff-policy criteria held (no new `unsafe`).
    pub safety_clean: bool,
    /// Primary v5 outcome: the repair ran, compiled independently, passed the
    /// hidden tests, and violated no safety criterion. Deliberately free of
    /// byte-canonicality.
    pub semantic_pass: bool,
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
    pub driver: LiveHoldoutDriver,
    pub profile: Option<String>,
    pub base_url: String,
    /// Build provenance of the treatment that produced this attempt. v5
    /// evidence spells this field `harness`; the alias keeps those raw
    /// observations re-scorable instead of stranding them.
    #[serde(alias = "harness")]
    pub build: TreatmentBuild,
    pub corpus: String,
    pub model_calls: u32,
    /// Provider-reported usage, absent when the provider did not report it.
    /// Unknown usage stays `null`; it never collapses to zero.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
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
    /// Primary v5 metric.
    pub semantic: Rate,
    pub safety_clean: Rate,
    /// Compiled and claimed success, but the hidden tests disagreed.
    pub semantic_false_green: Rate,
    pub compile_clean_reference_mismatch: Rate,
    pub compile_clean_tests_failed: Rate,
    pub tests_pass_reference_mismatch: Rate,
    pub reference_match: Rate,
    pub oracle: Rate,
    pub failure_classes: BTreeMap<String, u32>,
    pub repair_generations_total: u32,
    pub wall_ms_total: u64,
    pub model_calls_total: u64,
    pub tokens_in_total: u64,
    pub tokens_out_total: u64,
    /// Attempts whose provider reported no usage. Totals above sum only what
    /// was reported, so these counts are what stops a silent zero from
    /// reading as a measured zero.
    pub tokens_in_unknown: u32,
    pub tokens_out_unknown: u32,
}

/// The raw model endpoint an arm talked to. Driver and profile are treatment,
/// not endpoint, so they live in [`TreatmentIdentity`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Endpoint {
    pub model: String,
    pub temperature: f64,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrictReport {
    pub schema_version: u32,
    /// Must be identical across compared arms.
    pub protocol: ProtocolIdentity,
    /// Expected to differ across compared arms.
    pub treatment: TreatmentIdentity,
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

/// Per-family macro effect, so a headline cannot be carried by one family.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FamilyDelta {
    pub family: String,
    pub fixtures: u32,
    pub baseline_rate: Option<f64>,
    pub arm_rate: Option<f64>,
    pub delta: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmComparison {
    pub baseline: String,
    pub arm: String,
    /// Primary v5 treatment effect, pooled over attempts.
    pub semantic: MetricDelta,
    /// The same effect with fixtures as the unit of analysis. The E2 gate is
    /// stated on this bound, not on the pooled one.
    pub semantic_clustered: ClusteredDelta,
    /// Macro-averaged over families, so families count equally regardless of
    /// how many fixtures each contributes.
    pub semantic_family_macro: Option<f64>,
    pub families: Vec<FamilyDelta>,
    /// The smallest family-macro delta obtainable by dropping any one family.
    /// If this is not positive, the headline depends on a single family.
    pub leave_one_family_out_min: Option<f64>,
    pub safety_clean: MetricDelta,
    pub semantic_false_green: MetricDelta,
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

/// The direct default-versus-autonomous contrast, with its own gate answered.
///
/// The primary metric is baselined on the first arm (naive, in E2), so the two
/// Alloy profiles are never compared to each other by that path. This carries
/// that comparison separately and states the verdict instead of leaving a
/// reader to derive it from the interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutonomousContrast {
    /// Default-baselined comparison carrying the same clustered and
    /// per-family evidence as the primary comparisons.
    pub comparison: ArmComparison,
    /// Whether autonomous uplift over default may be claimed at all.
    pub clears_gate: bool,
    /// Why `clears_gate` reads as it does.
    pub gate_basis: String,
}

impl AutonomousContrast {
    fn new(comparison: ArmComparison) -> Self {
        let clears_gate = clears_autonomous_gate(&comparison.semantic_clustered);
        let gate_basis = match comparison.semantic_clustered.lower95 {
            Some(lower) if lower > 0.0 => "clustered_lower95_above_zero",
            Some(_) => "clustered_lower95_not_above_zero",
            None => "clustered_lower95_unbounded",
        }
        .to_owned();
        Self {
            comparison,
            clears_gate,
            gate_basis,
        }
    }
}

/// The E2 autonomous gate: uplift over the default profile counts only when
/// the fixture-clustered 95% lower bound is strictly above zero.
///
/// An unbounded delta — fewer than two fixtures, so between-fixture variance
/// is unestimable — certifies nothing. "Cannot bound" fails the gate; it is
/// never read as passing.
pub fn clears_autonomous_gate(delta: &ClusteredDelta) -> bool {
    delta.lower95.is_some_and(|lower| lower > 0.0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatrixComparison {
    pub schema_version: u32,
    /// The one protocol every arm was scored under. Arms that disagree here
    /// are never compared, so a comparison always has exactly one.
    pub protocol: ProtocolIdentity,
    pub repetitions: u32,
    pub baseline: String,
    pub arms: BTreeMap<String, StrictReport>,
    pub comparisons: Vec<ArmComparison>,
    /// Present only when the run holds exactly one Alloy `default` arm and
    /// exactly one Alloy `autonomous` arm. Any other shape leaves it absent:
    /// the question was not asked, which is not a measured null effect.
    #[serde(default)]
    pub autonomous_vs_default: Option<AutonomousContrast>,
    pub notes: Vec<String>,
}

impl MatrixComparison {
    /// Whether this run licenses a claim of autonomous uplift. Fails closed:
    /// no direct default-versus-autonomous contrast means no claim.
    pub fn autonomous_uplift_is_claimable(&self) -> bool {
        self.autonomous_vs_default
            .as_ref()
            .is_some_and(|contrast| contrast.clears_gate)
    }
}

/// A treatment effect with fixtures, not attempts, as the unit of analysis.
///
/// Repetitions of one fixture are correlated, so pooling them as i.i.d.
/// understates uncertainty. Arms share the corpus, so the difference is paired
/// per fixture and summarised across fixtures — deterministic, no resampling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClusteredDelta {
    pub fixtures: u32,
    pub mean_delta: Option<f64>,
    pub lower95: Option<f64>,
    pub upper95: Option<f64>,
}

/// Two-sided 95% Student-t critical values. The corpus has tens of fixtures,
/// not thousands, so the normal approximation is not safe at these degrees of
/// freedom.
fn t_critical_95(df: usize) -> f64 {
    const TABLE: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    TABLE.get(df.saturating_sub(1)).copied().unwrap_or(1.960)
}

/// Pairs `(rate, attempts)` per fixture, in matching fixture order.
fn clustered_delta(baseline: &[(f64, u32)], arm: &[(f64, u32)]) -> ClusteredDelta {
    let deltas: Vec<f64> = baseline
        .iter()
        .zip(arm)
        .map(|((base, _), (treated, _))| treated - base)
        .collect();
    let k = deltas.len();
    if k == 0 {
        return ClusteredDelta {
            fixtures: 0,
            mean_delta: None,
            lower95: None,
            upper95: None,
        };
    }
    let mean = deltas.iter().sum::<f64>() / k as f64;
    if k < 2 {
        // One cluster carries no between-fixture variance to estimate.
        return ClusteredDelta {
            fixtures: k as u32,
            mean_delta: Some(mean),
            lower95: None,
            upper95: None,
        };
    }
    let variance = deltas
        .iter()
        .map(|delta| (delta - mean).powi(2))
        .sum::<f64>()
        / (k - 1) as f64;
    let half_width = t_critical_95(k - 1) * (variance / k as f64).sqrt();
    ClusteredDelta {
        fixtures: k as u32,
        mean_delta: Some(mean),
        lower95: Some(mean - half_width),
        upper95: Some(mean + half_width),
    }
}

/// Repair family for macro-averaging and leave-one-family-out checks. Fixture
/// ids lead with their rustc error code, so that prefix is the family.
fn fixture_family(fixture_id: &str) -> String {
    let head = fixture_id.split('_').next().unwrap_or(fixture_id);
    let is_error_code =
        head.len() == 5 && head.starts_with('e') && head[1..].chars().all(|c| c.is_ascii_digit());
    if is_error_code {
        head.to_owned()
    } else {
        fixture_id.to_owned()
    }
}

/// Macro-averages each family's fixture rates, then differences them. Families
/// count equally, so a family with many fixtures cannot dominate the headline.
fn family_deltas(baseline: &[FixtureSummary], arm: &[FixtureSummary]) -> Vec<FamilyDelta> {
    let mut grouped: BTreeMap<String, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for (base, treated) in baseline.iter().zip(arm) {
        let entry = grouped.entry(fixture_family(&base.fixture_id)).or_default();
        if let Some(rate) = base.semantic.rate {
            entry.0.push(rate);
        }
        if let Some(rate) = treated.semantic.rate {
            entry.1.push(rate);
        }
    }
    let mean = |values: &[f64]| -> Option<f64> {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    grouped
        .into_iter()
        .map(|(family, (base_rates, arm_rates))| {
            let baseline_rate = mean(&base_rates);
            let arm_rate = mean(&arm_rates);
            FamilyDelta {
                family,
                fixtures: base_rates.len() as u32,
                baseline_rate,
                arm_rate,
                delta: baseline_rate.zip(arm_rate).map(|(b, a)| a - b),
            }
        })
        .collect()
}

/// The raw model endpoint the row reports. Separated from the treatment check
/// so an error can say which of the two disagreed.
fn endpoint_matches(row: &StrictObservation, endpoint: &Endpoint) -> bool {
    row.model == endpoint.model
        && row.temperature == endpoint.temperature
        && row.base_url == endpoint.base_url
}

/// The build, driver, and profile the row reports. Rows from two treatments
/// never belong in one arm, however alike their endpoints look.
fn treatment_matches(row: &StrictObservation, treatment: &TreatmentIdentity) -> bool {
    row.build == treatment.build
        && row.driver == treatment.driver
        && row.profile == treatment.profile
}

/// Naive observations carry no profile; Alloy observations must declare one
/// of the two shipped profiles. Fails closed on anything else.
fn validate_driver_profile(driver: LiveHoldoutDriver, profile: Option<&str>) -> Result<(), String> {
    match (driver, profile) {
        (LiveHoldoutDriver::Naive, None) => Ok(()),
        (LiveHoldoutDriver::Naive, Some(_)) => {
            Err("naive driver must not declare a profile".to_owned())
        }
        (LiveHoldoutDriver::Alloy, Some("default" | "autonomous")) => Ok(()),
        (LiveHoldoutDriver::Alloy, _) => {
            Err("alloy driver requires profile default or autonomous".to_owned())
        }
    }
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
    evidence: OracleEvidence,
) -> Result<StrictObservationFields, String> {
    let OracleEvidence {
        exit_code,
        compile_clean,
        cargo_check_exit,
        tests_pass,
        cargo_test_exit,
    } = evidence;
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
    let original = fixture_dir.join("workspace").join(&relative_target);
    let safety_clean = no_new_unsafe(&original, &actual)?;
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
        safety_clean,
        // Primary outcome. Byte canonicality is deliberately absent.
        semantic_pass: exit_code == 0 && postcheck_clean && posttest_clean && safety_clean,
        reference_match,
        oracle_pass: exit_code == 0 && postcheck_clean && posttest_clean && reference_match,
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
    pub safety_clean: bool,
    pub semantic_pass: bool,
    pub reference_match: bool,
    pub oracle_pass: bool,
    pub failure_class: String,
    pub cargo_check_exit: Option<i32>,
    pub cargo_test_exit: Option<i32>,
    pub repair_generations: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleEvidence {
    pub exit_code: i32,
    pub compile_clean: bool,
    pub cargo_check_exit: Option<i32>,
    pub tests_pass: bool,
    pub cargo_test_exit: Option<i32>,
}

/// `no_new_unsafe`: a repair may keep `unsafe` the fixture already had, but
/// may never introduce it. A missing repaired file is not a safety verdict —
/// the compile and test gates already fail that attempt.
fn no_new_unsafe(original: &Path, repaired: &Path) -> Result<bool, String> {
    if !repaired.is_file() {
        return Ok(true);
    }
    let after = fs::read_to_string(repaired)
        .map_err(|error| format!("read repaired {}: {error}", repaired.display()))?;
    let before = if original.is_file() {
        fs::read_to_string(original)
            .map_err(|error| format!("read original {}: {error}", original.display()))?
    } else {
        String::new()
    };
    Ok(unsafe_blocks(&after) <= unsafe_blocks(&before))
}

/// Counts `unsafe` as a Rust keyword, not as a substring of identifiers or
/// prose, so `is_unsafe_mode` and a doc comment do not read as violations.
fn unsafe_blocks(source: &str) -> usize {
    source
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| *token == "unsafe")
        .count()
}

fn classify(
    exit_code: i32,
    log: &str,
    postcheck_clean: bool,
    posttest_clean: bool,
    reference_match: bool,
) -> String {
    if exit_code == 0 && postcheck_clean && posttest_clean {
        return if reference_match {
            "pass"
        } else {
            // Behaviour is correct; only byte canonicality differs. This is
            // the E0308 trailing-newline case, not a repair failure.
            "pass_reference_mismatch"
        }
        .to_owned();
    }
    if exit_code == 0 && !postcheck_clean {
        return "process_claimed_success_but_compile_failed".to_owned();
    }
    // Compiled and claimed success, but the hidden tests disagreed.
    if exit_code == 0 && postcheck_clean && !posttest_clean {
        return "semantic_false_green".to_owned();
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

/// Per-attempt model usage extracted from one driver's evidence file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTelemetry {
    pub model_calls: u32,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

impl RunTelemetry {
    /// No telemetry was recorded at all.
    const UNRECORDED: Self = Self {
        model_calls: 0,
        tokens_in: None,
        tokens_out: None,
    };
}

#[derive(Debug, Deserialize)]
struct NaiveResult {
    model_calls: u32,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
}

/// Read `input` and extract the driver's model-call and token telemetry.
///
/// Missing naive telemetry is a harness error: the one-shot driver must
/// account for its one call. Alloy may report no events only when its runner
/// explicitly retained a successful empty export.
pub fn telemetry(driver: LiveHoldoutDriver, input: &Path) -> Result<RunTelemetry, String> {
    let raw = match fs::read_to_string(input) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return match driver {
                LiveHoldoutDriver::Naive => {
                    Err(format!("naive telemetry missing: {}", input.display()))
                }
                LiveHoldoutDriver::Alloy => Ok(RunTelemetry::UNRECORDED),
            };
        }
        Err(error) => return Err(format!("read {}: {error}", input.display())),
    };
    match driver {
        LiveHoldoutDriver::Naive => naive_telemetry(&raw),
        LiveHoldoutDriver::Alloy => alloy_telemetry(&raw),
    }
}

fn naive_telemetry(raw: &str) -> Result<RunTelemetry, String> {
    let result: NaiveResult =
        serde_json::from_str(raw).map_err(|error| format!("parse naive telemetry: {error}"))?;
    if result.model_calls != 1 {
        return Err(format!(
            "naive telemetry must record exactly one model call, got {}",
            result.model_calls
        ));
    }
    Ok(RunTelemetry {
        model_calls: result.model_calls,
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
    })
}

/// Count `model_call` events and sum the token fields the provider reported.
/// An empty export means the event dump produced nothing; it is reported as
/// zero calls with unknown usage rather than invented numbers.
///
/// An export that fills the page limit is rejected: `alloy events` returns
/// one page, so a full page means the run may have emitted more events than
/// were exported and any count taken from it would silently under-report.
fn alloy_telemetry(raw: &str) -> Result<RunTelemetry, String> {
    let mut telemetry = RunTelemetry::UNRECORDED;
    let mut events = 0usize;
    for (index, line) in raw
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let number = index + 1;
        let event: serde_json::Value =
            serde_json::from_str(line).map_err(|error| format!("event line {number}: {error}"))?;
        events += 1;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("model_call") {
            continue;
        }
        telemetry.model_calls = telemetry.model_calls.saturating_add(1);
        let payload = event.get("payload").unwrap_or(&serde_json::Value::Null);
        add_usage(&mut telemetry.tokens_in, payload, "input_tokens", number)?;
        add_usage(&mut telemetry.tokens_out, payload, "output_tokens", number)?;
    }
    if events >= EVENT_EXPORT_PAGE_LIMIT {
        return Err(format!(
            "event export holds {events} events, at the {EVENT_EXPORT_PAGE_LIMIT}-event page \
             limit; telemetry may be truncated"
        ));
    }
    Ok(telemetry)
}

fn add_usage(
    total: &mut Option<u64>,
    payload: &serde_json::Value,
    field: &str,
    line: usize,
) -> Result<(), String> {
    let Some(value) = payload.get(field).filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let count = value
        .as_u64()
        .ok_or_else(|| format!("event line {line}: {field} must be a non-negative integer"))?;
    *total = Some(total.unwrap_or(0).saturating_add(count));
    Ok(())
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

/// Deterministic digest over the corpus's oracle inputs: the sorted fixture
/// ids, and for each fixture its manifest, its whole starting workspace
/// (including the golden `.post` reference), and every hidden test.
///
/// This is what makes the corpus part of protocol identity rather than an
/// assumption. Editing a hidden test, a golden reference, or a starting
/// workspace moves the digest, so the edited corpus can no longer pass as the
/// one earlier arms were scored against. Non-oracle fixture material —
/// recordings, licences — is deliberately excluded: it cannot change what an
/// attempt is asked to do or how it is judged.
///
/// Every record is length-prefixed and path-tagged, so no rearrangement of
/// files can produce the same byte stream, and an absent input hashes as
/// absent rather than as empty.
pub fn corpus_digest(fixtures: &Path) -> Result<String, String> {
    let mut hasher = DigestHasher::new();
    hasher.update(CORPUS_DIGEST_DOMAIN);
    for id in fixture_ids(fixtures)? {
        hasher.update(b"fixture\0");
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
        let fixture = fixtures.join(&id);
        let mut inputs = vec![PathBuf::from("manifest.toml")];
        for directory in ["workspace", "oracle-tests"] {
            inputs.extend(files_under(&fixture.join(directory), Path::new(directory))?);
        }
        for relative in inputs {
            absorb_file(&mut hasher, &fixture, &relative)?;
        }
    }
    Ok(hasher.finish().as_hex().to_owned())
}

/// Every file under `root`, as paths relative to the fixture and sorted, so
/// the digest never depends on directory iteration order. A missing directory
/// contributes nothing; its absence still shows up as absent oracle inputs.
fn files_under(root: &Path, prefix: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", root.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", root.display()))?;
        let relative = prefix.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("stat {}: {error}", entry.path().display()))?;
        if kind.is_dir() {
            files.extend(files_under(&entry.path(), &relative)?);
        } else {
            files.push(relative);
        }
    }
    files.sort();
    Ok(files)
}

fn absorb_file(hasher: &mut DigestHasher, fixture: &Path, relative: &Path) -> Result<(), String> {
    let path = fixture.join(relative);
    hasher.update(relative.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    match fs::read(&path) {
        Ok(bytes) => {
            hasher.update(bytes.len().to_string().as_bytes());
            hasher.update(b"\0");
            hasher.update(&bytes);
        }
        // Defensive only: every path reaching here either came from a
        // directory listing taken moments ago or is the manifest that
        // `fixture_ids` already required. This arm exists so a file vanishing
        // in that window digests as absent instead of aborting the run; it is
        // not reachable by any corpus layout, so no test covers it.
        Err(error) if error.kind() == ErrorKind::NotFound => hasher.update(b"absent"),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    hasher.update(b"\n");
    Ok(())
}

/// Refuse evidence written under an older contract instead of coercing it.
///
/// The observation rows themselves carry no schema version and stay readable,
/// so the remedy for a legacy report is to re-score its raw observations under
/// the current contract — not to reinterpret a report whose fields meant
/// something else.
pub fn check_report_version(version: u32, source: &str) -> Result<(), String> {
    if version == REPORT_SCHEMA_VERSION {
        return Ok(());
    }
    if version > REPORT_SCHEMA_VERSION {
        return Err(format!(
            "{source} declares schema_version {version}, ahead of this evaluator's \
             v{REPORT_SCHEMA_VERSION}; score it with the build that wrote it"
        ));
    }
    Err(format!(
        "{source} is legacy evidence at schema_version {version}; this evaluator scores \
         v{REPORT_SCHEMA_VERSION}. Below v{REPORT_SCHEMA_VERSION} protocol identity (corpus, \
         corpus digest, evaluator revision, schema) is conflated with treatment identity \
         (binary bundle, driver, profile), and below v5 there is no semantic_pass evidence at \
         all. Re-score the raw observations under v{REPORT_SCHEMA_VERSION} rather than \
         comparing the legacy report"
    ))
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
        semantic: rate(rows, |row| row.semantic_pass),
        safety_clean: rate(rows, |row| row.safety_clean),
        semantic_false_green: rate(rows, |row| {
            row.process_pass && row.compile_clean && !row.tests_pass
        }),
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
        model_calls_total: rows.iter().map(|row| u64::from(row.model_calls)).sum(),
        // Totals sum reported usage only; rows with unknown usage contribute
        // nothing rather than a fabricated zero.
        tokens_in_total: rows.iter().filter_map(|row| row.tokens_in).sum(),
        tokens_out_total: rows.iter().filter_map(|row| row.tokens_out).sum(),
        tokens_in_unknown: rows.iter().filter(|row| row.tokens_in.is_none()).count() as u32,
        tokens_out_unknown: rows.iter().filter(|row| row.tokens_out.is_none()).count() as u32,
    }
}

pub fn score(
    fixtures: &Path,
    mut rows: Vec<StrictObservation>,
    arm: ArmIdentity,
    repetitions: u32,
) -> Result<StrictReport, String> {
    if repetitions == 0 {
        return Err("repetitions must be at least one".to_owned());
    }
    let ArmIdentity {
        evaluator_revision,
        endpoint,
        treatment,
    } = arm;
    if evaluator_revision.trim().is_empty() {
        return Err(
            "evaluator revision must name the checkout that scored this evidence".to_owned(),
        );
    }
    validate_driver_profile(treatment.driver, treatment.profile.as_deref())?;
    let ids = fixture_ids(fixtures)?;
    let expected: BTreeSet<_> = ids.iter().cloned().collect();
    let mut grouped: BTreeMap<String, Vec<StrictObservation>> =
        ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for row in &rows {
        if !expected.contains(&row.fixture_id) {
            return Err(format!("unknown fixture id {}", row.fixture_id));
        }
        if row.corpus != CORPUS || !endpoint_matches(row, &endpoint) {
            return Err(format!("endpoint identity mismatch for {}", row.fixture_id));
        }
        if !treatment_matches(row, &treatment) {
            return Err(format!(
                "treatment identity mismatch for {}",
                row.fixture_id
            ));
        }
        // The naive arm is exactly one completion with no retries. Missing
        // and extra calls both invalidate the attempted comparison.
        if row.driver == LiveHoldoutDriver::Naive && row.model_calls != 1 {
            return Err(format!(
                "naive driver recorded {} model calls for {}, expected exactly one",
                row.model_calls, row.fixture_id
            ));
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
        if row.semantic_pass
            != (row.process_pass
                && row.compile_clean
                && row.cargo_check_exit == Some(0)
                && row.tests_pass
                && row.cargo_test_exit == Some(0)
                && row.safety_clean)
        {
            return Err(format!(
                "semantic derivation inconsistency for {}",
                row.fixture_id
            ));
        }
        if row.oracle_pass
            != (row.process_pass
                && row.compile_clean
                && row.cargo_check_exit == Some(0)
                && row.tests_pass
                && row.cargo_test_exit == Some(0)
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
    let protocol = ProtocolIdentity {
        corpus: CORPUS.to_owned(),
        corpus_digest: corpus_digest(fixtures)?,
        evaluator_revision,
        schema_version: REPORT_SCHEMA_VERSION,
    };
    let summaries = ids
        .iter()
        .map(|id| summarize_fixture(id, grouped.get(id).expect("fixture id exists")))
        .collect::<Vec<_>>();
    let overall = summarize_fixture("overall", &rows);
    Ok(StrictReport {
        schema_version: REPORT_SCHEMA_VERSION,
        protocol,
        treatment,
        endpoint,
        repetitions,
        fixtures: summaries,
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

/// Builds one baseline-versus-arm comparison, including the fixture-clustered
/// interval and the per-family macro effects. Shared by the primary rows and
/// by the direct default-versus-autonomous contrast so both carry identical
/// rigour rather than the contrast being a weaker summary.
fn arm_comparison(
    baseline_name: &str,
    baseline: &StrictReport,
    arm_name: &str,
    report: &StrictReport,
) -> ArmComparison {
    let oracle = delta(&baseline.overall.oracle, &report.overall.oracle);
    // v5 assesses on semantics. `oracle` stays in the report as canonicality
    // telemetry but no longer decides the verdict.
    let semantic = delta(&baseline.overall.semantic, &report.overall.semantic);
    let (result, why_not, basis) = match semantic.delta {
        Some(value) if value > 0.0 => (
            "improved".to_owned(),
            None,
            "semantic_rate_delta_positive".to_owned(),
        ),
        Some(value) if value < 0.0 => (
            "why_not".to_owned(),
            Some("semantic_rate_decreased".to_owned()),
            "semantic_rate_delta_negative".to_owned(),
        ),
        _ => (
            "why_not".to_owned(),
            Some("no_semantic_rate_change".to_owned()),
            "semantic_rate_delta_zero".to_owned(),
        ),
    };
    let paired = |summaries: &[FixtureSummary]| -> Vec<(f64, u32)> {
        summaries
            .iter()
            .map(|f| (f.semantic.rate.unwrap_or(0.0), f.semantic.attempts))
            .collect()
    };
    let semantic_clustered =
        clustered_delta(&paired(&baseline.fixtures), &paired(&report.fixtures));
    let families = family_deltas(&baseline.fixtures, &report.fixtures);
    let family_deltas_only: Vec<f64> = families.iter().filter_map(|f| f.delta).collect();
    let macro_mean = |values: &[f64]| -> Option<f64> {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let semantic_family_macro = macro_mean(&family_deltas_only);
    // Drop each family in turn; the worst result shows how much the headline
    // leans on any single family.
    let leave_one_family_out_min = (family_deltas_only.len() > 1)
        .then(|| {
            (0..family_deltas_only.len())
                .filter_map(|skip| {
                    let kept: Vec<f64> = family_deltas_only
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != skip)
                        .map(|(_, v)| *v)
                        .collect();
                    macro_mean(&kept)
                })
                .fold(f64::INFINITY, f64::min)
        })
        .filter(|value| value.is_finite());
    ArmComparison {
        baseline: baseline_name.to_owned(),
        arm: arm_name.to_owned(),
        semantic,
        semantic_clustered,
        semantic_family_macro,
        families,
        leave_one_family_out_min,
        safety_clean: delta(&baseline.overall.safety_clean, &report.overall.safety_clean),
        semantic_false_green: delta(
            &baseline.overall.semantic_false_green,
            &report.overall.semantic_false_green,
        ),
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
}

/// The one arm running `profile` under the Alloy driver, by treatment identity
/// rather than by arm name or position. `None` unless exactly one arm carries
/// the profile, so an ambiguous matrix can never be silently resolved.
fn profile_arm<'a>(
    named_reports: &'a [(String, StrictReport)],
    profile: &str,
) -> Option<&'a (String, StrictReport)> {
    let mut matched = named_reports.iter().filter(|(_, report)| {
        report.treatment.driver == LiveHoldoutDriver::Alloy
            && report.treatment.profile.as_deref() == Some(profile)
    });
    let found = matched.next()?;
    matched.next().is_none().then_some(found)
}

/// Names the first component of the protocol that differs, or `None` when the
/// two arms were scored under exactly the same protocol.
fn protocol_drift(baseline: &ProtocolIdentity, other: &ProtocolIdentity) -> Option<String> {
    if other.corpus != baseline.corpus {
        return Some(format!(
            "corpus {} is not {}",
            other.corpus, baseline.corpus
        ));
    }
    if other.corpus_digest != baseline.corpus_digest {
        return Some(format!(
            "corpus_digest {} is not {}; the corpus changed underneath the arms",
            other.corpus_digest, baseline.corpus_digest
        ));
    }
    if other.evaluator_revision != baseline.evaluator_revision {
        return Some(format!(
            "evaluator_revision {} is not {}",
            other.evaluator_revision, baseline.evaluator_revision
        ));
    }
    (other.schema_version != baseline.schema_version).then(|| {
        format!(
            "schema_version {} is not {}",
            other.schema_version, baseline.schema_version
        )
    })
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
        check_report_version(report.schema_version, &format!("arm {name}"))?;
        if report.protocol.schema_version != report.schema_version {
            return Err(format!(
                "arm {name} declares schema_version {} but its protocol identity says {}",
                report.schema_version, report.protocol.schema_version
            ));
        }
        let ids: BTreeSet<_> = report
            .fixtures
            .iter()
            .map(|fixture| fixture.fixture_id.clone())
            .collect();
        if ids != baseline_ids {
            return Err(format!("arm {name} is incompatible with {baseline_name}"));
        }
        if report.repetitions != baseline.repetitions {
            return Err(format!(
                "arm {name} does not use repetitions={}",
                baseline.repetitions
            ));
        }
        // The treatment — build, driver, profile — is exactly what a
        // comparison is for, so it may differ freely. The protocol may not:
        // a comparison across two protocols measures the protocol change as
        // if it were a treatment effect.
        if let Some(drift) = protocol_drift(&baseline.protocol, &report.protocol) {
            return Err(format!(
                "arm {name} was scored under a different protocol than {baseline_name}: {drift}. \
                 Re-score every arm's raw observations with one evaluator over one corpus before \
                 comparing them"
            ));
        }
        if arms.insert(name.clone(), report.clone()).is_some() {
            return Err(format!("duplicate arm identity {name}"));
        }
    }
    let comparisons = named_reports[1..]
        .iter()
        .map(|(name, report)| arm_comparison(baseline_name, baseline, name, report))
        .collect();
    // The primary metric keeps its own baseline; this is an additional row so
    // the two Alloy profiles are compared to each other directly.
    let autonomous_vs_default = profile_arm(&named_reports, "default")
        .zip(profile_arm(&named_reports, "autonomous"))
        .map(
            |((default_name, default_report), (autonomous_name, autonomous_report))| {
                AutonomousContrast::new(arm_comparison(
                    default_name,
                    default_report,
                    autonomous_name,
                    autonomous_report,
                ))
            },
        );
    Ok(MatrixComparison {
        schema_version: REPORT_SCHEMA_VERSION,
        protocol: baseline.protocol.clone(),
        repetitions: baseline.repetitions,
        baseline: baseline_name.clone(),
        arms,
        comparisons,
        autonomous_vs_default,
        notes: vec![
            "Each arm retains its own denominator and Wilson interval.".to_owned(),
            "Arms are compared only when their protocol identity — corpus, corpus digest, \
             evaluator revision, schema — is identical, and their fixture sets and repetitions \
             match. Treatment identity (build, driver, profile) is expected to differ: that \
             difference is what is being measured."
                .to_owned(),
            "Deltas are descriptive; overlapping Wilson intervals are not a significance test."
                .to_owned(),
            "Autonomous uplift may be claimed only from autonomous_vs_default, and only when its \
             clustered 95% lower bound is above zero. An absent contrast is an unasked question, \
             not a null effect."
                .to_owned(),
            "Strict-oracle results are live-BYOM telemetry, not an offline release gate."
                .to_owned(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn treatment_build() -> TreatmentBuild {
        TreatmentBuild {
            source_revision: "a".repeat(40),
            binary_bundle_sha256: "b".repeat(64),
        }
    }

    fn treatment() -> TreatmentIdentity {
        TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Alloy,
            profile: Some("default".to_owned()),
        }
    }

    fn endpoint() -> Endpoint {
        Endpoint {
            model: "stub-model".to_owned(),
            temperature: 0.6,
            base_url: "http://127.0.0.1:8089/v1/".to_owned(),
        }
    }

    /// The evaluator revision is deliberately unlike any treatment revision:
    /// a test that passed by confusing the two would be measuring nothing.
    fn arm_identity() -> ArmIdentity {
        ArmIdentity {
            evaluator_revision: "c".repeat(40),
            endpoint: endpoint(),
            treatment: treatment(),
        }
    }

    fn arm_with(treatment: TreatmentIdentity) -> ArmIdentity {
        ArmIdentity {
            treatment,
            ..arm_identity()
        }
    }

    /// One arm's report over `ids`, every attempt a semantic pass, scored
    /// under the shared protocol but the given treatment.
    fn rescored_arm(
        fixtures: &Path,
        ids: &[&str],
        build: TreatmentBuild,
        profile: &str,
    ) -> StrictReport {
        let treatment = TreatmentIdentity {
            build,
            driver: LiveHoldoutDriver::Alloy,
            profile: Some(profile.to_owned()),
        };
        let rows = ids
            .iter()
            .map(|id| {
                let mut row = observation(id, 1);
                row.build = treatment.build.clone();
                row.profile = treatment.profile.clone();
                row
            })
            .collect();
        score(fixtures, rows, arm_with(treatment), 1).unwrap()
    }

    fn observation(fixture_id: &str, repetition: u32) -> StrictObservation {
        let endpoint = endpoint();
        let treatment = treatment();
        StrictObservation {
            fixture_id: fixture_id.to_owned(),
            repetition,
            exit_code: 0,
            process_pass: true,
            compile_clean: true,
            tests_pass: true,
            safety_clean: true,
            semantic_pass: true,
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
            driver: treatment.driver,
            profile: treatment.profile,
            base_url: endpoint.base_url,
            build: treatment.build,
            corpus: CORPUS.to_owned(),
            model_calls: 1,
            tokens_in: Some(100),
            tokens_out: Some(50),
        }
    }

    /// Derives observation fields the same way `oracle` does, without needing
    /// a workspace on disk, so the scoring rules can be tested directly.
    fn observation_fields(
        exit_code: i32,
        compile_clean: bool,
        tests_pass: bool,
        reference_match: bool,
        safety_clean: bool,
    ) -> StrictObservationFields {
        let postcheck_clean = compile_clean;
        let posttest_clean = tests_pass;
        StrictObservationFields {
            process_pass: exit_code == 0,
            compile_clean,
            tests_pass,
            safety_clean,
            semantic_pass: exit_code == 0 && postcheck_clean && posttest_clean && safety_clean,
            reference_match,
            oracle_pass: exit_code == 0 && postcheck_clean && posttest_clean && reference_match,
            failure_class: classify(
                exit_code,
                "",
                postcheck_clean,
                posttest_clean,
                reference_match,
            ),
            cargo_check_exit: Some(i32::from(!compile_clean)),
            cargo_test_exit: Some(i32::from(!tests_pass)),
            repair_generations: 0,
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
            row.failure_class = "pass_reference_mismatch".to_owned();
        }
        score(fixtures.path(), vec![row], arm_identity(), 1).unwrap()
    }

    fn report_for_driver(driver: LiveHoldoutDriver, profile: Option<String>) -> StrictReport {
        let fixtures = fixtures_with(&["shared"]);
        let treatment = TreatmentIdentity {
            build: treatment_build(),
            driver,
            profile,
        };
        let mut row = observation("shared", 1);
        row.driver = treatment.driver;
        row.profile = treatment.profile.clone();
        score(fixtures.path(), vec![row], arm_with(treatment), 1).unwrap()
    }

    #[test]
    fn classifies_strict_success_and_false_green() {
        assert_eq!(classify(0, "", true, true, true), "pass");
        // Tests failing is the headline whether or not bytes matched.
        assert_eq!(classify(0, "", true, false, true), "semantic_false_green");
        assert_eq!(classify(0, "", true, false, false), "semantic_false_green");
        // Correct behaviour, non-canonical bytes: a pass, not a mismatch.
        assert_eq!(
            classify(0, "", true, true, false),
            "pass_reference_mismatch"
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
    fn schema_version_is_six() {
        assert_eq!(REPORT_SCHEMA_VERSION, 6);
    }

    /// The E1 headline was an artefact: naive repaired E0308 correctly but
    /// dropped the trailing newline, so raw-byte matching failed. Semantic
    /// success must not depend on canonicality.
    #[test]
    fn semantic_pass_ignores_reference_match() {
        let fields = observation_fields(
            0, true,  /* compile */
            true,  /* tests */
            false, /* reference */
            true,  /* safety */
        );
        assert!(
            fields.semantic_pass,
            "byte mismatch must not fail semantics"
        );
        assert!(!fields.oracle_pass, "oracle still requires canonicality");
    }

    #[test]
    fn semantic_pass_requires_process_compile_tests_and_safety() {
        assert!(observation_fields(0, true, true, true, true).semantic_pass);
        assert!(!observation_fields(1, true, true, true, true).semantic_pass);
        assert!(!observation_fields(0, false, true, true, true).semantic_pass);
        assert!(!observation_fields(0, true, false, true, true).semantic_pass);
        assert!(
            !observation_fields(0, true, true, true, false).semantic_pass,
            "a safety or diff-policy violation is never a semantic pass"
        );
    }

    #[test]
    fn score_rejects_semantic_derivation_inconsistency() {
        let fixtures = fixtures_with(&["shared"]);
        let mut row = observation("shared", 1);
        row.semantic_pass = false; // contradicts its own evidence
        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
        assert!(error.contains("semantic derivation"), "got {error}");
    }

    #[test]
    fn score_rejects_semantic_pass_without_safety() {
        let fixtures = fixtures_with(&["shared"]);
        let mut row = observation("shared", 1);
        row.safety_clean = false;
        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
        assert!(error.contains("semantic derivation"), "got {error}");
    }

    #[test]
    fn summary_reports_semantic_rate_and_counts_unknown_usage() {
        let fixtures = fixtures_with(&["shared"]);
        let mut known = observation("shared", 1);
        known.tokens_in = Some(100);
        known.tokens_out = Some(50);
        let mut unknown = observation("shared", 2);
        unknown.tokens_in = None;
        unknown.tokens_out = None;
        let report = score(fixtures.path(), vec![known, unknown], arm_identity(), 2).unwrap();
        let summary = &report.overall;
        assert_eq!(summary.semantic.passes, 2);
        assert_eq!(summary.semantic.attempts, 2);
        // Unknown usage is counted, never summed as zero.
        assert_eq!(summary.tokens_in_total, 100);
        assert_eq!(summary.tokens_in_unknown, 1);
        assert_eq!(summary.tokens_out_unknown, 1);
    }

    /// An arm that fixes behaviour but not byte-canonicality must register as
    /// uplift; under the v4 oracle metric it registered as no change.
    #[test]
    fn compare_assesses_on_semantic_delta_not_reference() {
        let fixtures = fixtures_with(&["shared"]);
        let mut base_row = observation("shared", 1);
        base_row.tests_pass = false;
        base_row.cargo_test_exit = Some(101);
        base_row.reference_match = false;
        base_row.oracle_pass = false;
        base_row.semantic_pass = false;
        base_row.failure_class = "semantic_false_green".to_owned();
        let baseline = score(fixtures.path(), vec![base_row], arm_identity(), 1).unwrap();

        let mut arm_row = observation("shared", 1);
        arm_row.reference_match = false; // still not byte-canonical
        arm_row.oracle_pass = false;
        arm_row.failure_class = "pass_reference_mismatch".to_owned();
        let arm = score(fixtures.path(), vec![arm_row], arm_identity(), 1).unwrap();

        let matrix = compare(vec![
            ("naive".to_owned(), baseline),
            ("alloy".to_owned(), arm),
        ])
        .unwrap();
        let comparison = &matrix.comparisons[0];
        assert_eq!(comparison.semantic.delta, Some(1.0));
        assert_eq!(comparison.oracle.delta, Some(0.0));
        assert!(
            comparison.assessment.basis.contains("semantic"),
            "assessment must be based on semantics, got {}",
            comparison.assessment.basis
        );
    }

    /// Pooling fixture x repetition as i.i.d. understates uncertainty: ten
    /// repetitions of one fixture are not ten independent observations. The
    /// gate is stated on a fixture-clustered bound, so it must be computed
    /// that way — paired per fixture, since every arm sees the same corpus.
    #[test]
    fn clustered_delta_pairs_by_fixture_and_is_wider_than_pooled() {
        // Two fixtures, one arm strictly better on one of them.
        let baseline = vec![(0.5_f64, 10_u32), (0.5, 10)];
        let arm = vec![(1.0_f64, 10_u32), (0.5, 10)];
        let clustered = clustered_delta(&baseline, &arm);
        assert_eq!(clustered.fixtures, 2);
        assert_eq!(clustered.mean_delta, Some(0.25));
        // Per-fixture deltas are 0.5 and 0.0, so the interval must straddle
        // zero at k=2 — a pooled interval would wrongly exclude it.
        let lower = clustered.lower95.unwrap();
        assert!(lower < 0.0, "k=2 must not certify uplift, got {lower}");
    }

    #[test]
    fn clustered_delta_is_none_without_enough_clusters() {
        let single = clustered_delta(&[(0.5, 10)], &[(1.0, 10)]);
        assert_eq!(single.fixtures, 1);
        assert_eq!(single.lower95, None, "one fixture cannot bound variance");
    }

    #[test]
    fn clustered_delta_certifies_a_consistent_effect() {
        // Six fixtures, each improving by 0.3: a real, consistent effect.
        let baseline: Vec<(f64, u32)> = (0..6).map(|_| (0.3, 10)).collect();
        let arm: Vec<(f64, u32)> = (0..6).map(|_| (0.6, 10)).collect();
        let clustered = clustered_delta(&baseline, &arm);
        assert_eq!(clustered.mean_delta, Some(0.3));
        assert!(
            clustered.lower95.unwrap() > 0.0,
            "a consistent effect across six fixtures must clear zero"
        );
    }

    #[test]
    fn family_is_the_leading_error_code_of_the_fixture_id() {
        assert_eq!(fixture_family("e0502_preserve_old_01"), "e0502");
        assert_eq!(fixture_family("e0308_holdout_01"), "e0308");
        // Anything that is not a leading error code is its own family rather
        // than being silently lumped together.
        assert_eq!(fixture_family("weird-fixture"), "weird-fixture");
    }

    #[test]
    fn classifies_semantic_false_green() {
        // Exit 0 and compile clean, but the hidden tests failed: the process
        // claimed success on code that does not behave correctly.
        assert_eq!(
            classify(0, "", true, false, false),
            "semantic_false_green",
            "compile-clean-but-tests-failed is the false-green class"
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
        let report = score(fixtures.path(), rows, arm_identity(), 2).unwrap();
        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(report.overall.oracle.passes, 4);
        assert_eq!(report.overall.oracle.attempts, 4);
        assert_eq!(report.overall.compile_clean_reference_mismatch.passes, 0);
        assert_eq!(report.fixtures.len(), 2);
        assert_eq!(report.overall.model_calls_total, 4);
        assert_eq!(report.overall.tokens_in_total, 400);
        assert_eq!(report.overall.tokens_out_total, 200);
    }

    #[test]
    fn score_totals_skip_unreported_usage_without_inventing_zero() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.tokens_in = None;
        row.tokens_out = None;
        let report = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap();
        assert_eq!(report.overall.model_calls_total, 1);
        assert_eq!(report.overall.tokens_in_total, 0);
        assert_eq!(report.observations[0].tokens_in, None);
    }

    #[test]
    fn score_rejects_naive_rows_without_exactly_one_model_call() {
        let fixtures = fixtures_with(&["a"]);
        let arm = arm_with(TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Naive,
            profile: None,
        });
        for model_calls in [0, 2] {
            let mut row = observation("a", 1);
            row.driver = LiveHoldoutDriver::Naive;
            row.profile = None;
            row.model_calls = model_calls;
            let error = score(fixtures.path(), vec![row], arm.clone(), 1).unwrap_err();
            assert!(
                error.contains(&format!("naive driver recorded {model_calls} model calls")),
                "{error}"
            );
        }
    }

    #[test]
    fn alloy_telemetry_counts_calls_and_sums_reported_usage() {
        let events = concat!(
            r#"{"type":"model_call","payload":{"input_tokens":100,"output_tokens":20}}"#,
            "\n",
            r#"{"type":"model_call","payload":{"output_tokens":5,"input_tokens":null}}"#,
            "\n",
            r#"{"type":"run_completed","payload":{"dag_state":"succeeded"}}"#,
            "\n",
        );
        assert_eq!(
            alloy_telemetry(events).unwrap(),
            RunTelemetry {
                model_calls: 2,
                tokens_in: Some(100),
                tokens_out: Some(25),
            }
        );
    }

    #[test]
    fn alloy_telemetry_keeps_absent_usage_unknown_and_rejects_malformed_events() {
        let without_usage = "{\"type\":\"model_call\",\"payload\":{}}\n";
        assert_eq!(
            alloy_telemetry(without_usage).unwrap(),
            RunTelemetry {
                model_calls: 1,
                tokens_in: None,
                tokens_out: None,
            }
        );
        assert_eq!(alloy_telemetry("").unwrap(), RunTelemetry::UNRECORDED);
        assert!(alloy_telemetry("not json\n").is_err());
        assert!(
            alloy_telemetry("{\"type\":\"model_call\",\"payload\":{\"input_tokens\":-3}}\n")
                .is_err()
        );
    }

    #[test]
    fn alloy_telemetry_rejects_an_export_at_the_page_limit() {
        let event = "{\"type\":\"model_call\",\"payload\":{\"input_tokens\":1}}\n";
        let at_cap = event.repeat(EVENT_EXPORT_PAGE_LIMIT);
        let error = alloy_telemetry(&at_cap).unwrap_err();
        assert!(
            error.contains("page limit") && error.contains("truncated"),
            "{error}"
        );

        let below_cap = event.repeat(EVENT_EXPORT_PAGE_LIMIT - 1);
        assert_eq!(
            alloy_telemetry(&below_cap).unwrap().model_calls,
            (EVENT_EXPORT_PAGE_LIMIT - 1) as u32
        );
    }

    #[test]
    fn naive_telemetry_requires_exactly_one_model_call() {
        let one = r#"{"model_calls":1,"tokens_in":123,"tokens_out":45,"finish_reason":"stop"}"#;
        assert_eq!(
            naive_telemetry(one).unwrap(),
            RunTelemetry {
                model_calls: 1,
                tokens_in: Some(123),
                tokens_out: Some(45),
            }
        );
        let unknown_usage = r#"{"model_calls":1,"tokens_in":null,"tokens_out":null}"#;
        assert_eq!(naive_telemetry(unknown_usage).unwrap().tokens_in, None);
        assert!(naive_telemetry(r#"{"model_calls":2}"#).is_err());
        assert!(naive_telemetry("{}").is_err());
    }

    #[test]
    fn telemetry_rejects_missing_naive_evidence_but_keeps_alloy_unrecorded() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("naive-result.json");
        let error = telemetry(LiveHoldoutDriver::Naive, &missing).unwrap_err();
        assert!(
            error.contains("naive telemetry missing") && error.contains("naive-result.json"),
            "{error}"
        );
        assert_eq!(
            telemetry(LiveHoldoutDriver::Alloy, &missing).unwrap(),
            RunTelemetry::UNRECORDED
        );
    }

    #[test]
    fn score_rejects_endpoint_identity_mismatch() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.temperature = 0.7;
        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
        assert!(error.contains("endpoint identity mismatch"), "{error}");
    }

    /// Rows from two different treatments never belong in one arm, whether
    /// they differ by build, by driver, or by profile.
    #[test]
    fn score_rejects_treatment_mixing() {
        let fixtures = fixtures_with(&["a"]);
        for mutate in [
            Box::new(|row: &mut StrictObservation| {
                row.build.source_revision = "other".to_owned();
            }) as Box<dyn Fn(&mut StrictObservation)>,
            Box::new(|row: &mut StrictObservation| {
                row.build.binary_bundle_sha256 = "0".repeat(64);
            }),
            Box::new(|row: &mut StrictObservation| {
                row.profile = Some("autonomous".to_owned());
            }),
        ] {
            let mut row = observation("a", 1);
            mutate(&mut row);
            let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
            assert!(error.contains("treatment identity mismatch"), "{error}");
        }
    }

    #[test]
    fn score_rejects_naive_treatment_with_profile() {
        let fixtures = fixtures_with(&["a"]);
        let arm = arm_with(TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Naive,
            profile: Some("default".to_owned()),
        });
        let error = score(fixtures.path(), vec![], arm, 1).unwrap_err();
        assert!(
            error.contains("naive") && error.contains("profile"),
            "{error}"
        );
    }

    #[test]
    fn score_rejects_alloy_treatment_without_recognized_profile() {
        let fixtures = fixtures_with(&["a"]);
        let arm = arm_with(TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Alloy,
            profile: None,
        });
        let error = score(fixtures.path(), vec![], arm, 1).unwrap_err();
        assert!(
            error.contains("alloy") && error.contains("default") && error.contains("autonomous"),
            "{error}"
        );
    }

    #[test]
    fn score_rejects_process_compile_inconsistency() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.compile_clean = true;
        row.cargo_check_exit = Some(101);
        row.oracle_pass = false;
        row.failure_class = "process_claimed_success_but_compile_failed".to_owned();
        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
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
        let error = score(fixtures.path(), vec![test_row], arm_identity(), 1).unwrap_err();
        assert!(
            error.contains("process/compile/test evidence inconsistency"),
            "{error}"
        );

        let mut path_row = observation("a", 1);
        path_row.evidence_relpath = "../escape".to_owned();
        let error = score(fixtures.path(), vec![path_row], arm_identity(), 1).unwrap_err();
        assert!(error.contains("evidence path inconsistency"), "{error}");
    }

    #[test]
    fn score_rejects_oracle_pass_when_hidden_tests_fail() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.tests_pass = false;
        row.cargo_test_exit = Some(101);
        row.semantic_pass = false;
        row.failure_class = "semantic_false_green".to_owned();

        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
        assert!(error.contains("oracle derivation inconsistency"), "{error}");
    }

    #[test]
    fn score_rejects_spoofed_failure_class_on_process_success() {
        let fixtures = fixtures_with(&["a"]);
        let mut row = observation("a", 1);
        row.reference_match = false;
        row.oracle_pass = false;
        row.failure_class = "timeout".to_owned();
        let error = score(fixtures.path(), vec![row], arm_identity(), 1).unwrap_err();
        assert!(
            error.contains("failure-class consistency violation")
                && error.contains("pass_reference_mismatch"),
            "{error}"
        );
    }

    #[test]
    fn score_rejects_duplicate_and_missing_repetitions() {
        let fixtures = fixtures_with(&["a"]);
        let duplicate = score(
            fixtures.path(),
            vec![observation("a", 1), observation("a", 1)],
            arm_identity(),
            2,
        )
        .unwrap_err();
        assert!(duplicate.contains("repetitions"), "{duplicate}");

        let missing = score(
            fixtures.path(),
            vec![observation("a", 1)],
            arm_identity(),
            2,
        )
        .unwrap_err();
        assert!(missing.contains("repetitions"), "{missing}");
    }

    /// Scores one arm over `ids` where only `passing` fixtures reach a
    /// semantic pass, so arms can be made to differ by construction.
    fn arm_report(
        fixtures: &Path,
        ids: &[&str],
        treatment: TreatmentIdentity,
        passing: &[&str],
    ) -> StrictReport {
        let rows = ids
            .iter()
            .map(|id| {
                let mut row = observation(id, 1);
                row.driver = treatment.driver;
                row.profile = treatment.profile.clone();
                if !passing.contains(id) {
                    row.tests_pass = false;
                    row.cargo_test_exit = Some(101);
                    row.semantic_pass = false;
                    row.reference_match = false;
                    row.oracle_pass = false;
                    row.failure_class = "semantic_false_green".to_owned();
                }
                row
            })
            .collect();
        score(fixtures, rows, arm_with(treatment), 1).unwrap()
    }

    fn profile_report(
        fixtures: &Path,
        ids: &[&str],
        profile: &str,
        passing: &[&str],
    ) -> StrictReport {
        let treatment = TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Alloy,
            profile: Some(profile.to_owned()),
        };
        arm_report(fixtures, ids, treatment, passing)
    }

    fn naive_report(fixtures: &Path, ids: &[&str], passing: &[&str]) -> StrictReport {
        let treatment = TreatmentIdentity {
            build: treatment_build(),
            driver: LiveHoldoutDriver::Naive,
            profile: None,
        };
        arm_report(fixtures, ids, treatment, passing)
    }

    /// The approved E2 rule forbids claiming autonomous uplift without a
    /// direct default-versus-autonomous comparison. With naive as the
    /// baseline that contrast is never one of the baseline-vs-arm rows, so it
    /// must be emitted separately — and located by profile identity, not by
    /// arm name or array position.
    #[test]
    fn compare_emits_a_direct_default_versus_autonomous_contrast() {
        let ids = ["e0308_a", "e0502_b"];
        let dir = fixtures_with(&ids);
        let matrix = compare(vec![
            (
                "baseline-arm".to_owned(),
                naive_report(dir.path(), &ids, &[]),
            ),
            (
                "foo".to_owned(),
                profile_report(dir.path(), &ids, "default", &["e0308_a"]),
            ),
            (
                "bar".to_owned(),
                profile_report(dir.path(), &ids, "autonomous", &ids),
            ),
        ])
        .unwrap();

        // The primary metric keeps its own baseline and arm rows.
        assert_eq!(matrix.baseline, "baseline-arm");
        assert_eq!(matrix.comparisons.len(), 2);
        assert!(matrix
            .comparisons
            .iter()
            .all(|item| item.baseline == "baseline-arm"));

        let contrast = matrix
            .autonomous_vs_default
            .expect("one default arm and one autonomous arm are present");
        // Named nothing like their profiles: identity comes from the endpoint.
        assert_eq!(contrast.comparison.baseline, "foo");
        assert_eq!(contrast.comparison.arm, "bar");
        assert_eq!(contrast.comparison.semantic.delta, Some(0.5));
        // Same rigour as the primary comparison.
        assert_eq!(contrast.comparison.semantic_clustered.fixtures, 2);
        assert!(contrast.comparison.semantic_clustered.lower95.is_some());
        assert_eq!(contrast.comparison.families.len(), 2);
        assert!(contrast.comparison.semantic_family_macro.is_some());
    }

    #[test]
    fn compare_omits_the_contrast_without_exactly_one_default_and_autonomous_arm() {
        let ids = ["e0308_a"];
        let dir = fixtures_with(&ids);

        let two_arm = compare(vec![
            ("naive".to_owned(), naive_report(dir.path(), &ids, &[])),
            (
                "alloy-default".to_owned(),
                profile_report(dir.path(), &ids, "default", &ids),
            ),
        ])
        .unwrap();
        assert!(
            two_arm.autonomous_vs_default.is_none(),
            "no autonomous arm ran, so the comparison must be absent, not zero"
        );
        assert!(!two_arm.autonomous_uplift_is_claimable());

        let ambiguous = compare(vec![
            (
                "alloy-default".to_owned(),
                profile_report(dir.path(), &ids, "default", &[]),
            ),
            (
                "auto-a".to_owned(),
                profile_report(dir.path(), &ids, "autonomous", &ids),
            ),
            (
                "auto-b".to_owned(),
                profile_report(dir.path(), &ids, "autonomous", &[]),
            ),
        ])
        .unwrap();
        assert!(
            ambiguous.autonomous_vs_default.is_none(),
            "two autonomous arms must not be resolved by position"
        );
    }

    #[test]
    fn autonomous_gate_needs_a_positive_clustered_lower_bound() {
        let cleared = ClusteredDelta {
            fixtures: 6,
            mean_delta: Some(0.3),
            lower95: Some(0.05),
            upper95: Some(0.55),
        };
        assert!(clears_autonomous_gate(&cleared));
        assert!(!clears_autonomous_gate(&ClusteredDelta {
            lower95: Some(-0.01),
            ..cleared.clone()
        }));
        assert!(!clears_autonomous_gate(&ClusteredDelta {
            lower95: Some(0.0),
            ..cleared.clone()
        }));
        // "Cannot bound" is never a pass.
        assert!(!clears_autonomous_gate(&ClusteredDelta {
            fixtures: 1,
            mean_delta: Some(1.0),
            lower95: None,
            upper95: None,
        }));
    }

    #[test]
    fn contrast_answers_the_gate_in_both_directions() {
        let ids = [
            "e0308_a", "e0308_b", "e0502_c", "e0502_d", "e0716_e", "e0716_f",
        ];
        let dir = fixtures_with(&ids);

        // Autonomous wins on five of six fixtures: consistent enough to bound.
        let cleared = compare(vec![
            (
                "alloy-default".to_owned(),
                profile_report(dir.path(), &ids, "default", &[]),
            ),
            (
                "alloy-autonomous".to_owned(),
                profile_report(dir.path(), &ids, "autonomous", &ids[..5]),
            ),
        ])
        .unwrap();
        let contrast = cleared
            .autonomous_vs_default
            .as_ref()
            .expect("both profile arms ran");
        assert!(
            contrast.clears_gate,
            "{:?}",
            contrast.comparison.semantic_clustered
        );
        assert!(
            contrast.gate_basis.contains("above_zero"),
            "{}",
            contrast.gate_basis
        );
        assert!(cleared.autonomous_uplift_is_claimable());

        // A positive mean carried by one fixture must not clear the gate.
        let blocked = compare(vec![
            (
                "alloy-default".to_owned(),
                profile_report(dir.path(), &ids, "default", &[]),
            ),
            (
                "alloy-autonomous".to_owned(),
                profile_report(dir.path(), &ids, "autonomous", &ids[..1]),
            ),
        ])
        .unwrap();
        let contrast = blocked
            .autonomous_vs_default
            .as_ref()
            .expect("both profile arms ran");
        assert!(
            contrast
                .comparison
                .semantic_clustered
                .mean_delta
                .is_some_and(|mean| mean > 0.0),
            "the mean effect is positive"
        );
        assert!(
            !contrast.clears_gate,
            "one fixture cannot certify an arm-level effect"
        );
        assert!(!blocked.autonomous_uplift_is_claimable());
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

    /// A build difference between arms is a treatment difference, which is
    /// the measurement — not grounds for refusing to compare.
    #[test]
    fn compare_allows_a_differing_treatment_build_between_drivers() {
        let baseline = report_for_driver(LiveHoldoutDriver::Naive, None);
        let mut candidate = report_for_driver(LiveHoldoutDriver::Alloy, Some("default".to_owned()));
        candidate.treatment.build.source_revision = "e".repeat(40);

        let matrix = compare(vec![
            ("naive".to_owned(), baseline),
            ("alloy-default".to_owned(), candidate),
        ])
        .unwrap();

        assert_eq!(matrix.comparisons.len(), 1);
    }

    #[test]
    fn compare_allows_differing_driver_and_profile_under_one_protocol() {
        let baseline = report_for_driver(LiveHoldoutDriver::Naive, None);
        let candidate = report_for_driver(LiveHoldoutDriver::Alloy, Some("autonomous".to_owned()));
        let matrix = compare(vec![
            ("naive".to_owned(), baseline),
            ("alloy-autonomous".to_owned(), candidate),
        ])
        .unwrap();
        assert_eq!(matrix.comparisons.len(), 1);
    }

    /// A corpus with the files the oracle actually reads, so a digest over it
    /// means something.
    fn corpus_with_oracles(ids: &[&str]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for id in ids {
            let fixture = directory.path().join(id);
            fs::create_dir_all(fixture.join("workspace/src")).unwrap();
            fs::create_dir_all(fixture.join("oracle-tests")).unwrap();
            fs::create_dir_all(fixture.join("recordings")).unwrap();
            fs::write(
                fixture.join("manifest.toml"),
                "naive_target_path = \"src/lib.rs\"\n",
            )
            .unwrap();
            fs::write(
                fixture.join("workspace/src/lib.rs"),
                "pub fn broken() -> i32 { missing }\n",
            )
            .unwrap();
            fs::write(
                fixture.join("workspace/src/lib.rs.post"),
                "pub fn repaired() -> i32 { 42 }\n",
            )
            .unwrap();
            fs::write(
                fixture.join("oracle-tests/semantic.rs"),
                format!("#[test]\nfn t() {{ assert_eq!({id}::repaired(), 42); }}\n"),
            )
            .unwrap();
            fs::write(fixture.join("recordings/pre_repair.json"), "{}\n").unwrap();
        }
        directory
    }

    /// The corpus is protocol identity, so editing it silently must be
    /// impossible: the digest has to move when any oracle input moves.
    #[test]
    fn corpus_digest_covers_ids_and_oracle_inputs() {
        let directory = corpus_with_oracles(&["e0308_a", "e0502_b"]);
        let root = directory.path();
        let original = corpus_digest(root).unwrap();
        assert_eq!(original.len(), 64, "{original}");
        assert_eq!(
            original,
            corpus_digest(root).unwrap(),
            "the digest must be deterministic"
        );

        for edited in [
            "e0308_a/oracle-tests/semantic.rs",
            "e0308_a/workspace/src/lib.rs.post",
            "e0308_a/workspace/src/lib.rs",
            "e0308_a/manifest.toml",
        ] {
            let path = root.join(edited);
            let before = fs::read(&path).unwrap();
            fs::write(
                &path,
                if edited.ends_with("manifest.toml") {
                    "naive_target_path = \"src/other.rs\"\n".to_owned()
                } else {
                    String::from_utf8(before.clone()).unwrap() + "// edited\n"
                },
            )
            .unwrap();
            assert_ne!(
                original,
                corpus_digest(root).unwrap(),
                "{edited} is an oracle input; editing it must change the digest"
            );
            fs::write(&path, &before).unwrap();
        }
        assert_eq!(original, corpus_digest(root).unwrap(), "restored corpus");

        // Adding or removing a fixture is a different corpus too.
        let added = corpus_with_oracles(&["e0308_a", "e0502_b", "e0716_c"]);
        assert_ne!(original, corpus_digest(added.path()).unwrap());
        fs::remove_dir_all(root.join("e0502_b")).unwrap();
        assert_ne!(original, corpus_digest(root).unwrap());
    }

    /// A fixture whose oracle inputs are absent still digests, so the corpus
    /// identity of a partial corpus is defined rather than a hard error.
    #[test]
    fn corpus_digest_handles_absent_oracle_inputs() {
        let bare = fixtures_with(&["a"]);
        assert_eq!(corpus_digest(bare.path()).unwrap().len(), 64);
        assert!(corpus_digest(bare.path().join("missing").as_path()).is_err());
    }

    /// The whole point of the split: what scored an observation set (corpus,
    /// evaluator, schema) is recorded apart from what produced it (binaries,
    /// driver, profile).
    #[test]
    fn score_records_protocol_and_treatment_identity_separately() {
        let directory = corpus_with_oracles(&["e0308_a"]);
        let report = score(
            directory.path(),
            vec![observation("e0308_a", 1)],
            arm_identity(),
            1,
        )
        .unwrap();

        assert_eq!(report.protocol.corpus, CORPUS);
        assert_eq!(
            report.protocol.corpus_digest,
            corpus_digest(directory.path()).unwrap()
        );
        assert_eq!(report.protocol.evaluator_revision, "c".repeat(40));
        assert_eq!(report.protocol.schema_version, REPORT_SCHEMA_VERSION);
        // Nothing about the product build may leak into protocol identity.
        assert!(!report.protocol.evaluator_revision.contains(&"b".repeat(64)));

        assert_eq!(report.treatment.build, treatment_build());
        assert_eq!(report.treatment.driver, LiveHoldoutDriver::Alloy);
        assert_eq!(report.treatment.profile.as_deref(), Some("default"));
    }

    #[test]
    fn score_rejects_an_empty_evaluator_revision() {
        let directory = fixtures_with(&["a"]);
        let arm = ArmIdentity {
            evaluator_revision: String::new(),
            ..arm_identity()
        };
        let error = score(directory.path(), vec![], arm, 1).unwrap_err();
        assert!(error.contains("evaluator revision"), "{error}");
    }

    /// Pre- and post-intervention runs differ by exactly the treatment. Once
    /// both are scored by one evaluator over one corpus they must compare —
    /// under the old conflated identity this was refused outright.
    #[test]
    fn compare_allows_a_changed_treatment_build() {
        let directory = corpus_with_oracles(&["e0308_a", "e0502_b"]);
        let ids = ["e0308_a", "e0502_b"];
        let before = rescored_arm(directory.path(), &ids, treatment_build(), "default");
        let after = rescored_arm(
            directory.path(),
            &ids,
            TreatmentBuild {
                source_revision: "e".repeat(40),
                binary_bundle_sha256: "f".repeat(64),
            },
            "autonomous",
        );

        let matrix = compare(vec![
            ("before".to_owned(), before),
            ("after".to_owned(), after),
        ])
        .unwrap();

        assert_eq!(matrix.comparisons.len(), 1);
        assert_eq!(
            matrix.arms["before"].treatment.build.binary_bundle_sha256,
            "b".repeat(64)
        );
        assert_eq!(
            matrix.arms["after"].treatment.build.binary_bundle_sha256,
            "f".repeat(64)
        );
        assert_eq!(matrix.protocol, matrix.arms["before"].protocol);
    }

    /// A protocol change under the arms is not a treatment effect, so the
    /// comparison must fail closed and name what moved.
    #[test]
    fn compare_refuses_arms_scored_under_different_protocols() {
        let directory = corpus_with_oracles(&["e0308_a"]);
        let ids = ["e0308_a"];
        let baseline = rescored_arm(directory.path(), &ids, treatment_build(), "default");

        for (label, mutate) in [
            (
                "corpus_digest",
                Box::new(|report: &mut StrictReport| {
                    report.protocol.corpus_digest = "0".repeat(64);
                }) as Box<dyn Fn(&mut StrictReport)>,
            ),
            (
                "evaluator_revision",
                Box::new(|report: &mut StrictReport| {
                    report.protocol.evaluator_revision = "d".repeat(40);
                }),
            ),
            (
                "corpus",
                Box::new(|report: &mut StrictReport| {
                    report.protocol.corpus = "some-other-corpus".to_owned();
                }),
            ),
        ] {
            let mut drifted = rescored_arm(directory.path(), &ids, treatment_build(), "autonomous");
            mutate(&mut drifted);
            let error = compare(vec![
                ("baseline".to_owned(), baseline.clone()),
                ("drifted".to_owned(), drifted),
            ])
            .unwrap_err();
            assert!(
                error.contains("protocol") && error.contains(label),
                "{label}: {error}"
            );
        }
    }

    /// A report whose declared schema disagrees with its own protocol
    /// identity is corrupt evidence, not a comparable arm.
    #[test]
    fn compare_rejects_a_report_that_disagrees_with_its_own_protocol_schema() {
        let mut inconsistent = report_for(true);
        inconsistent.protocol.schema_version = REPORT_SCHEMA_VERSION - 1;
        let error = compare(vec![
            ("inconsistent".to_owned(), inconsistent),
            ("current".to_owned(), report_for(false)),
        ])
        .unwrap_err();
        assert!(error.contains("protocol identity says"), "{error}");
    }

    /// Evidence written under an older contract is refused as legacy, never
    /// coerced into the current one.
    #[test]
    fn legacy_schema_versions_are_refused_with_an_explicit_message() {
        assert!(check_report_version(REPORT_SCHEMA_VERSION, "report.json").is_ok());
        for legacy in [4, 5] {
            let error = check_report_version(legacy, "run1/naive.report.json").unwrap_err();
            assert!(error.contains("legacy"), "{error}");
            assert!(error.contains(&legacy.to_string()), "{error}");
            assert!(
                error.contains(&REPORT_SCHEMA_VERSION.to_string()),
                "{error}"
            );
            assert!(error.contains("run1/naive.report.json"), "{error}");
        }

        let mut stale = report_for(true);
        stale.schema_version = 5;
        let error = compare(vec![
            ("stale".to_owned(), stale),
            ("current".to_owned(), report_for(false)),
        ])
        .unwrap_err();
        assert!(error.contains("legacy"), "{error}");
    }

    /// The v5 observation line `eval/live-holdout/run.sh` writes today must
    /// keep loading, or the run in flight cannot be re-scored by this build.
    #[test]
    fn v5_observation_rows_load_under_the_current_evaluator() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observations.jsonl");
        let line = format!(
            "{{\"fixture_id\":\"e0308_a\",\"repetition\":1,\"exit_code\":0,\
             \"process_pass\":true,\"compile_clean\":true,\"tests_pass\":true,\
             \"safety_clean\":true,\"semantic_pass\":true,\"reference_match\":true,\
             \"oracle_pass\":true,\"failure_class\":\"pass\",\"cargo_check_exit\":0,\
             \"cargo_test_exit\":0,\"repair_generations\":0,\"wall_ms\":10,\
             \"evidence_relpath\":\"e0308_a/rep-1\",\"model\":\"stub-model\",\
             \"temperature\":0.6,\"driver\":\"alloy\",\"profile\":\"default\",\
             \"base_url\":\"http://127.0.0.1:8089/v1/\",\
             \"harness\":{{\"source_revision\":\"{}\",\"binary_bundle_sha256\":\"{}\"}},\
             \"corpus\":\"rfc0016-holdout-live\",\"model_calls\":1,\"tokens_in\":100,\
             \"tokens_out\":50}}\n",
            "a".repeat(40),
            "b".repeat(64)
        );
        fs::write(&path, line).unwrap();

        let rows = load_observations(&path).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].build, treatment_build());
        // And they score: the same raw evidence, re-scored by this evaluator.
        let corpus = corpus_with_oracles(&["e0308_a"]);
        let report = score(corpus.path(), rows, arm_identity(), 1).unwrap();
        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(report.overall.semantic.passes, 1);
    }

    #[test]
    fn compare_rejects_incompatible_inputs() {
        let fixtures_a = fixtures_with(&["a"]);
        let fixtures_b = fixtures_with(&["b"]);
        let left = score(
            fixtures_a.path(),
            vec![observation("a", 1)],
            arm_identity(),
            1,
        )
        .unwrap();
        let right = score(
            fixtures_b.path(),
            vec![observation("b", 1)],
            arm_identity(),
            1,
        )
        .unwrap();
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
