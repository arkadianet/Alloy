//! `reconcile_terminal_run` (RFC-0010 §5.20, RC1-RC8, amendments A2/A6).
//!
//! Closes the crash window where the **control plane** row already reached
//! a terminal state (gate deny/expiry writes `RunControlState::Failed`
//! directly — see `RunController::approve`/`expire_gate`) but the **DAG**
//! blob is still non-terminal because the live scheduler never observed
//! that resolution (it crashed, or the resolution landed after the
//! scheduler had already moved on). Nothing else terminalizes the DAG for a
//! run row that's already terminal — `start` refuses a terminal row, so
//! without this, the DAG would simply wedge non-terminal forever.

use super::checkpoint::{CheckpointCtx, GateDecision};
use super::loop_::{cancel_targets, non_terminal_except};
use super::LinearScheduler;
use crate::dag::{NodeKind, NodeState, TaskDag};
use crate::error::SchedError;
use crate::scheduler::DagState;
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{DagId, NodeId};

/// Lowest non-terminal `NodeId` (FN1's tie-break for non-gate attribution).
fn lowest_non_terminal(dag: &TaskDag) -> Option<NodeId> {
    dag.nodes
        .values()
        .filter(|n| {
            matches!(
                n.state,
                NodeState::Pending
                    | NodeState::Ready
                    | NodeState::Running
                    | NodeState::WaitingApproval
            )
        })
        .map(|n| n.id)
        .min()
}

/// RC4's gate-origin predicate is stated as two disjuncts: a `GateHuman`
/// node still durably `WaitingApproval`, **or** a generation-matched
/// durable `deny`/`expired` `ApprovalResolved` exists. Only the first is
/// implemented, and that is a completeness argument, not a narrowing:
/// every write that can move a gate node *out* of `WaitingApproval`
/// (`c9c_gate_deny`, the allow-fold path) sets `DagState` to a terminal-ish
/// value in the **same** CAS. So a durable `deny`/`expired` resolution with
/// no matching `WaitingApproval` node implies the DAG already reflects it —
/// `DagState` would already be terminal, and RC2 (checked before this ever
/// runs) returns `Ok(())` for that case before RC4 is reached. The second
/// disjunct is therefore unreachable given this scheduler's actual write
/// invariants, not merely rare; `c9c_gate_deny` also *requires*
/// `WaitingApproval` as a precondition, so implementing the second disjunct
/// would need its own, different write path for a state this design cannot
/// produce.
fn waiting_gate_node(dag: &TaskDag) -> Option<NodeId> {
    dag.nodes
        .values()
        .find(|n| n.kind == NodeKind::GateHuman && n.state == NodeState::WaitingApproval)
        .map(|n| n.id)
}

fn is_dag_terminal(state: DagState) -> bool {
    matches!(
        state,
        DagState::Succeeded | DagState::Failed | DagState::Cancelled
    )
}

impl LinearScheduler {
    pub(super) async fn reconcile_terminal_run_impl(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError> {
        // RC1
        if !matches!(
            terminal,
            DagState::Succeeded | DagState::Failed | DagState::Cancelled
        ) {
            return Err(SchedError::Config(
                "reconcile_terminal_run requires a terminal state".into(),
            ));
        }

        // RC2 (pre-ownership probe): cheap enough to skip acquiring
        // ownership at all for the overwhelmingly common already-terminal
        // case (every subsequent `resume` call for a row already reconciled).
        let probe = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| super::checkpoint::map_store_error_on_load(e, dag_id))?;
        let Some(probe) = probe else {
            return Err(SchedError::DagNotFound(dag_id));
        };
        if is_dag_terminal(probe.state) {
            return Ok(());
        }

        // RC3: a live run in this process owns terminalization.
        if self.lookup_owned(dag_id)?.is_some() {
            return Ok(());
        }

        // RC4/RC8: transient ownership — occupied means a `run()` or another
        // `cancel`/`reconcile_terminal_run` call won the race; whichever
        // acquired first owns terminalization, this call observes it and
        // returns `Ok(())` (RC8).
        let guard = match self.try_acquire_dag(dag_id, None, probe.session_id) {
            Ok(g) => g,
            Err(SchedError::AlreadyOwned(_)) => return Ok(()),
            Err(e) => return Err(e),
        };

        let result = self.reconcile_owned_body(dag_id, terminal).await;
        guard.owned.set_cancel_result(match &result {
            Ok(()) => Ok(terminal),
            Err(e) => Err(e.clone()),
        });
        result
    }

