//! RFC-0015 §12.2 — behaviour tests that need no sandbox or model: dry-run,
//! readonly refusal, cancel idempotence, index/decision wiring, event
//! cursors, secret hygiene. The full offline repair flow (gates, approve,
//! resume) lives in `cli_repair_e2e.rs`.
//!
//! Author: arkadianet

mod common;

use predicates::prelude::*;

fn dry_run_json(dir: &std::path::Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["run", "fix the compile error", "--dry-run", "--json"];
    args.extend_from_slice(extra);
    let out = common::alloy_in(dir).args(&args).output().unwrap();
    assert!(
        out.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("one JSON document")
}

/// CL12 — `--dry-run` plans and prints the DAG; nothing is dispatched, so
/// the session log carries no NodeState events.
#[test]
fn run_dry_run_plans_without_dispatch() {
    let dir = common::workspace();
    let doc = dry_run_json(dir.path(), &[]);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["template_id"], "RepairLocalDiagnostic");
    let nodes = doc["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4, "repair template has 4 nodes: {nodes:?}");

    // No NodeState events were appended (no dispatch happened).
    let events = common::alloy_in(dir.path())
        .args(["events", "--json"])
        .output()
        .unwrap();
    assert!(events.status.success());
    let text = String::from_utf8(events.stdout).unwrap();
    assert!(!text.contains("node_state"), "dry-run dispatched: {text}");
    assert!(text.contains("plan_produced"));
}

/// CL6/CL12 — `--template` works only under `--dry-run` (parse-level) and
/// passes through as the override.
#[test]
fn template_override_reaches_plan() {
    let dir = common::workspace();
    let doc = dry_run_json(dir.path(), &["--template", "repair_local_diagnostic"]);
    assert_eq!(doc["template_id"], "RepairLocalDiagnostic");
}

/// PF9 — readonly refuses a non-dry run before creating a session.
#[test]
fn readonly_refuses_run() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .args(["run", "goal", "--profile", "readonly"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("readonly"))
        .stderr(predicate::str::contains("--dry-run"));
    // Structurally refused before any session row: nothing recorded.
    common::alloy_in(dir.path())
        .args(["events"])
        .assert()
        .code(2);
    // --yes under readonly is a usage error (PF9).
    common::alloy_in(dir.path())
        .args(["run", "goal", "--profile", "readonly", "--yes"])
        .assert()
        .code(2);
    // readonly --dry-run works (plan-only) with budget 0.
    common::alloy_in(dir.path())
        .args(["run", "goal", "--profile", "readonly", "--dry-run"])
        .assert()
        .success();
}

/// SQ12 — cancel is idempotent from the user's view.
#[test]
fn cancel_is_idempotent() {
    let dir = common::workspace();
    let doc = dry_run_json(dir.path(), &[]);
    let run = doc["run"].as_str().unwrap().to_owned();
    // The dry-run already cancelled its run; cancelling again is Ok twice.
    for _ in 0..2 {
        common::alloy_in(dir.path())
            .args(["cancel", "--run", &run])
            .assert()
            .success()
            .stdout(predicate::str::contains("cancelled"));
    }
}

/// SQ4/SQ6 — the events cursor resumes exactly: no duplicates, no gaps.
#[test]
fn events_cursor_resumes_exactly() {
    let dir = common::workspace();
    let _ = dry_run_json(dir.path(), &[]);

    let all = events_seqs(dir.path(), &["--json"]);
    assert!(all.len() >= 3, "expected several events, got {all:?}");

    // Page with limit 1 from each cursor; concatenation equals the whole.
    let mut paged = Vec::new();
    let mut after: Option<u64> = None;
    loop {
        let mut args = vec!["--json".to_owned(), "--limit".to_owned(), "1".to_owned()];
        if let Some(a) = after {
            args.push("--after".to_owned());
            args.push(a.to_string());
        }
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = events_seqs(dir.path(), &argrefs);
        match page.as_slice() {
            [] => break,
            [one] => {
                paged.push(*one);
                after = Some(*one);
            }
            more => panic!("limit 1 returned {more:?}"),
        }
    }
    assert_eq!(paged, all, "cursor paging must be exact");

    // SQ6 — an oversized limit is clamped and reported, not rejected.
    common::alloy_in(dir.path())
        .args(["events", "--limit", "99999"])
        .assert()
        .success()
        .stderr(predicate::str::contains("clamped"));
}

fn events_seqs(dir: &std::path::Path, extra: &[&str]) -> Vec<u64> {
    let mut args = vec!["events"];
    args.extend_from_slice(extra);
    let out = common::alloy_in(dir).args(&args).output().unwrap();
    assert!(
        out.status.success(),
        "events failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).expect("JSONL")["seq"]
                .as_u64()
                .unwrap()
        })
        .collect()
}

