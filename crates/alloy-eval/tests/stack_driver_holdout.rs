//! RFC-0016 §5.9 live ControlPlane stack-driver thesis test.
//!
//! Requires `--features stack-driver`. Linux/Landlock only; skips when
//! Landlock is unavailable unless `ALLOY_REQUIRE_LANDLOCK=1` (then fails).
//!
//! Author: arkadianet

#![cfg(feature = "stack-driver")]

use std::path::PathBuf;

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, FixtureDriverKind, FixtureId, FixtureOutcome, FixtureSet,
    FixtureStatus,
};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn require_landlock() -> bool {
    std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
}

fn is_sandbox_skip(outcome: &FixtureOutcome) -> bool {
    outcome
        .error
        .as_ref()
        .is_some_and(|e| e.kind == "stack_driver_sandbox_unavailable")
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn stack_driver_holdout_is_linux_only() {
    assert!(
        !require_landlock(),
        "ALLOY_REQUIRE_LANDLOCK=1 but this OS has no Landlock backend"
    );
    eprintln!("skip: stack_driver_holdout is Linux/Landlock-only");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn holdout_01_live_control_plane_and_naive() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_holdout_01").unwrap();

    let mut control = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
    assert_eq!(control.manifest().driver, FixtureDriverKind::ControlPlane);
    let control_out = harness.run_fixture(&mut control).await;
    if is_sandbox_skip(&control_out) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but: {:?}",
            control_out.error
        );
        eprintln!(
            "skip: landlock unavailable ({:?}); set ALLOY_REQUIRE_LANDLOCK=1 to fail",
            control_out.error
        );
        return;
    }
    assert_eq!(control_out.status, FixtureStatus::Pass, "{control_out:?}");
    assert_eq!(control_out.compile_clean, Some(true));
    assert!(control_out.model_calls >= 2, "{control_out:?}");
    assert!(
        control_out.criteria.iter().all(|c| c.passed),
        "{control_out:?}"
    );
    assert!(control_out.error.is_none());
    assert!(
        control_out.retry_count.is_some_and(|n| n >= 1),
        "expected a replan retry, got {:?}",
        control_out.retry_count
    );

    // Fair naive baseline: golden full_file_replace + live cargo_check.
    // `run_holdout_with_naive` exercises the feature-gated naive live path.
    let report = harness.run_holdout_with_naive().await.unwrap();
    let naive = report
        .naive_fixtures
        .as_ref()
        .expect("naive fixtures")
        .iter()
        .find(|o| o.fixture_id.as_str() == "e0502_holdout_01")
        .expect("holdout_01 naive outcome");
    if is_sandbox_skip(naive) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but naive: {:?}",
            naive.error
        );
        eprintln!("skip: naive landlock unavailable ({:?})", naive.error);
        return;
    }
    assert_eq!(naive.status, FixtureStatus::Pass, "{naive:?}");
    assert_eq!(naive.compile_clean, Some(true));
    assert_eq!(naive.model_calls, 0, "naive must not call the model");
}
