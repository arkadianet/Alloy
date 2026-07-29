//! RFC-0013 §15.3 CI-enforceable source greps over `src/capabilities/**`.
//!
//! Mechanised as ordinary `#[test]`s over source text, matching RFC-0010's
//! and RFC-0011's convention (`rfc0010_ci_greps.rs`): they run under the
//! existing `cargo test --workspace` job, so no CI config changes.
//!
//! Full-line comments are stripped before matching so a rule may be
//! *documented* (naming the forbidden identifier in prose) without being
//! violated — the same allowance RFC-0010's M5 grep makes for its two
//! doc-comment files.
//!
//! Deviation record (see `capabilities/perms.rs` module docs): the merged
//! RFC-0008 §3.8.4 host authorization requires `GitWrite` for a mutating
//! `apply_patch`, so T11 pins `GitWrite` to `perms.rs` alone instead of
//! banning it outright; `Exec` and `Network` grants stay banned everywhere.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};

use alloy_runtime::{CAPABILITY_CATALOG, MAX_LLM_CAPABILITIES};

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read_dir entry: {e}"))
            .path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn capabilities_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/capabilities");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files under src/capabilities — the walk is broken, not the rule"
    );
    files
}

fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// Assert `needle` (plain substring) never appears on a non-comment line of
/// `file` unless `file` ends with one of `allowed_files`.
fn forbid_substring(rule: &str, needle: &str, allowed_files: &[&str]) {
    for file in capabilities_files() {
        let name = file.to_string_lossy().replace('\\', "/");
        if allowed_files.iter().any(|a| name.ends_with(a)) {
            continue;
        }
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (i, line) in content.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            assert!(
                !line.contains(needle),
                "{rule} violation: {}:{} contains `{needle}`\n  {line}",
                file.display(),
                i + 1
            );
        }
    }
}

/// Whole-word, case-insensitive identifier match (so `delegates` does not
/// trip a `gate` rule).
fn contains_word_ci(line: &str, word: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let word = word.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(pos) = lower[start..].find(&word) {
        let begin = start + pos;
        let end = begin + word.len();
        let left_ok = begin == 0 || {
            let c = bytes[begin - 1];
            !(c.is_ascii_alphanumeric() || c == b'_')
        };
        let right_ok = end == lower.len() || {
            let c = bytes[end];
            !(c.is_ascii_alphanumeric() || c == b'_')
        };
        if left_ok && right_ok {
            return true;
        }
        start = end;
    }
    false
}

fn forbid_word(rule: &str, word: &str) {
    for file in capabilities_files() {
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (i, line) in content.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            assert!(
                !contains_word_ci(line, word),
                "{rule} violation: {}:{} names `{word}`\n  {line}",
                file.display(),
                i + 1
            );
        }
    }
}

#[test]
fn rg1_at_most_four_capabilities_in_catalog() {
    // T2 / RG1 / RG2 / SEC6.
    assert_eq!(MAX_LLM_CAPABILITIES, 4);
    assert_eq!(CAPABILITY_CATALOG.len(), MAX_LLM_CAPABILITIES);
    assert_eq!(CAPABILITY_CATALOG, ["planning", "repair", "edit", "review"]);
}

#[test]
fn sec4_no_topology_fields_in_capabilities() {
    // T3 / SEC4 / OC6.
    for needle in [
        "follow_up_nodes",
        "graph_mutations",
        "next_nodes",
        "nodes_to_add",
    ] {
        forbid_substring("SEC4", needle, &[]);
    }
}

#[test]
fn sec3_no_graph_mutation_identifier() {
    // T4 / SEC3 (extends RFC-0011's workspace grep into this module).
    forbid_substring("SEC3", "GraphMutation", &[]);
    forbid_substring("SEC3", "ProjectGraph", &[]);
}

#[test]
fn pr1_prompt_pack_literals_only_in_prompt_rs() {
    // T6 / PR1: assembly stays with the context engine; the only permitted
    // message construction lives in prompt.rs.
    forbid_substring("PR1", "PromptPack {", &["capabilities/prompt.rs"]);
    forbid_substring("PR1", "PromptPack{", &["capabilities/prompt.rs"]);
    forbid_substring("PR1", "ChatMessage {", &["capabilities/prompt.rs"]);
    forbid_substring("PR1", "ChatMessage{", &["capabilities/prompt.rs"]);
}

