//! RFC-0015 §12.3 — the boundary greps T1–T10, enforced as tests over
//! `crates/alloy-cli/src/` (rule B8: CI-greppable, not review vigilance).
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};

fn src_files() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    collect(&root, &mut out);
    assert!(!out.is_empty());
    out
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let body = std::fs::read_to_string(&path).unwrap();
            out.push((path, body));
        }
    }
}

/// Strip `#[cfg(test)] mod … { … }` blocks (tests are exempt from T1).
fn without_test_modules(body: &str) -> String {
    let mut out = String::new();
    let mut skip_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut pending_test_attr = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if skip_depth.is_none() {
            if trimmed.starts_with("#[cfg(test)]") {
                pending_test_attr = true;
                continue;
            }
            if pending_test_attr {
                pending_test_attr = false;
                if trimmed.starts_with("mod ") {
                    skip_depth = Some(depth);
                    depth += line.matches('{').count();
                    depth = depth.saturating_sub(line.matches('}').count());
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
        if let Some(d) = skip_depth {
            if depth <= d {
                skip_depth = None;
            }
        }
    }
    out
}

fn assert_absent(needle: &str, rule: &str) {
    for (path, body) in src_files() {
        let body = without_test_modules(&body);
        assert!(
            !body.contains(needle),
            "{rule}: {needle:?} found in {}",
            path.display()
        );
    }
}

/// T1 (B7) — no process spawning outside the broker. `process::exit` is the
/// only `std::process` use allowed.
#[test]
fn t1_no_process_command() {
    assert_absent("std::process::Command", "T1");
    assert_absent("process::Command::new", "T1");
    assert_absent("tokio::process", "T1");
}

/// T2 (B2) — no scheduler-internal path imports.
#[test]
fn t2_no_scheduler_internal_imports() {
    assert_absent("scheduler::linear::", "T2");
}

/// T3 (B4) — no PermissionToken / Grant construction.
#[test]
fn t3_no_permission_token_construction() {
    assert_absent("PermissionToken {", "T3");
    assert_absent("Grant::", "T3");
}

/// T4 (B6) — no direct database access.
#[test]
fn t4_no_direct_sqlite() {
    assert_absent("rusqlite", "T4");
    assert_absent("sqlx", "T4");
    assert_absent(".sqlite", "T4");
}

/// T5 (B5) — no EditEngine::apply/rollback, no graph ingest writes.
#[test]
fn t5_no_edit_or_graph_writes() {
    assert_absent("EditEngine::apply", "T5");
    assert_absent("EditEngine::rollback", "T5");
    assert_absent(".rollback(", "T5");
    assert_absent("record_diagnostic", "T5");
    assert_absent("record_fix", "T5");
    assert_absent("apply_incremental", "T5");
}

/// T6 (SEC1) — no dotenv literal and no write whose path names one.
#[test]
fn t6_no_dotenv_literal() {
    let dotenv_literal = format!("{:?}", ".env"); // assembled to keep this file honest
    for (path, body) in src_files() {
        assert!(
            !body.contains(&dotenv_literal),
            "T6: dotenv literal in {}",
            path.display()
        );
    }
}

/// T7 (B3) — no model id, provider URL, or price literal.
#[test]
fn t7_no_model_or_price_literals() {
    assert_absent("https:", "T7");
    assert_absent("usd_per_mtok", "T7");
    assert_absent("api.openai", "T7");
    assert_absent("claude-", "T7");
    assert_absent("gpt-", "T7");
}

/// T8 (B1) — no retry machinery identifiers.
#[test]
fn t8_no_retry_identifiers() {
    for needle in ["fn retry", "max_attempts", "backoff"] {
        assert_absent(needle, "T8");
    }
}

/// T9 (B1/PF5) — the dependency list stays inside the allow-list; no HTTP
/// client, no TOML parser of the CLI's own.
#[test]
fn t9_dependency_allow_list() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let deps_section = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("[dev-dependencies]")
        .next()
        .unwrap();
    let allowed = [
        "alloy-runtime",
        "alloy-tools",
        "alloy-index",
        "clap",
        "tokio",
        "tracing",
        "serde_json",
    ];
    for line in deps_section.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let name = line.split(['=', ' ']).next().unwrap();
        assert!(
            allowed.contains(&name),
            "T9: dependency {name:?} is outside the RFC-0015 allow-list"
        );
    }
    for banned in ["reqwest", "toml =", "toml.workspace", "hyper", "ureq"] {
        assert!(
            !deps_section.contains(banned),
            "T9: banned dependency {banned:?}"
        );
    }
}

/// T10 — no unsafe (the crate root carries `#![forbid(unsafe_code)]`).
#[test]
fn t10_no_unsafe() {
    assert_absent("unsafe ", "T10");
    let (_, main) = src_files()
        .into_iter()
        .find(|(p, _)| p.ends_with("main.rs"))
        .unwrap();
    assert!(main.contains("#![forbid(unsafe_code)]"));
}
