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
    fence_untrusted, CreateSession, Goal, LanguageId, PlanContext, PlanService, ProfileId,
    ReviewPayload, ReviewSeverity, ReviewVerdict, RunId, SessionId, SessionRows, TemplateId,
};
use serde_json::json;

use crate::args::ReviewArgs;
use crate::assembly::{self, AssemblyOptions, FullAssembly};
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

/// Largest diff handed to the model. Larger input is truncated with a note
/// on stderr rather than silently reshaped by the context engine.
const MAX_DIFF_BYTES: usize = 128 * 1024;

/// The instruction line above the fenced diff. The capability's own system
/// instruction (`REVIEW_SYSTEM`) owns the schema; this only names the task.
const GOAL_PREAMBLE: &str =
    "Review the unified diff below. Report findings against the files and lines it touches.";

pub async fn exec(ctx: Ctx, args: ReviewArgs) -> Result<Exit, CliError> {
    let diff = read_diff(&args.diff)?;

    let base = assembly::assemble_read(ctx.cfg.clone()).await?;
    // The one template this command plans is read-only, so its DAG carries
    // no `GateHuman` for the scheduler's load-time V11 check to find.
    let mut full = assembly::assemble_full_with(
        base,
        &ctx.workspace_abs,
        ctx.readonly(),
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

/// Bound the diff on a UTF-8 boundary, reporting the cut on stderr.
fn bound_diff(diff: &str, quiet: bool) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.to_owned();
    }
    let mut end = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    if !quiet {
        eprintln!(
            "warning: diff truncated to {MAX_DIFF_BYTES} bytes of {} for review",
            diff.len()
        );
    }
    diff[..end].to_owned()
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

    // The diff is untrusted input: it rides the `<workspace>` fence every
    // capability instruction declares non-instructional (RFC-0013 PR12).
    let goal = Goal {
        text: format!(
            "{GOAL_PREAMBLE}\n\n{}",
            fence_untrusted("diff", &bound_diff(diff, ctx.quiet))
        ),
        constraints: vec![],
        attachments: vec![],
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
        &full.plan,
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id,
            goal,
            template_override: Some(TemplateId::ReviewDiff),
            policy_hash: alloy_runtime::policy_hash_digest(&profile, &ctx.cfg.budget_policy),
            tool_versions: alloy_runtime::tool_versions_digest(&toolchain),
            compiler_fingerprint: alloy_runtime::compiler_fingerprint_digest(&toolchain, &target),
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
                    "cost": cost,
                }),
            );
            println!("{doc}");
        } else {
            println!("review run {run} produced no verdict");
        }
        eprintln!("no review verdict: inspect the run with `alloy events --session {session}`");
        return Ok(run_exit);
    }

    let payload = review_payload(full, dag_id).await?;
    render(ctx, session, run, &payload, cost.as_deref());
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

/// OUT3 — one JSON document with `--json`; otherwise one line per finding,
/// then the summary and the verdict.
fn render(ctx: &Ctx, session: SessionId, run: RunId, payload: &ReviewPayload, cost: Option<&str>) {
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
                "truncated": payload.truncated,
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
    fn diff_is_bounded_on_a_char_boundary() {
        let big = "é".repeat(MAX_DIFF_BYTES);
        let bounded = bound_diff(&big, true);
        assert!(bounded.len() <= MAX_DIFF_BYTES);
        assert!(big.starts_with(&bounded));
        assert_eq!(bound_diff("small", true), "small");
    }
}
