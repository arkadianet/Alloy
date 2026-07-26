//! Serializable eval reports and fixture outcomes.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::cost_claim::CostClaimEnvelope;
use crate::error::{EvalError, ReportError};
use crate::gate::{GateResult, NaiveComparisonResult};
use crate::manifest::{FixtureId, FixtureSet, SuccessCriterion, ToolchainRecord};
use crate::metrics::{EvalMetrics, MetricField};
use crate::trajectory::EvalTrajectoryRecord;

/// Terminal status for one fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureStatus {
    /// Success criteria satisfied.
    Pass,
    /// Ran to completion but criteria failed.
    Fail,
    /// Harness or infrastructure failure.
    Error,
}

/// Outcome for one fixture in a logical eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureOutcome {
    /// Fixture id.
    pub fixture_id: FixtureId,
    /// Fixture set.
    pub set: FixtureSet,
    /// Terminal fixture status.
    pub status: FixtureStatus,
    /// Driver-finalized criterion results.
    pub criteria: Vec<CriterionResult>,
    /// Observed fixture wall time in milliseconds.
    pub wall_ms: u64,
    /// Number of attempted model calls.
    pub model_calls: u32,
    /// Total input tokens when complete.
    pub tokens_in: Option<u64>,
    /// Total output tokens when complete.
    pub tokens_out: Option<u64>,
    /// Derived USD for this fixture when computable; never a marketing claim.
    pub cost_usd: Option<f64>,
    /// Retry count when a full-stack driver supplies it.
    pub retry_count: Option<u32>,
    /// Human interventions when GateHuman supplies them.
    pub human_interventions: Option<u32>,
    /// Whether the candidate introduced unsafe.
    pub unsafe_introduced: Option<bool>,
    /// Compile-clean observation.
    pub compile_clean: Option<bool>,
    /// Serializable boundary error. Must be `Some` iff status is `Error`.
    pub error: Option<ReportError>,
}

/// Result for one manifest success criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionResult {
    /// Criterion name.
    pub name: SuccessCriterion,
    /// Whether the criterion passed.
    pub passed: bool,
    /// Bounded criterion detail; empty on pass.
    pub detail: String,
}

/// Top-level serializable eval report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    /// Report schema version; RFC-0016 uses `1`.
    pub schema_version: u32,
    /// Canonical lowercase hyphenated UUID v4.
    pub run_id: String,
    /// Whether the run was offline by construction.
    pub offline: bool,
    /// Toolchain record assembled from the logical run.
    pub toolchain: ToolchainRecord,
    /// Control or sole-run fixture outcomes.
    pub fixtures: Vec<FixtureOutcome>,
    /// Control or sole-run per-complete trajectory records.
    pub trajectories: Vec<EvalTrajectoryRecord>,
    /// Naive outcomes when `run_holdout_with_naive` was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naive_fixtures: Option<Vec<FixtureOutcome>>,
    /// Naive per-complete records when comparison execution was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naive_trajectories: Option<Vec<EvalTrajectoryRecord>>,
    /// Control or sole-run metrics.
    pub metrics: EvalMetrics,
    /// Cost claim for the control or sole run.
    pub cost_claim: CostClaimEnvelope,
    /// Gate result when evaluated.
    pub gate: Option<GateResult>,
    /// Naive comparison data when executed.
    pub naive_comparison: Option<NaiveComparisonResult>,
}

impl EvalReport {
    /// Render the exact line-oriented CI format in RFC-0016 §9.3.
    #[must_use]
    pub fn render_ci_summary(&self) -> String {
        let control = count_statuses(&self.fixtures);
        let naive = self
            .naive_fixtures
            .as_ref()
            .map(|fixtures| {
                let counts = count_statuses(fixtures);
                format!(
                    "naive pass={} fail={} error={}",
                    counts.pass, counts.fail, counts.error
                )
            })
            .unwrap_or_else(|| "naive absent".to_owned());
        let gate_state = self
            .gate
            .as_ref()
            .map(|gate| if gate.passed { "pass" } else { "fail" })
            .unwrap_or("absent");
        let gate_failures = self.gate.as_ref().map_or(0, |gate| gate.failures.len());

        [
            format!("alloy-eval run_id={}", self.run_id),
            format!("offline={}", self.offline),
            format!(
                "control pass={} fail={} error={}",
                control.pass, control.fail, control.error
            ),
            naive,
            format!(
                "metrics compile_success_rate={} success_rate={} unsafe_introduced_rate={}",
                render_metric_f64(&self.metrics.compile_success_rate),
                render_metric_f64(&self.metrics.success_rate),
                render_metric_f64(&self.metrics.unsafe_introduced_rate)
            ),
            "cost=uncalibrated".to_owned(),
            format!("gate={} failures={}", gate_state, gate_failures),
            "cost_disclaimer=internal-only".to_owned(),
        ]
        .join("\n")
    }

