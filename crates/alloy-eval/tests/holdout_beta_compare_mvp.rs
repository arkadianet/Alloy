//! RFC-0012 §14.2 weight-measurement prep: compare V2 default domain weights
//! against at least one alternate split under live `stack-driver`.
//!
//! Runs holdout_01 with [`DefaultContextEngine`] + FIFO
//! [`RecordingModelProvider`] (NullContextEngine fingerprints do not apply).
//! Prints a small metrics table — no marketing cost claims.
//!
//! Why-not (no DomainWeights / profile TOML change): with golden-derived /
//! committed-recording repair/edit JSON, success and compile rates are
//! identical across weight arms (provider outputs ignore PromptPack shape).
//! This harness is measurement prep only; keep `DomainWeights::v2_defaults()`
//! until independent model outputs produce a real signal (RFC-0012 §14.2).
//!
//! Requires `--features stack-driver` and `ALLOY_EVAL_LIVE_STACK=1`.
//!
//! Author: arkadianet

#![cfg(feature = "stack-driver")]

use std::path::PathBuf;
use std::sync::Once;

use alloy_eval::{
    EvalHarness, EvalHarnessConfig, FixtureId, FixtureOutcome, FixtureSet, FixtureStatus,
    StackLiveOptions,
};
use alloy_runtime::{ContextProfile, DomainWeights};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn enable_live_stack() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: test process; set before any concurrent live-stack work.
        std::env::set_var("ALLOY_EVAL_LIVE_STACK", "1");
    });
}

fn require_landlock() -> bool {
    match std::env::var("ALLOY_REQUIRE_LANDLOCK") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

fn is_sandbox_skip(outcome: &FixtureOutcome) -> bool {
    outcome
        .error
        .as_ref()
        .is_some_and(|e| e.kind == "stack_driver_sandbox_unavailable")
}

struct ArmMetrics {
    label: &'static str,
    success_rate: f64,
    compile_success_rate: f64,
    model_calls: u32,
    wall_ms: u64,
}

fn metrics(label: &'static str, out: &FixtureOutcome) -> ArmMetrics {
    let success_rate = if out.status == FixtureStatus::Pass {
        1.0
    } else {
        0.0
    };
    let compile_success_rate = if out.compile_clean == Some(true) {
        1.0
    } else {
        0.0
    };
    ArmMetrics {
        label,
        success_rate,
        compile_success_rate,
        model_calls: out.model_calls,
        wall_ms: out.wall_ms,
    }
}

fn print_table(rows: &[ArmMetrics]) {
    eprintln!(
        "{:<22} {:>12} {:>14} {:>11} {:>10}",
        "arm", "success_rate", "compile_ok", "model_calls", "wall_ms"
    );
    for row in rows {
        eprintln!(
            "{:<22} {:>12.2} {:>14.2} {:>11} {:>10}",
            row.label, row.success_rate, row.compile_success_rate, row.model_calls, row.wall_ms
        );
    }
}

fn profile_with_weights(weights: DomainWeights) -> ContextProfile {
    let mut profile = ContextProfile::v2_defaults();
    profile.weights = weights;
    profile
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn holdout_beta_compare_mvp_is_linux_only() {
    assert!(
        !require_landlock(),
        "ALLOY_REQUIRE_LANDLOCK=1 but this OS has no Landlock backend"
    );
    eprintln!("skip: holdout_beta_compare_mvp is Linux/Landlock-only");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn holdout_beta_compare_mvp() {
    enable_live_stack();
    let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(fixture_root())).unwrap();
    let id = FixtureId::new("e0502_holdout_01").unwrap();

    let v2 = profile_with_weights(DomainWeights::v2_defaults());
    // Alternate: more working_set, less conversation (same live sum 1.0).
    let more_ws = profile_with_weights(DomainWeights {
        conversation: 0.10,
        working_set: 0.65,
        artifacts: 0.25,
    });
    // Alternate: more conversation.
    let more_conv = profile_with_weights(DomainWeights {
        conversation: 0.35,
        working_set: 0.40,
        artifacts: 0.25,
    });

    let mut rows = Vec::new();
    for (label, profile) in [
        ("v2_defaults", v2),
        ("more_working_set", more_ws),
        ("more_conversation", more_conv),
    ] {
        let fx = harness.load_fixture(FixtureSet::Holdout, &id).unwrap();
        let out = harness
            .run_live_with_options(&fx, StackLiveOptions::with_context_profile(profile))
            .await;
        if is_sandbox_skip(&out) {
            assert!(
                !require_landlock(),
                "ALLOY_REQUIRE_LANDLOCK=1 but {label}: {:?}",
                out.error
            );
            eprintln!(
                "skip: landlock unavailable ({:?}); set ALLOY_REQUIRE_LANDLOCK=1 to fail",
                out.error
            );
            return;
        }
        assert_eq!(
            out.status,
            FixtureStatus::Pass,
            "weight arm {label} must still pass smoke: {out:?}"
        );
        rows.push(metrics(label, &out));
    }

    print_table(&rows);

    // No DomainWeights::v2_defaults() / profile TOML change: committed-recording
    // arms share the same success/compile signal (provider ignores pack shape).
    let v2 = &rows[0];
    for alt in &rows[1..] {
        assert!(
            (alt.success_rate - v2.success_rate).abs() < f64::EPSILON
                && (alt.compile_success_rate - v2.compile_success_rate).abs() < f64::EPSILON,
            "unexpected weight-arm signal (would justify a weights change): v2={:?} alt={:?}",
            (v2.success_rate, v2.compile_success_rate),
            (alt.label, alt.success_rate, alt.compile_success_rate)
        );
    }
}
