//! `alloy run` (RFC-0015 §7.1) — and the shared execute/monitor loop
//! `resume` reuses (§7.5 steps 4–5).
//!
//! Author: arkadianet

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, Approval, Constraint,
    CreateSession, DecisionKind, DecisionLog, DecisionRecord, EventSeq, Goal, LanguageId,
    PlanContext, PlanService, ProfileId, RunGoalRecord, RunId, SessionEvent, SessionEventType,
    SessionId, SessionRows, TemplateId,
};
use serde_json::json;

use super::gate::{self, GateMode};
use crate::args::RunArgs;
use crate::assembly::{self, FullAssembly};
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

/// The three `PlanContext` fingerprints, captured at the composition root
/// (research §7.11 item 3): the profile's budget policy plus a live
/// `rustc`/`cargo` probe.
fn plan_fingerprints(
    ctx: &Ctx,
) -> Result<
    (
        alloy_runtime::Digest,
        alloy_runtime::Digest,
        alloy_runtime::Digest,
    ),
    CliError,
> {
    let profile = ProfileId::new(ctx.profile.clone())
        .map_err(|e| CliError::new(Exit::Usage, e.to_string()))?;
    let toolchain = alloy_tools::toolchain::capture_toolchain();
    let target = alloy_tools::toolchain::host_triple();
    Ok((
        policy_hash_digest(&profile, &ctx.cfg.budget_policy),
        tool_versions_digest(&toolchain),
        compiler_fingerprint_digest(&toolchain, &target),
    ))
}

pub async fn exec(ctx: Ctx, args: RunArgs) -> Result<Exit, CliError> {
    // PF9 — readonly refuses structurally, before any session row.
    if ctx.readonly() && args.yes {
        return Err(CliError::new(
            Exit::Usage,
            "--yes is a usage error under the readonly profile (PF9)",
        ));
    }
    if ctx.readonly() && !args.dry_run {
        return Err(CliError::new(
            Exit::ProfileRefused,
            "profile readonly refuses `alloy run` without --dry-run: the repair template contains an Edit node (PF9); run `alloy run --dry-run` to inspect the plan",
        ));
    }
    // PF11 — a CLI constraint may only tighten the profile ceiling.
    if let Some(v) = args.max_usd {
        let ceiling = ctx.cfg.budget_policy.max_usd_per_run;
        if v > ceiling {
            return Err(CliError::new(
                Exit::Usage,
                format!(
                    "--max-usd {v} exceeds the profile ceiling [budgets].max_usd_per_run = {ceiling} (PF11); constraints may only tighten"
                ),
            ));
        }
    }

    if args.dry_run {
        return dry_run(ctx, args).await;
    }

    // §6.2 steps 1–12.
    let base = assembly::assemble_read(ctx.cfg.clone()).await?;
    let mut full = assembly::assemble_full(base, &ctx.workspace_abs, ctx.readonly()).await?;

    tracing::debug!(
        edit_engine_assembled = full.edit_engine_assembled,
        scheduler_refs = std::sync::Arc::strong_count(&full.scheduler),
        "composition root assembled"
    );

    // CR14 — arm signals before start.
    let cancel = full.base.handle.cancellation();
    let signal_task = crate::arm_signal_task(move || cancel.cancel());

    let result = run_after_assembly(&mut full, &ctx, &args).await;

    signal_task.abort();
    let FullAssembly { base, graph, .. } = full;
    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, Some(&graph), Duration::from_secs(10)).await?;
    result
}