    /// Day-1 always returns `None`; calibrated marketing claims are deferred.
    #[must_use]
    pub fn marketing_cost_claim(&self) -> Option<f64> {
        None
    }
}

impl fmt::Display for EvalReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render_ci_summary())
    }
}

/// Assemble report toolchain versions from non-error fixture sidecar records.
pub(crate) fn assemble_report_toolchain(
    pin_toolchain_channel: impl Into<String>,
    non_error_toolchains: &[ToolchainRecord],
) -> Result<ToolchainRecord, EvalError> {
    let channel = pin_toolchain_channel.into();
    let Some(first) = non_error_toolchains.first() else {
        return Ok(ToolchainRecord {
            channel,
            rustc_version: "none".to_owned(),
            cargo_version: "none".to_owned(),
        });
    };

    for record in &non_error_toolchains[1..] {
        if record != first {
            return Err(EvalError::Internal(
                "non-error fixture toolchain disagreement".to_owned(),
            ));
        }
    }

    Ok(ToolchainRecord {
        channel,
        rustc_version: first.rustc_version.clone(),
        cargo_version: first.cargo_version.clone(),
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct StatusCounts {
    pass: usize,
    fail: usize,
    error: usize,
}

fn count_statuses(fixtures: &[FixtureOutcome]) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for fixture in fixtures {
        match fixture.status {
            FixtureStatus::Pass => counts.pass += 1,
            FixtureStatus::Fail => counts.fail += 1,
            FixtureStatus::Error => counts.error += 1,
        }
    }
    counts
}

fn render_metric_f64(metric: &MetricField<f64>) -> String {
    match metric {
        MetricField::Measured(value) => format!("{value:.6}"),
        MetricField::Unmeasured { reason } => format!("unmeasured:{}", reason.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_claim::CostClaimEnvelope;
    use crate::fingerprint::RequestFingerprint;
    use crate::gate::{GateFailure, GateThresholds, NaiveComparisonResult};
    use crate::manifest::{FixtureSet, FixtureTurnId};
    use crate::metrics::{EvalMetrics, MetricField, UnmeasuredReason};
    use alloy_runtime::{
        CapabilityId, CompletionRequest, EndpointId, ErrorClass, ModelTier, ModelUsdSource,
        ProviderId, ResponseFormat, ToolChoice,
    };

    fn outcome(id: &str, status: FixtureStatus) -> FixtureOutcome {
        FixtureOutcome {
            fixture_id: FixtureId::new(id).unwrap(),
            set: FixtureSet::Train,
            status,
            criteria: vec![],
            wall_ms: 0,
            model_calls: 0,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: None,
        }
    }

    fn metrics() -> EvalMetrics {
        EvalMetrics {
            success_rate: MetricField::Measured(0.5),
            compile_success_rate: MetricField::Measured(1.0),
            token_efficiency: MetricField::Unmeasured {
                reason: UnmeasuredReason::CostInputsIncomplete,
            },
            latency_p50_ms: MetricField::Measured(10),
            latency_p95_ms: MetricField::Measured(10),
            cost_usd_p50: MetricField::Unmeasured {
                reason: UnmeasuredReason::CostUncalibrated,
            },
            retries_mean: MetricField::Unmeasured {
                reason: UnmeasuredReason::SkeletonDeferred,
            },
            human_interventions: MetricField::Unmeasured {
                reason: UnmeasuredReason::SkeletonDeferred,
            },
            unsafe_introduced_rate: MetricField::Unmeasured {
                reason: UnmeasuredReason::NotApplicable,
            },
        }
    }

    fn trajectory(id: &str, ordinal: u32, status: FixtureStatus) -> EvalTrajectoryRecord {
        let request = CompletionRequest {
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format: ResponseFormat::Text,
            temperature: None,
            max_output_tokens: None,
        };
        let fingerprint = RequestFingerprint::of(&request);
        let request_content_hash = fingerprint.as_digest().clone();
        EvalTrajectoryRecord {
            fixture_id: FixtureId::new(id).unwrap(),
            set: FixtureSet::Train,
            turn_id: FixtureTurnId {
                capability: CapabilityId::new("repair").unwrap(),
                node: None,
                ordinal,
            },
            request_content_hash,
            request_fingerprint: fingerprint,
            endpoint_id: EndpointId::new("eval-script").unwrap(),
            provider_id: ProviderId::new("eval-script").unwrap(),
            model_tier: ModelTier::Standard,
            input_tokens: Some(11),
            output_tokens: Some(7),
            usd: Some(0.000_039),
            usd_source: Some(ModelUsdSource::OperatorPriceTable),
            duration_ms: Some(12),
            confidence: None,
            error_class: (status == FixtureStatus::Error).then_some(ErrorClass::Internal),
            complete_ok: status != FixtureStatus::Error,
            fixture_status: status,
            compile_clean: Some(status == FixtureStatus::Pass),
        }
    }

    #[test]
    fn report_ci_summary_exact() {
        let report = EvalReport {
            schema_version: 1,
            run_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            offline: true,
            toolchain: ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "rustc".to_owned(),
                cargo_version: "cargo".to_owned(),
            },
            fixtures: vec![
                outcome("a", FixtureStatus::Pass),
                outcome("b", FixtureStatus::Fail),
            ],
            trajectories: vec![],
            naive_fixtures: None,
            naive_trajectories: None,
            metrics: metrics(),
            cost_claim: CostClaimEnvelope::uncalibrated(MetricField::Unmeasured {
                reason: UnmeasuredReason::CostInputsIncomplete,
            }),
            gate: None,
            naive_comparison: None,
        };
        let expected = "alloy-eval run_id=00000000-0000-4000-8000-000000000000\n\
offline=true\n\
control pass=1 fail=1 error=0\n\
naive absent\n\
metrics compile_success_rate=1.000000 success_rate=0.500000 unsafe_introduced_rate=unmeasured:not_applicable\n\
cost=uncalibrated\n\
gate=absent failures=0\n\
cost_disclaimer=internal-only";
        assert_eq!(report.render_ci_summary(), expected);
        assert_eq!(report.to_string(), expected);
        assert_eq!(report.marketing_cost_claim(), None);
    }

    #[test]
    fn report_serde_round_trip() {
        let mut error_fixture = outcome("round-trip-error", FixtureStatus::Error);
        error_fixture.error = Some(ReportError::from_eval(&EvalError::Manifest(
            "invalid fixture".to_owned(),
        )));
        let naive_metrics = EvalMetrics {
            compile_success_rate: MetricField::Measured(0.5),
            ..metrics()
        };
        let report = EvalReport {
            schema_version: 1,
            run_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            offline: true,
            toolchain: ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "rustc 1.97.1".to_owned(),
                cargo_version: "cargo 1.97.1".to_owned(),
            },
            fixtures: vec![
                outcome("round-trip-pass", FixtureStatus::Pass),
                error_fixture,
            ],
            trajectories: vec![
                trajectory("round-trip-pass", 0, FixtureStatus::Pass),
                trajectory("round-trip-error", 1, FixtureStatus::Error),
            ],
            naive_fixtures: Some(vec![outcome("round-trip-naive", FixtureStatus::Fail)]),
            naive_trajectories: Some(vec![trajectory("round-trip-naive", 0, FixtureStatus::Fail)]),
            metrics: metrics(),
            cost_claim: CostClaimEnvelope::uncalibrated(MetricField::Measured(0.000_039)),
            gate: Some(GateResult {
                passed: false,
                thresholds: GateThresholds::skeleton_defaults(),
                failures: vec![GateFailure::SuccessRate {
                    actual: "0.500000".to_owned(),
                    minimum: "1.000000".to_owned(),
                }],
            }),
            naive_comparison: Some(NaiveComparisonResult {
                control: metrics(),
                naive: naive_metrics,
                control_meets_or_beats_naive: true,
                detail: "naive_single_turn_patch: compile_success_rate comparison".to_owned(),
            }),
        };

        let json = serde_json::to_vec(&report).unwrap();
        let decoded: EvalReport = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn report_toolchain_assembly() {
        let toolchain = ToolchainRecord {
            channel: "manifest".to_owned(),
            rustc_version: "rustc 1.97.1".to_owned(),
            cargo_version: "cargo 1.97.1".to_owned(),
        };
        assert_eq!(
            assemble_report_toolchain("1.97.1", &[toolchain]).unwrap(),
            ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "rustc 1.97.1".to_owned(),
                cargo_version: "cargo 1.97.1".to_owned(),
            }
        );
        assert_eq!(
            assemble_report_toolchain("1.97.1", &[]).unwrap(),
            ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "none".to_owned(),
                cargo_version: "none".to_owned(),
            }
        );
    }
}
