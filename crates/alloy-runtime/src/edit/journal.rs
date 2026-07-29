//! Reading a run's applied edit transactions back out of the session log,
//! and undoing them.
//!
//! The `EditEngine` transaction store is in-process and private; the durable
//! record of "what did this run change" is the `edit_applied` session event
//! stream (RFC-0008 §9.3). Anything that wants to undo a run's edits —
//! notably the CLI's retry loop — needs that list, so the projection lives
//! here next to the payload type rather than in a driver.
//!
//! [`rollback_run_edits`] is the composed operation on top of it, and lives
//! here for the same reason [`seed_graph_diagnostics`] does: RFC-0015 rule
//! B5 forbids `alloy-cli` from calling `EditEngine::rollback` itself, so the
//! runtime owns the mechanic and drivers ask for the outcome.
//!
//! [`seed_graph_diagnostics`]: crate::seed_graph_diagnostics
//!
//! Author: arkadianet

use crate::edit::engine::EditEngine;
use crate::edit::types::EditContext;
use crate::events::{SessionEvent, SessionEventType};
use crate::types::ids::{RunId, TransactionId};

/// Transaction ids of the `edit_applied` events `run` recorded, in log
/// (oldest-first) order, deduplicated.
///
/// Events from other runs and malformed or unparseable payloads are skipped:
/// a projection over an append-only log must degrade to "fewer transactions",
/// never to an error.
///
/// Rollback eligibility is newest-first (RFC-0008 §5.11), so callers undoing
/// a run iterate the returned slice in reverse.
#[must_use]
pub fn transactions_of_run(events: &[SessionEvent], run: RunId) -> Vec<TransactionId> {
    let mut out: Vec<TransactionId> = Vec::new();
    for event in events {
        if event.type_ != SessionEventType::EditApplied || event.run_id != Some(run) {
            continue;
        }
        let Some(id) = event
            .payload
            .get("transaction_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| TransactionId::parse(raw).ok())
        else {
            continue;
        };
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// One declined rollback: the engine refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclinedRollback {
    /// The transaction the engine would not undo.
    pub transaction_id: TransactionId,
    /// The engine's reason, rendered for the operator and the log.
    pub reason: String,
}

/// What one [`rollback_run_edits`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackReport {
    /// Transactions the run's `edit_applied` events named.
    pub found: usize,
    /// Transactions the engine restored, in the order they were undone
    /// (newest-first).
    pub restored: Vec<TransactionId>,
    /// The refusal that stopped the pass, when there was one.
    pub declined: Option<DeclinedRollback>,
}

/// Undo every edit `run` applied, newest-first, so a caller can start again
/// from the pre-run workspace.
///
/// Newest-first is not a preference: only the newest committed transaction
/// is rollback-eligible (RFC-0008 §5.11), and undoing it makes its
/// predecessor the newest in turn.
///
/// **Never fails.** A refusal — the workspace drifted under the engine, the
/// transaction is no longer eligible, git is unavailable — stops the pass
/// and is reported, not raised. The engine has already verified that a
/// refused rollback left the tree untouched, so the caller's honest move is
/// to continue against the tree as it stands and say so. Failing the caller
/// instead would turn "we could not undo an edit" into "your run died",
/// which is strictly worse for anyone whose editor happened to save a file
/// mid-run.
pub async fn rollback_run_edits(
    engine: &dyn EditEngine,
    events: &[SessionEvent],
    run: RunId,
    ctx: &EditContext,
) -> RollbackReport {
    let transactions = transactions_of_run(events, run);
    let mut report = RollbackReport {
        found: transactions.len(),
        ..RollbackReport::default()
    };
    for tx in transactions.iter().rev().copied() {
        match engine.rollback(tx, ctx).await {
            Ok(()) => report.restored.push(tx),
            Err(err) => {
                let reason = err.to_string();
                tracing::warn!(
                    error = %reason,
                    tx = %tx,
                    "edit rollback declined; the workspace stands as it is"
                );
                report.declined = Some(DeclinedRollback {
                    transaction_id: tx,
                    reason,
                });
                // An older transaction cannot become eligible while a newer
                // one still stands: stop rather than pile up refusals.
                break;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::error::EditError;
    use crate::edit::types::{EditRequest, EditTransaction, EditValidation};
    use crate::events::NewSessionEvent;
    use crate::types::ids::ProfileId;
    use crate::types::ids::{EventSeq, SessionId, Timestamp};
    use crate::types::permission::PermissionToken;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn event(
        seq: u64,
        run: Option<RunId>,
        type_: SessionEventType,
        payload: serde_json::Value,
    ) -> SessionEvent {
        let session_id = SessionId::new();
        // Keep the envelope shape honest by round-tripping the "new" form.
        let new = NewSessionEvent {
            session_id,
            run_id: run,
            type_,
            payload,
        };
        SessionEvent {
            seq: EventSeq(seq),
            ts: Timestamp::now(),
            session_id: new.session_id,
            run_id: new.run_id,
            type_: new.type_,
            payload: new.payload,
        }
    }

    fn edit(seq: u64, run: Option<RunId>, tx: TransactionId) -> SessionEvent {
        event(
            seq,
            run,
            SessionEventType::EditApplied,
            serde_json::json!({ "transaction_id": tx.to_string() }),
        )
    }

    #[test]
    fn returns_only_this_run_in_log_order() {
        let run = RunId::new();
        let other = RunId::new();
        let (a, b, c) = (
            TransactionId::new(),
            TransactionId::new(),
            TransactionId::new(),
        );
        let events = vec![
            edit(1, Some(run), a),
            edit(2, Some(other), c),
            edit(3, Some(run), b),
        ];
        assert_eq!(transactions_of_run(&events, run), vec![a, b]);
        assert_eq!(transactions_of_run(&events, other), vec![c]);
    }

    #[test]
    fn deduplicates_repeats() {
        let run = RunId::new();
        let a = TransactionId::new();
        let events = vec![edit(1, Some(run), a), edit(2, Some(run), a)];
        assert_eq!(transactions_of_run(&events, run), vec![a]);
    }

    /// A projection over an append-only log degrades, never errors: other
    /// event types, unattributed edits and malformed payloads are skipped.
    #[test]
    fn skips_unusable_events() {
        let run = RunId::new();
        let good = TransactionId::new();
        let events = vec![
            event(
                1,
                Some(run),
                SessionEventType::ToolCall,
                serde_json::json!({ "transaction_id": TransactionId::new().to_string() }),
            ),
            edit(2, None, TransactionId::new()),
            event(
                3,
                Some(run),
                SessionEventType::EditApplied,
                serde_json::json!({ "transaction_id": "not-a-uuid" }),
            ),
            event(
                4,
                Some(run),
                SessionEventType::EditApplied,
                serde_json::json!({ "files_touched": [] }),
            ),
            edit(5, Some(run), good),
        ];
        assert_eq!(transactions_of_run(&events, run), vec![good]);
    }

    #[test]
    fn empty_when_the_run_edited_nothing() {
        assert!(transactions_of_run(&[], RunId::new()).is_empty());
    }

    /// Records the rollback order and refuses the transactions it was told
    /// to refuse.
    struct FakeEngine {
        seen: Mutex<Vec<TransactionId>>,
        refuse: Vec<TransactionId>,
    }

    impl FakeEngine {
        fn new(refuse: Vec<TransactionId>) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                refuse,
            }
        }

        fn seen(&self) -> Vec<TransactionId> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EditEngine for FakeEngine {
        async fn validate(
            &self,
            _req: EditRequest,
            _ctx: &EditContext,
        ) -> Result<EditValidation, EditError> {
            unreachable!("rollback_run_edits never validates")
        }

        async fn apply(
            &self,
            _req: EditRequest,
            _ctx: &EditContext,
        ) -> Result<EditTransaction, EditError> {
            unreachable!("rollback_run_edits never applies")
        }

        async fn rollback(&self, tx: TransactionId, _ctx: &EditContext) -> Result<(), EditError> {
            self.seen.lock().unwrap().push(tx);
            if self.refuse.contains(&tx) {
                return Err(EditError::WorkspaceDrifted(tx));
            }
            Ok(())
        }
    }

    fn edit_ctx(run: RunId) -> EditContext {
        EditContext {
            session_id: None,
            run_id: Some(run),
            perms: PermissionToken {
                profile: ProfileId::new("default").unwrap(),
                grants: vec![],
                expires: None,
                run_id: run,
            },
        }
    }

    #[tokio::test]
    async fn undoes_newest_first() {
        let run = RunId::new();
        let (a, b) = (TransactionId::new(), TransactionId::new());
        let events = vec![edit(1, Some(run), a), edit(2, Some(run), b)];
        let engine = FakeEngine::new(vec![]);
        let report = rollback_run_edits(&engine, &events, run, &edit_ctx(run)).await;
        assert_eq!(engine.seen(), vec![b, a], "eligibility is newest-first");
        assert_eq!(report.found, 2);
        assert_eq!(report.restored, vec![b, a]);
        assert_eq!(report.declined, None);
    }

    /// A refusal stops the pass — an older transaction cannot be eligible
    /// while a newer one still stands — and is reported, never raised.
    #[tokio::test]
    async fn a_refusal_stops_the_pass_and_is_reported() {
        let run = RunId::new();
        let (a, b) = (TransactionId::new(), TransactionId::new());
        let events = vec![edit(1, Some(run), a), edit(2, Some(run), b)];
        let engine = FakeEngine::new(vec![b]);
        let report = rollback_run_edits(&engine, &events, run, &edit_ctx(run)).await;
        assert_eq!(engine.seen(), vec![b], "the older tx must not be attempted");
        assert_eq!(report.found, 2);
        assert!(report.restored.is_empty());
        let declined = report.declined.expect("refusal reported");
        assert_eq!(declined.transaction_id, b);
        assert!(!declined.reason.is_empty());
    }

    #[tokio::test]
    async fn a_run_that_edited_nothing_is_a_no_op() {
        let run = RunId::new();
        let engine = FakeEngine::new(vec![]);
        let report = rollback_run_edits(&engine, &[], run, &edit_ctx(run)).await;
        assert!(engine.seen().is_empty());
        assert_eq!(report, RollbackReport::default());
    }
}