async fn run_after_assembly(
    full: &mut FullAssembly,
    ctx: &Ctx,
    args: &RunArgs,
) -> Result<Exit, CliError> {
    full.base.rt.start().await?;

    let sessions = full.base.plane.sessions();

    // §7.1 step 3 — create or reuse the session. (The RFC-0018 provenance
    // seam attaches here when it merges.)
    let session = match args.session {
        Some(id) => {
            let existing = sessions.resume(id).await?;
            // SQ5 — the session row's profile wins; a mismatch is reported.
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
            let mut budget = ctx.cfg.budget_policy.clone();
            if let Some(v) = args.max_usd {
                budget.max_usd_per_run = v;
            }
            sessions
                .create(CreateSession {
                    workspace_root: ctx.workspace_abs.clone(),
                    profile: ProfileId::new(ctx.profile.clone())
                        .map_err(|e| CliError::new(Exit::Usage, e.to_string()))?,
                    budget,
                    language_backends: vec![LanguageId::new("rust")
                        .map_err(|e| CliError::new(Exit::Internal, e.to_string()))?],
                    provenance: None,
                })
                .await?
        }
    };
    assembly::write_last_session(&full.base.cfg.data_dir, session);

    // IX3 — graph bootstrap before submit_goal; failures warn, never fail.
    if !args.no_index {
        bootstrap_index(full, ctx, session).await;
    }

    // §7.1 step 4 — submit the goal.
    let mut constraints = Vec::new();
    if let Some(v) = args.max_usd {
        constraints.push(Constraint::MaxUsd(v));
    }
    if args.require_cargo_check || full.base.cfg.gates.require_cargo_check {
        constraints.push(Constraint::RequireCargoCheck);
    }
    let goal = Goal {
        text: args.goal.clone(),
        constraints,
        attachments: vec![],
    };
    let run = sessions.submit_goal(session, goal.clone()).await?;
    let dag_id = dag_id_for_run(full, run).await?;

    if !ctx.quiet {
        eprintln!("session {session}  run {run}");
    }

    // Issue #53 — one verify pass before planning, so the repair worker's
    // generation-1 prompt carries the real rustc diagnostics instead of
    // guessing from the goal text. Best-effort: a missing toolchain or
    // sandbox must never fail a run before it starts.
    bootstrap_diagnostics(full, ctx, session, run, dag_id).await;

    // §7.1 step 6 — plan (template selection is the plan service's, SQ1).
    let (policy_hash, tool_versions, compiler_fingerprint) = plan_fingerprints(ctx)?;
    PlanService::plan(
        &full.plan,
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id,
            goal,
            template_override: None,
            policy_hash,
            tool_versions,
            compiler_fingerprint,
        },
    )
    .await?;

    // §7.1 steps 7–10 — dispatch + render + gates + terminal exit.
    let mode = if args.yes {
        GateMode::AutoAllow
    } else if args.no_input {
        GateMode::NoInput
    } else {
        GateMode::Interactive
    };
    execute_and_render(full, ctx, "run", session, run, mode).await
}

/// Issue #53 — pre-plan diagnostic seed; a failure is a warning, never
/// fatal (IX7 spirit). Reuses the scheduler's own compile verifier, so the
/// check runs sandboxed with the same policy as verify nodes.
async fn bootstrap_diagnostics(
    full: &FullAssembly,
    ctx: &Ctx,
    session: SessionId,
    run: RunId,
    dag_id: alloy_runtime::DagId,
) {
    let exec_ctx = alloy_runtime::NodeExecContext {
        meta: alloy_runtime::NodeExecRef {
            session_id: session,
            run_id: run,
            dag_id,
            node_id: alloy_runtime::NodeId::new(),
            workspace_root: ctx.workspace_abs.clone(),
            attempt: 1,
        },
        cancellation: full.base.handle.cancellation().child_token(),
    };
    match alloy_runtime::seed_graph_diagnostics(
        full.verify_compile.as_ref(),
        full.graph.as_ref(),
        &exec_ctx,
    )
    .await
    {
        Ok(n) => {
            if !ctx.quiet && n > 0 {
                eprintln!("seeded {n} diagnostic(s) from cargo check");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "diagnostic seed skipped");
        }
    }
}

