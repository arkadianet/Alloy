//! Structured results for the live-repair operator benchmark.
//!
//! The types here deliberately reuse the offline report vocabulary
//! ([`FixtureId`], [`FixtureStatus`], [`MetricField`], [`UnmeasuredReason`])
//! while remaining a *distinct* report type. A [`LiveRepairReport`] can never
//! be handed to [`crate::evaluate_gate`], never carries `offline = true`, and
//! always renders `holdout_gate=not_applicable`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::live_repair::manifest::{LiveRepairCorpus, LiveRepairExpectedOutcome};
use crate::live_repair::score::{wilson_interval, WilsonInterval, WILSON_Z_95};
use crate::manifest::FixtureId;
use crate::metrics::{MetricField, UnmeasuredReason};
use crate::report::FixtureStatus;

/// Report schema version for the live-repair benchmark.
pub const LIVE_REPAIR_REPORT_VERSION: u32 = 1;

/// Exit code produced by `timeout(1)` when the per-run budget is exceeded.
const TIMEOUT_EXIT_CODE: i32 = 124;

/// Whether RFC-0016 holdout gates apply to this report.
///
/// The only variant is [`Self::NotApplicable`]: live-endpoint results are
/// operator telemetry and MUST NOT gate a milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveRepairGateApplicability {
    /// Live results never feed a holdout gate.
    NotApplicable,
}

/// Endpoint configuration a live run was executed against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRepairEndpoint {
    /// Wire model id served by the endpoint.
    pub model: String,
    /// Sampling temperature requested from the endpoint.
    pub temperature: f64,
    /// OpenAI-compatible base URL.
    pub base_url: String,
}

/// One observed execution of one fixture against the live endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRepairObservation {
    /// Fixture that was executed.
    pub fixture_id: FixtureId,
    /// 1-based repetition index within the run.
    pub repetition: u32,
    /// Process exit code of the real `alloy` binary.
    pub exit_code: i32,
    /// Retry lines counted in the captured run log.
    pub retries: u32,
    /// Observed wall time in milliseconds.
    pub wall_ms: u64,
}

impl LiveRepairObservation {
    /// Classify this observation using the offline `Pass | Fail | Error`
    /// vocabulary.
    ///
    /// Exit `0` is a pass. A `timeout(1)` kill (`124`) or a shell
    /// could-not-execute code (`126`, `127`) is harness infrastructure
    /// failure, so it is [`FixtureStatus::Error`] and is excluded from the
    /// pass-rate denominator exactly like an offline `Error` fixture. Every
    /// other non-zero code is a genuine [`FixtureStatus::Fail`].
    #[must_use]
    pub fn status(&self) -> FixtureStatus {
        match self.exit_code {
            0 => FixtureStatus::Pass,
            TIMEOUT_EXIT_CODE | 126 | 127 => FixtureStatus::Error,
            _ => FixtureStatus::Fail,
        }
    }
}

/// Pass-rate population for a fixture or for the whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRepairPassRate {
    /// Non-error attempts, i.e. the pass-rate denominator.
    pub attempts: u32,
    /// Passing attempts.
    pub passes: u32,
    /// Failing attempts.
    pub failures: u32,
    /// Attempts excluded from the denominator as infrastructure errors.
    pub errors: u32,
    /// Pass rate over non-error attempts.
    pub rate: MetricField<f64>,
    /// Wilson 95% interval for `rate`.
    pub wilson95: MetricField<WilsonInterval>,
}

impl LiveRepairPassRate {
    fn from_observations(observations: &[&LiveRepairObservation]) -> Self {
        let mut passes = 0_u32;
        let mut failures = 0_u32;
        let mut errors = 0_u32;
        for observation in observations {
            match observation.status() {
                FixtureStatus::Pass => passes += 1,
                FixtureStatus::Fail => failures += 1,
                FixtureStatus::Error => errors += 1,
            }
        }
        let attempts = passes + failures;
        if attempts == 0 {
            return Self {
                attempts,
                passes,
                failures,
                errors,
                rate: MetricField::Unmeasured {
                    reason: UnmeasuredReason::EmptySample,
                },
                wilson95: MetricField::Unmeasured {
                    reason: UnmeasuredReason::EmptySample,
                },
            };
        }
        Self {
            attempts,
            passes,
            failures,
            errors,
            rate: MetricField::Measured(f64::from(passes) / f64::from(attempts)),
            wilson95: MetricField::Measured(wilson_interval(passes, attempts, WILSON_Z_95)),
        }
    }
}

