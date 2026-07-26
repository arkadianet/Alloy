//! RFC-0016 metric fields and outcome aggregation.

use serde::{Deserialize, Serialize};

use crate::manifest::SuccessCriterion;
use crate::report::{FixtureOutcome, FixtureStatus};

/// Population state for one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum MetricField<T> {
    /// Value was computed from observed fixture data this run.
    Measured(T),
    /// Value is intentionally absent and must not be treated as zero.
    Unmeasured {
        /// Reason the metric is absent.
        reason: UnmeasuredReason,
    },
}

/// Reason a metric is intentionally unmeasured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmeasuredReason {
    /// Skeleton / Day-1 path does not yet produce this signal.
    SkeletonDeferred,
    /// Owning RFC / subsystem not linked into this build.
    SubsystemAbsent,
    /// No samples in the selected fixture set.
    EmptySample,
    /// Tokens or prices insufficient for derivation.
    CostInputsIncomplete,
    /// Calibration gate not granted.
    CostUncalibrated,
    /// Explicitly not applicable to this gate profile.
    NotApplicable,
}

impl UnmeasuredReason {
    /// Return the RFC-pinned snake_case spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SkeletonDeferred => "skeleton_deferred",
            Self::SubsystemAbsent => "subsystem_absent",
            Self::EmptySample => "empty_sample",
            Self::CostInputsIncomplete => "cost_inputs_incomplete",
            Self::CostUncalibrated => "cost_uncalibrated",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// V2 §17.2 metrics with explicit population semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalMetrics {
    /// Passes over non-error fixtures.
    pub success_rate: MetricField<f64>,
    /// Compile-clean outcomes over non-error fixtures.
    pub compile_success_rate: MetricField<f64>,
    /// Passes per known input+output token.
    pub token_efficiency: MetricField<f64>,
    /// Nearest-rank p50 wall-clock latency for non-error fixtures.
    pub latency_p50_ms: MetricField<u64>,
    /// Nearest-rank p95 wall-clock latency for non-error fixtures.
    pub latency_p95_ms: MetricField<u64>,
    /// Day-1 is always unmeasured; numeric p50 is internal to the cost envelope.
    pub cost_usd_p50: MetricField<f64>,
    /// Mean retry count when the full stack supplies it.
    pub retries_mean: MetricField<f64>,
    /// Mean human interventions when GateHuman exists.
    pub human_interventions: MetricField<f64>,
    /// Introduced unsafe rate over fixtures that include `NoNewUnsafe`.
    pub unsafe_introduced_rate: MetricField<f64>,
}

impl EvalMetrics {
    /// Construct the empty-population Day-1 metric set.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            success_rate: unmeasured(UnmeasuredReason::EmptySample),
            compile_success_rate: unmeasured(UnmeasuredReason::EmptySample),
            token_efficiency: unmeasured(UnmeasuredReason::EmptySample),
            latency_p50_ms: unmeasured(UnmeasuredReason::EmptySample),
            latency_p95_ms: unmeasured(UnmeasuredReason::EmptySample),
            cost_usd_p50: unmeasured(UnmeasuredReason::CostUncalibrated),
            retries_mean: unmeasured(UnmeasuredReason::SkeletonDeferred),
            human_interventions: unmeasured(UnmeasuredReason::SkeletonDeferred),
            unsafe_introduced_rate: unmeasured(UnmeasuredReason::EmptySample),
        }
    }
}

/// Aggregates finalized fixture outcomes into RFC-0016 metrics.
pub(crate) struct MetricsAggregator;

