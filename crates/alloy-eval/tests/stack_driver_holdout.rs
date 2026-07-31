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
use alloy_runtime::PlannerMode;

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

fn pass_rate(outcomes: &[FixtureOutcome]) -> f64 {
    let n = outcomes.len();
    if n == 0 {
        return 0.0;
    }
    let passes = outcomes
        .iter()
        .filter(|o| o.status == FixtureStatus::Pass)
        .count();
    passes as f64 / n as f64
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
    assert!(
        report.gate.as_ref().unwrap().passed,
        "live holdout gate must pass when sandbox available: {:?}",
        report.gate
    );
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

/// RFC-0017 §12.4: LLM planner non-inferiority vs template on the live
/// local-diagnostic holdout arm (ScriptedProposer; gen2 repair/edit unchanged).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn holdout_template_vs_llm_planner_non_inferior() {
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_holdout_01").unwrap();

    let template_fx = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
    let template_out = harness
        .run_live_with_planner_mode(&template_fx, PlannerMode::Template)
        .await;
    if is_sandbox_skip(&template_out) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but template: {:?}",
            template_out.error
        );
        eprintln!(
            "skip: landlock unavailable ({:?}); set ALLOY_REQUIRE_LANDLOCK=1 to fail",
            template_out.error
        );
        return;
    }

    let llm_fx = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
    let llm_out = harness
        .run_live_with_planner_mode(&llm_fx, PlannerMode::Llm)
        .await;
    if is_sandbox_skip(&llm_out) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but llm: {:?}",
            llm_out.error
        );
        eprintln!("skip: llm landlock unavailable ({:?})", llm_out.error);
        return;
    }

    assert_eq!(
        template_out.status,
        FixtureStatus::Pass,
        "template arm: {template_out:?}"
    );
    assert_eq!(llm_out.status, FixtureStatus::Pass, "llm arm: {llm_out:?}");
    assert!(
        llm_out.retry_count.is_some_and(|n| n >= 1),
        "llm arm must still replan; got {:?}",
        llm_out.retry_count
    );

    let template_pass_rate = pass_rate(std::slice::from_ref(&template_out));
    let llm_pass_rate = pass_rate(std::slice::from_ref(&llm_out));
    assert!(
        llm_pass_rate >= template_pass_rate,
        "§12.4 non-inferiority: llm_pass_rate={llm_pass_rate} < template_pass_rate={template_pass_rate}"
    );

    // One-line citation for the default-flip PR (RFC-0017 §12.4).
    eprintln!(
        "RFC-0017 §12.4 flip citation: stack_driver_holdout e0502_holdout_01 template_pass_rate={template_pass_rate} llm_pass_rate={llm_pass_rate} (both Pass; ALLOY_REQUIRE_LANDLOCK=1 cargo test -p alloy-eval --features stack-driver --test stack_driver_holdout)"
    );
}