/// Aggregate results for one fixture across its repetitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRepairFixtureReport {
    /// Fixture id.
    pub fixture_id: FixtureId,
    /// Error-class tags from the fixture manifest.
    pub tags: Vec<String>,
    /// Expected outcome from the fixture manifest.
    pub expected_outcome: LiveRepairExpectedOutcome,
    /// Pass-rate population for this fixture.
    pub pass_rate: LiveRepairPassRate,
    /// Total retry lines observed across repetitions.
    pub retries_total: u32,
    /// Passing attempts that needed at least one retry.
    pub passes_via_retry: u32,
    /// Mean wall time over non-error attempts.
    pub mean_wall_ms: MetricField<f64>,
}

/// Top-level structured result of one live-repair benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRepairReport {
    /// Report schema version; always [`LIVE_REPAIR_REPORT_VERSION`].
    pub schema_version: u32,
    /// Canonical lowercase hyphenated UUID v4.
    pub run_id: String,
    /// Always `false`: this run talked to a live endpoint.
    pub offline: bool,
    /// Always [`LiveRepairGateApplicability::NotApplicable`].
    pub holdout_gate: LiveRepairGateApplicability,
    /// Endpoint the run executed against.
    pub endpoint: LiveRepairEndpoint,
    /// Per-fixture aggregates in fixture-id order.
    pub fixtures: Vec<LiveRepairFixtureReport>,
    /// Whole-run pass-rate population.
    pub overall: LiveRepairPassRate,
    /// Raw observations in input order.
    pub observations: Vec<LiveRepairObservation>,
}

impl LiveRepairReport {
    /// Aggregate raw observations against a loaded corpus.
    ///
    /// Ownership: borrows `corpus` and `endpoint`, consumes `observations`.
    ///
    /// # Errors
    ///
    /// [`EvalError::Manifest`] when an observation names a fixture that is not
    /// in `corpus`; the benchmark must never silently score an unknown id.
    pub fn assemble(
        run_id: impl Into<String>,
        endpoint: LiveRepairEndpoint,
        corpus: &LiveRepairCorpus,
        observations: Vec<LiveRepairObservation>,
    ) -> Result<Self, EvalError> {
        let mut grouped: BTreeMap<&str, Vec<&LiveRepairObservation>> = BTreeMap::new();
        for observation in &observations {
            if corpus.get(&observation.fixture_id).is_none() {
                return Err(EvalError::Manifest(format!(
                    "observation names unknown live-repair fixture: {}",
                    observation.fixture_id
                )));
            }
            grouped
                .entry(observation.fixture_id.as_str())
                .or_default()
                .push(observation);
        }

        let mut fixtures = Vec::with_capacity(corpus.fixtures().len());
        for fixture in corpus.fixtures() {
            let manifest = fixture.manifest();
            let observed = grouped.remove(manifest.id.as_str()).unwrap_or_default();
            fixtures.push(LiveRepairFixtureReport {
                fixture_id: manifest.id.clone(),
                tags: manifest.tags.clone(),
                expected_outcome: manifest.expected_outcome,
                pass_rate: LiveRepairPassRate::from_observations(&observed),
                retries_total: observed
                    .iter()
                    .fold(0_u32, |total, o| total.saturating_add(o.retries)),
                passes_via_retry: observed
                    .iter()
                    .filter(|o| o.status() == FixtureStatus::Pass && o.retries > 0)
                    .count() as u32,
                mean_wall_ms: mean_wall_ms(&observed),
            });
        }

        let all: Vec<&LiveRepairObservation> = observations.iter().collect();
        Ok(Self {
            schema_version: LIVE_REPAIR_REPORT_VERSION,
            run_id: run_id.into(),
            offline: false,
            holdout_gate: LiveRepairGateApplicability::NotApplicable,
            endpoint,
            fixtures,
            overall: LiveRepairPassRate::from_observations(&all),
            observations,
        })
    }

    /// Total retry lines observed across the whole run.
    #[must_use]
    pub fn retries_total(&self) -> u32 {
        self.fixtures.iter().fold(0_u32, |total, fixture| {
            total.saturating_add(fixture.retries_total)
        })
    }

    /// Passing attempts across the run that needed at least one retry.
    #[must_use]
    pub fn passes_via_retry(&self) -> u32 {
        self.fixtures.iter().fold(0_u32, |total, fixture| {
            total.saturating_add(fixture.passes_via_retry)
        })
    }

