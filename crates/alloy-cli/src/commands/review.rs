//! `alloy review` — review a unified diff with the `review` capability and
//! render its findings.
//!
//! The diff is supplied by the caller (`--diff <PATH>` or `--diff -` for
//! stdin) because the CLI spawns no process, not even `git` (RFC-0015 rule
//! B7 / boundary grep T1). Everything else follows §7.1's shape: assemble,
//! create a session, submit the goal, let the plan service instantiate the
//! `review_diff` template, dispatch through the scheduler, render.
//!
//! Author: arkadianet

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    truncation_marker, ArtifactKind, ArtifactPut, ArtifactStore, CreateSession, Goal, LanguageId,
    PlanContext, PlanService, ProfileId, ReviewPayload, ReviewSeverity, ReviewVerdict, RunId,
    SessionId, SessionRows, TemplateId,
};
use serde_json::json;

use crate::args::ReviewArgs;
use crate::assembly::{self, AssemblyOptions, FullAssembly};
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

/// Largest diff handed to the model. A larger diff is cut here, and the cut
/// is declared in three places that must agree: the prompt (an `[alloy:
/// truncated …]` marker inside the fenced diff), the human output, and the
/// `--json` envelope.
const MAX_DIFF_BYTES: usize = 128 * 1024;

/// The goal text: a short human description of the task, and nothing more.
///
/// The diff itself never appears here. Goal text is sanitised on its way
/// through the context engine (`sanitize_untrusted`: per-line `trim_end`,
/// fence-marker stripping), which is right for prose and fatal for a
/// whitespace-sensitive patch — it strips the leading space off blank
/// context lines and rewrites `>>>>>>>` conflict markers. The diff rides
/// out of band instead, as a goal attachment in the artifact CAS that the
/// `review` worker fences verbatim.
const GOAL_TEXT: &str = "Review the attached unified diff. Report findings against the files \
and lines it touches.";

pub async fn exec(ctx: Ctx, args: ReviewArgs) -> Result<Exit, CliError> {
    let diff = read_diff(&args.diff)?;

    let base = assembly::assemble_read(ctx.cfg.clone()).await?;
    // The one template this command plans is read-only, so its DAG carries
    // no `GateHuman` for the scheduler's load-time V11 check to find.
    let mut full = assembly::assemble_full_with(
        base,
        &ctx.workspace_abs,
        ctx.readonly(),
        &ctx.profile,
        AssemblyOptions {
            require_gates: false,
        },
    )
    .await?;

    // CR14 — arm signals before start.
    let cancel = full.base.handle.cancellation();
    let signal_task = crate::arm_signal_task(move || cancel.cancel());

    let result = review_after_assembly(&mut full, &ctx, &args, &diff).await;

    signal_task.abort();
    let FullAssembly { base, graph, .. } = full;
    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, Some(&graph), Duration::from_secs(10)).await?;
    result
}

/// Read the diff from a file or stdin (`-`). Empty input is a usage error:
/// there is nothing to review and no reason to pay for a model call.
fn read_diff(path: &Path) -> Result<String, CliError> {
    let raw = if path.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliError::new(Exit::Usage, format!("--diff -: cannot read stdin: {e}")))?;
        buf
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| CliError::new(Exit::Usage, format!("--diff {}: {e}", path.display())))?
    };
    if raw.trim().is_empty() {
        return Err(CliError::new(
            Exit::Usage,
            format!(
                "--diff {} is empty; pipe a diff instead: git diff | alloy review --diff -",
                path.display()
            ),
        ));
    }
    Ok(raw)
}

/// The diff as the model will see it, plus what the caller must be told
/// about the cut.
struct BoundedDiff {
    /// Body sent to the model: the diff, then the truncation marker when it
    /// was cut. The marker is *inside* the fenced content so the reviewer
    /// cannot mistake a fragment for the whole change.
    body: String,
    /// Bytes of the original diff that survived.
    kept_bytes: usize,
    /// Bytes of the diff as supplied.
    total_bytes: usize,
}

impl BoundedDiff {
    fn truncated(&self) -> bool {
        self.kept_bytes < self.total_bytes
    }
}

