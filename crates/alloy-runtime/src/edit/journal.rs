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

use crate::dag::{NodeKind, NodeState, TaskDag};
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
    /// Succeeded `Edit` nodes the run's DAG records while the journal named
    /// no transaction at all — edits that are on disk and cannot be undone.
    ///
    /// Always `0` when [`Self::found`] is non-zero, or when
    /// [`rollback_run_edits`] was given no DAG to cross-check against.
    pub unjournaled_edits: usize,
}

/// Succeeded `Edit` nodes in `dag`.
///
/// A succeeded `Edit` node means `apply_patch` ran with `dry_run: false` and
/// the tool did not report an error (RFC-0014 EW7, `capabilities::workers::edit`),
/// so the edit engine committed a transaction for it. Cross-checking that
/// count against the journal is the only way to notice a *lost* `EditApplied`:
/// the append happens after the commit point and is deliberately best-effort
/// (`alloy-tools` `edit::engine`, "EditApplied append failed after commit"),
/// so a sink failure or a missing `session_id` drops the event while the edit
/// stays on disk.
#[must_use]
fn succeeded_edit_nodes(dag: &TaskDag) -> usize {
    dag.nodes
        .values()
        .filter(|n| n.kind == NodeKind::Edit && n.state == NodeState::Succeeded)
        .count()
}

