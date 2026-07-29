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

fn crate_src_files(rel: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    let mut files = Vec::new();
    if dir.exists() {
        walk_rs_files(&dir, &mut files);
    }
    files
}

/// AC 46 (PS1): `DagStore::{put, put_if_generation, replace_for_replan}` is
/// called from `planner/persist.rs` only — no other planner (or driver)
/// module names a DAG write. Test modules are excluded: fixtures may arrange
/// store states directly; PS1 constrains production plan writes.
#[test]
fn ac46_plan_writes_only_through_plan_persistence() {
    let mut files = crate_src_files("src/planner");
    files.extend(crate_src_files("src/driver"));
    assert!(!files.is_empty(), "no planner sources found — walk broken");
    let write_calls = [".put_if_generation(", ".replace_for_replan(", "dags.put("];
    for file in files {
        if file.ends_with("persist.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let mut in_tests = false;
        for (idx, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("mod tests") {
                in_tests = true;
            }
            if in_tests || is_comment_line(line) {
                continue;
            }
            for needle in write_calls {
                assert!(
                    !line.contains(needle),
                    "AC 46 violated: {}:{} calls {needle} outside planner/persist.rs",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 48 (RX2): the driver never emits a lifecycle event, never writes a
/// run row, never calls `request_replan`, and never re-enters
/// `RunController::start`. Test modules are excluded (fixtures drive runs
/// through `start` on purpose); RX2 constrains the production driver.
#[test]
fn ac48_driver_never_touches_run_lifecycle() {
    let files = crate_src_files("src/driver");
    assert!(!files.is_empty(), "no driver sources found — walk broken");
    let needles = [
        "RunAccepted",
        "RunCompleted",
        "RunFinished",
        "upsert_state",
        "request_replan",
        ".start(",
    ];
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let mut in_tests = false;
        for (idx, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("mod tests") {
                in_tests = true;
            }
            if in_tests || is_comment_line(line) {
                continue;
            }
            for needle in needles {
                assert!(
                    !line.contains(needle),
                    "AC 48 violated: {}:{} references {needle}",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 34: every shipped profile keeps `mode = "template"` — the LLM planner
/// is opt-in and eval-gated (RFC-0017 §12.4); no catalog profile enables it.
#[test]
fn ac34_shipped_profiles_stay_template_mode() {
    let profiles = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let entries = std::fs::read_dir(&profiles)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", profiles.display()));
    let mut seen = 0usize;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for (idx, line) in text.lines().enumerate() {
            // TOML comments (`#`) may *document* the rule without violating it.
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.replace(' ', "").contains("mode=\"llm\""),
                "AC 34 violated: {}:{} sets planner mode llm",
                path.display(),
                idx + 1
            );
        }
        assert!(
            text.replace(' ', "").contains("mode=\"template\""),
            "AC 34: {} does not pin [planner] mode = \"template\"",
            path.display()
        );
    }
    assert_eq!(seen, 3, "expected the three shipped profiles");
}

/// AC 14b (grep half): nothing under `src/planner` reads the process CWD —
/// the proposer's workspace root comes from `ProposerDeps` (PP1).
#[test]
fn ac14b_planner_never_reads_current_dir() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/planner");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (idx, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            assert!(
                !line.contains("current_dir"),
                "AC 14b violated: {}:{} reads the process CWD",
                file.display(),
                idx + 1
            );
        }
    }
}

/// AC 41: `capabilities/**` imports no plan service or driver symbol
/// (PW2 / T8 extended) — a worker holding a `PlanService` could write
/// topology from inside a node.
#[test]
fn ac41_capabilities_import_no_plan_service_or_driver() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/capabilities");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(!files.is_empty());
    let needles = ["PlanService", "LlmPlanService", "GenerationDriver"];
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (idx, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            for needle in needles {
                assert!(
                    !line.contains(needle),
                    "AC 41 violated: {}:{} references {needle}",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 38 (grep half): `rationale` from a proposal is audit-only — no prompt
/// assembly code under `capabilities/prompt.rs` or `context/` interpolates a
/// proposal rationale into model-facing content.
#[test]
fn ac38_rationale_never_enters_downstream_prompts() {
    for sub in ["src/capabilities/prompt.rs", "src/context"] {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub);
        let mut files = Vec::new();
        if root.is_dir() {
            walk_rs_files(&root, &mut files);
        } else {
            files.push(root);
        }
        for file in files {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for (idx, line) in text.lines().enumerate() {
                if is_comment_line(line) {
                    continue;
                }
                assert!(
                    !line.contains(".rationale"),
                    "AC 38 violated: {}:{} touches a proposal rationale",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 40 (B6 extended): `scheduler/**` imports neither `planner::` nor
/// `driver::` — the scheduler stays a single-generation executor (RP4); the
/// generation loop lives in `alloy_runtime::driver`.
#[test]
fn ac40_scheduler_imports_no_planner_or_driver() {
    for file in scheduler_files() {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (idx, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            for needle in ["planner::", "driver::"] {
                assert!(
                    !line.contains(needle),
                    "AC 40 violated: {}:{} references {needle}",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 42 (MG4 / B1 / SQ2): `alloy-cli` contains no run-retry machinery — no
/// `max_retries` / `max-retries` symbol — and no execution entry point
/// besides `RunController::start`: no `Scheduler::run`, `run_dag`, or
/// `run_within` call. The interim issue-#53 CLI retry loop is gone; the
/// in-run generation loop (RFC-0017 §5.5) replaced it.
#[test]
fn ac42_cli_has_no_retry_loop_and_no_scheduler_entry() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../alloy-cli/src");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(
        !files.is_empty(),
        "no alloy-cli sources found — walk broken"
    );
    let needles = [
        "max_retries",
        "max-retries",
        "Scheduler::run",
        "run_dag",
        "run_within",
    ];
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (idx, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            for needle in needles {
                assert!(
                    !line.contains(needle),
                    "AC 42 violated: {}:{} references {needle}",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// AC 43: no `.env` file reference in RFC-0017's new modules (Alloy MUST
/// NEVER write `.env`), and the five-crate map is unchanged — no sixth
/// crate appeared.
#[test]
fn ac43_no_dotenv_writes_and_no_sixth_crate() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in [
        "src/planner",
        "src/driver",
        "src/dag/proposal.rs",
        "src/session/run_executor.rs",
    ] {
        let root = manifest.join(rel);
        let mut files = Vec::new();
        if root.is_dir() {
            walk_rs_files(&root, &mut files);
        } else {
            files.push(root);
        }
        for file in files {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for (idx, line) in text.lines().enumerate() {
                assert!(
                    !line.contains("\".env"),
                    "AC 43 violated: {}:{} names a .env file",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
    let crates_dir = manifest.join("..");
    let crate_count = std::fs::read_dir(&crates_dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(
        crate_count, 5,
        "the five-crate map is frozen (no sixth crate)"
    );
}