/// Bound the diff on a UTF-8 boundary, appending the §5.4 truncation marker
/// when anything was dropped.
fn bound_diff(diff: &str) -> BoundedDiff {
    let total_bytes = diff.len();
    if total_bytes <= MAX_DIFF_BYTES {
        return BoundedDiff {
            body: diff.to_owned(),
            kept_bytes: total_bytes,
            total_bytes,
        };
    }
    let mut end = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &diff[..end];
    let marker = truncation_marker(end, total_bytes);
    let separator = if kept.ends_with('\n') { "" } else { "\n" };
    BoundedDiff {
        body: format!("{kept}{separator}{marker}"),
        kept_bytes: end,
        total_bytes,
    }
}

async fn review_after_assembly(
    full: &mut FullAssembly,
    ctx: &Ctx,
    args: &ReviewArgs,
    diff: &str,
) -> Result<Exit, CliError> {
    full.base.rt.start().await?;

    let sessions = full.base.plane.sessions();
    let session = match args.session {
        Some(id) => {
            let existing = sessions.resume(id).await?;
            if existing.profile.as_str() != ctx.profile {
                return Err(CliError::new(
                    Exit::Usage,
                    format!(
                        "session {id} was created with profile {:?}, not {:?} (SQ5); rerun without --profile or with the recorded one",
                        existing.profile.as_str(),
                        ctx.profile
                    ),
                ));
            }
            id
        }
        None => {
            sessions
                .create(CreateSession {
                    workspace_root: ctx.workspace_abs.clone(),
                    profile: ProfileId::new(ctx.profile.clone())
                        .map_err(|e| CliError::new(Exit::Usage, e.to_string()))?,
                    budget: ctx.cfg.budget_policy.clone(),
                    language_backends: vec![LanguageId::new("rust")
                        .map_err(|e| CliError::new(Exit::Internal, e.to_string()))?],
                    provenance: None,
                })
                .await?
        }
    };
    assembly::write_last_session(&full.base.cfg.data_dir, session);

    // The diff travels out of band, as a goal attachment in the artifact
    // CAS. The `review` worker reads those bytes back and wraps them in the
    // `<workspace>` fence every capability instruction declares
    // non-instructional (RFC-0013 PR12) — verbatim, because a reviewer
    // reading a reshaped patch reviews something the author never wrote.
    let bounded = bound_diff(diff);
    if bounded.truncated() && !ctx.quiet {
        eprintln!(
            "warning: diff truncated to {} bytes of {} for review",
            bounded.kept_bytes, bounded.total_bytes
        );
    }
    let diff_artifact = full
        .base
        .storage
        .artifacts()
        .put(ArtifactPut {
            bytes: bounded.body.clone().into_bytes(),
            kind: ArtifactKind::Patch,
            content_type: Some("text/x-diff".into()),
            session_id: Some(session),
            run_id: None,
            labels: serde_json::Map::from_iter([("purpose".to_owned(), json!("review_diff"))]),
        })
        .await?;
    let goal = Goal {
        text: GOAL_TEXT.to_owned(),
        constraints: vec![],
        attachments: vec![diff_artifact],
    };
    let run = sessions.submit_goal(session, goal.clone()).await?;
    let dag_id = dag_id_for_run(full, run).await?;

    if !ctx.quiet {
        eprintln!("session {session}  run {run}");
    }

    // Template selection stays the plan service's (SQ1); the subcommand
    // names the template its grammar is, exactly as `--dry-run` may.
    let profile = ProfileId::new(ctx.profile.clone())
        .map_err(|e| CliError::new(Exit::Usage, e.to_string()))?;
    let toolchain = alloy_tools::toolchain::capture_toolchain();
    let target = alloy_tools::toolchain::host_triple();
    PlanService::plan(
        &*full.plan,
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id,
            goal,
            template_override: Some(TemplateId::ReviewDiff),
            policy_hash: alloy_runtime::policy_hash_digest(&profile, &ctx.cfg.budget_policy),
            tool_versions: alloy_runtime::tool_versions_digest(&toolchain),
            compiler_fingerprint: alloy_runtime::compiler_fingerprint_digest(&toolchain, &target),
            prior_source: None,
            prior_proposal_artifact: None,
        },
    )
    .await?;

    // Dispatch. The template has no gate, so there is nothing to answer
    // while the run is in flight — `start` returns when the DAG is terminal.
    let runs = full.base.plane.runs();
    let dispatch = runs.start(run).await;

    full.cost_meters.release(run);
    alloy_runtime::RunRouterProvider::release(full.routers.as_ref(), run);

    if let Err(e) = dispatch {
        return Err(e.into());
    }

    let events = super::all_session_events(sessions.as_ref(), session).await?;
    let run_exit = super::exit_from_run_events(&events, run).unwrap_or_else(|| {
        if full.base.handle.cancellation().is_cancelled() {
            Exit::Cancelled
        } else {
            Exit::Internal
        }
    });
    let cost = super::cost_summary(full.base.storage.events().as_ref(), session, run).await;
    if let Some(cost) = &cost {
        if !ctx.quiet {
            eprintln!("{cost}");
        }
    }

    if run_exit != Exit::Ok {
        // The review never ran to a verdict; the run's own exit is the
        // honest answer (§9.3).
        if ctx.json {
            let doc = outfmt::envelope(
                "review",
                run_exit,
                Some(&ctx.cfg),
                json!({
                    "session": session.to_string(),
                    "run": run.to_string(),
                    "diff_bytes": bounded.kept_bytes,
                    "diff_total_bytes": bounded.total_bytes,
                    "diff_truncated": bounded.truncated(),
                    "cost": cost,
                }),
            );
            println!("{doc}");
        } else {
            print_truncation_note(&bounded);
            println!("review run {run} produced no verdict");
        }
        eprintln!("no review verdict: inspect the run with `alloy events --session {session}`");
        return Ok(run_exit);
    }

    let payload = review_payload(full, dag_id).await?;
    render(ctx, session, run, &payload, &bounded, cost.as_deref());
    Ok(match payload.verdict {
        ReviewVerdict::Approve => Exit::Ok,
        ReviewVerdict::RequestChanges => Exit::ReviewChanges,
    })
}

