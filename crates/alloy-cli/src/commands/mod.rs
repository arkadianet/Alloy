//! RFC-0015 §7 — subcommand handlers. Control-plane calls plus rendering
//! only (B1); every read goes through `SessionService`, `obs::query`, the
//! `SessionRows` seam, or `SqliteProjectGraph` (B6).
//!
//! Author: arkadianet

mod approve;
mod cancel;
mod events;
mod gate;
mod index;
mod resume;
mod rollback;
mod run;

use alloy_runtime::{EventSeq, RunId, SessionEvent, SessionEventType, SessionId, SessionService};

use crate::args::{Commands, Globals};
use crate::errx::{CliError, Exit};
use crate::resolve;

/// Route a parsed subcommand to its handler.
pub async fn dispatch(globals: Globals, command: Commands) -> Result<Exit, CliError> {
    let ctx = resolve::resolve(&globals)?;
    match command {
        Commands::Run(args) => run::exec(ctx, args).await,
        Commands::Events(args) => events::exec(ctx, args).await,
        Commands::Approve(args) => approve::exec(ctx, args).await,
        Commands::Cancel(args) => cancel::exec(ctx, args).await,
        Commands::Resume(args) => resume::exec(ctx, args).await,
        Commands::Index(args) => index::exec(ctx, args).await,
        Commands::Host => unreachable!("host is dispatched in main"),
    }
}

/// Collect every event for `session`, paging on the merged cursor contract
/// (SQ4). Used for terminal-state derivation, not live progress.
pub(crate) async fn all_session_events(
    sessions: &dyn SessionService,
    session: SessionId,
) -> Result<Vec<SessionEvent>, CliError> {
    let mut out = Vec::new();
    let mut after: Option<EventSeq> = None;
    loop {
        let page = sessions
            .events(session, after, alloy_runtime::MAX_EVENTS_PAGE)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some(last.seq);
        let full_page = page.len() == alloy_runtime::MAX_EVENTS_PAGE;
        out.extend(page);
        if !full_page {
            break;
        }
    }
    Ok(out)
}

/// Derive the terminal exit for a run from its durable events (§9.3): the
/// `RunCompleted` payload's `dag_state`, refined by the latest `Error`
/// event's `class` when the DAG failed.
pub(crate) fn exit_from_run_events(events: &[SessionEvent], run: RunId) -> Option<Exit> {
    let run_events: Vec<&SessionEvent> = events.iter().filter(|e| e.run_id == Some(run)).collect();
    let completed = run_events
        .iter()
        .rev()
        .find(|e| e.type_ == SessionEventType::RunCompleted)?;
    let state: alloy_runtime::DagState =
        serde_json::from_value(completed.payload.get("dag_state")?.clone()).ok()?;
    let mut exit = crate::errx::exit_for_dag_state(state, false);
    if exit == Exit::RunFailed {
        if let Some(class) = run_events
            .iter()
            .rev()
            .filter(|e| e.type_ == SessionEventType::Error)
            .find_map(|e| e.payload.get("class").and_then(|c| c.as_str()))
            .and_then(crate::errx::parse_error_class)
        {
            exit = crate::errx::exit_for_error_class(class);
        }
    }
    Some(exit)
}

/// CR19 — RFC-0010 FOW5's workspace-modified inference: an `EditApplied`
/// event (or an edit-node success recorded in the log) means the workspace
/// may be modified. Absence proves nothing and is not printed as clean.
pub(crate) fn workspace_maybe_modified(events: &[SessionEvent], run: RunId) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.run_id == Some(run) && e.type_ == SessionEventType::EditApplied)
        .map(|e| {
            let checkpoint = e
                .payload
                .get("checkpoint")
                .or_else(|| e.payload.get("transaction_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            format!(
                "workspace may be modified (an edit was applied in run {run}); checkpoint: {checkpoint}"
            )
        })
}

/// OUT6 — measured spend line from the merged reaccumulation helper; no
/// savings or comparative claims.
pub(crate) async fn cost_summary(
    store: &dyn alloy_runtime::EventStore,
    session: SessionId,
    run: RunId,
) -> Option<String> {
    let meter = alloy_runtime::reaccumulate_cost_from_events(store, session, Some(run))
        .await
        .ok()?;
    let snap = meter.snapshot();
    // OUT6: `None` is "no reported USD", never a measured zero.
    let usd = match snap.usd_spent {
        Some(usd) => format!("${usd:.3} measured"),
        None => "no USD reported".to_owned(),
    };
    Some(format!(
        "cost {usd} ({} model calls, {} tokens in, {} tokens out)",
        snap.model_calls, snap.tokens_in, snap.tokens_out
    ))
}