    async fn reconcile_owned_body(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError> {
        // RC4: re-load under ownership — closes the window between the
        // unowned probe and acquiring ownership.
        let Some(mut dag) = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| super::checkpoint::map_store_error_on_load(e, dag_id))?
        else {
            return Err(SchedError::DagNotFound(dag_id));
        };
        if is_dag_terminal(dag.state) {
            return Ok(()); // RC2/RC8: someone else terminalized it first.
        }

        let checkpoint = self.checkpoint();
        let ctx = CheckpointCtx {
            session_id: dag.session_id,
            run_id: None, // reconcile is not run-attributed (A2: not scheduler-aware caller).
        };

        match terminal {
            DagState::Cancelled => {
                let (cancelled, skipped) = cancel_targets(&dag);
                checkpoint
                    .c6_cancel(&mut dag, ctx, &cancelled, &skipped)
                    .await?;
                Ok(())
            }
            DagState::Succeeded => {
                // RC6: never invent success — always resolves to `Failed`.
                tracing::warn!(
                    dag_id = %dag_id,
                    "reconcile_terminal_run(Succeeded) requested with a non-terminal DAG; \
                     writing Failed instead (RC6)"
                );
                self.reconcile_failed(
                    &checkpoint,
                    &mut dag,
                    ctx,
                    "control row succeeded with unfinished nodes",
                )
                .await
            }
            DagState::Failed => {
                self.reconcile_failed(&checkpoint, &mut dag, ctx, "reconciled after crash")
                    .await
            }
            DagState::Pending
            | DagState::Running
            | DagState::WaitingApproval
            | DagState::ReplanRequired => unreachable!("RC1 already validated `terminal`"),
        }
    }

    /// RC4/RC5 `Failed` write: gate-origin (`Cancelled` + `Approval`, FN2)
    /// when a `GateHuman` node is still `WaitingApproval`; otherwise
    /// non-gate (`Failed` + `Internal`, FN1) attributed to the lowest
    /// non-terminal `NodeId`, or a bare CAS with `bare_notes` when nothing
    /// non-terminal remains.
    async fn reconcile_failed(
        &self,
        checkpoint: &super::checkpoint::Checkpoint,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        bare_notes: &str,
    ) -> Result<(), SchedError> {
        if let Some(gate_node_id) = waiting_gate_node(dag) {
            let skipped = non_terminal_except(dag, gate_node_id);
            let failure = FailureIr {
                node: gate_node_id,
                error_class: ErrorClass::Approval,
                retry: RetryDisposition::NonRetryable,
                diagnostics: vec![],
                notes: "reconciled: gate resolution never observed by a live run".into(),
            };
            checkpoint
                .c9c_gate_deny(
                    dag,
                    ctx,
                    gate_node_id,
                    GateDecision::Expired,
                    &failure,
                    &skipped,
                )
                .await?;
            return Ok(());
        }

        match lowest_non_terminal(dag) {
            None => {
                checkpoint
                    .c_reconcile_bare(dag, ctx, DagState::Failed, bare_notes)
                    .await
            }
            Some(target) => {
                let skipped = non_terminal_except(dag, target);
                let failure = FailureIr {
                    node: target,
                    error_class: ErrorClass::Internal,
                    retry: RetryDisposition::NonRetryable,
                    diagnostics: vec![],
                    notes: "reconciled after crash: no observed cause".into(),
                };
                checkpoint
                    .c7_terminal_failed(dag, ctx, target, None, &failure, &skipped)
                    .await?;
                Ok(())
            }
        }
    }
}
