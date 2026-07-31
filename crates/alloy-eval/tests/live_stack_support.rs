//! Shared helpers for live `stack-driver` integration tests.
//!
//! Author: arkadianet

use std::path::PathBuf;

use alloy_eval::FixtureOutcome;

pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Require live-stack support already enabled in the parent process.
///
/// Callers must set `ALLOY_EVAL_LIVE_STACK=1` (or `true`) before invoking
/// `cargo test` — e.g. CI `env:` or a shell export. This helper does not
/// mutate the process environment.
pub fn enable_live_stack() {
    assert!(
        live_stack_env_enabled(),
        "live stack tests require ALLOY_EVAL_LIVE_STACK=1 in the parent process \
         (e.g. ALLOY_EVAL_LIVE_STACK=1 cargo test -p alloy-eval --features stack-driver \
         --test stack_driver_holdout)"
    );
}

fn live_stack_env_enabled() -> bool {
    match std::env::var("ALLOY_EVAL_LIVE_STACK") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

pub fn require_landlock() -> bool {
    match std::env::var("ALLOY_REQUIRE_LANDLOCK") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

pub fn is_sandbox_skip(outcome: &FixtureOutcome) -> bool {
    outcome
        .error
        .as_ref()
        .is_some_and(|e| e.kind == "stack_driver_sandbox_unavailable")
}
