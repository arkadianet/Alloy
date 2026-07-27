//! RFC-0010 CI-enforceable source greps (§4.1 rules B6/M5, AC 57, AC 73,
//! AC 83).
//!
//! Mechanised as ordinary `#[test]`s rather than a separate CI job: this
//! crate's tests already run under `cargo test --workspace`
//! (`.github/workflows/ci.yml`'s "Tests (DoD 3, 4)" step), so no new CI
//! config is needed, and a violation is caught locally on `cargo test` too,
//! not just in CI.
//!
//! # RFC-0010 §13 acceptance-criteria sweep — known test-coverage gaps
//!
//! A manual pass over all 95 ACs (cross-referencing test names, then
//! reading source where names were ambiguous) found the following with no
//! dedicated test today. None of these are known *implementation* bugs
//! (the one that was — AC 66/89's FO6 violation in
//! `assemble_already_terminal_outcome` — was fixed and given its own
//! `fo1_r9_*`/`fo2_r9_*`/`fo3_r9_*` tests in `scheduler/linear/loop_.rs`
//! immediately, not left on this list). Also tracked as RFC-0010 §15 Q8.
//! Numbers are RFC-0010 §13 AC numbers.
//!
//! - **AC 12**: `run` on a DAG with no bound run row returns
//!   `RunBindingMissing` — the code path exists, no test calls it directly.
//! - **AC 13**: `run` with `validate_on_load` on an invalid blob returns
//!   `Invariant` with no CAS issued — no dedicated test.
//! - **AC 22**: checkpoint write order (artifacts → CAS → events) is
//!   correct by code inspection for every `cN_*` method but untested via a
//!   call-order-tracking store double, unlike the RFC's own suggested
//!   "Recorded store" mechanism.
//! - **AC 23/24**: `repair_node_state` (RF3, general non-gate crash repair)
//!   is unit-tested in isolation but never called from `loop_.rs` — only
//!   `repair_approval_requested` (gate-specific) is wired in, as of P7.
//!   `adopt_running` (R13, crash-mid-`Running` adoption) has no dedicated
//!   end-to-end test exercising `run()` on a pre-seeded `Running` node.
//! - **AC 26**: restart with a `Ready` node + one recorded failed attempt
//!   waiting the full remaining backoff (paused clock) before C3 — backoff
//!   computation and live-run interruption are both tested; this specific
//!   fresh-process restart scenario isn't.
//! - **AC 33**: gate-allow resume is tested from one intermediate crash
//!   point, not "each" per the AC's plural wording.
//! - **AC 35/36**: resume-with-`WaitingApproval`-and-no-resolution
//!   (re-register only, no double `ApprovalRequested`/CAS) and
//!   resume-with-durable-`expired` (terminalizes, `expire_gate` not called
//!   again) both lack dedicated tests.
//! - **AC 38**: the scheduler never emits `ModelCall`/`ToolCall` (true by
//!   construction — nothing in `scheduler::` calls anything that would)
//!   and cost-sum-no-double-counting have no regression test.
//! - **AC 39**: `gate.rs`'s closed-receiver `RunControlState` classification
//!   (§5.7.9) has zero dedicated tests — `gate.rs`'s own test module only
//!   covers its pure helpers (`parse_gate_resolution` etc.), not this state
//!   machine.
//! - **AC 49**: `OwnedGuard::drop` releasing ownership when the run body
//!   itself panics (not just cancels/errors) has no dedicated test.
//! - **AC 62**: `reaccumulate_cost_from_events` has its own RFC-0004 test;
//!   the scheduler-level "a resumed run's meter total doesn't double" isn't
//!   independently tested at this layer.
//! - **AC 64**: stall detection (DS4: unsatisfiable Data predecessor forces
//!   a bulk-`Skipped` re-derive) has no dedicated test.
//! - **AC 65** (partial): the general run-timeout path is tested; T7's
//!   node-vs-run tie-break and T8's no-`Running`-node attribution fallback
//!   chain aren't isolated in their own tests.
//! - **AC 71**: `GateHuman` never reaching C3 while unresolved is true by
//!   construction (`gate.rs` routes before `dispatch_node`) but untested as
//!   a regression.
//! - **AC 76**: Appendix F's multi-run-row tie-break (RB6 `Running`
//!   preference, then RB5 `created_at`/`run_id` ordering) has no test with
//!   more than one candidate row.
//! - **AC 79**: a stale-generation `ApprovalResolved` being ignored by
//!   `scan_gate_resolution`'s generation filter has no dedicated test.
//! - **AC 80**: `expire_gate`'s `Err(other)` retry-up-to-`EXPIRE_RETRY_MAX`
//!   loop is only exercised via its happy path (`gate_expiry_terminalizes_*`
//!   in `loop_.rs`); the retry-then-exhaust behavior isn't.
//! - **AC 82**: BE4 (`ObsError`/`DecisionLog` failure logged-not-aborted
//!   after a committed CAS, mapped to `Store` before one) has no dedicated
//!   scheduler-level test.
//! - **AC 88**: R4b's re-load observing a concurrent unowned-cancel's
//!   terminal write (short-circuiting at R9 instead of overwriting) has no
//!   dedicated race test.
//!
//! None of these block AC 57/73/83 (this file) or the sweep's own
//! conclusion that P1-P9 collectively deliver the RFC's normative behavior;
//! they're coverage debt, prioritized here for whoever picks this up next.

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
    // or recover_checkpoint." RFC-0008's EditEngine lives outside this
    // RFC's ownership boundary entirely (R15's edit-tx resume is a no-op —
    // see loop_.rs's module doc); this only needs to stay true, not become
    // true, but it's cheap enough to make that an enforced fact rather than
    // an assumption.
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