#[test]
fn ew1_no_edit_engine_in_capabilities() {
    // T7 / EW1 / SEC2: the patch builtin is the only mutation path.
    forbid_substring("EW1", "EditEngine", &[]);
    forbid_substring("EW1", ".rollback(", &[]);
    forbid_substring("EW1", "recover_checkpoint", &[]);
}

#[test]
fn pw2_no_plan_service_in_capabilities() {
    // T8 / PW2 / AM-0009-1: topology has exactly one writer.
    forbid_substring("PW2", "PlanService", &[]);
}

#[test]
fn bg2_no_meter_writes_in_capabilities() {
    // T9 / BG2 / AM-0007-1: metering happens once, inside RFC-0007.
    forbid_substring("BG2", "add_model_usage", &[]);
    forbid_substring("BG2", "add_worker_metrics", &[]);
}

#[test]
fn pm2_no_permission_token_literals_outside_perms() {
    // T10 / PM2.
    forbid_substring("PM2", "PermissionToken {", &["capabilities/perms.rs"]);
    forbid_substring("PM2", "PermissionToken{", &["capabilities/perms.rs"]);
}

#[test]
fn sec8_no_exec_network_gitwrite_grants() {
    // T11 / PM3 / SEC8. `GitWrite` and `Grant::Exec` are pinned to perms.rs
    // (see its module docs for the RFC-0008 deviation record: the merged
    // host authz + git checkpoint require them on a mutating apply_patch,
    // and the exec grant there is git-only, unit-tested). `Network` grants
    // stay banned everywhere.
    forbid_substring("SEC8", "Grant::Exec", &["capabilities/perms.rs"]);
    forbid_substring("SEC8", "Grant::Network", &[]);
    forbid_substring("SEC8", "GitWrite", &["capabilities/perms.rs"]);
}

#[test]
fn sec1_no_verify_capability_names() {
    // T13 / SEC1 / TL7: no verification, testing, gating, or compilation
    // identifier under capabilities/**.
    for word in ["verify", "gate", "cargo_check", "cargo_test", "compile"] {
        forbid_word("SEC1", word);
    }
}

#[test]
fn ob1_no_model_or_tool_call_records_in_capabilities() {
    // T15 / OB1: RFC-0007 and RFC-0006 own those records.
    forbid_substring("OB1", "record_model_call", &[]);
    forbid_substring("OB1", "record_tool_call", &[]);
}

#[test]
fn sec2_no_direct_io_in_capabilities() {
    // T16 / SEC2 / CW6: all I/O is ToolCaller / router / context engine /
    // artifact store / graph handle.
    for needle in [
        "std::fs",
        "std::process",
        "tokio::fs",
        "tokio::process",
        "std::env",
    ] {
        forbid_substring("SEC2", needle, &[]);
    }
    forbid_word("SEC2", "Command");
}

#[test]
fn sec3_no_graph_write_methods_in_capabilities() {
    // T17 / SEC3.
    for word in [
        "apply_incremental",
        "rebuild",
        "record_diagnostic",
        "record_fix",
        "ingest",
    ] {
        forbid_word("SEC3", word);
    }
}

#[test]
fn sec5_no_graph_query_or_bash_selectors() {
    // T18 / SEC5.
    forbid_substring("SEC5", "graph_query", &[]);
    forbid_word("SEC5", "bash");
    forbid_word("SEC5", "sh"); // no shell-name selector either.
}

#[test]
fn no_todo_or_unimplemented_in_capabilities() {
    // §1.5(14): comments included — a TODO is a placeholder wherever it
    // lives.
    for file in capabilities_files() {
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (i, line) in content.lines().enumerate() {
            for needle in ["todo!", "unimplemented!", "TODO", "FIXME"] {
                assert!(
                    !line.contains(needle),
                    "placeholder violation: {}:{} contains `{needle}`\n  {line}",
                    file.display(),
                    i + 1
                );
            }
        }
    }
}