    /// Render the operator summary block.
    ///
    /// The first line names `alloy-eval-live-repair`, never `alloy-eval`, and
    /// lines two and three state the separation from the offline gates
    /// outright. Returns newline-separated lines with no trailing newline.
    #[must_use]
    pub fn render_summary(&self) -> String {
        [
            format!("alloy-eval-live-repair run_id={}", self.run_id),
            format!("offline={}", self.offline),
            "holdout_gate=not_applicable".to_owned(),
            format!(
                "endpoint model={} temperature={:.6}",
                self.endpoint.model, self.endpoint.temperature
            ),
            format!(
                "overall pass={} fail={} error={}",
                self.overall.passes, self.overall.failures, self.overall.errors
            ),
            format!("pass_rate={}", render_rate(&self.overall.rate)),
            format!("wilson95={}", render_wilson(&self.overall.wilson95)),
            format!(
                "retries_total={} passes_via_retry={}",
                self.retries_total(),
                self.passes_via_retry()
            ),
            "cost=uncalibrated".to_owned(),
            "cost_disclaimer=internal-only".to_owned(),
        ]
        .join("\n")
    }

    /// Render one line per fixture: `<id> <passes>/<attempts> <wilson>`.
    #[must_use]
    pub fn render_fixture_lines(&self) -> String {
        self.fixtures
            .iter()
            .map(|fixture| {
                format!(
                    "fixture {} pass={}/{} error={} retries={} wilson95={} tags={}",
                    fixture.fixture_id,
                    fixture.pass_rate.passes,
                    fixture.pass_rate.attempts,
                    fixture.pass_rate.errors,
                    fixture.retries_total,
                    render_wilson(&fixture.pass_rate.wilson95),
                    fixture.tags.join(",")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl std::fmt::Display for LiveRepairReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render_summary())
    }
}

/// Parse newline-delimited JSON observations emitted by the shell wrapper.
///
/// Blank lines are skipped. Ownership: borrows `src`; returns owned records.
///
/// # Errors
///
/// [`EvalError::Json`] naming the 1-based line number for a malformed record,
/// an unknown field, or a missing field.
pub fn parse_observations_jsonl(src: &str) -> Result<Vec<LiveRepairObservation>, EvalError> {
    let mut observations = Vec::new();
    for (index, line) in src.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let observation: LiveRepairObservation = serde_json::from_str(line).map_err(|err| {
            EvalError::Json(crate::error::bound_message(format!(
                "observations line {}: {err}",
                index + 1
            )))
        })?;
        observations.push(observation);
    }
    Ok(observations)
}

fn mean_wall_ms(observations: &[&LiveRepairObservation]) -> MetricField<f64> {
    let non_error: Vec<&&LiveRepairObservation> = observations
        .iter()
        .filter(|o| o.status() != FixtureStatus::Error)
        .collect();
    if non_error.is_empty() {
        return MetricField::Unmeasured {
            reason: UnmeasuredReason::EmptySample,
        };
    }
    let total: u64 = non_error
        .iter()
        .fold(0_u64, |sum, o| sum.saturating_add(o.wall_ms));
    MetricField::Measured(total as f64 / non_error.len() as f64)
}

fn render_rate(metric: &MetricField<f64>) -> String {
    match metric {
        MetricField::Measured(value) => format!("{value:.6}"),
        MetricField::Unmeasured { reason } => format!("unmeasured:{}", reason.as_str()),
    }
}

fn render_wilson(metric: &MetricField<WilsonInterval>) -> String {
    match metric {
        MetricField::Measured(interval) => interval.render(),
        MetricField::Unmeasured { reason } => format!("unmeasured:{}", reason.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::live_repair::manifest::LIVE_REPAIR_MANIFEST_FILE;

    fn manifest_src(id: &str, tags: &str) -> String {
        format!(
            r#"
live_manifest_version = 1
id = "{id}"
goal = "fix the compile error in src/main.rs"
expected_outcome = "compile_clean"
tags = {tags}

[license]
class = "permitted"
spdx = "Alloy-Original"
source_note = "Alloy-original live-repair fixture by arkadianet."

[workspace]
path = "workspace"
package = "{id}"
"#
        )
    }

    fn write_fixture(root: &Path, id: &str, tags: &str) {
        let dir = root.join(id);
        fs::create_dir_all(dir.join("workspace/src")).unwrap();
        fs::write(dir.join(LIVE_REPAIR_MANIFEST_FILE), manifest_src(id, tags)).unwrap();
        fs::write(dir.join("LICENSE"), "license text\n").unwrap();
        fs::write(dir.join("workspace/Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.join("workspace/src/main.rs"), "fn main() {}\n").unwrap();
    }

    fn corpus() -> (tempfile::TempDir, LiveRepairCorpus) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), "aaa", "[\"e0384\"]");
        write_fixture(dir.path(), "bbb", "[\"e0308\", \"types\"]");
        let corpus = LiveRepairCorpus::load(dir.path()).unwrap();
        (dir, corpus)
    }

    fn observation(
        id: &str,
        rep: u32,
        exit: i32,
        retries: u32,
        wall_ms: u64,
    ) -> LiveRepairObservation {
        LiveRepairObservation {
            fixture_id: FixtureId::new(id).unwrap(),
            repetition: rep,
            exit_code: exit,
            retries,
            wall_ms,
        }
    }

    fn endpoint() -> LiveRepairEndpoint {
        LiveRepairEndpoint {
            model: "stub-model".to_owned(),
            temperature: 0.6,
            base_url: "http://127.0.0.1:11434/v1/".to_owned(),
        }
    }

    #[test]
    fn observation_status_vocabulary() {
        assert_eq!(observation("aaa", 1, 0, 0, 1).status(), FixtureStatus::Pass);
        assert_eq!(observation("aaa", 1, 1, 0, 1).status(), FixtureStatus::Fail);
        assert_eq!(observation("aaa", 1, 2, 0, 1).status(), FixtureStatus::Fail);
        for infra in [124, 126, 127] {
            assert_eq!(
                observation("aaa", 1, infra, 0, 1).status(),
                FixtureStatus::Error,
                "exit {infra} must be infrastructure error"
            );
        }
    }

    #[test]
    fn assemble_aggregates_per_fixture_and_overall() {
        let (_dir, corpus) = corpus();
        let observations = vec![
            observation("aaa", 1, 0, 0, 1_000),
            observation("aaa", 2, 0, 2, 3_000),
            observation("aaa", 3, 101, 1, 2_000),
            observation("bbb", 1, 124, 0, 600_000),
            observation("bbb", 2, 0, 0, 500),
        ];
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            observations,
        )
        .unwrap();

        assert_eq!(report.schema_version, LIVE_REPAIR_REPORT_VERSION);
        assert!(!report.offline);
        assert_eq!(
            report.holdout_gate,
            LiveRepairGateApplicability::NotApplicable
        );

        let aaa = &report.fixtures[0];
        assert_eq!(aaa.fixture_id.as_str(), "aaa");
        assert_eq!(aaa.tags, vec!["e0384"]);
        assert_eq!(aaa.pass_rate.attempts, 3);
        assert_eq!(aaa.pass_rate.passes, 2);
        assert_eq!(aaa.pass_rate.failures, 1);
        assert_eq!(aaa.pass_rate.errors, 0);
        assert_eq!(aaa.retries_total, 3);
        assert_eq!(aaa.passes_via_retry, 1);
        assert_eq!(aaa.mean_wall_ms, MetricField::Measured(2_000.0));

        let bbb = &report.fixtures[1];
        assert_eq!(bbb.pass_rate.attempts, 1);
        assert_eq!(bbb.pass_rate.passes, 1);
        assert_eq!(bbb.pass_rate.errors, 1);
        assert_eq!(bbb.mean_wall_ms, MetricField::Measured(500.0));

        assert_eq!(report.overall.attempts, 4);
        assert_eq!(report.overall.passes, 3);
        assert_eq!(report.overall.failures, 1);
        assert_eq!(report.overall.errors, 1);
        assert_eq!(report.overall.rate, MetricField::Measured(0.75));
        assert_eq!(
            report.overall.wilson95,
            MetricField::Measured(wilson_interval(3, 4, WILSON_Z_95))
        );
        assert_eq!(report.retries_total(), 3);
        assert_eq!(report.passes_via_retry(), 1);
    }

    #[test]
    fn assemble_reports_unmeasured_for_fixture_without_observations() {
        let (_dir, corpus) = corpus();
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            vec![observation("aaa", 1, 0, 0, 10)],
        )
        .unwrap();
        let bbb = &report.fixtures[1];
        assert_eq!(bbb.pass_rate.attempts, 0);
        assert_eq!(
            bbb.pass_rate.rate,
            MetricField::Unmeasured {
                reason: UnmeasuredReason::EmptySample
            }
        );
        assert_eq!(
            bbb.pass_rate.wilson95,
            MetricField::Unmeasured {
                reason: UnmeasuredReason::EmptySample
            }
        );
    }

