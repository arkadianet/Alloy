//! Determinism checks for RFC-0016 report scrubbing.

use std::path::PathBuf;

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, EvalReport, FixtureSet, MetricField, UnmeasuredReason,
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
    let mut reports = Vec::new();
    for _ in 0..8 {
        reports.push(scrub(&harness.run_batch(FixtureSet::Train).await.unwrap()));
    }
    for report in &reports[1..] {
        assert_eq!(&reports[0], report);
    }
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
