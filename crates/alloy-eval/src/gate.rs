//! Pure holdout gate evaluation and naive-baseline comparison types.

use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::manifest::{FixtureId, FixtureSet};
use crate::metrics::{EvalMetrics, MetricField, UnmeasuredReason};
use crate::report::{EvalReport, FixtureOutcome, FixtureStatus};

/// Thresholds applied by RFC-0016 gates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateThresholds {
    /// Minimum measured compile success rate.
    pub min_compile_success_rate: f64,
    /// Minimum measured success rate.
    pub min_success_rate: f64,
    /// Maximum measured unsafe-introduced rate.
    pub max_unsafe_introduced_rate: f64,
    /// When true, control must meet or beat naive compile success.
    pub require_beat_naive: bool,
    /// Epsilon added to control for naive comparison.
    pub naive_epsilon: f64,
    /// Fixture set the gate evaluates.
    pub set: FixtureSet,
}

impl GateThresholds {
    /// M1 / M7 holdout defaults.
    #[must_use]
    pub fn milestone_holdout_defaults() -> Self {
        Self {
            min_compile_success_rate: 1.0,
            min_success_rate: 1.0,
            max_unsafe_introduced_rate: 0.0,
            require_beat_naive: true,
            naive_epsilon: 0.0,
            set: FixtureSet::Holdout,
        }
    }

    /// Skeleton CI defaults.
    #[must_use]
    pub fn skeleton_defaults() -> Self {
        Self {
            min_compile_success_rate: 1.0,
            min_success_rate: 1.0,
            max_unsafe_introduced_rate: 0.0,
            require_beat_naive: false,
            naive_epsilon: 0.0,
            set: FixtureSet::Train,
        }
    }

    /// Reject unusable thresholds before a run starts.
    pub fn validate(&self) -> Result<(), EvalError> {
        validate_thresholds(self).map_err(EvalError::Manifest)
    }
}

/// Result of evaluating a gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    /// True iff there are no failures.
    pub passed: bool,
    /// Thresholds used for evaluation.
    pub thresholds: GateThresholds,
    /// Stable ordered failures.
    pub failures: Vec<GateFailure>,
}

/// One stable gate failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateFailure {
    /// Compile success was below the minimum.
    CompileSuccessRate {
        /// Actual rate formatted to six decimals.
        actual: String,
        /// Minimum rate formatted to six decimals.
        minimum: String,
    },
    /// Success was below the minimum.
    SuccessRate {
        /// Actual rate formatted to six decimals.
        actual: String,
        /// Minimum rate formatted to six decimals.
        minimum: String,
    },
    /// Unsafe-introduced rate was above the maximum.
    UnsafeIntroducedRate {
        /// Actual rate formatted to six decimals.
        actual: String,
        /// Maximum rate formatted to six decimals.
        maximum: String,
    },
    /// Control lost to the naive baseline.
    LostToNaiveBaseline {
        /// Control compile success formatted to six decimals.
        control: String,
        /// Naive compile success formatted to six decimals.
        naive: String,
        /// Epsilon formatted to six decimals.
        epsilon: String,
    },
    /// A required metric was unmeasured.
    MetricUnmeasured {
        /// Pinned metric field name.
        field: String,
        /// Unmeasured reason.
        reason: UnmeasuredReason,
    },
    /// A measured metric was invalid.
    InvalidMeasuredMetric {
        /// Pinned metric field name.
        field: String,
        /// Pinned detail string.
        detail: String,
    },
    /// Fixture set disagreed with thresholds.
    SetMismatch {
        /// Pinned source: `control` or `naive`.
        source: String,
        /// Fixture id.
        fixture_id: FixtureId,
        /// Expected set.
        expected: FixtureSet,
        /// Actual set.
        actual: FixtureSet,
    },
    /// Naive comparison control copy differed from report metrics.
    InconsistentNaiveComparison {
        /// Pinned `EvalMetrics` field name.
        field: String,
    },
    /// Required naive comparison is absent.
    MissingNaiveComparison,
    /// Error outcomes were present.
    FixtureErrorsPresent {
        /// Saturating count of control plus naive error fixtures.
        count: u32,
    },
    /// Thresholds failed validation.
    InvalidThreshold {
        /// Validation message.
        message: String,
    },
}

/// Pinned label for the naive baseline.
pub const NAIVE_BASELINE_LABEL: &str = "naive_single_turn_patch";

/// Naive comparison metrics and summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NaiveComparisonResult {
    /// Control metrics copied at comparison assembly.
    pub control: EvalMetrics,
    /// Naive metrics.
    pub naive: EvalMetrics,
    /// True iff control compile success plus epsilon meets or beats naive.
    pub control_meets_or_beats_naive: bool,
    /// Human-readable comparison detail.
    pub detail: String,
}