    #[test]
    fn assemble_rejects_unknown_fixture_id() {
        let (_dir, corpus) = corpus();
        assert!(matches!(
            LiveRepairReport::assemble(
                "00000000-0000-4000-8000-000000000000",
                endpoint(),
                &corpus,
                vec![observation("zzz", 1, 0, 0, 10)],
            ),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn summary_is_unmistakably_not_an_offline_gate_report() {
        let (_dir, corpus) = corpus();
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            vec![
                observation("aaa", 1, 0, 1, 10),
                observation("bbb", 1, 1, 0, 20),
            ],
        )
        .unwrap();
        let expected = "alloy-eval-live-repair run_id=00000000-0000-4000-8000-000000000000\n\
offline=false\n\
holdout_gate=not_applicable\n\
endpoint model=stub-model temperature=0.600000\n\
overall pass=1 fail=1 error=0\n\
pass_rate=0.500000\n\
wilson95=[0.094529,0.905471]\n\
retries_total=1 passes_via_retry=1\n\
cost=uncalibrated\n\
cost_disclaimer=internal-only";
        assert_eq!(report.render_summary(), expected);
        assert_eq!(report.to_string(), expected);
        // The offline CI summary header must never be produced here.
        assert!(!report.render_summary().starts_with("alloy-eval run_id="));
    }

    #[test]
    fn fixture_lines_render_tags_and_intervals() {
        let (_dir, corpus) = corpus();
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            vec![observation("bbb", 1, 0, 0, 20)],
        )
        .unwrap();
        let lines = report.render_fixture_lines();
        assert!(lines.contains("fixture aaa pass=0/0"));
        assert!(lines.contains("wilson95=unmeasured:empty_sample"));
        assert!(lines.contains("fixture bbb pass=1/1"));
        assert!(lines.contains("tags=e0308,types"));
    }

    #[test]
    fn observations_jsonl_round_trip_and_errors() {
        let src = "\n{\"fixture_id\":\"aaa\",\"repetition\":1,\"exit_code\":0,\"retries\":2,\"wall_ms\":1500}\n\n\
{\"fixture_id\":\"bbb\",\"repetition\":1,\"exit_code\":124,\"retries\":0,\"wall_ms\":600000}\n";
        let parsed = parse_observations_jsonl(src).unwrap();
        assert_eq!(
            parsed,
            vec![
                observation("aaa", 1, 0, 2, 1_500),
                observation("bbb", 1, 124, 0, 600_000),
            ]
        );

        for bad in [
            "{\"fixture_id\":\"aaa\"}",
            "{\"fixture_id\":\"aaa\",\"repetition\":1,\"exit_code\":0,\"retries\":0,\"wall_ms\":1,\"extra\":1}",
            "not json",
            "{\"fixture_id\":\"BAD ID\",\"repetition\":1,\"exit_code\":0,\"retries\":0,\"wall_ms\":1}",
        ] {
            assert!(
                matches!(parse_observations_jsonl(bad), Err(EvalError::Json(_))),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn report_serde_round_trip() {
        let (_dir, corpus) = corpus();
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            vec![
                observation("aaa", 1, 0, 0, 10),
                observation("bbb", 1, 124, 0, 20),
            ],
        )
        .unwrap();
        let json = serde_json::to_vec(&report).unwrap();
        let decoded: LiveRepairReport = serde_json::from_slice(&json).unwrap();
        // `serde_json` is not built with `float_roundtrip` here, so a parsed
        // f64 may land one ULP away; the report contract is the structure and
        // the six-decimal rendering, not bit-identical floats.
        assert_eq!(decoded.render_summary(), report.render_summary());
        assert_eq!(
            decoded.render_fixture_lines(),
            report.render_fixture_lines()
        );
        let (MetricField::Measured(decoded_wilson), MetricField::Measured(source_wilson)) =
            (&decoded.overall.wilson95, &report.overall.wilson95)
        else {
            panic!("overall wilson95 must be measured");
        };
        assert!((decoded_wilson.low - source_wilson.low).abs() < 1e-12);
        assert!((decoded_wilson.high - source_wilson.high).abs() < 1e-12);
        assert_eq!(decoded.schema_version, LIVE_REPAIR_REPORT_VERSION);
        assert_eq!(decoded.run_id, report.run_id);
        assert!(!decoded.offline);
        assert_eq!(
            decoded.holdout_gate,
            LiveRepairGateApplicability::NotApplicable
        );
        assert_eq!(decoded.overall.passes, report.overall.passes);
        assert_eq!(decoded.observations, report.observations);
    }

    #[test]
    fn report_rejects_unknown_json_fields() {
        let (_dir, corpus) = corpus();
        let report = LiveRepairReport::assemble(
            "00000000-0000-4000-8000-000000000000",
            endpoint(),
            &corpus,
            vec![observation("aaa", 1, 0, 0, 10)],
        )
        .unwrap();
        let mut value: serde_json::Value = serde_json::to_value(&report).unwrap();
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<LiveRepairReport>(value).is_err());
    }
}
