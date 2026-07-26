//! Determinism checks for RFC-0016 report scrubbing.

use std::path::PathBuf;

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, EvalMetrics, EvalReport, FixtureOutcome, FixtureSet,
    FixtureStatus, MetricField, UnmeasuredReason,
};
use serde_json::Value;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn scrub(report: &EvalReport) -> Value {
    let mut value = serde_json::to_value(report).unwrap();
    value["run_id"] = Value::String("scrubbed".into());
    scrub_latency(&mut value["metrics"]);
    if let Some(fixtures) = value.get_mut("fixtures").and_then(Value::as_array_mut) {
        for fixture in &mut *fixtures {
            fixture["wall_ms"] = Value::from(0);
        }
        fixtures.sort_by(|a, b| {
            a["fixture_id"]
                .as_str()
                .unwrap()
                .cmp(b["fixture_id"].as_str().unwrap())
        });
    }
    if let Some(rows) = value.get_mut("trajectories").and_then(Value::as_array_mut) {
        for row in rows {
            row["duration_ms"] = Value::Null;
        }
    }
    if let Some(naive) = value
        .get_mut("naive_fixtures")
        .and_then(Value::as_array_mut)
    {
        for fixture in naive {
            fixture["wall_ms"] = Value::from(0);
        }
    }
    if let Some(rows) = value
        .get_mut("naive_trajectories")
        .and_then(Value::as_array_mut)
    {
        for row in rows {
            row["duration_ms"] = Value::Null;
        }
    }
    if let Some(comparison) = value.get_mut("naive_comparison") {
        scrub_latency(&mut comparison["control"]);
        scrub_latency(&mut comparison["naive"]);
    }
    value
}

fn scrub_latency(metrics: &mut Value) {
    metrics["latency_p50_ms"] = serde_json::json!({
        "state": "unmeasured",
        "value": { "reason": "empty_sample" }
    });
    metrics["latency_p95_ms"] = serde_json::json!({
        "state": "unmeasured",
        "value": { "reason": "empty_sample" }
    });
}

fn assert_wall_latency_observed(report: &EvalReport) {
    let non_error: Vec<&FixtureOutcome> = report
        .fixtures
        .iter()
        .filter(|fixture| fixture.status != FixtureStatus::Error)
        .collect();
    assert!(
        !non_error.is_empty(),
        "test fixture set must contain samples"
    );
    assert!(
        non_error.iter().all(|fixture| serde_json::to_value(fixture)
            .unwrap()
            .get("wall_ms")
            .is_some_and(|value| value.is_number())),
        "non-error fixtures must carry wall_ms before report scrubbing"
    );
    assert_measured_latency(&report.metrics);
}

fn assert_measured_latency(metrics: &EvalMetrics) {
    assert!(
        matches!(metrics.latency_p50_ms, MetricField::Measured(_)),
        "latency_p50_ms must be measured before report scrubbing"
    );
    assert!(
        matches!(metrics.latency_p95_ms, MetricField::Measured(_)),
        "latency_p95_ms must be measured before report scrubbing"
    );
}

#[tokio::test]
async fn determinism_same_input_same_output() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let a = scrub(&harness.run_batch(FixtureSet::Train).await.unwrap());
    let b = scrub(&harness.run_batch(FixtureSet::Train).await.unwrap());
    assert_eq!(a, b);
}

#[tokio::test]
async fn determinism_concurrent_batch() {
    let mut config = EvalHarnessConfig::skeleton(fixture_root());
    config.max_concurrency = 8;
    let harness = EvalHarness::new(config).unwrap();
    let f0 = harness.run_batch(FixtureSet::Train);
    let f1 = harness.run_batch(FixtureSet::Train);
    let f2 = harness.run_batch(FixtureSet::Train);
    let f3 = harness.run_batch(FixtureSet::Train);
    let f4 = harness.run_batch(FixtureSet::Train);
    let f5 = harness.run_batch(FixtureSet::Train);
    let f6 = harness.run_batch(FixtureSet::Train);
    let f7 = harness.run_batch(FixtureSet::Train);
    let results = tokio::join!(f0, f1, f2, f3, f4, f5, f6, f7);
    let reports = [
        scrub(&results.0.unwrap()),
        scrub(&results.1.unwrap()),
        scrub(&results.2.unwrap()),
        scrub(&results.3.unwrap()),
        scrub(&results.4.unwrap()),
        scrub(&results.5.unwrap()),
        scrub(&results.6.unwrap()),
        scrub(&results.7.unwrap()),
    ];
    for report in &reports[1..] {
        assert_eq!(&reports[0], report);
    }
}

#[tokio::test]
async fn wall_latency_remain_observational() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let a = harness.run_batch(FixtureSet::Train).await.unwrap();
    let b = harness.run_batch(FixtureSet::Train).await.unwrap();

    assert_wall_latency_observed(&a);
    assert_wall_latency_observed(&b);
    assert_eq!(scrub(&a), scrub(&b));
}

#[tokio::test]
async fn trajectory_duration_is_scrubbed() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let report = harness.run_holdout_with_naive().await.unwrap();
    assert!(!report.trajectories.is_empty());
    assert!(report
        .naive_trajectories
        .as_ref()
        .is_some_and(|rows| !rows.is_empty()));

    let scrubbed = scrub(&report);
    for field in ["trajectories", "naive_trajectories"] {
        let rows = scrubbed[field].as_array().unwrap();
        assert!(
            rows.iter().all(|row| row["duration_ms"].is_null()),
            "{field} retained a duration"
        );
    }
}

#[tokio::test]
async fn day1_public_cost_is_always_uncalibrated() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let report = harness.run_holdout_with_naive().await.unwrap();
    assert!(matches!(
        report.metrics.cost_usd_p50,
        MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated
        }
    ));
    let comparison = report.naive_comparison.as_ref().unwrap();
    assert!(matches!(
        comparison.control.cost_usd_p50,
        MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated
        }
    ));
    assert!(matches!(
        comparison.naive.cost_usd_p50,
        MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated
        }
    ));
    assert!(report.marketing_cost_claim().is_none());
}