/// Pure, deterministic threshold evaluation.
#[must_use]
pub fn evaluate_gate(thresholds: &GateThresholds, report: &EvalReport) -> GateResult {
    let mut failures = Vec::new();

    if let Err(message) = validate_thresholds(thresholds) {
        failures.push(GateFailure::InvalidThreshold { message });
        return GateResult {
            passed: false,
            thresholds: thresholds.clone(),
            failures,
        };
    }

    append_set_mismatches(&mut failures, "control", thresholds.set, &report.fixtures);
    if let Some(naive) = &report.naive_fixtures {
        append_set_mismatches(&mut failures, "naive", thresholds.set, naive);
    }

    let error_count = count_errors(&report.fixtures).saturating_add(
        report
            .naive_fixtures
            .as_ref()
            .map_or(0, |fixtures| count_errors(fixtures)),
    );
    if error_count > 0 {
        failures.push(GateFailure::FixtureErrorsPresent { count: error_count });
    }

    if let Some(comparison) = &report.naive_comparison {
        append_inconsistent_naive_metrics(&mut failures, &report.metrics, &comparison.control);
    }

    let control_compile = evaluate_min_rate(
        &mut failures,
        "compile_success_rate",
        &report.metrics.compile_success_rate,
        thresholds.min_compile_success_rate,
        |actual, minimum| GateFailure::CompileSuccessRate { actual, minimum },
    );
    evaluate_min_rate(
        &mut failures,
        "success_rate",
        &report.metrics.success_rate,
        thresholds.min_success_rate,
        |actual, minimum| GateFailure::SuccessRate { actual, minimum },
    );
    evaluate_max_rate(
        &mut failures,
        "unsafe_introduced_rate",
        &report.metrics.unsafe_introduced_rate,
        thresholds.max_unsafe_introduced_rate,
    );

    if thresholds.require_beat_naive {
        match &report.naive_comparison {
            None => failures.push(GateFailure::MissingNaiveComparison),
            Some(comparison) => {
                let naive_compile = read_gate_rate(
                    &mut failures,
                    "naive_compile_success_rate",
                    &comparison.naive.compile_success_rate,
                );
                if let (Some(control), Some(naive)) = (control_compile, naive_compile) {
                    if control + thresholds.naive_epsilon < naive {
                        failures.push(GateFailure::LostToNaiveBaseline {
                            control: format_six(control),
                            naive: format_six(naive),
                            epsilon: format_six(thresholds.naive_epsilon),
                        });
                    }
                }
            }
        }
    }

    GateResult {
        passed: failures.is_empty(),
        thresholds: thresholds.clone(),
        failures,
    }
}

fn validate_thresholds(thresholds: &GateThresholds) -> Result<(), String> {
    for (name, value) in [
        (
            "min_compile_success_rate",
            thresholds.min_compile_success_rate,
        ),
        ("min_success_rate", thresholds.min_success_rate),
        (
            "max_unsafe_introduced_rate",
            thresholds.max_unsafe_introduced_rate,
        ),
    ] {
        if !value.is_finite() {
            return Err(format!("{name} must be finite"));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(format!("{name} must be within 0.0..=1.0"));
        }
    }
    if !thresholds.naive_epsilon.is_finite() {
        return Err("naive_epsilon must be finite".to_owned());
    }
    if thresholds.naive_epsilon < 0.0 {
        return Err("naive_epsilon must be non-negative".to_owned());
    }
    Ok(())
}

fn append_set_mismatches(
    failures: &mut Vec<GateFailure>,
    source: &str,
    expected: FixtureSet,
    fixtures: &[FixtureOutcome],
) {
    let mut mismatches: Vec<&FixtureOutcome> = fixtures
        .iter()
        .filter(|fixture| fixture.set != expected)
        .collect();
    mismatches.sort_by(|a, b| a.fixture_id.as_str().cmp(b.fixture_id.as_str()));
    for fixture in mismatches {
        failures.push(GateFailure::SetMismatch {
            source: source.to_owned(),
            fixture_id: fixture.fixture_id.clone(),
            expected,
            actual: fixture.set,
        });
    }
}

fn count_errors(fixtures: &[FixtureOutcome]) -> u32 {
    fixtures.iter().fold(0_u32, |count, fixture| {
        if fixture.status == FixtureStatus::Error {
            count.saturating_add(1)
        } else {
            count
        }
    })
}

