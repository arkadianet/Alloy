//! `alloy index` (RFC-0015 §10) — the ingest trigger (RFC-0011 IN1). Reads
//! and writes the graph only through `SqliteProjectGraph::rebuild_reported`
//! (IX9); records a `graph_rebuild` decision (IX4) and writes
//! `sessions.graph_version` via amendment A3 (IX5).
//!
//! Author: arkadianet

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{DecisionKind, DecisionLog, DecisionRecord, EventDecisionLog, SessionRows};
use serde_json::json;

use crate::args::IndexArgs;
use crate::assembly;
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

pub async fn exec(ctx: Ctx, args: IndexArgs) -> Result<Exit, CliError> {
    let mut base = assembly::assemble_read(ctx.cfg.clone()).await?;
    let graph = assembly::open_graph(&base).await?;

    // CL8 — index is long-running; arm the signal task before start.
    let cancel = base.handle.cancellation();
    let signal_task = crate::arm_signal_task(move || cancel.cancel());
    base.rt.start().await?;

    let result = index_after_start(&base, &graph, &ctx, &args).await;

    signal_task.abort();
    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, Some(&graph), Duration::from_secs(10)).await?;
    result
}

async fn index_after_start(
    base: &assembly::ReadAssembly,
    graph: &alloy_index::SqliteProjectGraph,
    ctx: &Ctx,
    args: &IndexArgs,
) -> Result<Exit, CliError> {
    if args.stats {
        // IX8 — read-only diagnostic path: print and exit without writing.
        let m = graph.metrics();
        if ctx.json {
            let doc = outfmt::envelope(
                "index",
                Exit::Ok,
                Some(&ctx.cfg),
                json!({
                    "stats": {
                        "rebuilds": m.rebuilds,
                        "rebuilds_unchanged": m.rebuilds_unchanged,
                        "incrementals": m.incrementals,
                        "queries": m.queries,
                        "queries_stub": m.queries_stub,
                        "queries_truncated": m.queries_truncated,
                        "diagnostics_recorded": m.diagnostics_recorded,
                        "fixes_recorded": m.fixes_recorded,
                        "snapshots": m.snapshots,
                        "busy_errors": m.busy_errors,
                        "quarantines": m.quarantines,
                        "files_skipped": m.files_skipped,
                    },
                }),
            );
            println!("{doc}");
        } else {
            println!("graph metrics: {m:?}");
        }
        return Ok(Exit::Ok);
    }

    // IX2 — rebuild through the merged trigger. `--rebuild` and the default
    // both call `rebuild_reported`; the store's digest tracking makes an
    // up-to-date pass cheap and reports `unchanged`.
    let report = graph
        .rebuild_reported(&ctx.workspace_abs)
        .await
        .map_err(|e| CliError::new(Exit::Graph, format!("graph rebuild: {e}")))?;

    // IX4 — record the decision through EventDecisionLog. Decision events
    // are session events, so this needs a session; without one (fresh
    // workspace, no prior run) the fact is reported on stderr instead.
    let session = assembly::read_last_session(&base.cfg.data_dir);
    match session {
        Some(session) => {
            let decisions =
                EventDecisionLog::from_handle(base.handle.clone(), Arc::clone(&base.storage))
                    .map_err(|e| CliError::new(Exit::Internal, format!("decision log: {e}")))?;
            let record = DecisionRecord {
                session,
                run: None,
                node: None,
                kind: DecisionKind::Custom("graph_rebuild".into()),
                metadata: serde_json::to_value(&report).unwrap_or_else(|_| json!({})),
                content_hash: None,
                prompt_body: None,
            };
            if let Err(e) = decisions.record(record).await {
                eprintln!("warning: graph_rebuild decision not recorded: {e}");
            }
            // IX5 / amendment A3 — write sessions.graph_version.
            if let Err(e) = base
                .storage
                .sessions()
                .set_graph_version(session, report.version)
                .await
            {
                eprintln!("warning: sessions.graph_version not written: {e}");
            }
        }
        None => {
            eprintln!(
                "note: no session exists in this workspace yet; graph_rebuild decision and sessions.graph_version will be recorded by the first `alloy run`"
            );
        }
    }

    if ctx.json {
        let doc = outfmt::envelope(
            "index",
            Exit::Ok,
            Some(&ctx.cfg),
            json!({
                "report": serde_json::to_value(&report).unwrap_or_else(|_| json!({})),
                "session": session.map(|s| s.to_string()),
            }),
        );
        println!("{doc}");
    } else {
        println!(
            "graph: {} crates, {} modules, {} files, version {}{}",
            report.crates,
            report.modules,
            report.files,
            report.version.0,
            if report.unchanged { " (unchanged)" } else { "" }
        );
        for w in &report.warnings {
            eprintln!("warning: {w}");
        }
        if session.is_some() {
            println!("decision recorded: graph_rebuild");
        }
    }
    Ok(Exit::Ok)
}
