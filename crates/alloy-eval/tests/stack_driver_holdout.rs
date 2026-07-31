//! RFC-0016 §5.9 live ControlPlane stack-driver integration smoke.
//!
//! Exercises scheduler/Landlock/MCP/EditEngine wiring with committed
//! `recordings/{repair_plan,edit_patch}.json` worker payloads — plumbing,
//! not thesis citation. LLM arm uses production CapabilityPlanProposer +
//! PlanningWorker over `recordings/planning_proposal.json` (non-gating).
//!
//! Requires `--features stack-driver` and `ALLOY_EVAL_LIVE_STACK=1` (set by
//! each test). Linux/Landlock only; skips when Landlock is unavailable unless
//! `ALLOY_REQUIRE_LANDLOCK=1` (then fails).
//!
//! Author: arkadianet

#![cfg(feature = "stack-driver")]

#[path = "live_stack_support.rs"]
mod live_stack_support;
use live_stack_support::{enable_live_stack, fixture_root, is_sandbox_skip, require_landlock};

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, FixtureDriverKind, FixtureId, FixtureOutcome, FixtureSet,
    FixtureStatus, StackLiveOptions,
};
use alloy_runtime::PlannerMode;

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

fn holdout_ids() -> [FixtureId; 2] {
    [
        FixtureId::new("e0502_holdout_01").unwrap(),
        FixtureId::new("e0502_holdout_02").unwrap(),
    ]
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
async fn holdout_live_control_plane_and_naive_both_fixtures() {
    enable_live_stack();
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();

    for id in &holdout_ids() {
        let mut control = harness.load_fixture(FixtureSet::Holdout, id).unwrap();
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
        assert_eq!(
            control_out.status,
            FixtureStatus::Pass,
            "{}: {control_out:?}",
            id.as_str()
        );
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
    }

    // Golden full_file_replace + live cargo_check (naive arm by design).
    let report = harness.run_holdout_with_naive().await.unwrap();
    assert!(
        !report.trajectories.is_empty(),
        "live control must emit trajectories"
    );
    assert!(
        report
            .naive_trajectories
            .as_ref()
            .is_some_and(|rows| !rows.is_empty()),
        "live naive must emit trajectories"
    );
    assert!(
        report.gate.as_ref().unwrap().passed,
        "live holdout gate must pass when sandbox available: {:?}",
        report.gate
    );
    let naive = report.naive_fixtures.as_ref().expect("naive fixtures");
    for id in &holdout_ids() {
        let row = naive
            .iter()
            .find(|o| o.fixture_id.as_str() == id.as_str())
            .unwrap_or_else(|| panic!("missing naive outcome for {}", id.as_str()));
        if is_sandbox_skip(row) {
            assert!(
                !require_landlock(),
                "ALLOY_REQUIRE_LANDLOCK=1 but naive: {:?}",
                row.error
            );
            eprintln!("skip: naive landlock unavailable ({:?})", row.error);
            return;
        }
        assert_eq!(row.status, FixtureStatus::Pass, "{row:?}");
        assert_eq!(row.compile_clean, Some(true));
        assert_eq!(row.model_calls, 0, "naive must not call the model");
    }
}

/// Template-mode ablation: `max_repair_generations ∈ {0, 2}` on both holdouts.
///
/// Gen1 analyze/edit are inert, so `max=0` cannot replan into a real repair
/// and is expected to fail compile_clean; `max=2` must pass.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn holdout_template_max_repair_generations_ablation() {
    enable_live_stack();
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();

    let mut outs_0 = Vec::new();
    let mut outs_2 = Vec::new();
    for id in &holdout_ids() {
        let fx0 = harness.load_fixture(FixtureSet::Holdout, id).unwrap();
        let out0 = harness
            .run_live_with_options(&fx0, StackLiveOptions::template().max_repair_generations(0))
            .await;
        if is_sandbox_skip(&out0) {
            assert!(
                !require_landlock(),
                "ALLOY_REQUIRE_LANDLOCK=1 but: {:?}",
                out0.error
            );
            eprintln!("skip: landlock unavailable ({:?})", out0.error);
            return;
        }
        outs_0.push(out0);

        let fx2 = harness.load_fixture(FixtureSet::Holdout, id).unwrap();
        let out2 = harness
            .run_live_with_options(&fx2, StackLiveOptions::template().max_repair_generations(2))
            .await;
        if is_sandbox_skip(&out2) {
            assert!(
                !require_landlock(),
                "ALLOY_REQUIRE_LANDLOCK=1 but: {:?}",
                out2.error
            );
            eprintln!("skip: landlock unavailable ({:?})", out2.error);
            return;
        }
        outs_2.push(out2);
    }

    let rate_0 = pass_rate(&outs_0);
    let rate_2 = pass_rate(&outs_2);
    eprintln!("template ablation: max0_pass_rate={rate_0} max2_pass_rate={rate_2}");
    assert!(
        rate_2 > rate_0,
        "max_repair_generations=2 must beat max=0 under inert-gen1 stack-driver: {outs_0:?} vs {outs_2:?}"
    );
    assert_eq!(
        rate_2, 1.0,
        "template max=2 must pass both holdouts: {outs_2:?}"
    );
    assert_eq!(
        rate_0, 0.0,
        "template max=0 cannot repair with inert gen1: {outs_0:?}"
    );
}

/// Non-gating CapabilityPlanProposer / PlanningWorker LLM-arm smoke.
///
/// Exercises `PlannerMode::Llm` through the production proposer + model-branch
/// PlanningWorker keyed on PLANNING_SYSTEM fingerprints. **Not** RFC-0017
/// §12.4 flip evidence — that gate still needs independent model outputs and
/// must not flip shipped profiles (AC 34 remains `mode = "template"`).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn holdout_capability_plan_proposer_llm_arm_smoke_non_gating() {
    enable_live_stack();
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();

    let mut template_outs = Vec::new();
    let mut llm_outs = Vec::new();
    for id in &holdout_ids() {
        let template_fx = harness.load_fixture(FixtureSet::Holdout, id).unwrap();
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
        template_outs.push(template_out);

        let llm_fx = harness.load_fixture(FixtureSet::Holdout, id).unwrap();
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
        assert!(
            llm_out.retry_count.is_some_and(|n| n >= 1),
            "llm arm must still replan; got {:?}",
            llm_out.retry_count
        );
        llm_outs.push(llm_out);
    }

    let template_pass_rate = pass_rate(&template_outs);
    let llm_pass_rate = pass_rate(&llm_outs);
    assert_eq!(template_pass_rate, 1.0, "template arm: {template_outs:?}");
    assert_eq!(llm_pass_rate, 1.0, "llm arm: {llm_outs:?}");
    assert!(
        llm_pass_rate >= template_pass_rate,
        "CapabilityPlanProposer smoke: llm_pass_rate={llm_pass_rate} < template_pass_rate={template_pass_rate} (non-gating wiring check only)"
    );
}