/// Read the pre-minted `DagId` back from the run row's goal record.
async fn dag_id_for_run(full: &FullAssembly, run: RunId) -> Result<alloy_runtime::DagId, CliError> {
    let row = full
        .base
        .storage
        .sessions()
        .get_run(run)
        .await?
        .ok_or_else(|| CliError::new(Exit::NotFound, format!("run {run} not found")))?;
    let record: alloy_runtime::RunGoalRecord = serde_json::from_value(row.goal_json)
        .map_err(|e| CliError::new(Exit::Internal, format!("goal record: {e}")))?;
    Ok(record.dag_id)
}

/// The Review node's success payload, read back from the node output
/// envelope the scheduler wrote (RFC-0010 C4).
async fn review_payload(
    full: &FullAssembly,
    dag_id: alloy_runtime::DagId,
) -> Result<ReviewPayload, CliError> {
    use alloy_runtime::{ArtifactStore, DagStore};

    let dag =
        full.base.storage.dags().get(dag_id).await?.ok_or_else(|| {
            CliError::new(Exit::Internal, format!("dag {dag_id} missing after run"))
        })?;
    let node = dag
        .nodes
        .values()
        .find(|n| n.kind == alloy_runtime::NodeKind::Review)
        .ok_or_else(|| CliError::new(Exit::Internal, "review node missing from the plan"))?;
    let output = node.output_ref.ok_or_else(|| {
        CliError::new(
            Exit::Internal,
            format!("review node {} succeeded without an output", node.id),
        )
    })?;
    let blob = full.base.storage.artifacts().get(output).await?;
    let envelope: alloy_runtime::NodeOutputEnvelope = serde_json::from_slice(&blob.bytes)
        .map_err(|e| CliError::new(Exit::Internal, format!("review output envelope: {e}")))?;
    serde_json::from_value(envelope.payload)
        .map_err(|e| CliError::new(Exit::Internal, format!("review payload: {e}")))
}

fn severity_name(s: ReviewSeverity) -> &'static str {
    match s {
        ReviewSeverity::Info => "info",
        ReviewSeverity::Warning => "warning",
        ReviewSeverity::Blocker => "blocker",
    }
}

