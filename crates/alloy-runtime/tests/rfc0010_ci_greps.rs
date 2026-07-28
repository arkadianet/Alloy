//! RFC-0010 CI-enforceable source greps (§4.1 rules B6/M5, AC 57, AC 73,
//! AC 83).
//!
//! Mechanised as ordinary `#[test]`s rather than a separate CI job: this
//! crate's tests already run under `cargo test --workspace`
//! (`.github/workflows/ci.yml`'s "Tests (DoD 3, 4)" step), so no new CI
//! config is needed, and a violation is caught locally on `cargo test` too,
//! not just in CI.
//!
//! # RFC-0010 §13 acceptance-criteria sweep — coverage-gap ledger (closed)
//!
//! A manual pass over all 95 ACs (cross-referencing test names, then
//! reading source where names were ambiguous) found ~20 ACs with no
//! dedicated test. Also tracked as RFC-0010 §15 Q8. Numbers are RFC-0010
//! §13 AC numbers.
//!
//! The sweep, and the reviewer rounds that followed it, turned up real
//! implementation bugs as well as test debt. Those were fixed and tested
//! during review: FO6/FN2 attribution (ACs 66/89/92), RF3+RF7 resume
//! wiring (ACs 23/24), B4's resume backoff (AC 26), the
//! `CapabilityExecContext` budget/deadline contract, BE4's pre-CAS
//! ordering on both the retry and budget paths (AC 82), the
//! `pending_cancels` leak, §5.3.2 row 4 gate adoption, the run half of
//! the §5.3.1/§5.7.2/GR4/RF6 scan keys, R15's ER4/ER5 re-verify rules
//! (ACs 84/90/95), the C9b→C3 crash window that silently expired an
//! approved gate, and DS4 stall recovery.
//!
//! ACs 84/90/95 were previously carried as an out-of-scope deferral on the
//! grounds that ER4 needed a `TaskNode.needs_reverify` field the codebase
//! does not have. That reading was wrong: ER4 defines `needs_reverify` as a
//! *derived* predicate over node states and edge reachability, so nothing
//! needed to be added to `TaskNode`. They are covered by `ready.rs`'s
//! `needs_reverify_*` unit tests plus `loop_.rs`'s `er4_*` / `er5_*`
//! end-to-end tests.
//!
//! The remaining test-only debt was then paid down in a dedicated pass;
//! every gap the sweep listed now has a named test:
//!
//! - **AC 12** — `run_without_a_bound_run_row_is_run_binding_missing`.
//! - **AC 13** — `run_with_validate_on_load_rejects_invalid_blob_without_cas`.
//! - **AC 22** — `checkpoint_write_order_is_artifacts_then_cas_then_events`
//!   (`checkpoint.rs`): the RFC's "Recorded store" mechanism, driving
//!   C3/C4/C7/C8/C9c through call-order-tracking store doubles.
//! - **AC 23/24** — RF3/RF7 are wired into R9 via `repair_gate_terminal`;
//!   `adopt_running` has end-to-end tests for both §5.3.2 gate rows.
//!   General non-gate RF3-on-adoption remains unit-tested only
//!   (`repair_node_state_*`).
//! - **AC 26** — `b4_resumed_ready_node_with_prior_attempts_*` plus
//!   `b4_in_loop_retry_does_not_double_wait_*`.
//! - **AC 33** — each crash point: `WaitingApproval` +
//!   durable allow (`gate_resume_with_durable_resolution_never_calls_adapter`),
//!   `Ready` (`resume_with_a_ready_gate_rescans_the_durable_approval`),
//!   `Running` (`adopt_running_gate_with_durable_allow_resumes_the_fold`),
//!   post-C9b (`gate_resume_after_completed_fold_finishes_the_dag_naturally`).
//! - **AC 35** — `gate_resume_waiting_without_resolution_reregisters_without_a_second_request`.
//! - **AC 36** — `gate_resume_with_durable_expired_terminalizes_without_reexpiring`.
//! - **AC 38** — `scheduler_never_records_model_or_tool_calls`.
//! - **AC 39** — the seven `closed_receiver_while_*` tests cover every
//!   §5.7.9 `RunControlState` row, including the `GATE_REREGISTER_MAX`
//!   bound and the Failed-with-contradictory-resolution invariant.
//! - **AC 49** — `owned_guard_drop_releases_ownership_when_the_run_body_panics`.
//! - **AC 62** — `resumed_run_meter_is_rebuilt_not_double_counted`.
//! - **AC 64** — `ds4_stalled_dag_is_terminalized_instead_of_wedged`.
//! - **AC 65** — `attribution_target_follows_the_t8_fallback_chain` plus
//!   `t7_deadline_tie_attributes_to_the_run_not_the_node` /
//!   `t7_node_deadline_inside_run_budget_is_a_retryable_node_timeout`.
//! - **AC 71** — `gate_node_reaches_running_only_after_a_durable_resolution`.
//! - **AC 76** — `rb6_prefers_the_running_row_over_a_newer_non_running_row`
//!   and `rb5_orders_candidates_by_created_at_then_run_id`.
//! - **AC 79** — `scan_gate_resolution_ignores_a_stale_generation_event`.
//! - **AC 80** — `expire_gate_transient_error_is_retried_then_durable` and
//!   `expire_gate_exhausts_retries_and_terminalizes_locally`.
//! - **AC 82** — pre-CAS halves:
//!   `be4_pre_cas_decision_failure_aborts_the_retry_checkpoint` /
//!   `be4_pre_cas_budget_decision_failure_aborts_the_stop_checkpoint`;
//!   post-CAS half:
//!   `be4_post_cas_gate_decision_failure_is_logged_not_aborted`.
//! - **AC 88** — `r4b_reload_short_circuits_on_a_concurrent_unowned_cancel_write`.
//!
//! Unless a path is qualified, the tests above live in `loop_.rs`'s test
//! module. This ledger is only useful if it is true — if a test above is
//! renamed or removed, update this list in the same change.

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
fn ac73_no_artifact_kind_json_in_scheduler_or_adapters() {
    // AC 73: "Structured artifacts use ArtifactKind::Blob + content_type:
    // application/json; verify_raw uses Log; no ArtifactKind::Json appears
    // in scheduler/adapter code."
    let src = crate_root().join("src");
    for sub in ["scheduler", "adapters"] {
        let mut files = Vec::new();
        walk_rs_files(&src.join(sub), &mut files);
        for file in &files {
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for (i, line) in content.lines().enumerate() {
                assert!(
                    !line.contains("ArtifactKind::Json"),
                    "AC73 violation: {}:{} uses ArtifactKind::Json — structured \
                     artifacts MUST use Blob + content_type: application/json\n  {line}",
                    file.display(),
                    i + 1
                );
            }
        }
    }
}

#[test]
fn ac83_scheduler_never_calls_edit_engine() {
    // AC 83: "Scheduler code paths never call EditEngine::{apply,rollback}
    // or recover_checkpoint." ER3 puts RFC-0008's EditEngine outside this
    // RFC's ownership boundary entirely — R15 now implements ER4/ER5, and it
    // does so purely from the DAG blob (node states + edge reachability),
    // never by touching the edit stack. This only needs to stay true, not
    // become true, but it's cheap enough to make it an enforced fact rather
    // than an assumption.
    let mut files = Vec::new();
    walk_rs_files(&crate_root().join("src/scheduler"), &mut files);
    for file in &files {
        let content = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (i, line) in content.lines().enumerate() {
            assert!(
                !line.contains("EditEngine") && !line.contains("recover_checkpoint"),
                "AC83 violation: {}:{} references EditEngine/recover_checkpoint — \
                 the scheduler MUST NOT call into EditEngine directly\n  {line}",
                file.display(),
                i + 1
            );
        }
    }
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