/// `--decisions-only` returns only Decision | ModelCall | ToolCall.
#[test]
fn events_decisions_only_uses_query_helper() {
    let dir = common::workspace();
    let _ = dry_run_json(dir.path(), &[]);
    // Index after a session exists so a graph_rebuild decision is recorded.
    common::alloy_in(dir.path())
        .args(["index"])
        .assert()
        .success();
    let out = common::alloy_in(dir.path())
        .args(["events", "--decisions-only", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        !text.is_empty(),
        "expected at least the graph_rebuild decision"
    );
    for line in text.lines() {
        let doc: serde_json::Value = serde_json::from_str(line).unwrap();
        let t = doc["type"].as_str().unwrap();
        assert!(
            ["decision", "model_call", "tool_call"].contains(&t),
            "unexpected type {t}"
        );
    }
}

/// IX4/IX5 — `alloy index` records the graph_rebuild decision with the
/// report counts once a session exists.
#[test]
fn index_records_graph_rebuild_decision() {
    let dir = common::workspace();
    let _ = dry_run_json(dir.path(), &[]);
    let out = common::alloy_in(dir.path())
        .args(["index", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["report"]["version"].is_number());
    assert!(doc["session"].is_string(), "session attached: {doc}");

    let events = common::alloy_in(dir.path())
        .args(["events", "--decisions-only", "--json"])
        .output()
        .unwrap();
    let text = String::from_utf8(events.stdout).unwrap();
    assert!(
        text.contains("graph_rebuild"),
        "decision event missing: {text}"
    );
}

/// IX8 — `--stats` writes nothing: the graph version is unchanged after.
#[test]
fn index_stats_writes_nothing() {
    let dir = common::workspace();
    let first = index_version(dir.path());
    common::alloy_in(dir.path())
        .args(["index", "--stats"])
        .assert()
        .success();
    let second = index_version(dir.path());
    assert_eq!(first, second, "--stats must not bump the graph version");
}

fn index_version(dir: &std::path::Path) -> u64 {
    let out = common::alloy_in(dir)
        .args(["index", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    doc["report"]["version"].as_u64().unwrap()
}

/// IX7 — an empty graph changes no CLI behaviour: dry-run planning works in
/// a workspace with no cargo manifest at all.
#[test]
fn empty_graph_does_not_change_behaviour() {
    let dir = common::workspace();
    // `--no-index`: no bootstrap; graph stays empty.
    let doc = dry_run_json(dir.path(), &[]);
    assert_eq!(doc["ok"], true);
}

/// SEC4/OUT4 — JSON output contains neither the API key value nor any
/// *KEY*/*TOKEN*/*SECRET*/*PASSWORD* field.
#[test]
fn json_contains_no_secrets() {
    let secret = "sk-super-secret-value-1234567890";
    let dir = common::workspace();
    let out = common::alloy_in(dir.path())
        .env("ALLOY_API_KEY", secret)
        .args(["index", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains(secret));
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_no_secret_keys(&doc);

    let dry = common::alloy_in(dir.path())
        .env("ALLOY_API_KEY", secret)
        .args(["run", "g", "--dry-run", "--json"])
        .output()
        .unwrap();
    let text = String::from_utf8(dry.stdout).unwrap();
    assert!(!text.contains(secret));
}

fn assert_no_secret_keys(v: &serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let upper = k.to_uppercase();
                for bad in ["KEY", "TOKEN", "SECRET", "PASSWORD"] {
                    assert!(
                        !upper.contains(bad),
                        "field name {k:?} looks like a secret carrier"
                    );
                }
                assert_no_secret_keys(v);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(assert_no_secret_keys),
        _ => {}
    }
}
