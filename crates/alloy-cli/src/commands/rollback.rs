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
    rollback_run_edits, DagId, DagStore, DecisionKind, DecisionLog, DecisionRecord, EditContext,
    NodeExecRef, NodeId, RollbackReport, RunId, SessionId, TransactionId, WorkerToolClass,
};
use serde_json::json;

use crate::assembly::FullAssembly;
use crate::resolve::Ctx;

/// Whether the pre-rollback diagnostic probe still describes the tree the
/// next attempt will start from.
///
/// The retry loop skips its pre-plan bootstrap when it does — re-running
/// cargo for an identical answer costs a compile for nothing. It may only
/// say so when the pass left the workspace alone. A restore obviously
/// changes it; a *refusal* is not proof it did not, because
/// `EditError::RollbackFailed` and the post-restore digest check are raised
/// after the restore was attempted (see `rollback_run_edits`), which can
/// leave the tree between its pre- and post-edit states.
///
/// `unjournaled_edits` is deliberately not consulted: those edits were never
/// touched by this pass, so the probe that ran over them still holds.
#[must_use]
pub(crate) fn probe_still_describes_tree(report: &RollbackReport) -> bool {
    report.restored.is_empty() && report.declined.is_none()
}

/// The operator-facing line for one rollback pass, or `None` when the failed
/// attempt edited nothing and there is nothing to report.
#[must_use]
pub(crate) fn summary(report: &RollbackReport) -> Option<String> {
    if report.found == 0 {
        // Nothing in the journal is only good news if nothing was edited.
        // A succeeded edit node says otherwise: the `EditApplied` append is
        // best-effort after the commit point, so the edit can be on disk with
        // no transaction id anyone can undo it by.
        if report.unjournaled_edits > 0 {
            return Some(format!(
                "{} edit node(s) of the failed attempt succeeded but named no edit \
                 transaction in the session log, so nothing could be rolled back; \
                 the retry starts from the workspace as it stands",
                report.unjournaled_edits
            ));
        }
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

    // The DAG is the second witness: it knows an edit node succeeded even
    // when the `EditApplied` append that would have named the transaction was
    // lost. Unreadable → no cross-check, never a failure.
    let dag = match full.base.storage.dags().get(dag_id).await {
        Ok(dag) => dag,
        Err(e) => {
            tracing::warn!(error = %e, "dag unreadable; no unjournaled-edit cross-check");
            None
        }
    };

    let report = rollback_run_edits(engine.as_ref(), &events, run, dag.as_ref(), &edit_ctx).await;
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
            ..RollbackReport::default()
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
            ..RollbackReport::default()
        };
        let line = summary(&report).unwrap();
        assert!(line.contains("rolled back 0/2"), "{line}");
        assert!(line.contains("workspace drifted"), "{line}");
        assert!(line.contains(&tx.to_string()), "{line}");
        assert!(line.contains("as it stands"), "{line}");
    }

    /// A pass that restored some and was then refused must read as *both*:
    /// the count says what came back, the clause says what did not.
    #[test]
    fn a_partial_rollback_reports_both_halves() {
        let report = RollbackReport {
            found: 3,
            restored: vec![TransactionId::new()],
            declined: Some(DeclinedRollback {
                transaction_id: TransactionId::new(),
                reason: "not newest".into(),
            }),
            ..RollbackReport::default()
        };
        let line = summary(&report).unwrap();
        assert!(line.contains("rolled back 1/3"), "{line}");
        assert!(line.contains("not newest"), "{line}");
        assert!(line.contains("as it stands"), "{line}");
    }

    /// The journal named nothing but the DAG says an edit node succeeded:
    /// silence would be a lie — the edit is on disk and is not coming back.
    #[test]
    fn an_unjournaled_edit_is_named_even_though_nothing_was_found() {
        let report = RollbackReport {
            found: 0,
            unjournaled_edits: 1,
            ..RollbackReport::default()
        };
        let line = summary(&report).expect("silence would hide an applied edit");
        assert!(line.contains("1 edit node"), "{line}");
        assert!(line.contains("as it stands"), "{line}");
    }

    /// The retry's stale-probe shortcut may only stand when the pass neither
    /// restored nor attempted-and-failed a restore. A refusal is not proof the
    /// tree is untouched: `RollbackFailed` and the post-restore digest check
    /// are raised *after* the restore ran.
    #[test]
    fn only_an_untouched_tree_keeps_the_probes_diagnostics() {
        assert!(probe_still_describes_tree(&RollbackReport::default()));
        assert!(probe_still_describes_tree(&RollbackReport {
            found: 0,
            unjournaled_edits: 2,
            ..RollbackReport::default()
        }));
        assert!(!probe_still_describes_tree(&RollbackReport {
            found: 1,
            restored: vec![TransactionId::new()],
            ..RollbackReport::default()
        }));
        assert!(!probe_still_describes_tree(&RollbackReport {
            found: 1,
            restored: vec![],
            declined: Some(DeclinedRollback {
                transaction_id: TransactionId::new(),
                reason: "rollback failed".into(),
            }),
            ..RollbackReport::default()
        }));
        assert!(!probe_still_describes_tree(&RollbackReport {
            found: 2,
            restored: vec![TransactionId::new()],
            declined: Some(DeclinedRollback {
                transaction_id: TransactionId::new(),
                reason: "rollback failed".into(),
            }),
            ..RollbackReport::default()
        }));
    }
}