/// IX3/IX4/IX5 — bootstrap ingest; a failure is a warning, never fatal (IX7).
async fn bootstrap_index(full: &FullAssembly, ctx: &Ctx, session: SessionId) {
    match full.graph.rebuild_reported(&ctx.workspace_abs).await {
        Ok(report) => {
            let decision = DecisionRecord {
                session,
                run: None,
                node: None,
                kind: DecisionKind::Custom("graph_rebuild".into()),
                metadata: serde_json::to_value(&report).unwrap_or_else(|_| json!({})),
                content_hash: None,
                prompt_body: None,
            };
            if let Err(e) = full.decisions.record(decision).await {
                tracing::warn!(error = %e, "graph_rebuild decision not recorded");
            }
            if let Err(e) = full
                .base
                .storage
                .sessions()
                .set_graph_version(session, report.version)
                .await
            {
                tracing::warn!(error = %e, "sessions.graph_version not written");
            }
        }
        Err(e) => {
            eprintln!("warning: graph bootstrap failed (continuing without it): {e}");
        }
    }
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
    let record: RunGoalRecord = serde_json::from_value(row.goal_json)
        .map_err(|e| CliError::new(Exit::Internal, format!("goal record: {e}")))?;
    Ok(record.dag_id)
}

/// CL12 — plan and print the DAG; `RunController::start` is never called.
async fn dry_run(ctx: Ctx, args: RunArgs) -> Result<Exit, CliError> {
    let base = assembly::assemble_read(ctx.cfg.clone()).await?;
    let mut base = base;
    base.rt.start().await?;

    let template_override = match &args.template {
        None => None,
        Some(t) => Some(parse_template(t)?),
    };

    let sessions = base.plane.sessions();
    let session = match args.session {
        Some(id) => id,
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
    let goal = Goal {
        text: args.goal.clone(),
        constraints: vec![],
        attachments: vec![],
    };
    assembly::write_last_session(&base.cfg.data_dir, session);
    let run = sessions.submit_goal(session, goal.clone()).await?;
    let row = base
        .storage
        .sessions()
        .get_run(run)
        .await?
        .ok_or_else(|| CliError::new(Exit::Internal, "run row missing after submit"))?;
    let record: RunGoalRecord = serde_json::from_value(row.goal_json)
        .map_err(|e| CliError::new(Exit::Internal, format!("goal record: {e}")))?;

    let plan = alloy_runtime::TemplatePlanService::from_storage(&base.storage);
    let (policy_hash, tool_versions, compiler_fingerprint) = plan_fingerprints(&ctx)?;
    let result = alloy_runtime::PlanService::plan(
        &plan,
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id: record.dag_id,
            goal,
            template_override,
            policy_hash,
            tool_versions,
            compiler_fingerprint,
        },
    )
    .await?;

    // The dry-run run is never dispatched; cancel it so the session has no
    // dangling non-terminal run for `resume` to trip over.
    base.plane.cancel(run).await?;

    let mut nodes: Vec<_> = result.dag.nodes.values().collect();
    nodes.sort_by_key(|n| n.id);
    if ctx.json {
        let doc = outfmt::envelope(
            "run",
            Exit::Ok,
            Some(&ctx.cfg),
            json!({
                "dry_run": true,
                "session": session.to_string(),
                "run": run.to_string(),
                "template_id": format!("{:?}", result.template_id),
                "snapshot_artifact": result.snapshot_artifact.to_string(),
                "nodes": nodes
                    .iter()
                    .map(|n| json!({
                        "id": n.id.to_string(),
                        "kind": format!("{:?}", n.kind),
                        "capability": n.capability.as_ref().map(ToString::to_string),
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
        println!("{doc}");
    } else {
        println!("template {:?}:", result.template_id);
        for n in &nodes {
            let cap = n
                .capability
                .as_ref()
                .map_or_else(|| "-".to_owned(), ToString::to_string);
            println!("  {cap:<16} {:?}", n.kind);
        }
    }

    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, None, Duration::from_secs(5)).await?;
    Ok(Exit::Ok)
}

fn parse_template(t: &str) -> Result<TemplateId, CliError> {
    // TemplateId is a closed enum; MVP ships one template.
    match t {
        "repair_local_diagnostic" => Ok(TemplateId::RepairLocalDiagnostic),
        other => Err(CliError::new(
            Exit::Usage,
            format!("unknown template {other:?}; known: repair_local_diagnostic"),
        )),
    }
}

/// Steps 7–10 shared by `run` and `resume`: dispatch through
/// `RunController::start` (SQ2), render progress from the event log, answer
/// gates (§8), map the terminal state (§9.3).
pub(crate) async fn execute_and_render(
    full: &FullAssembly,
    ctx: &Ctx,
    command: &str,
    session: SessionId,
    run: RunId,
    mode: GateMode,
) -> Result<Exit, CliError> {
    let sessions = full.base.plane.sessions();
    let runs = full.base.plane.runs();
    let plane = full.base.plane.clone();

    let start_runs = Arc::clone(&runs);
    let start_task = tokio::spawn(async move { start_runs.start(run).await });

    let mut cursor: Option<EventSeq> = None;
    let mut pending_gates: BTreeSet<String> = BTreeSet::new();
    let mut gate_payloads: Vec<SessionEvent> = Vec::new();
    let mut latest_edit: Option<SessionEvent> = None;
    let mut gate_required_exit: Option<String> = None;

    'monitor: loop {
        let page = sessions
            .events(session, cursor, alloy_runtime::MAX_EVENTS_PAGE)
            .await?;
        let page_empty = page.is_empty();
        for ev in page {
            cursor = Some(ev.seq);
            if ev.run_id != Some(run) {
                continue;
            }
            if !ctx.quiet {
                // OUT2 — with --json the progress stream on stderr is JSONL.
                if ctx.json {
                    eprintln!("{}", outfmt::event_json(&ev));
                } else {
                    eprintln!("{}", outfmt::event_line(&ev));
                }
            }
            match ev.type_ {
                SessionEventType::ApprovalRequested => {
                    if let Some(gate) = ev.payload.get("gate_id").and_then(|v| v.as_str()) {
                        pending_gates.insert(gate.to_owned());
                        gate_payloads.push(ev);
                    }
                }
                SessionEventType::ApprovalResolved => {
                    if let Some(gate) = ev.payload.get("gate_id").and_then(|v| v.as_str()) {
                        pending_gates.remove(gate);
                    }
                }
                SessionEventType::EditApplied => latest_edit = Some(ev),
                _ => {}
            }
        }

        // Only act on gates once the log is drained: a resumed run's stale
        // requests are already paired with resolutions above (GA1).
        if page_empty {
            if start_task.is_finished() {
                break 'monitor;
            }
            if let Some(gate) = pending_gates.iter().next().cloned() {
                let payload = gate_payloads
                    .iter()
                    .rev()
                    .find(|e| {
                        e.payload.get("gate_id").and_then(|v| v.as_str()) == Some(gate.as_str())
                    })
                    .map(|e| e.payload.clone())
                    .unwrap_or_default();
                let block = gate::render_block(
                    &run.to_string(),
                    &payload,
                    latest_edit.as_ref(),
                    full.base.cfg.gate_timeout,
                );
                let answered =
                    answer_gate(&plane, &full.base.handle, run, &gate, &block, mode).await?;
                match answered {
                    GateStep::Resolved => {
                        pending_gates.remove(&gate);
                    }
                    GateStep::Unavailable => {
                        gate_required_exit = Some(gate);
                        // GA5: leave the run durable in waiting_approval —
                        // abort the in-process dispatch without cancelling.
                        start_task.abort();
                        break 'monitor;
                    }
                    GateStep::RunCancelled => break 'monitor,
                }
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    // Surface dispatch errors (join errors on a GA5 abort are expected).
    let aborted = gate_required_exit.is_some();
    if aborted {
        let _ = start_task.await;
    } else {
        match start_task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(e) if e.is_cancelled() => {}
            Err(e) => return Err(CliError::new(Exit::Internal, format!("dispatch task: {e}"))),
        }
    }

    // CR21 — per-run objects are released on run terminal.
    full.cost_meters.release(run);
    alloy_runtime::RunRouterProvider::release(full.routers.as_ref(), run);

    // Terminal rendering (§7.1 step 10).
    let events = super::all_session_events(sessions.as_ref(), session).await?;
    if let Some(gate) = gate_required_exit {
        let exit = Exit::GateRequired;
        if ctx.json {
            let doc = outfmt::envelope(
                command,
                exit,
                Some(&ctx.cfg),
                json!({
                    "session": session.to_string(),
                    "run": run.to_string(),
                    "gate_required": gate,
                }),
            );
            println!("{doc}");
        } else {
            // GA5 — the gate id on stdout so CI can approve out of band.
            println!("gate_required {gate}");
            println!("run {run}");
        }
        eprintln!(
            "approval needed: alloy approve --run {run} --gate {gate} --decision allow; then alloy resume --session {session}"
        );
        return Ok(exit);
    }

    let mut exit = super::exit_from_run_events(&events, run).unwrap_or_else(|| {
        if full.base.handle.cancellation().is_cancelled() {
            Exit::Cancelled
        } else {
            Exit::Internal
        }
    });
    // CR18 — an operator signal is a cancellation even when the interrupted
    // node's failure was classified before the cancel reached it.
    if full.base.handle.cancellation().is_cancelled()
        && matches!(exit, Exit::RunFailed | Exit::Internal)
    {
        exit = Exit::Cancelled;
    }

    if exit == Exit::Cancelled {
        // CR18/CR19.
        eprintln!("run {run} cancelled; resume with: alloy resume --session {session}");
        if let Some(notice) = super::workspace_maybe_modified(&events, run) {
            eprintln!("{notice}");
        }
    }
    if exit == Exit::Replan {
        eprintln!(
            "replan required; MVP does not auto-replan (SQ3): alloy resume --session {session}"
        );
    }

    let cost = super::cost_summary(full.base.storage.events().as_ref(), session, run).await;
    if let Some(cost) = &cost {
        if !ctx.quiet {
            eprintln!("{cost}");
        }
    }

    if ctx.json {
        let doc = outfmt::envelope(
            command,
            exit,
            Some(&ctx.cfg),
            json!({
                "session": session.to_string(),
                "run": run.to_string(),
                "cost": cost,
            }),
        );
        println!("{doc}");
    } else {
        println!(
            "run {run} {}",
            match exit {
                Exit::Ok => "succeeded",
                Exit::Cancelled => "cancelled",
                Exit::GateDenied => "denied at gate",
                Exit::Replan => "needs replan",
                Exit::Budget => "stopped at budget ceiling",
                Exit::Timeout => "timed out",
                _ => "failed",
            }
        );
    }
    Ok(exit)
}

enum GateStep {
    Resolved,
    Unavailable,
    RunCancelled,
}

/// §8 — answer one pending gate according to the mode.
async fn answer_gate(
    plane: &alloy_runtime::SessionPlane,
    handle: &alloy_runtime::RuntimeHandle,
    run: RunId,
    gate: &str,
    block: &str,
    mode: GateMode,
) -> Result<GateStep, CliError> {
    let gate_id = alloy_runtime::GateId::parse(gate)
        .map_err(|e| CliError::new(Exit::Internal, format!("gate id in event: {e}")))?;
    match mode {
        GateMode::NoInput => Ok(GateStep::Unavailable),
        GateMode::AutoAllow => {
            // GA4 — print the same block so the log shows what was approved.
            eprint!("{block}");
            eprintln!("> y (--yes)");
            plane.approve(run, gate_id, Approval::Allow).await?;
            Ok(GateStep::Resolved)
        }
        GateMode::Interactive => {
            let block_owned = block.to_owned();
            let cancel = handle.cancellation();
            let prompt = tokio::task::spawn_blocking(move || gate::prompt_via_tty(&block_owned));
            tokio::select! {
                answer = prompt => {
                    match answer {
                        Ok(Some(decision)) => {
                            plane.approve(run, gate_id, decision).await?;
                            Ok(GateStep::Resolved)
                        }
                        // /dev/tty unavailable or EOF → GA5.
                        Ok(None) => Ok(GateStep::Unavailable),
                        Err(e) => Err(CliError::new(Exit::Internal, format!("prompt: {e}"))),
                    }
                }
                () = cancel.cancelled() => {
                    // GA8 — Ctrl-C at a prompt cancels the run, not just the
                    // prompt (the signal task already cancelled the token).
                    let _ = plane.cancel(run).await;
                    Ok(GateStep::RunCancelled)
                }
            }
        }
    }
}
