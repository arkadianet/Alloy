//! RFC-0011 CI-enforceable source greps (§13.8, T7–T14 plus AC 11).
//!
//! Mechanised as ordinary `#[test]`s: this crate's tests already run under
//! `cargo test --workspace`, so a violation is caught locally and in CI with
//! no new CI config — the same shape RFC-0010's grep suite uses.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // crates/alloy-index -> workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn rs_files(rel: &str) -> Vec<PathBuf> {
    let dir = workspace_root().join(rel);
    let mut out = Vec::new();
    walk_rs_files(&dir, &mut out);
    assert!(!out.is_empty(), "grep walk found zero files under {rel}");
    out
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// T7 / SEC2: no worker-facing `graph_query` MCP tool. In `alloy-tools` the
// string may appear only in comments, `fn no_*` negative-assertion tests,
// explicit deny-list lines, or `#[cfg(test)]` code — never as a production
// registration. A bare string literal is deliberately NOT exempt: a real
// `register("graph_query")` must trip this test.
#[test]
fn sec2_no_graph_query_tool_outside_negative_assertions() {
    for file in rs_files("crates/alloy-tools/src") {
        let text = read(&file);
        // Everything after the first `#[cfg(test)]` marker is test code.
        let test_start_line = text
            .lines()
            .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
            .unwrap_or(usize::MAX);
        for (i, line) in text.lines().enumerate() {
            if !line.contains("graph_query") {
                continue;
            }
            let trimmed = line.trim_start();
            let negative = trimmed.starts_with("//")
                || trimmed.starts_with("//!")
                || trimmed.starts_with("fn no_") // negative-assertion test fns
                || line.contains("No `graph_query`")
                || line.contains("forbidden")
                || i >= test_start_line;
            assert!(
                negative,
                "{}:{}: graph_query outside a negative assertion: {line}",
                file.display(),
                i + 1
            );
        }
    }
    // And it must never appear in alloy-index at all.
    for file in rs_files("crates/alloy-index/src") {
        assert!(
            !read(&file).contains("graph_query"),
            "{}: graph_query must not exist in alloy-index",
            file.display()
        );
    }
}

// T8 / SEC3: the identifier `GraphMutation` does not exist in the workspace.
#[test]
fn sec3_no_graph_mutation_type_anywhere() {
    for rel in [
        "crates/alloy-runtime/src",
        "crates/alloy-tools/src",
        "crates/alloy-index/src",
        "crates/alloy-cli/src",
        "crates/alloy-eval/src",
    ] {
        for file in rs_files(rel) {
            assert!(
                !read(&file).contains("GraphMutation"),
                "{}: GraphMutation is forbidden (SEC3)",
                file.display()
            );
        }
    }
}

// T9 / C2: alloy-runtime does not depend on alloy-index.
#[test]
fn c2_alloy_runtime_does_not_depend_on_alloy_index() {
    let manifest = read(&workspace_root().join("crates/alloy-runtime/Cargo.toml"));
    assert!(
        !manifest.contains("alloy-index"),
        "alloy-runtime must not depend on alloy-index (C2)"
    );
}

// T10 / C4: the seam module has no SQL and no rusqlite.
#[test]
fn c4_graph_seam_has_no_sql_or_rusqlite() {
    for file in rs_files("crates/alloy-runtime/src/graph") {
        let text = read(&file);
        for needle in ["rusqlite", "CREATE TABLE", "SELECT "] {
            assert!(
                !text.contains(needle),
                "{}: seam contains {needle:?} (C4)",
                file.display()
            );
        }
    }
}

// T11 / SEC1: the worker handle exposes no write surface.
#[test]
fn sec1_graph_view_handle_exposes_no_write_method() {
    let text = read(&workspace_root().join("crates/alloy-runtime/src/graph/handle.rs"));
    for needle in [
        "fn rebuild",
        "fn record_",
        "fn apply_",
        "fn snapshot",
        "fn inner",
    ] {
        assert!(
            !text.contains(needle),
            "GraphViewHandle must not expose {needle:?} (SEC1)"
        );
    }
}

// T12 / SEC5: no network or exec in alloy-index.
#[test]
fn sec5_alloy_index_has_no_network_or_exec() {
    let manifest = read(&workspace_root().join("crates/alloy-index/Cargo.toml"));
    for dep in ["reqwest", "rustls", "landlock", "rustix", "libc"] {
        assert!(
            !manifest.contains(dep),
            "alloy-index must not depend on {dep} (SEC5)"
        );
    }
    for file in rs_files("crates/alloy-index/src") {
        assert!(
            !read(&file).contains("std::process::Command"),
            "{}: process execution is forbidden in alloy-index (SEC5)",
            file.display()
        );
    }
}

// T13 / SEC8: alloy-index never writes .env.
#[test]
fn sec8_alloy_index_never_writes_dot_env() {
    for file in rs_files("crates/alloy-index/src") {
        assert!(
            !read(&file).contains("\".env\""),
            "{}: .env literal is forbidden in alloy-index (SEC8)",
            file.display()
        );
    }
}

// T14 / IN9, as amended by RFC-0014 SY3: `Item` nodes are constructed by
// the `syn` deep pass and nowhere else — outside `src/lang/`,
// `GraphNodeKind::Item` still appears only in seam-mapping match/rank code.
// (Item ids stay derived: the G3 grep below covers the whole crate.)
#[test]
fn in9_item_node_construction_only_in_the_lang_pass() {
    for file in rs_files("crates/alloy-index/src") {
        if file.components().any(|c| c.as_os_str() == "lang") {
            continue; // RFC-0014's syn pass — the one legal producer (SY3).
        }
        let text = read(&file);
        for (i, line) in text.lines().enumerate() {
            if !line.contains("GraphNodeKind::Item") {
                continue;
            }
            let mapping = line.contains("=>") || line.trim_start().starts_with("//");
            assert!(
                mapping,
                "{}:{}: GraphNodeKind::Item outside a mapping arm: {line}",
                file.display(),
                i + 1
            );
        }
    }
}

// AC 11 / G3: node ids are always derived — `GraphNodeId::new()` appears
// nowhere in alloy-index.
#[test]
fn g3_no_random_graph_node_ids_in_alloy_index() {
    for file in rs_files("crates/alloy-index/src") {
        assert!(
            !read(&file).contains("GraphNodeId::new()"),
            "{}: GraphNodeId::new() is forbidden; use derive_node_id (G3)",
            file.display()
        );
    }
}

// AC 59 / OB1: alloy-index appends no session events and records no
// decisions.
#[test]
fn ob1_alloy_index_emits_no_session_events_or_decisions() {
    for file in rs_files("crates/alloy-index/src") {
        let text = read(&file);
        for needle in [
            "EventSink",
            "DecisionLog",
            "DecisionRecord",
            "append_session",
        ] {
            assert!(
                !text.contains(needle),
                "{}: {needle} is forbidden in alloy-index (OB1)",
                file.display()
            );
        }
    }
}