impl MetricsAggregator {
    /// Aggregate one logical run of fixture outcomes.
    #[must_use]
    pub(crate) fn aggregate(outcomes: &[FixtureOutcome]) -> EvalMetrics {
        let non_error: Vec<&FixtureOutcome> = outcomes
            .iter()
            .filter(|outcome| outcome.status != FixtureStatus::Error)
            .collect();

        if non_error.is_empty() {
            return EvalMetrics::empty();
        }

        let denominator = non_error.len() as f64;
        let passes = non_error
            .iter()
            .filter(|outcome| outcome.status == FixtureStatus::Pass)
            .count();
        let compile_clean = non_error
            .iter()
            .filter(|outcome| outcome.compile_clean == Some(true))
            .count();

        let mut latencies: Vec<u64> = non_error.iter().map(|outcome| outcome.wall_ms).collect();
        latencies.sort_unstable();

        EvalMetrics {
            success_rate: MetricField::Measured(passes as f64 / denominator),
            compile_success_rate: MetricField::Measured(compile_clean as f64 / denominator),
            token_efficiency: token_efficiency(&non_error, passes),
            latency_p50_ms: percentile_u64(&latencies, 0.50),
            latency_p95_ms: percentile_u64(&latencies, 0.95),
            cost_usd_p50: unmeasured(UnmeasuredReason::CostUncalibrated),
            retries_mean: unmeasured(UnmeasuredReason::SkeletonDeferred),
            human_interventions: unmeasured(UnmeasuredReason::SkeletonDeferred),
            unsafe_introduced_rate: unsafe_introduced_rate(&non_error),
        }
    }

    /// Compute the internal uncalibrated cost p50 population for the cost envelope.
    #[must_use]
    pub(crate) fn internal_cost_usd_p50(outcomes: &[FixtureOutcome]) -> MetricField<f64> {
        if outcomes.is_empty() {
            return unmeasured(UnmeasuredReason::CostInputsIncomplete);
        }
        let mut costs = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            let Some(cost) = outcome.cost_usd else {
                return unmeasured(UnmeasuredReason::CostInputsIncomplete);
            };
            if !cost.is_finite() {
                return unmeasured(UnmeasuredReason::CostInputsIncomplete);
            }
            costs.push(cost);
        }
        costs.sort_by(f64::total_cmp);
        nearest_rank_index(costs.len(), 0.50)
            .map(|idx| MetricField::Measured(costs[idx]))
            .unwrap_or_else(|| unmeasured(UnmeasuredReason::CostInputsIncomplete))
    }
}

/// Return the nearest-rank percentile index for a sorted population.
#[must_use]
pub(crate) fn nearest_rank_index(len: usize, p: f64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let rank = (p * len as f64).ceil() as usize;
    Some(rank.saturating_sub(1).min(len - 1))
}

fn unmeasured<T>(reason: UnmeasuredReason) -> MetricField<T> {
    MetricField::Unmeasured { reason }
}

fn percentile_u64(samples: &[u64], p: f64) -> MetricField<u64> {
    nearest_rank_index(samples.len(), p)
        .map(|idx| MetricField::Measured(samples[idx]))
        .unwrap_or_else(|| unmeasured(UnmeasuredReason::EmptySample))
}

fn token_efficiency(outcomes: &[&FixtureOutcome], passes: usize) -> MetricField<f64> {
    let mut total_in = 0_u64;
    let mut total_out = 0_u64;
    for outcome in outcomes {
        let (Some(input), Some(output)) = (outcome.tokens_in, outcome.tokens_out) else {
            return unmeasured(UnmeasuredReason::CostInputsIncomplete);
        };
        total_in = total_in.saturating_add(input);
        total_out = total_out.saturating_add(output);
    }
    let denominator = total_in.saturating_add(total_out).max(1) as f64;
    MetricField::Measured(passes as f64 / denominator)
}