fn append_inconsistent_naive_metrics(
    failures: &mut Vec<GateFailure>,
    report: &EvalMetrics,
    comparison: &EvalMetrics,
) {
    for (field, equal) in [
        (
            "success_rate",
            metric_f64_eq(&report.success_rate, &comparison.success_rate),
        ),
        (
            "compile_success_rate",
            metric_f64_eq(
                &report.compile_success_rate,
                &comparison.compile_success_rate,
            ),
        ),
        (
            "token_efficiency",
            metric_f64_eq(&report.token_efficiency, &comparison.token_efficiency),
        ),
        (
            "latency_p50_ms",
            report.latency_p50_ms == comparison.latency_p50_ms,
        ),
        (
            "latency_p95_ms",
            report.latency_p95_ms == comparison.latency_p95_ms,
        ),
        (
            "cost_usd_p50",
            metric_f64_eq(&report.cost_usd_p50, &comparison.cost_usd_p50),
        ),
        (
            "retries_mean",
            metric_f64_eq(&report.retries_mean, &comparison.retries_mean),
        ),
        (
            "human_interventions",
            metric_f64_eq(&report.human_interventions, &comparison.human_interventions),
        ),
        (
            "unsafe_introduced_rate",
            metric_f64_eq(
                &report.unsafe_introduced_rate,
                &comparison.unsafe_introduced_rate,
            ),
        ),
    ] {
        if !equal {
            failures.push(GateFailure::InconsistentNaiveComparison {
                field: field.to_owned(),
            });
        }
    }
}

fn metric_f64_eq(left: &MetricField<f64>, right: &MetricField<f64>) -> bool {
    match (left, right) {
        (MetricField::Measured(a), MetricField::Measured(b)) => a.to_bits() == b.to_bits(),
        (MetricField::Unmeasured { reason: left }, MetricField::Unmeasured { reason: right }) => {
            left == right
        }
        _ => false,
    }
}

fn evaluate_min_rate(
    failures: &mut Vec<GateFailure>,
    field: &str,
    metric: &MetricField<f64>,
    minimum: f64,
    failure: impl FnOnce(String, String) -> GateFailure,
) -> Option<f64> {
    let actual = read_gate_rate(failures, field, metric)?;
    if actual < minimum {
        failures.push(failure(format_six(actual), format_six(minimum)));
    }
    Some(actual)
}

fn evaluate_max_rate(
    failures: &mut Vec<GateFailure>,
    field: &str,
    metric: &MetricField<f64>,
    maximum: f64,
) -> Option<f64> {
    let actual = read_gate_rate(failures, field, metric)?;
    if actual > maximum {
        failures.push(GateFailure::UnsafeIntroducedRate {
            actual: format_six(actual),
            maximum: format_six(maximum),
        });
    }
    Some(actual)
}

fn read_gate_rate(
    failures: &mut Vec<GateFailure>,
    field: &str,
    metric: &MetricField<f64>,
) -> Option<f64> {
    match metric {
        MetricField::Measured(value) if !value.is_finite() => {
            failures.push(GateFailure::InvalidMeasuredMetric {
                field: field.to_owned(),
                detail: "rate is non-finite".to_owned(),
            });
            None
        }
        MetricField::Measured(value) if !(0.0..=1.0).contains(value) => {
            failures.push(GateFailure::InvalidMeasuredMetric {
                field: field.to_owned(),
                detail: "rate is outside 0.0..=1.0".to_owned(),
            });
            None
        }
        MetricField::Measured(value) => Some(*value),
        MetricField::Unmeasured { reason } => {
            failures.push(GateFailure::MetricUnmeasured {
                field: field.to_owned(),
                reason: *reason,
            });
            None
        }
    }
}

