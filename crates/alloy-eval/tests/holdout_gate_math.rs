//! Pure gate / naive comparison math integration tests.

use alloy_eval::{
    evaluate_gate, CostClaimEnvelope, EvalMetrics, EvalReport, FixtureId, FixtureOutcome,
    FixtureSet, FixtureStatus, GateThresholds, MetricField, NaiveComparisonResult, ToolchainRecord,
    UnmeasuredReason, NAIVE_BASELINE_LABEL,
};

fn measured(v: f64) -> MetricField<f64> {
    MetricField::Measured(v)
}

fn metrics(compile: f64, success: f64, unsafe_rate: f64) -> EvalMetrics {
    EvalMetrics {
        success_rate: measured(success),
        compile_success_rate: measured(compile),
        token_efficiency: MetricField::Unmeasured {
            reason: UnmeasuredReason::EmptySample,
        },
        latency_p50_ms: MetricField::Unmeasured {
            reason: UnmeasuredReason::EmptySample,
        },
        latency_p95_ms: MetricField::Unmeasured {
            reason: UnmeasuredReason::EmptySample,
        },
        cost_usd_p50: MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated,
        },
        retries_mean: MetricField::Unmeasured {
            reason: UnmeasuredReason::SkeletonDeferred,
        },
        human_interventions: MetricField::Unmeasured {
            reason: UnmeasuredReason::SkeletonDeferred,
        },
        unsafe_introduced_rate: measured(unsafe_rate),
    }
}

fn outcome(id: &str, set: FixtureSet) -> FixtureOutcome {
    FixtureOutcome {
        fixture_id: FixtureId::new(id).unwrap(),
        set,
        status: FixtureStatus::Pass,
        criteria: vec![],
        wall_ms: 1,
        model_calls: 1,
        tokens_in: Some(1),
        tokens_out: Some(1),
        cost_usd: Some(0.0),
        retry_count: None,
        human_interventions: None,
        unsafe_introduced: Some(false),
        compile_clean: Some(true),
        error: None,
    }
}

fn report(control: EvalMetrics, naive: Option<EvalMetrics>) -> EvalReport {
    let comparison = naive.map(|naive_metrics| NaiveComparisonResult {
        control: control.clone(),
        naive: naive_metrics,
        control_meets_or_beats_naive: true,
        detail: NAIVE_BASELINE_LABEL.to_owned(),
    });
    EvalReport {
        schema_version: 1,
        run_id: "00000000-0000-4000-8000-000000000001".into(),
        offline: true,
        toolchain: ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "none".into(),
            cargo_version: "none".into(),
        },
        fixtures: vec![outcome("a", FixtureSet::Holdout)],
        trajectories: vec![],
        naive_fixtures: comparison
            .as_ref()
            .map(|_| vec![outcome("a", FixtureSet::Holdout)]),
        naive_trajectories: comparison.as_ref().map(|_| vec![]),
        metrics: control,
        cost_claim: CostClaimEnvelope::uncalibrated(MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated,
        }),
        gate: None,
        naive_comparison: comparison,
    }
}

#[test]
fn naive_tie_meets_or_beats() {
    let thresholds = GateThresholds::milestone_holdout_defaults();
    let control = metrics(1.0, 1.0, 0.0);
    let naive = metrics(1.0, 1.0, 0.0);
    let mut report = report(control, Some(naive));
    let result = evaluate_gate(&thresholds, &report);
    assert!(result.passed, "{result:?}");
    report
        .naive_comparison
        .as_mut()
        .unwrap()
        .control_meets_or_beats_naive = false;
    // Gate recomputes meets-or-beats; stored bool is ignored.
    let result = evaluate_gate(&thresholds, &report);
    assert!(result.passed, "{result:?}");
}

#[test]
fn naive_loss_fails() {
    let mut thresholds = GateThresholds::milestone_holdout_defaults();
    // Control compile meets the configured floor; failure must come only from
    // the naive-baseline comparison.
    thresholds.min_compile_success_rate = 0.5;
    let control = metrics(0.5, 1.0, 0.0);
    let naive = metrics(1.0, 1.0, 0.0);
    let report = report(control, Some(naive));
    let result = evaluate_gate(&thresholds, &report);
    assert!(!result.passed);
    assert_eq!(result.failures.len(), 1);
    assert!(matches!(
        &result.failures[0],
        alloy_eval::GateFailure::LostToNaiveBaseline { .. }
    ));
}

#[test]
fn unmeasured_cost_not_marketed() {
    let report = report(metrics(1.0, 1.0, 0.0), None);
    assert!(report.marketing_cost_claim().is_none());
    assert!(matches!(
        report.metrics.cost_usd_p50,
        MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated
        }
    ));
}