/// Undo every edit `run` applied, newest-first, so a caller can start again
/// from the pre-run workspace.
///
/// Newest-first is not a preference: only the newest committed transaction
/// is rollback-eligible (RFC-0008 §5.11), and undoing it makes its
/// predecessor the newest in turn.
///
/// Pass `dag` — the run's DAG, as it stands after the failure — to have the
/// pass notice edits the journal never recorded; see
/// [`RollbackReport::unjournaled_edits`].
///
/// **Never fails.** A refusal — the workspace drifted under the engine, the
/// transaction is no longer eligible, git is unavailable — stops the pass
/// and is reported, not raised. Failing the caller instead would turn "we
/// could not undo an edit" into "your run died", which is strictly worse for
/// anyone whose editor happened to save a file mid-run.
///
/// **The report is not a promise about the tree.** Most refusals are
/// pre-flight (eligibility, drift, denied paths) and leave the workspace
/// exactly as they found it, but `EditError::RollbackFailed` and a failed
/// post-restore digest check (`alloy-tools` `edit::engine::rollback_record`)
/// are raised *after* the restore was attempted, so the tree may sit between
/// its pre- and post-edit states. A caller holding stale knowledge of the
/// workspace — cached diagnostics, a prior probe — must therefore re-derive
/// it whenever [`RollbackReport::restored`] is non-empty **or**
/// [`RollbackReport::declined`] is set.
pub async fn rollback_run_edits(
    engine: &dyn EditEngine,
    events: &[SessionEvent],
    run: RunId,
    dag: Option<&TaskDag>,
    ctx: &EditContext,
) -> RollbackReport {
    let transactions = transactions_of_run(events, run);
    let mut report = RollbackReport {
        found: transactions.len(),
        ..RollbackReport::default()
    };
    if transactions.is_empty() {
        report.unjournaled_edits = dag.map_or(0, succeeded_edit_nodes);
        if report.unjournaled_edits > 0 {
            tracing::warn!(
                edit_nodes = report.unjournaled_edits,
                run = %run,
                "edit node(s) succeeded but the session log named no edit \
                 transaction; those edits cannot be rolled back"
            );
        }
    }
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
    use crate::dag::{Backoff, RetryPolicy, TaskNode};
    use crate::edit::error::EditError;
    use crate::edit::types::{EditRequest, EditTransaction, EditValidation};
    use crate::events::NewSessionEvent;
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::ids::ProfileId;
    use crate::types::ids::{ArtifactId, DagId, NodeId};
    use crate::types::ids::{EventSeq, SessionId, Timestamp};
    use crate::types::permission::PermissionToken;
    use crate::DagState;
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
        let report = rollback_run_edits(&engine, &events, run, None, &edit_ctx(run)).await;
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
        let report = rollback_run_edits(&engine, &events, run, None, &edit_ctx(run)).await;
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
        let report = rollback_run_edits(&engine, &[], run, None, &edit_ctx(run)).await;
        assert!(engine.seen().is_empty());
        assert_eq!(report, RollbackReport::default());
    }

    /// The partial case the review asked about: the newest transaction comes
    /// back, the one under it is refused. Both halves must be on the report —
    /// a caller that only sees the refusal would think the tree was untouched.
    #[tokio::test]
    async fn a_partial_pass_reports_both_halves() {
        let run = RunId::new();
        let (a, b) = (TransactionId::new(), TransactionId::new());
        let events = vec![edit(1, Some(run), a), edit(2, Some(run), b)];
        let engine = FakeEngine::new(vec![a]);
        let report = rollback_run_edits(&engine, &events, run, None, &edit_ctx(run)).await;
        assert_eq!(engine.seen(), vec![b, a]);
        assert_eq!(report.found, 2);
        assert_eq!(report.restored, vec![b], "the newest one did come back");
        assert_eq!(
            report.declined.map(|d| d.transaction_id),
            Some(a),
            "and the one under it did not"
        );
    }

    fn dag_with_edit_node(state: NodeState) -> TaskDag {
        let node = TaskNode {
            id: NodeId::new(),
            kind: NodeKind::Edit,
            capability: None,
            input_ref: ArtifactId::new(),
            output_ref: None,
            state,
            retry: RetryPolicy {
                max_attempts: 1,
                backoff: Backoff::Fixed { delay_ms: 0 },
                retry_on: vec![],
                escalate_after: None,
                escalate_to_tier: None,
            },
            cache_key: None,
            budget: TokenBudget {
                max_input: 1,
                max_output: 1,
            },
            model_tier: ModelTier::Standard,
            approval: None,
            timeout_ms: 1000,
        };
        TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: std::iter::once((node.id, node)).collect(),
            edges: vec![],
            state: DagState::Failed,
        }
    }

    /// A succeeded `Edit` node means `apply_patch` committed (EW7), so a run
    /// whose journal names no transaction has lost an `EditApplied` append —
    /// the edit is on disk and unrollbackable. Say so rather than reporting a
    /// silent no-op.
    #[tokio::test]
    async fn a_succeeded_edit_node_with_an_empty_journal_is_flagged() {
        let run = RunId::new();
        let engine = FakeEngine::new(vec![]);
        let dag = dag_with_edit_node(NodeState::Succeeded);
        let report = rollback_run_edits(&engine, &[], run, Some(&dag), &edit_ctx(run)).await;
        assert_eq!(report.found, 0);
        assert_eq!(report.unjournaled_edits, 1);
    }

    /// An edit node that did not succeed applied nothing, so an empty journal
    /// is the truth, not a lost append.
    #[tokio::test]
    async fn an_unsucceeded_edit_node_is_not_flagged() {
        let run = RunId::new();
        let engine = FakeEngine::new(vec![]);
        let dag = dag_with_edit_node(NodeState::Failed);
        let report = rollback_run_edits(&engine, &[], run, Some(&dag), &edit_ctx(run)).await;
        assert_eq!(report.unjournaled_edits, 0);
    }

    /// The flag is about a journal that named *nothing*: once a transaction is
    /// on the report, the rollback path is the honest signal.
    #[tokio::test]
    async fn a_journalled_run_is_never_flagged() {
        let run = RunId::new();
        let a = TransactionId::new();
        let events = vec![edit(1, Some(run), a)];
        let engine = FakeEngine::new(vec![]);
        let dag = dag_with_edit_node(NodeState::Succeeded);
        let report = rollback_run_edits(&engine, &events, run, Some(&dag), &edit_ctx(run)).await;
        assert_eq!(report.restored, vec![a]);
        assert_eq!(report.unjournaled_edits, 0);
    }
}