fn format_six(value: f64) -> String {
    format!("{value:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_claim::CostClaimEnvelope;
    use crate::manifest::ToolchainRecord;
    use crate::metrics::{EvalMetrics, MetricField, UnmeasuredReason};

    fn metrics(
        compile: MetricField<f64>,
        success: MetricField<f64>,
        unsafe_rate: MetricField<f64>,
    ) -> EvalMetrics {
        EvalMetrics {
            success_rate: success,
            compile_success_rate: compile,
            token_efficiency: MetricField::Unmeasured {
                reason: UnmeasuredReason::CostInputsIncomplete,
            },
            latency_p50_ms: MetricField::Measured(1),
            latency_p95_ms: MetricField::Measured(1),
            cost_usd_p50: MetricField::Unmeasured {
                reason: UnmeasuredReason::CostUncalibrated,
            },
            retries_mean: MetricField::Unmeasured {
                reason: UnmeasuredReason::SkeletonDeferred,
            },
            human_interventions: MetricField::Unmeasured {
                reason: UnmeasuredReason::SkeletonDeferred,
            },
            unsafe_introduced_rate: unsafe_rate,
        }
    }

    fn report(metrics: EvalMetrics) -> EvalReport {
        EvalReport {
            schema_version: 1,
            run_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            offline: true,
            toolchain: ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "none".to_owned(),
                cargo_version: "none".to_owned(),
            },
            fixtures: vec![],
            trajectories: vec![],
            naive_fixtures: None,
            naive_trajectories: None,
            metrics,
            cost_claim: CostClaimEnvelope::uncalibrated(MetricField::Unmeasured {
                reason: UnmeasuredReason::CostInputsIncomplete,
            }),
            gate: None,
            naive_comparison: None,
        }
    }

    #[test]
    fn gate_thresholds_deny_unknown_fields() {
        let toml = "\
min_compile_success_rate = 1.0
min_success_rate = 1.0
max_unsafe_introduced_rate = 0.0
require_beat_naive = false
naive_epsilon = 0.0
set = \"train\"
unknown = true
";
        assert!(toml::from_str::<GateThresholds>(toml).is_err());
    }

    #[test]
    fn gate_numeric_strings_fixed_six() {
        let mut thresholds = GateThresholds::skeleton_defaults();
        thresholds.min_compile_success_rate = 0.75;
        thresholds.min_success_rate = 0.75;
        thresholds.max_unsafe_introduced_rate = 0.25;
        let report = report(metrics(
            MetricField::Measured(0.5),
            MetricField::Measured(0.25),
            MetricField::Measured(0.5),
        ));
        let result = evaluate_gate(&thresholds, &report);
        assert_eq!(
            result.failures,
            vec![
                GateFailure::CompileSuccessRate {
                    actual: "0.500000".to_owned(),
                    minimum: "0.750000".to_owned(),
                },
                GateFailure::SuccessRate {
                    actual: "0.250000".to_owned(),
                    minimum: "0.750000".to_owned(),
                },
                GateFailure::UnsafeIntroducedRate {
                    actual: "0.500000".to_owned(),
                    maximum: "0.250000".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn gate_omits_fixture_errors_when_none() {
        let report = report(metrics(
            MetricField::Measured(1.0),
            MetricField::Measured(1.0),
            MetricField::Measured(0.0),
        ));
        let result = evaluate_gate(&GateThresholds::skeleton_defaults(), &report);
        assert!(result.passed);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn gate_failure_string_fields_are_exact() {
        let report = report(metrics(
            MetricField::Unmeasured {
                reason: UnmeasuredReason::EmptySample,
            },
            MetricField::Measured(f64::INFINITY),
            MetricField::Unmeasured {
                reason: UnmeasuredReason::NotApplicable,
            },
        ));
        let result = evaluate_gate(&GateThresholds::skeleton_defaults(), &report);
        assert_eq!(
            result.failures,
            vec![
                GateFailure::MetricUnmeasured {
                    field: "compile_success_rate".to_owned(),
                    reason: UnmeasuredReason::EmptySample,
                },
                GateFailure::InvalidMeasuredMetric {
                    field: "success_rate".to_owned(),
                    detail: "rate is non-finite".to_owned(),
                },
                GateFailure::MetricUnmeasured {
                    field: "unsafe_introduced_rate".to_owned(),
                    reason: UnmeasuredReason::NotApplicable,
                },
            ]
        );
    }

    #[test]
    fn require_naive_comparison_fails_closed() {
        let report = report(metrics(
            MetricField::Measured(1.0),
            MetricField::Measured(1.0),
            MetricField::Measured(0.0),
        ));
        let result = evaluate_gate(&GateThresholds::milestone_holdout_defaults(), &report);
        assert!(result
            .failures
            .contains(&GateFailure::MissingNaiveComparison));
    }

    #[test]
    fn threshold_validation_rejects_invalid_values() {
        let mut thresholds = GateThresholds::skeleton_defaults();
        thresholds.naive_epsilon = -0.1;
        assert!(thresholds.validate().is_err());
        let result = evaluate_gate(&thresholds, &report(EvalMetrics::empty()));
        assert!(matches!(
            result.failures.as_slice(),
            [GateFailure::InvalidThreshold { message }] if message == "naive_epsilon must be non-negative"
        ));
    }

    #[test]
    fn lost_to_naive_baseline_uses_canonical_control() {
        let control = metrics(
            MetricField::Measured(0.5),
            MetricField::Measured(1.0),
            MetricField::Measured(0.0),
        );
        let naive = metrics(
            MetricField::Measured(0.75),
            MetricField::Measured(1.0),
            MetricField::Measured(0.0),
        );
        let mut report = report(control.clone());
        report.naive_comparison = Some(NaiveComparisonResult {
            control,
            naive,
            control_meets_or_beats_naive: true,
            detail: NAIVE_BASELINE_LABEL.to_owned(),
        });
        let result = evaluate_gate(&GateThresholds::milestone_holdout_defaults(), &report);
        assert!(result.failures.contains(&GateFailure::LostToNaiveBaseline {
            control: "0.500000".to_owned(),
            naive: "0.750000".to_owned(),
            epsilon: "0.000000".to_owned(),
        }));
    }
}
