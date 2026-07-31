//! Shared helpers for live `stack-driver` integration tests.
//!
//! Author: arkadianet

use std::path::PathBuf;
use std::sync::Once;

use alloy_eval::FixtureOutcome;

pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

pub fn enable_live_stack() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: test process; set before any concurrent live-stack work.
        std::env::set_var("ALLOY_EVAL_LIVE_STACK", "1");
    });
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
