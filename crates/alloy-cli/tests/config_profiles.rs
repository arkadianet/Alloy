//! RFC-0015 §12.2 — config and profile tests.
//!
//! Author: arkadianet

mod common;

use std::time::Duration;

use alloy_runtime::{ConfigPaths, RuntimeConfig};
use predicates::prelude::*;

const EX_CONFIG: i32 = 3;

/// PF2/PF3 — the three catalog profiles all load into the one struct.
#[test]
fn three_catalog_profiles_parse() {
    let dir = common::workspace();
    for id in ["default", "autonomous", "readonly"] {
        common::alloy_in(dir.path())
            .args(["index", "--stats", "--profile", id])
            .assert()
            .success();
    }
}

/// AC14 — `default` matches Architecture V2 Appendix B value-for-value
/// across every parsed table.
#[test]
fn default_profile_matches_appendix_b() {
    let dir = common::workspace();
    let cfg = RuntimeConfig::load(ConfigPaths::for_workspace(dir.path().to_path_buf())).unwrap();

    assert_eq!(cfg.profile_id.as_deref(), Some("default"));

    // [gates]
    assert!(cfg.gates.require_cargo_check);
    assert!(cfg.gates.require_human_on_public_api);
    assert!(cfg.gates.require_human_on_new_unsafe);
    assert!(cfg.gates.require_human_on_new_dependency);
    assert!(!cfg.gates.allow_raw_bash);

    // [sandbox] echo
    let sandbox = cfg.sandbox_echo.as_ref().unwrap();
    assert_eq!(sandbox.check, "landlock");
    assert_eq!(sandbox.test, "container");
    assert_eq!(sandbox.network, "deny");
    assert!(sandbox.quarantine_deps);

    // [budgets]
    assert_eq!(cfg.budget_policy.max_usd_per_run, 5.0);
    assert_eq!(cfg.budget_policy.max_tokens_per_run, 2_000_000);
    assert_eq!(cfg.budget_policy.max_parallel_nodes, 1);
    assert_eq!(cfg.budget_policy.max_parallel_cargo, 1);
    assert_eq!(cfg.budget_policy.max_parallel_edits, 1);

    // [context]
    assert_eq!(cfg.context_profile.total_token_budget, 32_000);

    // [observability]
    assert!(!cfg.retain_full_prompts);
    assert!(!cfg.retain_tool_bodies);

    // [limits] (amendment A2)
    assert_eq!(cfg.run_timeout, Duration::from_secs(1800));
    assert_eq!(cfg.gate_timeout, None);
}

/// PF4 — `[profile].id` must equal the selected catalog id.
#[test]
fn profile_id_mismatch_is_config_error() {
    let dir = common::workspace();
    // profiles/autonomous.toml declaring id = "default".
    std::fs::write(
        dir.path().join("profiles/autonomous.toml"),
        common::DEFAULT_PROFILE,
    )
    .unwrap();
    common::alloy_in(dir.path())
        .args(["index", "--stats", "--profile", "autonomous"])
        .assert()
        .code(EX_CONFIG)
        .stderr(predicate::str::contains("mismatch"));
}

fn assert_default_profile_rejected(mutation: (&str, &str), needle: &str) {
    let dir = common::workspace();
    let body = common::DEFAULT_PROFILE.replace(mutation.0, mutation.1);
    assert_ne!(body, common::DEFAULT_PROFILE, "mutation had no effect");
    common::set_default_profile(dir.path(), &body);
    common::alloy_in(dir.path())
        .args(["index", "--stats"])
        .assert()
        .code(EX_CONFIG)
        .stderr(predicate::str::contains(needle));
}

/// PF6 — allow_raw_bash = true is rejected in every profile.
#[test]
fn allow_raw_bash_true_is_rejected() {
    assert_default_profile_rejected(
        ("allow_raw_bash = false", "allow_raw_bash = true          "),
        "allow_raw_bash",
    );
}

/// PF7 — require_cargo_check = false is rejected.
#[test]
fn require_cargo_check_false_is_rejected() {
    assert_default_profile_rejected(
        ("require_cargo_check = true ", "require_cargo_check = false"),
        "require_cargo_check",
    );
}

/// PF8 — network != "deny" is rejected before any broker construction
/// (index constructs no broker at all — CR11).
#[test]
fn network_allow_is_rejected_before_broker() {
    assert_default_profile_rejected(("network = \"deny\" ", "network = \"allow\""), "network");
}

/// PF12 — max_parallel_* other than 1 is a config error.
#[test]
fn parallel_knobs_must_be_one() {
    assert_default_profile_rejected(
        ("max_parallel_nodes = 1 ", "max_parallel_nodes = 2 "),
        "max_parallel_nodes",
    );
}

/// PF13 — [context] weights must sum to 1.0 ± 1e-6.
#[test]
fn context_weights_must_sum_to_one() {
    assert_default_profile_rejected(
        (
            "weights = { conversation = 0.20, working_set = 0.55, artifacts = 0.25 }",
            "weights = { conversation = 0.50, working_set = 0.55, artifacts = 0.25 }",
        ),
        "context",
    );
}

/// PF14 — unknown top-level tables are an error, not a warning.
#[test]
fn unknown_table_is_rejected() {
    assert_default_profile_rejected(("[sandbox]", "[sandbxo]"), "profile");
}

/// PR1 — ALLOY_DATA_DIR wins and the provenance says so.
#[test]
fn env_beats_flag_for_data_dir() {
    let dir = common::workspace();
    let custom = dir.path().join("custom-data");
    let out = common::alloy_in(dir.path())
        .env("ALLOY_DATA_DIR", &custom)
        .args(["index", "--stats", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("one JSON document on stdout");
    assert_eq!(doc["config"]["data_dir_rule"], "ALLOY_DATA_DIR");
    assert_eq!(
        doc["config"]["data_dir"],
        custom.display().to_string().as_str()
    );
}

/// PR3 — ALLOY_PROFILE file and --profile id must agree.
#[test]
fn alloy_profile_env_and_profile_flag_must_agree() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .env("ALLOY_PROFILE", "profiles/default.toml")
        .args(["index", "--stats", "--profile", "autonomous"])
        .assert()
        .code(EX_CONFIG)
        .stderr(predicate::str::contains("mismatch"));
}

/// PR4 — every --json invocation reports config provenance.
#[test]
fn json_reports_config_provenance() {
    let dir = common::workspace();
    let out = common::alloy_in(dir.path())
        .args(["index", "--stats", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["schema"], "alloy.cli/v1");
    for key in ["data_dir", "data_dir_rule", "profile_path", "router_path"] {
        assert!(doc["config"][key].is_string(), "missing config.{key}");
    }
}

/// PR2 — a relative `--workspace` is joined exactly once.
#[test]
fn relative_workspace_not_double_joined() {
    let parent = tempfile::tempdir().unwrap();
    let ws = parent.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    common::write_profiles(&ws);
    std::fs::write(ws.join("router.toml"), alloy_runtime::default_router_toml()).unwrap();
    std::fs::write(ws.join("example.env"), "ALLOY_API_KEY=\n").unwrap();

    common::alloy_in(parent.path())
        .args(["index", "--stats", "--workspace", "ws"])
        .assert()
        .success();
    // The data dir landed under ws/.alloy, not ws/ws/.alloy.
    assert!(ws.join(".alloy").is_dir());
    assert!(!ws.join("ws").exists());
}
