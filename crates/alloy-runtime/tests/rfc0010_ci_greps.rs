//! RFC-0010 CI-enforceable source greps (§4.1 rules B6/M5, AC 57).
//!
//! Mechanised as ordinary `#[test]`s rather than a separate CI job: this
//! crate's tests already run under `cargo test --workspace`
//! (`.github/workflows/ci.yml`'s "Tests (DoD 3, 4)" step), so no new CI
//! config is needed, and a violation is caught locally on `cargo test` too,
//! not just in CI.

use std::path::{Path, PathBuf};

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

/// `crates/alloy-runtime` — this integration test crate's own manifest dir.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn b6_scheduler_and_adapters_never_import_planner() {
    // RFC-0010 §4.1 B6 / AC 57: "The scheduler MUST NOT import `planner::*`."
    // Checked over `adapters/` too, matching where this repo's own module
    // doc (scheduler/linear/mod.rs) already asserts the same boundary.
    let src = crate_root().join("src");
    let mut checked_any = false;
    for sub in ["scheduler", "adapters"] {
        let dir = src.join(sub);
        let mut files = Vec::new();
        walk_rs_files(&dir, &mut files);
        assert!(
            !files.is_empty(),
            "expected .rs files under src/{sub} — the walk itself is broken \
             if this fires, not the rule"
        );
        for file in &files {
            checked_any = true;
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for (i, line) in content.lines().enumerate() {
                assert!(
                    !line.contains("planner::"),
                    "B6 violation: {}:{} imports `planner::` — scheduler/adapters \
                     MUST NOT depend on the planner\n  {line}",
                    file.display(),
                    i + 1
                );
            }
        }
    }
    assert!(checked_any, "grep walk found zero files — test is broken");
}

#[test]
fn m5_no_mcp_platform_type_names_outside_the_rule_doc_comments() {
    // RFC-0010 §4.1 M5: "No module in `alloy-runtime` may name `ToolHandle`,
    // `McpError`, `McpPlatform`, or `SandboxError`." Two files document the
    // rule by naming the forbidden types inside a `//!`/`///` comment
    // explaining *why* they're forbidden (`adapters/tool_caller.rs`,
    // `adapters/verify.rs`) — that is the only permitted exception, and only
    // on doc-comment lines in exactly those two files.
    let forbidden = ["ToolHandle", "McpError", "McpPlatform", "SandboxError"];
    let allowed_doc_files = ["src/adapters/tool_caller.rs", "src/adapters/verify.rs"];

    let root = crate_root();
    let mut files = Vec::new();
    walk_rs_files(&root.join("src"), &mut files);
    assert!(
        !files.is_empty(),
        "expected .rs files under src/ — walk is broken"
    );

    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let content = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (i, line) in content.lines().enumerate() {
            let Some(name) = forbidden.iter().find(|name| line.contains(**name)) else {
                continue;
            };
            let trimmed = line.trim_start();
            let is_doc_line = trimmed.starts_with("//!") || trimmed.starts_with("///");
            let is_allowed_file = allowed_doc_files.contains(&rel.as_str());
            assert!(
                is_doc_line && is_allowed_file,
                "M5 violation: {rel}:{} names `{name}` outside an allowed rule-doc \
                 comment (only {allowed_doc_files:?}, and only //! / /// lines, may \
                 mention these names)\n  {line}",
                i + 1
            );
        }
    }
}
