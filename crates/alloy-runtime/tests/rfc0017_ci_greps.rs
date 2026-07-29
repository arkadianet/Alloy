//! RFC-0017 CI-enforceable source greps (Stage 1: the AC 31 scheduler
//! isolation rule).
//!
//! Mechanised as ordinary `#[test]`s over source text, matching the
//! `rfc0010_ci_greps.rs` / `rfc0013_ci_greps.rs` convention. Full-line
//! comments are stripped before matching so a rule may be *documented*
//! without being violated.
//!
//! Author: arkadianet

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

fn scheduler_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scheduler");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files under src/scheduler — the walk is broken, not the rule"
    );
    files
}

fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// AC 31: the repair-generation bound lives on `RuntimeConfig`, never on
/// `SchedConfig` — `max_repair_generations` appears nowhere under
/// `src/scheduler/`, comments included on non-comment lines.
#[test]
fn ac31_scheduler_never_names_max_repair_generations() {
    for file in scheduler_files() {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (idx, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            assert!(
                !line.contains("max_repair_generations"),
                "AC 31 violated: {}:{} names max_repair_generations",
                file.display(),
                idx + 1
            );
        }
    }
}