fn unsafe_introduced_rate(outcomes: &[&FixtureOutcome]) -> MetricField<f64> {
    let mut sampled = 0_usize;
    let mut introduced = 0_usize;

    for outcome in outcomes {
        if !outcome
            .criteria
            .iter()
            .any(|criterion| criterion.name == SuccessCriterion::NoNewUnsafe)
        {
            continue;
        }
        let Some(value) = outcome.unsafe_introduced else {
            return unmeasured(UnmeasuredReason::SubsystemAbsent);
        };
        sampled += 1;
        if value {
            introduced += 1;
        }
    }

    if sampled == 0 {
        unmeasured(UnmeasuredReason::NotApplicable)
    } else {
        MetricField::Measured(introduced as f64 / sampled as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{FixtureId, FixtureSet};
    use crate::report::CriterionResult;

    fn outcome(
        id: &str,
        status: FixtureStatus,
        tokens: Option<(u64, u64)>,
        unsafe_introduced: Option<bool>,
    ) -> FixtureOutcome {
        FixtureOutcome {
            fixture_id: FixtureId::new(id).unwrap(),
            set: FixtureSet::Train,
            status,
            criteria: vec![CriterionResult {
                name: SuccessCriterion::NoNewUnsafe,
                passed: unsafe_introduced != Some(true),
                detail: String::new(),
            }],
            wall_ms: 10,
            model_calls: 1,
            tokens_in: tokens.map(|value| value.0),
            tokens_out: tokens.map(|value| value.1),
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced,
            compile_clean: Some(status == FixtureStatus::Pass),
            error: None,
        }
    }

    #[test]
    fn nearest_rank_percentile_helper() {
        assert_eq!(nearest_rank_index(0, 0.50), None);
        assert_eq!(nearest_rank_index(1, 0.50), Some(0));
        assert_eq!(nearest_rank_index(1, 0.95), Some(0));
        assert_eq!(nearest_rank_index(4, 0.50), Some(1));
        assert_eq!(nearest_rank_index(4, 0.95), Some(3));
        assert_eq!(nearest_rank_index(4, 1.00), Some(3));
    }

    #[test]
    fn empty_metrics_are_unmeasured() {
        let metrics = MetricsAggregator::aggregate(&[]);
        assert_eq!(
            metrics.success_rate,
            unmeasured(UnmeasuredReason::EmptySample)
        );
        assert_eq!(
            metrics.cost_usd_p50,
            unmeasured(UnmeasuredReason::CostUncalibrated)
        );
        assert_eq!(
            metrics.retries_mean,
            unmeasured(UnmeasuredReason::SkeletonDeferred)
        );
        assert_eq!(
            metrics.compile_success_rate,
            unmeasured(UnmeasuredReason::EmptySample)
        );
        assert_eq!(
            metrics.token_efficiency,
            unmeasured(UnmeasuredReason::EmptySample)
        );
        assert_eq!(
            metrics.latency_p50_ms,
            unmeasured(UnmeasuredReason::EmptySample)
        );
        assert_eq!(
            metrics.latency_p95_ms,
            unmeasured(UnmeasuredReason::EmptySample)
        );
        assert_eq!(
            metrics.human_interventions,
            unmeasured(UnmeasuredReason::SkeletonDeferred)
        );
        assert_eq!(
            metrics.unsafe_introduced_rate,
            unmeasured(UnmeasuredReason::EmptySample)
        );
    }

    #[test]
    fn compile_rate_none_is_false() {
        let mut outcome = outcome(
            "missing-compile",
            FixtureStatus::Pass,
            Some((1, 1)),
            Some(false),
        );
        outcome.compile_clean = None;
        let outcomes = [outcome];

        let metrics = MetricsAggregator::aggregate(&outcomes);

        assert_eq!(metrics.compile_success_rate, MetricField::Measured(0.0));
        assert_eq!(outcomes[0].criteria[0].name, SuccessCriterion::NoNewUnsafe);
        assert!(outcomes[0].criteria[0].passed);
    }

    #[test]
    fn latency_excludes_errors() {
        let mut fast = outcome("fast", FixtureStatus::Pass, Some((1, 1)), Some(false));
        fast.wall_ms = 10;
        let mut slow = outcome("slow", FixtureStatus::Fail, Some((1, 1)), Some(false));
        slow.wall_ms = 30;
        let mut error = outcome("error", FixtureStatus::Error, Some((1, 1)), Some(false));
        error.wall_ms = 10_000;

        let metrics = MetricsAggregator::aggregate(&[fast, error, slow]);

        assert_eq!(metrics.latency_p50_ms, MetricField::Measured(10));
        assert_eq!(metrics.latency_p95_ms, MetricField::Measured(30));
    }

    #[test]
    fn token_efficiency_partition_is_exhaustive() {
        let measured = MetricsAggregator::aggregate(&[
            outcome("a", FixtureStatus::Pass, Some((2, 3)), Some(false)),
            outcome("b", FixtureStatus::Fail, Some((5, 0)), Some(false)),
        ]);
        assert_eq!(measured.token_efficiency, MetricField::Measured(1.0 / 10.0));

        let incomplete = MetricsAggregator::aggregate(&[
            outcome("a", FixtureStatus::Pass, Some((2, 3)), Some(false)),
            outcome("b", FixtureStatus::Fail, None, Some(false)),
        ]);
        assert_eq!(
            incomplete.token_efficiency,
            unmeasured(UnmeasuredReason::CostInputsIncomplete)
        );

        let empty = MetricsAggregator::aggregate(&[]);
        assert_eq!(
            empty.token_efficiency,
            unmeasured(UnmeasuredReason::EmptySample)
        );
    }

    #[test]
    fn token_sums_saturate() {
        let metrics = MetricsAggregator::aggregate(&[outcome(
            "saturated",
            FixtureStatus::Pass,
            Some((u64::MAX, u64::MAX)),
            Some(false),
        )]);
        assert_eq!(
            metrics.token_efficiency,
            MetricField::Measured(1.0 / u64::MAX as f64)
        );
    }

    #[test]
    fn internal_cost_p50_requires_complete_finite_inputs() {
        let mut outcomes = vec![
            outcome("a", FixtureStatus::Pass, Some((1, 1)), Some(false)),
            outcome("b", FixtureStatus::Pass, Some((1, 1)), Some(false)),
            outcome("c", FixtureStatus::Pass, Some((1, 1)), Some(false)),
        ];
        for (outcome, cost) in outcomes.iter_mut().zip([1.0, 2.0, 100.0]) {
            outcome.cost_usd = Some(cost);
        }
        assert_eq!(
            MetricsAggregator::internal_cost_usd_p50(&outcomes),
            MetricField::Measured(2.0)
        );

        outcomes[1].cost_usd = None;
        assert_eq!(
            MetricsAggregator::internal_cost_usd_p50(&outcomes),
            unmeasured(UnmeasuredReason::CostInputsIncomplete)
        );
        outcomes[1].cost_usd = Some(f64::NAN);
        assert_eq!(
            MetricsAggregator::internal_cost_usd_p50(&outcomes),
            unmeasured(UnmeasuredReason::CostInputsIncomplete)
        );
        outcomes[1].cost_usd = Some(f64::INFINITY);
        assert_eq!(
            MetricsAggregator::internal_cost_usd_p50(&outcomes),
            unmeasured(UnmeasuredReason::CostInputsIncomplete)
        );
    }

    #[test]
    fn day1_retries_and_human_are_skeleton_deferred() {
        let metrics = MetricsAggregator::aggregate(&[outcome(
            "a",
            FixtureStatus::Pass,
            Some((0, 0)),
            Some(false),
        )]);
        assert_eq!(
            metrics.retries_mean,
            unmeasured(UnmeasuredReason::SkeletonDeferred)
        );
        assert_eq!(
            metrics.human_interventions,
            unmeasured(UnmeasuredReason::SkeletonDeferred)
        );
    }

    #[test]
    fn unsafe_population_is_criterion_scoped() {
        let mut no_criterion = outcome("a", FixtureStatus::Pass, Some((1, 1)), None);
        no_criterion.criteria.clear();
        let metrics = MetricsAggregator::aggregate(&[no_criterion]);
        assert_eq!(
            metrics.unsafe_introduced_rate,
            unmeasured(UnmeasuredReason::NotApplicable)
        );

        let missing =
            MetricsAggregator::aggregate(&[outcome("a", FixtureStatus::Pass, Some((1, 1)), None)]);
        assert_eq!(
            missing.unsafe_introduced_rate,
            unmeasured(UnmeasuredReason::SubsystemAbsent)
        );
    }
}