fn verdict_name(v: ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Approve => "approve",
        ReviewVerdict::RequestChanges => "request_changes",
    }
}

/// Tell the caller, on stdout, that the reviewer saw only part of the diff.
///
/// Not `--quiet`-suppressible and not stderr-only: a verdict over a fragment
/// is a different claim from a verdict over the change, and the difference
/// belongs with the verdict.
fn print_truncation_note(bounded: &BoundedDiff) {
    if bounded.truncated() {
        println!(
            "(diff truncated: {} of {} bytes reviewed)",
            bounded.kept_bytes, bounded.total_bytes
        );
    }
}

/// OUT3 — one JSON document with `--json`; otherwise one line per finding,
/// then the summary and the verdict.
fn render(
    ctx: &Ctx,
    session: SessionId,
    run: RunId,
    payload: &ReviewPayload,
    bounded: &BoundedDiff,
    cost: Option<&str>,
) {
    let exit = match payload.verdict {
        ReviewVerdict::Approve => Exit::Ok,
        ReviewVerdict::RequestChanges => Exit::ReviewChanges,
    };
    if ctx.json {
        let doc = outfmt::envelope(
            "review",
            exit,
            Some(&ctx.cfg),
            json!({
                "session": session.to_string(),
                "run": run.to_string(),
                "verdict": verdict_name(payload.verdict),
                "summary": payload.summary,
                // Two different truncations, two different names: the
                // model's findings list, and the diff it was shown.
                "findings_truncated": payload.truncated,
                "diff_bytes": bounded.kept_bytes,
                "diff_total_bytes": bounded.total_bytes,
                "diff_truncated": bounded.truncated(),
                "confidence": payload.confidence,
                "findings": payload.findings.iter().map(|f| json!({
                    "severity": severity_name(f.severity),
                    "file": f.file,
                    "line": f.line,
                    "message": f.message,
                })).collect::<Vec<_>>(),
                "cost": cost,
            }),
        );
        println!("{doc}");
        return;
    }
    for f in &payload.findings {
        match f.line {
            Some(line) => println!(
                "{} {}:{line} {}",
                severity_name(f.severity),
                f.file,
                f.message
            ),
            None => println!("{} {} {}", severity_name(f.severity), f.file, f.message),
        }
    }
    if payload.truncated {
        println!("(findings truncated)");
    }
    print_truncation_note(bounded);
    println!("summary: {}", payload.summary);
    println!("verdict: {}", verdict_name(payload.verdict));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_is_a_usage_error_naming_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.diff");
        std::fs::write(&path, "   \n").unwrap();
        let err = read_diff(&path).unwrap_err();
        assert_eq!(err.exit, Exit::Usage);
        assert!(err.message.contains("--diff"), "{}", err.message);
    }

    #[test]
    fn missing_diff_file_names_the_path() {
        let err = read_diff(Path::new("no/such/file.diff")).unwrap_err();
        assert_eq!(err.exit, Exit::Usage);
        assert!(err.message.contains("no/such/file.diff"), "{}", err.message);
    }

    #[test]
    fn diff_is_bounded_on_a_char_boundary_and_marked() {
        let big = "é".repeat(MAX_DIFF_BYTES);
        let bounded = bound_diff(&big);
        assert!(bounded.truncated());
        assert!(bounded.kept_bytes <= MAX_DIFF_BYTES);
        assert_eq!(bounded.total_bytes, big.len());
        let marker = truncation_marker(bounded.kept_bytes, big.len());
        assert!(bounded.body.ends_with(&marker), "{}", bounded.body);
        assert!(big.starts_with(&bounded.body[..bounded.kept_bytes]));
    }

    /// A diff that fits is passed through byte for byte — no marker, no
    /// reshaping of any kind.
    #[test]
    fn a_small_diff_is_passed_through_verbatim() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n \n-a   \n+a\t\n";
        let bounded = bound_diff(diff);
        assert!(!bounded.truncated());
        assert_eq!(bounded.body, diff);
        assert_eq!(bounded.kept_bytes, diff.len());
    }
}
