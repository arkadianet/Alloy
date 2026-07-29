//! RFC-0015 §12.2 — composition-root process tests. The in-crate unit tests
//! in `src/assembly.rs` cover CR5/CR6/PF10 wiring directly; these cover the
//! process-observable rules.
//!
//! Author: arkadianet

mod common;

use predicates::prelude::*;

/// CR11 — read-only subcommands construct no broker, MCP host, or
/// scheduler: `events` works (here: fails only with NOT_FOUND for an
/// unknown session, exit 12) even though no sandbox probe could have run —
/// no key is exported and no backend is required.
#[test]
fn read_only_subcommands_skip_broker() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .args([
            "events",
            "--session",
            "00000000-0000-4000-8000-000000000000",
        ])
        .assert()
        .code(12)
        .stderr(predicate::str::contains("not found"));
}

/// §9.4 — a missing API key fails as EX_CONFIG naming the variable and
/// example.env, before any session row is written (§5.5 step 6 precedes
/// the sandbox probe).
#[test]
fn missing_api_key_is_config_error_naming_variable() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .args(["run", "fix something"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("ALLOY_API_KEY"))
        .stderr(predicate::str::contains("example.env"));
    // No session row was created: events still has nothing to report.
    common::alloy_in(dir.path())
        .args(["events"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no session recorded"));
}

/// SEC6 — router.toml.example is never auto-copied; the error prints the
/// copy command instead.
#[test]
fn router_example_is_never_auto_copied() {
    let dir = tempfile::tempdir().unwrap();
    common::write_profiles(dir.path());
    std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();
    // router.toml absent; only the .example template exists.
    std::fs::write(
        dir.path().join("router.toml.example"),
        alloy_runtime::default_router_toml(),
    )
    .unwrap();
    common::alloy_in(dir.path())
        .args(["index", "--stats"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("router.toml.example"));
    assert!(
        !dir.path().join("router.toml").exists(),
        "router.toml must never be auto-created (SEC6)"
    );
}
