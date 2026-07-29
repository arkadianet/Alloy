//! Undoing a failed attempt's edits before the retry loop submits the next
//! one (issue #53 follow-through).
//!
//! Wiring only, no mechanics and no policy. *Whether* to retry stays where
//! it already is in [`super::run`]; *how* to undo a run's edits — which
//! transactions, in what order, what a refusal means — lives in
//! `alloy_runtime::rollback_run_edits`, because RFC-0015 rule B5 forbids
//! this crate from driving the edit engine's undo entry point itself (the
//! T5 boundary grep checks it). What is left here is what a composition
//! root legitimately does: read the session log, mint the token, report the
//! outcome (rule B1). `seed_graph_diagnostics` is the same shape — the CLI
//! may not write graph diagnostics either, so the runtime owns that
//! mechanic too and the driver asks for a report.
//!
//! Author: arkadianet

use alloy_runtime::{
    rollback_run_edits, DagId, DecisionKind, DecisionLog, DecisionRecord, EditContext, NodeExecRef,
    NodeId, RollbackReport, RunId, SessionId, TransactionId, WorkerToolClass,
};
use serde_json::json;

use crate::assembly::FullAssembly;
use crate::resolve::Ctx;

/// The operator-facing line for one rollback pass, or `None` when the failed
/// attempt edited nothing and there is nothing to report.
#[must_use]
pub(crate) fn summary(report: &RollbackReport) -> Option<String> {
    if report.found == 0 {
        return None;
    }
    let mut line = format!(
        "rolled back {}/{} edit transaction(s) from the failed attempt",
        report.restored.len(),
        report.found
    );
    if let Some(declined) = &report.declined {
        // Honest, not reassuring: name the refusal and say what the retry
        // will actually run against.
        line.push_str(&format!(
            "; {} was refused ({}), so the retry starts from the workspace as it stands",
            declined.transaction_id, declined.reason
        ));
    }
    Some(line)
}

/// Roll the failed run's applied edits back so the next attempt starts from
/// the pre-run workspace.
///
/// Never fails: an unreadable log or an unmintable token degrades to "left
/// as-is" plus a warning, exactly as a refusal from the engine does.
pub(crate) async fn rollback_run(
    full: &FullAssembly,
    ctx: &Ctx,
    session: SessionId,
    run: RunId,
    dag_id: DagId,
) -> RollbackReport {
    let empty = RollbackReport::default();
    // PF10 — readonly assembles no engine, and applied no edit either.
    let Some(engine) = full.edit_engine.as_ref() else {
        return empty;
    };
    let events = match super::all_session_events(full.base.plane.sessions().as_ref(), session).await
    {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(error = %e, "edit journal unreadable; skipping pre-retry rollback");
            return empty;
        }
    };

    // One token minted by the same authority the patch builtin uses, bound
    // to the run whose edits these are (the engine re-checks that binding).
    let exec_ref = NodeExecRef {
        session_id: session,
        run_id: run,
        dag_id,
        node_id: NodeId::new(),
        workspace_root: ctx.workspace_abs.clone(),
        attempt: 1,
    };
    let perms = match full
        .worker_perms
        .token_for(&exec_ref, WorkerToolClass::Patch)
        .await
    {
        Ok(perms) => perms,
        Err(e) => {
            tracing::warn!(error = %e, "no write token; skipping pre-retry rollback");
            return empty;
        }
    };
    let edit_ctx = EditContext {
        session_id: Some(session),
        run_id: Some(run),
        perms,
    };

    let report = rollback_run_edits(engine.as_ref(), &events, run, &edit_ctx).await;
    for tx in &report.restored {
        record_rollback(full, session, run, *tx, None).await;
    }
    if let Some(declined) = &report.declined {
        record_rollback(
            full,
            session,
            run,
            declined.transaction_id,
            Some(declined.reason.clone()),
        )
        .await;
    }
    report
}

/// Write the outcome into the session log.
///
/// The edit engine's undo path emits no session event of its own, so without
/// this the log would keep claiming an `edit_applied` whose changes are no
/// longer on disk — and the next attempt's context pack is assembled from
/// that log.
async fn record_rollback(
    full: &FullAssembly,
    session: SessionId,
    run: RunId,
    tx: TransactionId,
    declined: Option<String>,
) {
    let metadata = match &declined {
        None => json!({ "transaction_id": tx.to_string(), "restored": true }),
        Some(reason) => json!({
            "transaction_id": tx.to_string(),
            "restored": false,
            "reason": reason,
        }),
    };
    let record = DecisionRecord {
        session,
        run: Some(run),
        node: None,
        kind: DecisionKind::Custom("edit_rolled_back".into()),
        metadata,
        content_hash: None,
        prompt_body: None,
    };
    if let Err(e) = full.decisions.record(record).await {
        tracing::warn!(error = %e, tx = %tx, "edit_rolled_back decision not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::DeclinedRollback;

    #[test]
    fn a_run_that_edited_nothing_says_nothing() {
        assert_eq!(summary(&RollbackReport::default()), None);
    }

    #[test]
    fn a_clean_rollback_reports_the_count() {
        let report = RollbackReport {
            found: 2,
            restored: vec![TransactionId::new(), TransactionId::new()],
            declined: None,
        };
        let line = summary(&report).unwrap();
        assert!(line.contains("rolled back 2/2"), "{line}");
        assert!(!line.contains("as it stands"), "{line}");
    }

    /// A refusal must read as a refusal: the count is honest about what was
    /// *not* restored, and the line names the reason and what runs next.
    #[test]
    fn a_declined_rollback_names_the_reason() {
        let tx = TransactionId::new();
        let report = RollbackReport {
            found: 2,
            restored: vec![],
            declined: Some(DeclinedRollback {
                transaction_id: tx,
                reason: "workspace drifted".into(),
            }),
        };
        let line = summary(&report).unwrap();
        assert!(line.contains("rolled back 0/2"), "{line}");
        assert!(line.contains("workspace drifted"), "{line}");
        assert!(line.contains(&tx.to_string()), "{line}");
        assert!(line.contains("as it stands"), "{line}");
    }
}
