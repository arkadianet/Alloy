//! Golden skeleton and holdout integration tests for RFC-0016.

use std::path::PathBuf;

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, FixtureDriverKind, FixtureId, FixtureSet, FixtureStatus,
    MetricField, UnmeasuredReason,
};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

#[tokio::test]
async fn golden_skeleton_pass() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_local_borrow").unwrap();
    let mut fixture = harness.load_fixture(FixtureSet::Train, &id).unwrap();
    let outcome = harness.run_fixture(&mut fixture).await;
    assert_eq!(outcome.status, FixtureStatus::Pass, "{outcome:?}");
    assert!(outcome.compile_clean == Some(true));
    assert!(outcome.error.is_none());
}

#[tokio::test]
async fn gate_skeleton_defaults_pass() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let report = harness.run_batch(FixtureSet::Train).await.unwrap();
    assert!(report.offline);
    assert!(report.gate.as_ref().unwrap().passed, "{:?}", report.gate);
    assert!(matches!(
        report.metrics.cost_usd_p50,
        MetricField::Unmeasured {
            reason: UnmeasuredReason::CostUncalibrated
        }
    ));
    assert!(report.marketing_cost_claim().is_none());
}

#[tokio::test]
async fn golden_holdout_control_plane_pass() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_holdout_01").unwrap();
    let fixture = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
    assert_eq!(fixture.manifest().driver, FixtureDriverKind::ControlPlane);
    let mut fixture = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
    let outcome = harness.run_fixture(&mut fixture).await;
    assert_eq!(outcome.status, FixtureStatus::Pass, "{outcome:?}");
    assert!(outcome.compile_clean == Some(true));
    assert!(outcome.error.is_none());
}

#[tokio::test]
async fn e2e_holdout_with_naive() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let report = harness.run_holdout_with_naive().await.unwrap();
    assert!(report.naive_fixtures.is_some());
    assert!(report.naive_trajectories.is_some());
    assert!(report.naive_comparison.is_some());
    assert!(report.gate.as_ref().unwrap().passed, "{:?}", report.gate);
    assert!(
        report
            .naive_comparison
            .as_ref()
            .unwrap()
            .control_meets_or_beats_naive
    );
    for outcome in &report.fixtures {
        assert_eq!(outcome.status, FixtureStatus::Pass, "{outcome:?}");
        let fixture = harness
            .load_fixture(FixtureSet::Holdout, &outcome.fixture_id)
            .unwrap();
        assert_eq!(
            fixture.manifest().driver,
            FixtureDriverKind::ControlPlane,
            "holdout control fixtures must use the control_plane driver at M7"
        );
    }
    for outcome in report.naive_fixtures.as_ref().unwrap() {
        assert_eq!(outcome.status, FixtureStatus::Pass, "{outcome:?}");
    }
}

#[tokio::test]
async fn golden_pre_and_post_recordings() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_local_borrow").unwrap();
    let fixture = harness.load_fixture(FixtureSet::Train, &id).unwrap();
    assert!(!fixture.pre_repair().compile_clean().unwrap());
    assert!(fixture.post_repair().compile_clean().unwrap());
}

#[tokio::test]
async fn turn_node_absent_in_day1_fixtures() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    for (set, id) in [
        (FixtureSet::Train, "e0502_local_borrow"),
        (FixtureSet::Holdout, "e0502_holdout_01"),
    ] {
        let fixture = harness
            .load_fixture(set, &FixtureId::new(id).unwrap())
            .unwrap();
        for turn in &fixture.manifest().turns {
            assert!(turn.turn_id.node.is_none());
        }
    }
}

#[tokio::test]
async fn trajectories_survive_batch_and_group() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(fixture_root())).unwrap();
    let report = harness.run_batch(FixtureSet::Train).await.unwrap();
    let calls: u32 = report.fixtures.iter().map(|f| f.model_calls).sum();
    assert_eq!(report.trajectories.len(), calls as usize);
    let grouped = report.group_trajectories_by(|row| row.fixture_id.as_str().to_owned());
    assert!(!grouped.is_empty());
    assert!(report.trajectories.iter().all(|row| row.complete_ok));
}
