//! The serial scheduling loop (RFC-0010 §5.1 R1-R18, §5.2 L1-L16, §5.6
//! dispatch table, §5.9 success path, §5.14 `Aggregate` fold).
//!
//! [`LinearScheduler::run`]/[`LinearScheduler::cancel`] implement the
//! [`crate::Scheduler`] trait here. The surrounding machinery lives in
//! sibling modules: `retry.rs` (pure §5.11 admission), `gate.rs` (§5.7's
//! full state machine, including the deadline/expiry and durable-resolution
//! resume scans this module routes into from R13/R16), `own.rs` (§4.3-4.4
//! ownership and the race-free cancel wait), `budget.rs` (§5.16.1 effective
//! ceilings), and `checkpoint.rs` (every CAS plus the §5.3.3 crash repairs).
//!
//! R15 implements ER4/ER5 (§6.5): `needs_reverify` is a *derived* predicate
//! over node states and Data∪Sequence reachability (`ready.rs`), not a stored
//! `TaskNode` field, so it needs nothing from RFC-0008 and stays inside ER3's
//! boundary — the scheduler never touches the edit stack.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::budget;
use super::checkpoint::{map_store_error, map_store_error_on_load, Checkpoint, CheckpointCtx};
use super::envelopes::{self, InputShape};
use super::gate::GateResolution;
use super::ready::{
    backoff_delay, derive_dag_state, er4_blocked_kind, needs_reverify, promotable_nodes,
    ready_nodes, verifies_reachable_from_succeeded_edits, DeriveFlags,
};
use super::retry::{self, Admission, Escalation};
use super::LinearScheduler;
use crate::adapters::{
    CapabilityExecContext, CapabilityExecError, CapabilityOutcome, NodeExecContext, NodeExecRef,
    VerifyOutcome,
};
use crate::dag::{DagValidator, NodeKind, NodeOutputEnvelope, NodeState, TaskDag};
use crate::error::{AdapterError, SchedError};
use crate::obs::{
    maybe_signal_budget_warning, reaccumulate_cost_from_events, DecisionKind, DecisionRecord,
};
use crate::scheduler::{DagOutcome, DagState, Scheduler};
use crate::session::{RunControlState, RunGoalRecord, Session};
use crate::storage::RunRow;
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{DagId, GateId, NodeId, RunId};

/// What a single loop iteration produced.
///
/// `pub(super)`: shared with `gate.rs`, a sibling module under
/// `scheduler::linear` that implements the §5.7 state machine.
pub(super) enum StepOutcome {
    /// Keep looping.
    Continue,
    /// Leave the loop: derive the terminal state naturally (§5.17) and
    /// commit C7 (R18).
    NaturalExit,
    /// Already terminalized (cancel/replan/budget/timeout/durable failure);
    /// return this outcome directly.
    Terminal(DagOutcome),
}

/// Per-`run` state threaded through the loop.
///
/// `pub(super)`: shared with `gate.rs` (see [`StepOutcome`]).
pub(super) struct RunCtx<'a> {
    pub(super) checkpoint: &'a Checkpoint,
    pub(super) ctx: CheckpointCtx,
    pub(super) session: &'a Session,
    pub(super) run_cancel: &'a CancellationToken,
    pub(super) run_id: RunId,
    pub(super) run_started: Instant,
    pub(super) run_timeout: Duration,
    /// §5.16.1 BG1-BG6: folded once at R8, before any budget check or
    /// dispatch (B7). Every enforcement point (L6, A5, the post-node
    /// warning) uses this, never `deps.budget_policy` directly.
    pub(super) effective_budget: crate::types::budget::BudgetPolicy,
    /// §5.19 T1: wall time spent inside gate waits, excluded from the
    /// charged run elapsed. `std::sync::Mutex` (not `Cell`) because `&RunCtx`
    /// crosses `.await` points and the held future must stay `Send`, which
    /// requires `RunCtx: Sync`; only the gate route mutates it.
    gate_wait_total: std::sync::Mutex<Duration>,
    /// §5.7.8 GT3: `(run_id, gate_id)` keys this `run` invocation has already
    /// called `expire_gate` for (successfully, or found durably expired, or
    /// exhausted retries and self-terminalized). Process-local and scoped to
    /// this one `run` call, not durable — matches GT3's "in-run set" wording.
    pub(super) expired_gates: std::sync::Mutex<std::collections::HashSet<GateId>>,
    /// §5.7.9 `GATE_REREGISTER_MAX`: per-gate re-registration attempts
    /// consumed so far this `run` invocation.
    gate_reregister_counts: std::sync::Mutex<std::collections::HashMap<GateId, u32>>,
    /// B4: nodes whose retry backoff this `run` invocation has already slept
    /// (§5.11.3). Backoff elapsed time is not durable, so on resume a `Ready`
    /// node with `attempts_started >= 1` must re-wait the full delay before
    /// C3. Membership here is what distinguishes "this process already served
    /// the wait, in `apply_soft_failure`" from "this attempt counter came off
    /// the event log and we owe the wait" — without it the in-loop retry path
    /// would sleep twice per retry.
    backoff_served: std::sync::Mutex<std::collections::HashSet<NodeId>>,
}

impl RunCtx<'_> {
    fn gate_wait_total(&self) -> Duration {
        *self
            .gate_wait_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn add_gate_wait(&self, delta: Duration) {
        let mut g = self
            .gate_wait_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g += delta;
    }

    /// §5.19: `remaining_run = run_timeout - (elapsed - gate_wait_total)`.
    pub(super) fn remaining_run(&self) -> Duration {
        let elapsed_charged = self
            .run_started
            .elapsed()
            .saturating_sub(self.gate_wait_total());
        self.run_timeout.saturating_sub(elapsed_charged)
    }

    /// GT3: has `(run_id, gate_id)` already had its expiry resolved (by
    /// `expire_gate`, a durable scan, or local exhaustion) during this `run`?
    pub(super) fn gate_already_expired(&self, gate: GateId) -> bool {
        self.expired_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&gate)
    }

    /// GT3: record that `(run_id, gate_id)` has had its expiry resolved.
    pub(super) fn mark_gate_expired(&self, gate: GateId) {
        self.expired_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(gate);
    }

    /// B4: record that this `run` has slept `node`'s retry backoff, so the
    /// dispatch that follows does not re-serve it.
    fn mark_backoff_served(&self, node: NodeId) {
        self.backoff_served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(node);
    }

    /// B4: has this `run` already slept `node`'s retry backoff?
    fn backoff_already_served(&self, node: NodeId) -> bool {
        self.backoff_served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&node)
    }

    /// §5.7.9 `GATE_REREGISTER_MAX`: consume one re-registration attempt for
    /// `gate`, returning `true` if one was available (caller may
    /// re-register) or `false` once the bound is exhausted (caller MUST
    /// fall back to `Internal`).
    pub(super) fn try_consume_gate_reregister(&self, gate: GateId) -> bool {
        const GATE_REREGISTER_MAX: u32 = 3;
        let mut counts = self
            .gate_reregister_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = counts.entry(gate).or_insert(0);
        if *count >= GATE_REREGISTER_MAX {
            false
        } else {
            *count += 1;
            true
        }
    }
}

#[async_trait]
impl Scheduler for LinearScheduler {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        self.run_impl(dag_id).await
    }

    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError> {
        self.cancel_impl(dag_id).await
    }

    async fn reconcile_terminal_run(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError> {
        self.reconcile_terminal_run_impl(dag_id, terminal).await
    }
}

impl LinearScheduler {
    pub(super) fn checkpoint(&self) -> Checkpoint {
        Checkpoint::new(
            Arc::clone(&self.deps.dags),
            Arc::clone(&self.deps.artifacts),
            Arc::clone(&self.deps.events),
            Arc::clone(&self.metrics),
        )
    }

    // -----------------------------------------------------------------
    // §5.1 `run` entry sequence
    // -----------------------------------------------------------------

    async fn run_impl(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        let checkpoint = self.checkpoint();

        // R1
        let dag = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| map_store_error_on_load(e, dag_id))?
            .ok_or(SchedError::DagNotFound(dag_id))?;

        // R2
        if self.deps.config.validate_on_load {
            DagValidator::validate(&dag, self.deps.config.validate_opts).map_err(|e| {
                SchedError::Invariant(format!("dag {dag_id} failed load validation: {e}"))
            })?;
        }

        // R3
        let run_row = self.resolve_run_binding(&dag).await?;
        let run_id = run_row.id;
        let session_id = dag.session_id;

        // R4: real DAG ownership (§4.3-4.4) — `AlreadyOwned` if contended.
        let guard = self.try_acquire_dag(dag_id, Some(run_id), session_id)?;
        self.metrics.inc_runs_started();

        // O3: `cancel_result` MUST be written before `guard` drops (notifying
        // any waiting `cancel`), on every exit — success, planned terminal,
        // or `Err`. Run the owned body to completion first, capture its
        // `Result` without an early `?`, write the result, THEN return it —
        // `guard` drops at the end of this function's scope either way.
        let result = self
            .run_owned(&checkpoint, dag_id, run_id, &guard.owned)
            .await;
        if let Ok(outcome) = &result {
            self.metrics.inc_run_terminal(outcome.state);
        }
        guard.owned.set_cancel_result(match &result {
            Ok(outcome) => Ok(outcome.state),
            Err(e) => Err(e.clone()),
        });
        result
    }

    /// R4b-R18: the owned body of `run`. Reloads the DAG under ownership and
    /// runs it to a terminal `DagOutcome` or a propagated `SchedError`.
    /// Every return here is captured by [`Self::run_impl`]'s O3 chokepoint —
    /// this function itself MUST NOT touch `cancel_result`.
    #[tracing::instrument(
        name = "sched.run",
        skip_all,
        fields(
            dag_id = %dag_id,
            run_id = %run_id,
            session_id = tracing::field::Empty,
            generation = tracing::field::Empty,
        )
    )]
    async fn run_owned(
        &self,
        checkpoint: &Checkpoint,
        dag_id: DagId,
        run_id: RunId,
        owned: &super::own::OwnedDag,
    ) -> Result<DagOutcome, SchedError> {
        // R4b: re-load under ownership.
        let mut dag = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| map_store_error_on_load(e, dag_id))?
            .ok_or(SchedError::DagNotFound(dag_id))?;
        tracing::Span::current().record("generation", dag.generation);
        if self.deps.config.validate_on_load
            && !matches!(
                dag.state,
                DagState::Succeeded
                    | DagState::Failed
                    | DagState::Cancelled
                    | DagState::ReplanRequired
            )
        {
            DagValidator::validate(&dag, self.deps.config.validate_opts).map_err(|e| {
                SchedError::Invariant(format!("dag {dag_id} failed load validation: {e}"))
            })?;
        }

        // R9
        if matches!(
            dag.state,
            DagState::Succeeded | DagState::Failed | DagState::Cancelled
        ) {
            return self
                .assemble_already_terminal_outcome(checkpoint, &dag, run_id)
                .await;
        }
        // R10
        if dag.state == DagState::ReplanRequired {
            return Ok(DagOutcome {
                dag_id,
                generation: dag.generation,
                state: DagState::ReplanRequired,
                failed_node: None,
                failure: None,
            });
        }

        // R5 / PC2: a `cancel(dag_id)` that arrived before this `run()` won
        // ownership left its intent in `pending_cancels`, not on
        // `owned.run_cancel` (that token did not exist yet) — fire it now so
        // L1 stops the loop before dispatching a node, not just eventually
        // via that `cancel_impl` call's own "AlreadyOwned, retry" fallback.
        {
            let was_pending = {
                let mut pending = self
                    .pending_cancels
                    .lock()
                    .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
                pending.remove(&dag_id)
            };
            if was_pending {
                owned.run_cancel.cancel();
            }
        }

        // R6
        let session = self
            .deps
            .sessions
            .get_session(dag.session_id)
            .await
            .map_err(|e| map_store_error(e, dag_id))?
            .ok_or_else(|| {
                SchedError::Invariant(format!("session row missing for dag {dag_id}"))
            })?;
        tracing::Span::current().record("session_id", tracing::field::display(session.id));

        // R8 (rebuild before any budget check, B7/B8).
        let rebuilt = reaccumulate_cost_from_events(&*self.deps.events, session.id, Some(run_id))
            .await
            .map_err(|e| SchedError::Store(e.to_string()))?; // B10
        let meter = self.deps.cost_meters.meter_for(run_id);
        meter.with_mut(|m| *m = rebuilt);

        // §5.16.1 BG1-BG6: fold the effective budget once, before any check.
        let run_row = self
            .deps
            .sessions
            .get_run(run_id)
            .await
            .map_err(|e| map_store_error(e, dag_id))?
            .ok_or_else(|| SchedError::Invariant(format!("run row missing for dag {dag_id}")))?;
        let goal = serde_json::from_value::<RunGoalRecord>(run_row.goal_json)
            .map_err(|e| SchedError::Invariant(format!("corrupt goal_json for run {run_id}: {e}")))?
            .goal;
        let effective = budget::effective_budget(&self.deps.budget_policy, &session.budget, &goal);

        let ctx = CheckpointCtx {
            session_id: session.id,
            run_id: Some(run_id),
        };
        let rc = RunCtx {
            checkpoint,
            ctx,
            session: &session,
            run_cancel: &owned.run_cancel, // O1: fired by `cancel(dag_id)`.
            run_id,
            run_started: Instant::now(), // R12
            run_timeout: self.deps.run_timeout,
            effective_budget: effective.policy.clone(),
            gate_wait_total: std::sync::Mutex::new(Duration::ZERO),
            expired_gates: std::sync::Mutex::new(std::collections::HashSet::new()),
            gate_reregister_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
            backoff_served: std::sync::Mutex::new(std::collections::HashSet::new()),
        };
        // BG1/BG5: record once, now that `rc` exists to record through.
        self.record_budget_ignored(&rc, &effective).await;

        // R13: adopt any node durably Running (crash resume).
        if let Some(outcome) = self.adopt_running(&mut dag, &rc).await? {
            return Ok(outcome);
        }

        // R14
        let performed_c1 = dag.state == DagState::Pending;
        if performed_c1 {
            checkpoint.c1_start(&mut dag).await?;
        }

        // R15 (ER5): resume-only. Skipped when this call performed C1 — a DAG
        // that was still `Pending` never got as far as an edit.
        if !performed_c1 {
            if let Some(outcome) = self.er5_edit_without_verify(&mut dag, &rc).await? {
                return Ok(outcome);
            }
        }

        // R16: resume with a durable WaitingApproval DAG (§5.7.2/§5.7.3).
        if dag.state == DagState::WaitingApproval {
            if let Some(gate_node) = dag
                .nodes
                .values()
                .find(|n| n.state == NodeState::WaitingApproval)
                .map(|n| n.id)
            {
                match self.gate_route(&mut dag, &rc, gate_node, true).await? {
                    StepOutcome::Terminal(outcome) => return Ok(outcome),
                    StepOutcome::Continue | StepOutcome::NaturalExit => {}
                }
            }
        }

        // R16b: a gate left durably `Ready` with the DAG already `Running` is
        // the C9b→C3 crash window: C9b committed `WaitingApproval → Ready` and
        // `DagState → Running`, then the process died before GA4's C3. R16
        // above cannot see it (the DAG is no longer `WaitingApproval`) and
        // `adopt_running` cannot either (no node is `Running`), so without
        // this the loop reaches L13 and takes the *first-schedule* path:
        // a second `ApprovalRequested`, a fresh waiter, and the approval the
        // human already granted is discarded and left to expire. Route it
        // through the resume path so the durable resolution is re-scanned.
        if !performed_c1 {
            let ready_gate = dag
                .nodes
                .values()
                .find(|n| n.kind == NodeKind::GateHuman && n.state == NodeState::Ready)
                .map(|n| n.id);
            if let Some(gate_node) = ready_gate {
                match self.gate_route(&mut dag, &rc, gate_node, true).await? {
                    StepOutcome::Terminal(outcome) => return Ok(outcome),
                    StepOutcome::Continue | StepOutcome::NaturalExit => {}
                }
            }
        }

        // R17
        let outcome = self.loop_run(&mut dag, &rc).await?;
        Ok(outcome)
    }

    /// Appendix F: resolve the run row bound to `dag`.
    async fn resolve_run_binding(&self, dag: &TaskDag) -> Result<RunRow, SchedError> {
        let rows = self
            .deps
            .sessions
            .list_runs(dag.session_id)
            .await
            .map_err(|e| map_store_error(e, dag.id))?;
        let mut matches: Vec<RunRow> = rows
            .into_iter()
            .filter(|r| {
                serde_json::from_value::<RunGoalRecord>(r.goal_json.clone())
                    .ok()
                    .is_some_and(|g| g.dag_id == dag.id)
            })
            .collect();
        if matches.is_empty() {
            return Err(SchedError::RunBindingMissing(dag.id));
        }
        if matches.len() == 1 {
            return Ok(matches.remove(0));
        }
        // RB6: prefer a non-terminal `Running` row, then RB5 ordering
        // (created_at ascending, tie-break run_id ascending — "last" is the
        // maximum under that order).
        let running: Vec<RunRow> = matches
            .iter()
            .filter(|r| RunControlState::parse(&r.state) == Some(RunControlState::Running))
            .cloned()
            .collect();
        let pool = if running.is_empty() {
            let non_terminal: Vec<RunRow> = matches
                .iter()
                .filter(|r| RunControlState::parse(&r.state).is_none_or(|s| !s.is_terminal()))
                .cloned()
                .collect();
            if non_terminal.is_empty() {
                matches
            } else {
                non_terminal
            }
        } else {
            running
        };
        let winner = pool
            .into_iter()
            .max_by(|a, b| (a.created_at.0, a.id).cmp(&(b.created_at.0, b.id)))
            .expect("pool is non-empty");
        Ok(winner)
    }

    /// R9: the DAG is already durably terminal; assemble the outcome from
    /// the persisted blob without any state CAS (§5.18 FO1-FO6).
    ///
    /// `failed_node` follows the §5.18 ladder in order:
    /// - **FN1** — the lowest `NodeId` in `NodeState::Failed`.
    /// - **FN2** — otherwise the lowest `Cancelled` node carrying a durable
    ///   `ErrorClass::Approval` failure. This is the gate deny/expiry shape
    ///   (C9c) and the gate-origin `reconcile_terminal_run` shape (RC4/RC5):
    ///   both leave the *gate* `Cancelled`, never `Failed`, so FN1 alone
    ///   reports `failed_node: None` on every resumed gate-denied DAG and
    ///   breaks the FO4a contract RC4 depends on.
    /// - **FN3** — otherwise `None` (FO4(i) D8 all-`Skipped`, FO4(ii)
    ///   RC4/RC6 bare-CAS reconcile).
    ///
    /// Before FN2 selection this applies **RF7** to every `GateHuman` node
    /// sitting in `Cancelled` (R9's normative "apply RF7 before FO
    /// assembly", AC 92): a gate CAS that committed but lost its `NodeState`
    /// event, or whose `failure_ir` artifact is gone, would otherwise be
    /// invisible to FN2 and degrade to FO3's synthetic `Internal` — which
    /// FN2 rejects, losing the attribution entirely. RF7 is idempotent and
    /// writes nothing when the event and artifact are both intact.
    ///
    /// `failure` is then recovered per FO1/FO2/FO3 from that node's own
    /// terminal transition, so FO6 (`Failed` + `Some(node)` ⇒ `Some(failure)`)
    /// holds on both ladders.
    async fn assemble_already_terminal_outcome(
        &self,
        checkpoint: &Checkpoint,
        dag: &TaskDag,
        run_id: RunId,
    ) -> Result<DagOutcome, SchedError> {
        match dag.state {
            DagState::Succeeded => Ok(DagOutcome {
                dag_id: dag.id,
                generation: dag.generation,
                state: DagState::Succeeded,
                failed_node: None,
                failure: None,
            }),
            // FO5: a plain cancel never attributes a node.
            DagState::Cancelled => Ok(DagOutcome {
                dag_id: dag.id,
                generation: dag.generation,
                state: DagState::Cancelled,
                failed_node: None,
                failure: None,
            }),
            DagState::Failed => {
                let ctx = CheckpointCtx {
                    session_id: dag.session_id,
                    run_id: Some(run_id),
                };
                // `dag.nodes` is a BTreeMap, so iteration is already
                // ascending `NodeId` — "lowest" is the first match.
                let fn1 = dag
                    .nodes
                    .iter()
                    .find(|(_, n)| n.state == NodeState::Failed)
                    .map(|(id, _)| *id);

                let (failed_node, failure) = match fn1 {
                    Some(node_id) => {
                        let failure = checkpoint
                            .recover_failure_ir(
                                dag.id,
                                ctx,
                                node_id,
                                dag.generation,
                                NodeState::Failed,
                            )
                            .await?;
                        (Some(node_id), Some(failure))
                    }
                    None => self
                        .fn2_approval_attribution(checkpoint, dag, ctx)
                        .await?
                        .map_or((None, None), |(id, f)| (Some(id), Some(f))),
                };

                Ok(DagOutcome {
                    dag_id: dag.id,
                    generation: dag.generation,
                    state: DagState::Failed,
                    failed_node,
                    failure,
                })
            }
            other => Err(SchedError::Invariant(format!(
                "assemble_already_terminal_outcome called for non-terminal state {other:?}"
            ))),
        }
    }

    /// FN2 (§5.18): the lowest `Cancelled` node whose durable failure is an
    /// `ErrorClass::Approval` one, with RF7 applied first to any `GateHuman`
    /// candidate so a lost post-CAS event still attributes.
    ///
    /// Recovering the failure *is* the test: FO1/FO2 report `Approval` only
    /// when a durable record says so, and FO3's eventless floor is
    /// `Internal`, which correctly excludes a node cancelled for any other
    /// reason from FN2.
    async fn fn2_approval_attribution(
        &self,
        checkpoint: &Checkpoint,
        dag: &TaskDag,
        ctx: CheckpointCtx,
    ) -> Result<Option<(NodeId, FailureIr)>, SchedError> {
        for (id, node) in &dag.nodes {
            if node.state != NodeState::Cancelled {
                continue;
            }
            if node.kind == NodeKind::GateHuman {
                // RF7 before selection. Best-effort: a repair that cannot
                // write must not stop R9 from returning the outcome it can
                // still assemble from the blob (FO3's "MUST NOT block").
                if let Err(e) = checkpoint
                    .repair_gate_terminal(dag.id, ctx, *id, dag.generation)
                    .await
                {
                    tracing::warn!(node_id = %id, error = %e, "RF7 gate repair failed at R9");
                }
            }
            let failure = checkpoint
                .recover_failure_ir(dag.id, ctx, *id, dag.generation, NodeState::Cancelled)
                .await?;
            if failure.error_class == ErrorClass::Approval {
                return Ok(Some((*id, failure)));
            }
        }
        Ok(None)
    }

    /// R15 / ER5 (§6.5): on resume, an `Edit` that durably succeeded while
    /// every reachable verify ended terminal *without* success means the
    /// workspace was mutated and never re-verified. Nothing will re-run those
    /// verifies (they are terminal), so the run cannot honestly continue.
    ///
    /// Returns `Some(outcome)` when ER5 fired and terminalized; `None` to
    /// continue into R16+.
    ///
    /// Guards, all normative and all load-bearing:
    /// - **Every node terminal ⇒ MUST NOT fire.** §6.3's all-terminal derive
    ///   owns that case (it may well be D7 `Succeeded`), and firing here
    ///   would rewrite a terminal — forbidden outright.
    /// - **Verify-less DAGs MUST NOT fire.** No reachable verify from the
    ///   succeeded edit (e.g. `Edit → GateHuman`) means the human *is* the
    ///   check; there is nothing missing.
    /// - **Any succeeded/cached verify clears it.** That is exactly ER4's
    ///   `needs_reverify` being false for a reason other than "no verify".
    /// - **Any still-runnable verify clears it.** The re-verify can still
    ///   happen; ER4's dispatch filter will make sure it happens first.
    /// - Target is the lowest `Pending`/`Ready` node (A10 edges only). No
    ///   `WaitingApproval → Failed`, no terminal rewrites. With no such
    ///   target, skip and continue rather than inventing one.
    ///
    /// On well-formed blobs a terminal-without-success verify normally
    /// co-terminalizes the DAG through C6/C7, so R9 short-circuits long
    /// before R15 — ER5 is a defensive guard for synthetic or repaired blobs.
    async fn er5_edit_without_verify(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<Option<DagOutcome>, SchedError> {
        let terminal = |s: NodeState| {
            matches!(
                s,
                NodeState::Succeeded
                    | NodeState::Failed
                    | NodeState::Skipped
                    | NodeState::Cancelled
                    | NodeState::CachedHit
            )
        };
        if dag.nodes.values().all(|n| terminal(n.state)) {
            return Ok(None);
        }

        let verifies = verifies_reachable_from_succeeded_edits(dag);
        if verifies.is_empty() {
            return Ok(None); // no succeeded edit, or a verify-less DAG
        }
        let all_terminal_without_success = verifies.iter().all(|id| {
            let state = dag.nodes[id].state;
            terminal(state) && !matches!(state, NodeState::Succeeded | NodeState::CachedHit)
        });
        if !all_terminal_without_success {
            return Ok(None);
        }

        let Some(target) = dag
            .nodes
            .iter()
            .find(|(_, n)| matches!(n.state, NodeState::Pending | NodeState::Ready))
            .map(|(id, _)| *id)
        else {
            return Ok(None); // no A10-legal target; continue to R16/derive
        };

        let failure = FailureIr {
            node: target,
            error_class: ErrorClass::Internal,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "edit succeeded without successful verify after restart".into(),
        };
        Ok(Some(
            self.terminal_failed(dag, rc, target, None, failure).await?,
        ))
    }

    /// R13 (§5.3.2): adopt a node durably `Running` on entry (crash
    /// resume). Returns `Some(outcome)` when adoption itself terminalized
    /// the run (retries exhausted); `None` to continue into R14+.
    async fn adopt_running(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<Option<DagOutcome>, SchedError> {
        let running: Vec<NodeId> = dag
            .nodes
            .values()
            .filter(|n| n.state == NodeState::Running)
            .map(|n| n.id)
            .collect();
        if running.is_empty() {
            return Ok(None);
        }
        if running.len() > 1 {
            return Err(SchedError::Invariant(
                "multiple running nodes after restart".into(),
            ));
        }
        let node_id = running[0];
        if dag.nodes[&node_id].kind == NodeKind::GateHuman {
            // §5.3.2 rows 3/4. A gate only reaches `Running` through GA4's
            // post-allow C3, so the question is whether that allow is still
            // durable:
            //   row 4 — durable allow: resume mid-fold. `gate_allow_fold`'s
            //     `Running` arm already implements exactly this (fold + C4,
            //     skipping the CASes the blob shows as done), so route into
            //     it rather than duplicating the sequence here.
            //   row 3 — no durable allow: genuinely illegal, and the
            //     unconditional error this replaced was correct for it.
            // Without row 4 a crash in the window between the post-allow C3
            // and the fold's C4 strands the run permanently: every retry
            // reloads the same `Running` gate and re-raises `Invariant`.
            let gate_id = dag.nodes[&node_id]
                .approval
                .as_ref()
                .map(|a| a.gate)
                .ok_or_else(|| {
                    SchedError::Invariant(format!("gate node {node_id} has no approval"))
                })?;
            let resolution = self
                .scan_gate_resolution(dag.id, rc.ctx, gate_id, dag.generation)
                .await?;
            return match resolution {
                Some(r @ (GateResolution::Allow | GateResolution::AllowOnce)) => {
                    match self.gate_apply_resolution(dag, rc, node_id, r).await? {
                        StepOutcome::Terminal(outcome) => Ok(Some(outcome)),
                        StepOutcome::Continue | StepOutcome::NaturalExit => Ok(None),
                    }
                }
                Some(GateResolution::Deny) | Some(GateResolution::Expired) | None => {
                    Err(SchedError::Invariant("gate node running".into()))
                }
            };
        }
        let attempts_started = rc
            .checkpoint
            .rebuild_attempts_started(dag.id, rc.ctx, node_id, dag.generation, true)
            .await?;
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Internal,
            retry: RetryDisposition::Retryable,
            diagnostics: vec![],
            notes: "adopted after restart".into(),
        };
        match self
            .admit_retry(dag, rc, node_id, attempts_started, failure)
            .await?
        {
            StepOutcome::Terminal(outcome) => Ok(Some(outcome)),
            StepOutcome::Continue | StepOutcome::NaturalExit => Ok(None),
        }
    }

    // -----------------------------------------------------------------
    // §5.2 loop steps
    // -----------------------------------------------------------------

    async fn loop_run(&self, dag: &mut TaskDag, rc: &RunCtx<'_>) -> Result<DagOutcome, SchedError> {
        loop {
            match self.loop_step(dag, rc).await? {
                StepOutcome::Continue => continue,
                StepOutcome::NaturalExit => return self.finish_natural(dag, rc).await,
                StepOutcome::Terminal(outcome) => return Ok(outcome),
            }
        }
    }

    async fn loop_step(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<StepOutcome, SchedError> {
        // L1
        if rc.run_cancel.is_cancelled() || self.deps.runtime_cancel.is_cancelled() {
            return self.cancel_path(dag, rc).await;
        }
        // L2
        let cancelled_here = {
            let mut pending = self
                .pending_cancels
                .lock()
                .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
            pending.remove(&dag.id)
        };
        if cancelled_here {
            return self.cancel_path(dag, rc).await;
        }

        // RS3 defensive scan (§5.4): a Succeeded Data predecessor without
        // output_ref is a corruption, not a "not yet satisfied" signal.
        for edge in &dag.edges {
            if edge.kind != crate::dag::EdgeKind::Data {
                continue;
            }
            if let Some(pred) = dag.nodes.get(&edge.from) {
                if pred.state == NodeState::Succeeded && pred.output_ref.is_none() {
                    return Err(SchedError::Invariant(format!(
                        "succeeded node {} has no output_ref",
                        edge.from
                    )));
                }
            }
        }

        let ready_before = ready_nodes(dag);
        let promotable = promotable_nodes(dag);
        let any_in_flight = dag
            .nodes
            .values()
            .any(|n| matches!(n.state, NodeState::Running | NodeState::WaitingApproval));

        // L3
        if ready_before.is_empty() && promotable.is_empty() && !any_in_flight {
            return Ok(StepOutcome::NaturalExit);
        }

        // L4
        if rc.remaining_run() == Duration::ZERO {
            return self
                .run_timeout_path(dag, rc, ready_before.first().copied())
                .await;
        }

        // L5
        if let Some(run_row) = self
            .deps
            .sessions
            .get_run(rc.run_id)
            .await
            .map_err(|e| map_store_error(e, dag.id))?
        {
            if RunControlState::parse(&run_row.state) == Some(RunControlState::ReplanRequested) {
                return self.replan_path(dag, rc).await;
            }
        }

        // L6: BG3 (effective_usd <= 0 is exhausted before `check_budget` can
        // ever see it — `spent` starts `None`) OR'd with the ordinary check
        // against the effective ceiling (BG4), never `deps.budget_policy`
        // directly.
        let meter = self.deps.cost_meters.meter_for(rc.run_id);
        if budget::is_pre_dispatch_exhausted(&rc.effective_budget)
            || meter.check_budget(&rc.effective_budget).is_exhausted()
        {
            return self
                .budget_exhausted_path(dag, rc, ready_before.first().copied())
                .await;
        }

        // L7
        if !promotable.is_empty() {
            rc.checkpoint.c2_promote(dag, rc.ctx, &promotable).await?;
        }

        // L8
        let ready = ready_nodes(dag);
        if ready.len() > 1 {
            return Err(SchedError::Invariant(format!(
                "multiple ready nodes: {ready:?}"
            )));
        }
        // L9
        let Some(selected) = ready.into_iter().next() else {
            return Ok(StepOutcome::NaturalExit);
        };

        // L14 / ER4: while a succeeded `Edit` still has a non-terminal
        // reachable verify, the workspace is mutated but unverified. Model
        // kinds MUST NOT be dispatched into that state — they would build on
        // an unverified tree. Verify/gate/aggregate kinds stay dispatchable,
        // which is what lets the pending verify run and clear the condition.
        //
        // Under the serial MVP this is a dispatch filter, not a second
        // scheduler: there is exactly one Ready node, so "filtered" means
        // "nothing else can run". ER4 is explicit that the node is then
        // durably failed via C7, not reported as `Err(Invariant)` — a wedged
        // blob has to reach a terminal state an operator can act on.
        if er4_blocked_kind(dag.nodes[&selected].kind) && needs_reverify(dag) {
            let failure = FailureIr {
                node: selected,
                error_class: ErrorClass::Internal,
                retry: RetryDisposition::NonRetryable,
                diagnostics: vec![],
                notes: "blocked by pending re-verify after edit (ER4)".into(),
            };
            return Ok(StepOutcome::Terminal(
                self.terminal_failed(dag, rc, selected, None, failure)
                    .await?,
            ));
        }

        // L10 done (selected). L13: gate route before any C3.
        if dag.nodes[&selected].kind == NodeKind::GateHuman {
            return self.gate_route(dag, rc, selected, false).await;
        }

        self.dispatch_node(dag, rc, selected).await
    }

    /// L11-L15 for a non-gate node.
    #[tracing::instrument(
        name = "sched.node",
        skip_all,
        fields(
            node_id = %node_id,
            kind = tracing::field::Empty,
            attempt = tracing::field::Empty,
            effective_tier = tracing::field::Empty,
        )
    )]
    async fn dispatch_node(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
    ) -> Result<StepOutcome, SchedError> {
        // L11
        let input =
            envelopes::assemble_input(rc.checkpoint, &*self.deps.artifacts, dag, rc.ctx, node_id)
                .await?;

        let attempts_started = rc
            .checkpoint
            .rebuild_attempts_started(dag.id, rc.ctx, node_id, dag.generation, false)
            .await?;
        let attempt = attempts_started + 1;

        // B4: backoff elapsed time is not durable (§12), so a `Ready` node
        // whose attempt counter came off the event log rather than from this
        // process's own C8 owes the full wait again before C3. Deliberately
        // over-waits after a crash — never under-waits.
        if attempts_started >= 1 && !rc.backoff_already_served(node_id) {
            let delay = backoff_delay(
                &dag.nodes[&node_id].retry.backoff,
                attempts_started,
                self.deps.config.max_backoff,
            );
            rc.mark_backoff_served(node_id);
            if !delay.is_zero() {
                // B3: cancel during backoff is immediate.
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = rc.run_cancel.cancelled() => {
                        return self.cancel_path(dag, rc).await;
                    }
                    () = self.deps.runtime_cancel.cancelled() => {
                        return self.cancel_path(dag, rc).await;
                    }
                }
            }
        }

        // L12 (§5.11.4 ES1-ES6): decided before C3 (ES4).
        let kind_for_escalation = dag.nodes[&node_id].kind;
        let effective_tier = if is_capability_kind(kind_for_escalation) {
            match retry::escalation_for_attempt(&dag.nodes[&node_id].retry, attempt) {
                Escalation::To(tier) => {
                    self.metrics.inc_escalations();
                    tier
                }
                Escalation::SkippedNoTarget => {
                    self.record_escalation_skipped(rc, node_id, attempt).await?;
                    dag.nodes[&node_id].model_tier
                }
                Escalation::None => dag.nodes[&node_id].model_tier,
            }
        } else {
            dag.nodes[&node_id].model_tier // ES5: adapter kinds never escalate.
        };
        let span = tracing::Span::current();
        span.record("kind", tracing::field::debug(kind_for_escalation));
        span.record("attempt", attempt);
        span.record("effective_tier", tracing::field::debug(effective_tier));

        let node_timeout_ms = dag.nodes[&node_id].timeout_ms;
        let node_budget_timeout = Duration::from_millis(node_timeout_ms);
        let remaining_run = rc.remaining_run();
        // T7: attribution when node_deadline = min(node.timeout_ms, remaining_run).
        // `<=` (not `<`) is deliberate: on an exact tie the run budget is the
        // binding constraint, so a timeout at that instant is a run timeout
        // (T8, non-retryable) rather than a node timeout (retryable). Ties are
        // reachable whenever a node's `timeout_ms` equals the run budget, which
        // single-node templates make routine.
        let run_attributed = remaining_run <= node_budget_timeout;
        let node_deadline = node_budget_timeout.min(remaining_run);

        if node_deadline == Duration::ZERO {
            // T3
            return if run_attributed {
                self.run_timeout_path(dag, rc, Some(node_id)).await
            } else {
                // node_deadline == 0 without run attribution cannot happen
                // (min would be 0 only if node_timeout_ms == 0, which is a
                // config concern outside this RFC's scope) — fail closed.
                Err(SchedError::Invariant(format!(
                    "node {node_id} has zero timeout_ms"
                )))
            };
        }

        // C3 (DP1: dispatch happens after this commits).
        rc.checkpoint
            .c3_dispatch(dag, rc.ctx, node_id, attempt)
            .await?;

        let plan = DispatchPlan {
            kind: dag.nodes[&node_id].kind,
            capability: dag.nodes[&node_id].capability.clone(),
            effective_tier,
            budget: dag.nodes[&node_id].budget.clone(),
            deadline: node_deadline,
            input,
        };
        let meta = NodeExecRef {
            session_id: rc.session.id,
            run_id: rc.run_id,
            dag_id: dag.id,
            node_id,
            workspace_root: rc.session.workspace_root.clone(),
            attempt,
        };

        let outcome = tokio::select! {
            res = tokio::time::timeout(node_deadline, self.dispatch_kind(plan, &meta, rc)) => {
                match res {
                    Ok(inner) => inner?,
                    Err(_elapsed) => {
                        return if run_attributed {
                            self.run_timeout_path(dag, rc, Some(node_id)).await
                        } else {
                            let failure = FailureIr {
                                node: node_id,
                                error_class: ErrorClass::Timeout,
                                retry: RetryDisposition::Retryable,
                                diagnostics: vec![],
                                notes: format!("node timeout after {}ms", node_deadline.as_millis()),
                            };
                            self.metrics.inc_node_timeouts();
                            self.apply_soft_failure(dag, rc, node_id, attempt, failure).await
                        };
                    }
                }
            }
            () = rc.run_cancel.cancelled() => {
                return self.cancel_path(dag, rc).await;
            }
        };

        // §5.16.3 "after each node completes": best-effort, whether this
        // dispatch succeeded or failed — cost was incurred either way.
        self.signal_post_node_budget_warning(rc).await;

        match outcome {
            DispatchResult::Succeeded(payload) => {
                self.apply_success(dag, rc, node_id, attempt, payload).await
            }
            DispatchResult::Failed(mut failure) => {
                failure.node = node_id; // DP4/CE2
                self.apply_soft_failure(dag, rc, node_id, attempt, failure)
                    .await
            }
        }
    }

    /// §5.6 dispatch table (capability/verify/aggregate; `GateHuman` is
    /// routed by the caller before C3).
    async fn dispatch_kind(
        &self,
        plan: DispatchPlan,
        meta: &NodeExecRef,
        rc: &RunCtx<'_>,
    ) -> Result<DispatchResult, SchedError> {
        let DispatchPlan {
            kind,
            capability,
            effective_tier,
            budget,
            deadline,
            input,
        } = plan;
        match kind {
            NodeKind::Plan | NodeKind::Analyze | NodeKind::Edit | NodeKind::Review => {
                let capability = capability.ok_or_else(|| {
                    SchedError::Invariant(format!("node {} has no capability", meta.node_id))
                })?;
                let ctx = CapabilityExecContext {
                    meta: meta.clone(),
                    cancellation: rc.run_cancel.clone(),
                    capability,
                    kind,
                    effective_tier,
                    budget,
                    timeout: deadline,
                    input,
                    attempt: meta.attempt,
                    cost_meter: self.deps.cost_meters.meter_for(meta.run_id),
                };
                match self.deps.capabilities.execute(&ctx).await {
                    Ok(CapabilityOutcome::Succeeded { payload }) => {
                        Ok(DispatchResult::Succeeded(payload))
                    }
                    Ok(CapabilityOutcome::Failed { failure }) => {
                        Ok(DispatchResult::Failed(failure))
                    }
                    Err(e) => Ok(DispatchResult::Failed(failure_from_capability_exec_error(
                        meta.node_id,
                        e,
                    ))),
                }
            }
            // §9.1 `sched.verify` OB2: counts and codes only, never stdout —
            // `VerifyOutcome` doesn't carry exit_code/signal/truncated (the
            // MCP adapter consumes those internally to decide the outcome
            // shape; they never reach this layer), so only `tool` and a
            // `diagnostics` count are populated.
            NodeKind::VerifyCompile => {
                let ctx = NodeExecContext {
                    meta: meta.clone(),
                    cancellation: rc.run_cancel.clone(),
                };
                let span = tracing::info_span!(
                    "sched.verify",
                    tool = "cargo_check",
                    diagnostics = tracing::field::Empty
                );
                let result = {
                    use tracing::Instrument;
                    self.deps
                        .verify_compile
                        .check(&ctx)
                        .instrument(span.clone())
                        .await
                };
                match result {
                    Ok(outcome) => {
                        span.record("diagnostics", outcome.diagnostics.len());
                        Ok(verify_outcome_to_result(
                            outcome,
                            ErrorClass::Compile,
                            "cargo check failed",
                        ))
                    }
                    Err(e) => Ok(DispatchResult::Failed(failure_from_adapter_error(
                        meta.node_id,
                        e,
                    ))),
                }
            }
            NodeKind::VerifyTest => {
                let ctx = NodeExecContext {
                    meta: meta.clone(),
                    cancellation: rc.run_cancel.clone(),
                };
                let span = tracing::info_span!(
                    "sched.verify",
                    tool = "cargo_test",
                    diagnostics = tracing::field::Empty
                );
                let result = {
                    use tracing::Instrument;
                    self.deps
                        .verify_test
                        .test(&ctx)
                        .instrument(span.clone())
                        .await
                };
                match result {
                    Ok(outcome) => {
                        span.record("diagnostics", outcome.diagnostics.len());
                        Ok(verify_outcome_to_result(
                            outcome,
                            ErrorClass::Test,
                            "cargo test failed",
                        ))
                    }
                    Err(e) => Ok(DispatchResult::Failed(failure_from_adapter_error(
                        meta.node_id,
                        e,
                    ))),
                }
            }
            // AG1: structural fold, no worker/adapter/model call. C3 still
            // fires (Aggregate is a regular Ready -> Running -> Succeeded
            // node); the real payload is computed by `apply_success`'s
            // Aggregate branch, which overwrites this placeholder.
            NodeKind::Aggregate => Ok(DispatchResult::Succeeded(serde_json::Value::Null)),
            NodeKind::GateHuman => unreachable!("GateHuman is routed before dispatch_kind"),
        }
    }

    // -----------------------------------------------------------------
    // §5.9 success path / §5.14 Aggregate fold
    // -----------------------------------------------------------------

    /// `pub(super)`: called from `gate.rs`'s allow-fold (GA1) as well as the
    /// ordinary dispatch path.
    pub(super) async fn apply_success(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        payload: serde_json::Value,
    ) -> Result<StepOutcome, SchedError> {
        let payload = if dag.nodes[&node_id].kind == NodeKind::Aggregate {
            self.run_aggregate(dag, node_id)?
        } else {
            payload
        };
        let envelope = NodeOutputEnvelope::new(
            dag.id,
            node_id,
            dag.nodes[&node_id].kind,
            dag.generation,
            attempt,
            payload,
        );
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|e| SchedError::Internal(format!("encode output envelope: {e}")))?;
        rc.checkpoint
            .c4_succeed(dag, rc.ctx, node_id, attempt, bytes)
            .await?;
        Ok(StepOutcome::Continue)
    }

    /// AG1-AG5: structural fold over incoming `Data` predecessors, ascending
    /// `NodeId`.
    fn run_aggregate(
        &self,
        dag: &TaskDag,
        node_id: NodeId,
    ) -> Result<serde_json::Value, SchedError> {
        let InputShape::Data(preds) = envelopes::classify_input_shape(dag, node_id) else {
            return Err(SchedError::Invariant(format!(
                "Aggregate node {node_id} has zero incoming Data edges"
            ))); // AG4
        };
        let mut entries = Vec::with_capacity(preds.len());
        for pred_id in preds {
            let pred = dag.nodes.get(&pred_id).ok_or_else(|| {
                SchedError::Invariant(format!("unknown Aggregate predecessor {pred_id}"))
            })?;
            let output_ref = pred.output_ref.ok_or_else(|| {
                SchedError::Invariant(format!("succeeded node {pred_id} has no output_ref"))
            })?;
            entries.push(serde_json::json!({
                "node_id": pred_id.to_string(),
                "kind": pred.kind,
                "output_ref": output_ref.to_string(),
            }));
        }
        Ok(serde_json::json!({ "aggregate": true, "preds": entries })) // AG2
    }

    // -----------------------------------------------------------------
    // Soft-failure application (L15) — dispatches to the P5 retry seam.
    // -----------------------------------------------------------------

    async fn apply_soft_failure(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        failure: FailureIr,
    ) -> Result<StepOutcome, SchedError> {
        match self.admit_retry(dag, rc, node_id, attempt, failure).await? {
            StepOutcome::Terminal(outcome) => Ok(StepOutcome::Terminal(outcome)),
            other => Ok(other),
        }
    }

    /// §5.11.1 A1-A6 admission, §5.8.3 C8, §5.11.2 the `Retry` decision
    /// record, and §5.11.3 B3's interruptible backoff sleep.
    async fn admit_retry(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        failure: FailureIr,
    ) -> Result<StepOutcome, SchedError> {
        let node = &dag.nodes[&node_id];
        let retry_policy = node.retry.clone();
        let kind = node.kind;
        let cancelled = rc.run_cancel.is_cancelled(); // A4 (child of runtime_cancel — §4.3 O1)
        let meter = self.deps.cost_meters.meter_for(rc.run_id);
        // A5 (§5.16.3): the same BG3-or-check_budget test L6 uses, against
        // the effective ceiling (BG4).
        let budget_exhausted = budget::is_pre_dispatch_exhausted(&rc.effective_budget)
            || meter.check_budget(&rc.effective_budget).is_exhausted();
        let remaining_run = rc.remaining_run(); // A6

        let decision = retry::admit(
            attempt,
            retry::AdmissionInput {
                retry_disposition: failure.retry,
                error_class: failure.error_class,
                retry_on: &retry_policy.retry_on,
                max_attempts: retry_policy.max_attempts,
                cancelled,
                budget_exhausted,
                remaining_run,
                backoff: &retry_policy.backoff,
                max_backoff: self.deps.config.max_backoff,
            },
        );

        match decision {
            Admission::Reject(reason) => {
                // BE4: this record describes a decision whose CAS (C7, inside
                // `terminal_failed`) has not happened yet, so a `DecisionLog`
                // failure here MUST surface as `Err(Store)` and MUST NOT let
                // the CAS proceed. Only *after* a committed CAS is logging
                // best-effort.
                self.record_retry_rejected(rc, node_id, attempt, &failure, reason)
                    .await?;
                self.metrics.inc_retries_rejected();
                Ok(StepOutcome::Terminal(
                    self.terminal_failed(dag, rc, node_id, Some(attempt), failure)
                        .await?,
                ))
            }
            Admission::Admit {
                next_attempt,
                delay,
            } => {
                let escalated_to = if is_capability_kind(kind) {
                    match retry::escalation_for_attempt(&retry_policy, next_attempt) {
                        Escalation::To(tier) => Some(tier),
                        Escalation::None | Escalation::SkippedNoTarget => None,
                    }
                } else {
                    None // ES5
                };
                let backoff_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                // BE4, as above: pre-CAS, so failure aborts before C8.
                self.record_retry_admitted(
                    rc,
                    node_id,
                    attempt,
                    next_attempt,
                    &failure,
                    backoff_ms,
                    escalated_to,
                )
                .await?;

                rc.checkpoint
                    .c8_retry(
                        dag,
                        rc.ctx,
                        node_id,
                        attempt,
                        &failure,
                        next_attempt,
                        backoff_ms,
                    )
                    .await?;

                // B4: this process is serving the wait for `next_attempt`, so
                // the re-dispatch that follows must not serve it a second time.
                rc.mark_backoff_served(node_id);
                if !delay.is_zero() {
                    // B3: cancel during backoff is immediate.
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = rc.run_cancel.cancelled() => {
                            return self.cancel_path(dag, rc).await;
                        }
                    }
                }
                Ok(StepOutcome::Continue)
            }
        }
    }

    /// §5.11.2: one `DecisionKind::Retry` record per admission decision.
    ///
    /// BE4: this runs *before* the C7/C8 it describes, so a `DecisionLog`
    /// failure maps to `Err(Store)` and the caller MUST NOT proceed with that
    /// CAS. (It is not best-effort — that posture only applies to records
    /// appended after a committed CAS.)
    #[allow(clippy::too_many_arguments)]
    async fn record_retry_admitted(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        next_attempt: u32,
        failure: &FailureIr,
        backoff_ms: u64,
        escalated_to: Option<ModelTier>,
    ) -> Result<(), SchedError> {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "next_attempt": next_attempt,
            "error_class": failure.error_class,
            "retry_admitted": true,
            "backoff_ms": backoff_ms,
            "escalated_to": escalated_to,
        });
        self.record_decision(rc, node_id, metadata).await
    }

    async fn record_retry_rejected(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        failure: &FailureIr,
        reason: retry::RejectReason,
    ) -> Result<(), SchedError> {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "error_class": failure.error_class,
            "retry_admitted": false,
            "reason": reason.as_str(),
        });
        self.record_decision(rc, node_id, metadata).await
    }

    /// ES2: `escalate_after` is due but no `escalate_to_tier` is configured.
    /// BE4: recorded before the C3 this escalation decision applies to, so a
    /// `DecisionLog` failure aborts the dispatch rather than silently
    /// dispatching at a tier no audit trail explains.
    async fn record_escalation_skipped(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
    ) -> Result<(), SchedError> {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "escalation_skipped": "no target tier",
        });
        self.record_decision(rc, node_id, metadata).await
    }

    /// BE4: every caller of this records a decision whose CAS has **not**
    /// committed yet, so an `ObsError` maps to `Err(Store(..))` and the
    /// caller MUST NOT proceed with that CAS. Post-CAS records (§5.16's
    /// budget signals, the gate records) stay best-effort and do not route
    /// through here.
    async fn record_decision(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        metadata: serde_json::Value,
    ) -> Result<(), SchedError> {
        let rec = DecisionRecord {
            session: rc.ctx.session_id,
            run: rc.ctx.run_id,
            node: Some(node_id),
            kind: DecisionKind::Retry,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        self.deps
            .decisions
            .record(rec)
            .await
            .map(|_seq| ())
            .map_err(|e| SchedError::Store(format!("pre-CAS decision record failed: {e}")))
    }

    /// Wraps [`Checkpoint::c7_terminal_failed`] with the SK3 "every other
    /// non-terminal node is skipped" node-selection rule.
    async fn terminal_failed(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: Option<u32>,
        failure: FailureIr,
    ) -> Result<DagOutcome, SchedError> {
        let skipped = non_terminal_except(dag, node_id);
        // `FailureIr` carries no `failure_ref` field itself (that lives on
        // the NodeState event / artifact label per Appendix G); the
        // artifact id `c7_terminal_failed` returns is durable provenance
        // only, not something this in-memory `DagOutcome` needs to embed.
        let _failure_ref = rc
            .checkpoint
            .c7_terminal_failed(dag, rc.ctx, node_id, attempt, &failure, &skipped)
            .await?;
        Ok(DagOutcome {
            dag_id: dag.id,
            generation: dag.generation,
            state: DagState::Failed,
            failed_node: Some(node_id),
            failure: Some(failure),
        })
    }

    // §5.7 gate orchestration lives in `gate.rs` (`LinearScheduler::gate_route`
    // and friends) — a separate `impl LinearScheduler` block in that file.

    // -----------------------------------------------------------------
    // Cancel / replan / budget / run-timeout terminal paths
    // -----------------------------------------------------------------

    /// §5.12.2 owned cancel (run-side): in-flight node (`Running`/`Ready`/
    /// `WaitingApproval`) → `Cancelled`; every other non-terminal node →
    /// `Skipped`.
    ///
    /// `pub(super)`: also the §5.7.10/§5.7.9 cancel routing target from
    /// `gate.rs`.
    pub(super) async fn cancel_path(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<StepOutcome, SchedError> {
        let (cancelled, skipped) = cancel_targets(dag);
        rc.checkpoint
            .c6_cancel(dag, rc.ctx, &cancelled, &skipped)
            .await?;
        Ok(StepOutcome::Terminal(DagOutcome {
            dag_id: dag.id,
            generation: dag.generation,
            state: DagState::Cancelled,
            failed_node: None,
            failure: None,
        }))
    }

    /// `pub(super)`: also the §5.7.9 `ReplanRequested` classification target
    /// from `gate.rs`.
    pub(super) async fn replan_path(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<StepOutcome, SchedError> {
        rc.checkpoint.c10_replan(dag).await?;
        Ok(StepOutcome::Terminal(DagOutcome {
            dag_id: dag.id,
            generation: dag.generation,
            state: DagState::ReplanRequired,
            failed_node: None,
            failure: None,
        }))
    }

    /// §5.16.3 L6: attribute to selected Ready, else lowest Ready, else
    /// lowest Pending (mirrored by run timeout's T8).
    async fn budget_exhausted_path(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        selected_ready: Option<NodeId>,
    ) -> Result<StepOutcome, SchedError> {
        let target = attribution_target(dag, selected_ready);
        let Some(node_id) = target else {
            return Err(SchedError::Invariant(
                "budget exhausted with no node to attribute".into(),
            ));
        };
        let check = self
            .deps
            .cost_meters
            .meter_for(rc.run_id)
            .check_budget(&rc.effective_budget);
        // BE1: one Budget decision + a BudgetWarning, before terminalizing.
        // BE4: pre-CAS, so a failure here aborts before the C7 below.
        self.signal_budget_exhaustion(rc, Some(node_id), check)
            .await?;
        self.metrics.inc_budget_stops();
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Budget,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: format!("budget exhausted: {check:?}"),
        };
        Ok(StepOutcome::Terminal(
            self.terminal_failed(dag, rc, node_id, None, failure)
                .await?,
        ))
    }

    /// BE1: append one `DecisionKind::Budget` record and a `BudgetWarning`.
    /// `check` is `meter.check_budget(&rc.effective_budget)` at the moment
    /// of exhaustion, so the caller doesn't compute it twice.
    ///
    /// BE4: the Budget record precedes the C7 that terminalizes the run, so a
    /// `DecisionLog` failure maps to `Err(Store)` and the caller MUST NOT
    /// proceed with that CAS — the same rule the retry path already follows.
    /// The `BudgetWarning` signal afterwards is a separate obs channel and
    /// stays best-effort.
    async fn signal_budget_exhaustion(
        &self,
        rc: &RunCtx<'_>,
        node_id: Option<NodeId>,
        check: crate::obs::BudgetCheck,
    ) -> Result<(), SchedError> {
        let metadata = serde_json::json!({
            "check": format!("{check:?}"),
            "effective_usd": rc.effective_budget.max_usd_per_run,
            "effective_tokens": rc.effective_budget.max_tokens_per_run,
        });
        self.record_budget_decision(rc, node_id, metadata).await?;

        let meter = self.deps.cost_meters.meter_for(rc.run_id);
        if budget::is_pre_dispatch_exhausted(&rc.effective_budget) && !check.is_exhausted() {
            // BG3: effective_usd <= 0 but `spent` is still `None`, so
            // `maybe_signal_budget_warning`'s own `check_budget` call
            // wouldn't fire — signal directly with that specific message.
            let snapshot = meter.with_mut(|m| m.to_budget_snapshot());
            if let Err(e) = self
                .deps
                .session_plane
                .signal_budget_warning(
                    rc.ctx.session_id,
                    Some(rc.run_id),
                    snapshot,
                    "budget exhausted: effective_usd <= 0 (pre-dispatch)",
                )
                .await
            {
                tracing::warn!(error = %e, "budget warning (pre-dispatch) failed");
            }
        } else if let Err(e) = maybe_signal_budget_warning(
            &self.deps.session_plane,
            rc.ctx.session_id,
            Some(rc.run_id),
            &meter,
            &rc.effective_budget,
        )
        .await
        {
            tracing::warn!(error = %e, "budget warning failed");
        }
        Ok(())
    }

    /// §5.16.3 "after each node completes": best-effort spend-accrual
    /// warning, against the effective ceiling (BG4), not `deps.budget_policy`.
    async fn signal_post_node_budget_warning(&self, rc: &RunCtx<'_>) {
        let meter = self.deps.cost_meters.meter_for(rc.run_id);
        if let Err(e) = maybe_signal_budget_warning(
            &self.deps.session_plane,
            rc.ctx.session_id,
            Some(rc.run_id),
            &meter,
            &rc.effective_budget,
        )
        .await
        {
            tracing::warn!(error = %e, "post-node budget warning failed");
        }
    }

    /// BG1/BG5: record once per run, right after `rc` is constructed.
    async fn record_budget_ignored(&self, rc: &RunCtx<'_>, effective: &budget::EffectiveBudget) {
        if !effective.ignored_max_usd_non_finite && effective.ignored_parallelism.is_empty() {
            return;
        }
        let mut metadata = serde_json::json!({
            "check": "ignored",
            "effective_usd": effective.policy.max_usd_per_run,
            "effective_tokens": effective.policy.max_tokens_per_run,
        });
        if effective.ignored_max_usd_non_finite {
            metadata["ignored_max_usd"] = serde_json::json!("non_finite");
        }
        if !effective.ignored_parallelism.is_empty() {
            let ignored: serde_json::Map<String, serde_json::Value> = effective
                .ignored_parallelism
                .iter()
                .map(|p| (p.field.to_string(), serde_json::Value::from(p.value)))
                .collect();
            metadata["ignored_parallelism"] = serde_json::Value::Object(ignored);
        }
        // BG1/BG5 is a run-level note, not a pre-CAS record — no CAS depends
        // on it, so BE4 does not apply and a sink outage must not fail the run.
        if let Err(e) = self.record_budget_decision(rc, None, metadata).await {
            tracing::warn!(error = %e, "budget-ignored decision record failed");
        }
    }

    /// Shared `DecisionKind::Budget` recorder (BE1/BG1/BG5). `node_id` is
    /// `None` for run-level notes (BG1/BG5), `Some` for an exhaustion
    /// attribution (BE1).
    ///
    /// Returns the `ObsError` rather than swallowing it: BE1's caller records
    /// *before* the C7 it describes and must honour BE4 (pre-CAS failure ⇒
    /// `Err(Store)`, no CAS). BG1/BG5 are not tied to a CAS at all and stay
    /// best-effort at their own call site.
    async fn record_budget_decision(
        &self,
        rc: &RunCtx<'_>,
        node_id: Option<NodeId>,
        metadata: serde_json::Value,
    ) -> Result<(), SchedError> {
        let rec = DecisionRecord {
            session: rc.ctx.session_id,
            run: rc.ctx.run_id,
            node: node_id,
            kind: DecisionKind::Budget,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        self.deps
            .decisions
            .record(rec)
            .await
            .map(|_seq| ())
            .map_err(|e| SchedError::Store(format!("pre-CAS decision record failed: {e}")))
    }

    /// §5.19 T8: run timeout with no `Running` node in the general case
    /// attributes to the selected Ready node, else lowest Ready, else
    /// lowest Pending. When `hint` names an in-flight node directly (T7's
    /// "node_deadline fired" case), that node is used verbatim.
    async fn run_timeout_path(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        hint: Option<NodeId>,
    ) -> Result<StepOutcome, SchedError> {
        let target = hint.or_else(|| attribution_target(dag, None));
        let Some(node_id) = target else {
            return Err(SchedError::Invariant(
                "run timeout with no node to attribute".into(),
            ));
        };
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Timeout,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: format!("run timeout after {}ms", rc.run_timeout.as_millis()),
        };
        self.metrics.inc_run_timeouts();
        Ok(StepOutcome::Terminal(
            self.terminal_failed(dag, rc, node_id, None, failure)
                .await?,
        ))
    }

    // -----------------------------------------------------------------
    // R18: natural loop exit
    // -----------------------------------------------------------------

    async fn finish_natural(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
    ) -> Result<DagOutcome, SchedError> {
        let state = derive_dag_state(dag, DeriveFlags::default())?;
        if state == DagState::Succeeded {
            rc.checkpoint.c7_terminal_succeeded(dag).await?;
            return Ok(DagOutcome {
                dag_id: dag.id,
                generation: dag.generation,
                state: DagState::Succeeded,
                failed_node: None,
                failure: None,
            });
        }

        // DS4 stall recovery (§5.17). The loop exited naturally, so no
        // `Ready`, `Running`, or `WaitingApproval` node remains — yet the
        // derive says non-terminal. That means one or more `Pending` nodes can
        // never be promoted (a `Skipped`/`Failed`/`Cancelled` Data
        // predecessor). §5.15's SK3 bulk-terminalize normally forecloses this,
        // so it is unreachable on blobs this scheduler wrote — but a replan or
        // a repaired/hand-built blob can present it, and returning `Err` here
        // left the DAG durably `Running` with nothing that would ever revisit
        // it: a permanent wedge whose only operator signal is an `Invariant`
        // string. Skip the stalled nodes, re-derive (D8 takes an all-`Skipped`
        // graph to `Failed`), and commit that terminal instead.
        let stalled: Vec<NodeId> = dag
            .nodes
            .iter()
            .filter(|(_, n)| n.state == NodeState::Pending)
            .map(|(id, _)| *id)
            .collect();

        if !stalled.is_empty() {
            // Derive against a copy first: DS4 only sanctions the CAS if the
            // skip actually resolves the stall.
            let mut probe = dag.clone();
            for id in &stalled {
                probe
                    .nodes
                    .get_mut(id)
                    .expect("id came from this map")
                    .state = NodeState::Skipped;
            }
            let after = derive_dag_state(&probe, DeriveFlags::default())?;
            if after != DagState::Running {
                tracing::warn!(
                    dag_id = %dag.id,
                    stalled = ?stalled,
                    resolved_to = ?after,
                    "dag stalled: unsatisfiable Data predecessors; skipping and terminalizing (DS4)"
                );
                rc.checkpoint
                    .c7_terminal_stalled(dag, rc.ctx, &stalled, after)
                    .await?;
                // FO4(i): an all-`Skipped` terminal attributes no node.
                return Ok(DagOutcome {
                    dag_id: dag.id,
                    generation: dag.generation,
                    state: after,
                    failed_node: None,
                    failure: None,
                });
            }
        }

        // DS4's own fallback: the skip did not resolve it, so the blob is
        // genuinely inconsistent rather than merely stalled.
        Err(SchedError::Invariant(format!("dag stalled: {stalled:?}")))
    }

    // -----------------------------------------------------------------
    // `cancel` (§5.12 — full return table + race-free O4 wait)
    // -----------------------------------------------------------------

    #[tracing::instrument(
        name = "sched.cancel",
        skip_all,
        fields(dag_id = %dag_id, owned = tracing::field::Empty, forced = tracing::field::Empty)
    )]
    async fn cancel_impl(&self, dag_id: DagId) -> Result<(), SchedError> {
        self.metrics.inc_cancels(); // §5.12.2 step 1

        // Bounded retry for the razor-thin TOCTOU window where a lookup
        // between two ownership-map operations can observe a transient gap
        // (contended insert, then the owner drops before our follow-up
        // lookup). Two attempts, not unbounded recursion (async self-recursion
        // needs boxing to compile at all; this path is rare enough that a
        // small bound is simpler and just as correct).
        for _ in 0..2 {
            if let Some(owned) = self.lookup_owned(dag_id)? {
                tracing::Span::current().record("owned", true);
                // §5.12.2: fire the run's own token; L1/L2 or the in-flight
                // `select!` (dispatch_node) observes it and writes C6.
                owned.run_cancel.cancel();
                return self.wait_for_cancel_result(&owned).await;
            }
            tracing::Span::current().record("owned", false);

            // Unowned (§5.12.4).
            // Step 1: run binding is for event attribution only — a missing
            // binding is tolerated (step 4), any other error propagates.
            let Some(probe) = self
                .deps
                .dags
                .get(dag_id)
                .await
                .map_err(|e| map_store_error_on_load(e, dag_id))?
            else {
                return Err(SchedError::DagNotFound(dag_id));
            };

            // PC1 gates the insert on the durable DAG being non-terminal, so
            // the probe has to come first. PC4 then requires removal on
            // *every* exit — a leaked entry cancels a later, unrelated run of
            // the same `DagId` (a replan keeps the id and only bumps the
            // generation), which is precisely the hazard PC4 names. The guard
            // covers the `?` returns, the `continue`, and the loop's own
            // contention exit alike; hand-rolled removals covered only the
            // success path.
            let _pending = if terminal_state(probe.state) {
                None
            } else {
                Some(PendingCancelGuard::insert(&self.pending_cancels, dag_id)?)
            };
            let run_id = match self.resolve_run_binding(&probe).await {
                Ok(row) => Some(row.id),
                Err(SchedError::RunBindingMissing(_)) => None,
                Err(e) => return Err(e),
            };

            // Step 2: transient ownership insert — occupied means a `run()`
            // won the race between our lookup above and this insert; retry
            // the loop, which will find it via `lookup_owned` this time.
            let guard = match self.try_acquire_dag(dag_id, run_id, probe.session_id) {
                Ok(g) => g,
                Err(SchedError::AlreadyOwned(_)) => continue,
                Err(e) => return Err(e),
            };

            let result = self.cancel_unowned_body(dag_id, run_id).await;
            guard.owned.set_cancel_result(match &result {
                Ok(()) => Ok(DagState::Cancelled),
                Err(e) => Err(e.clone()),
            });
            // Step 8's removal is `_pending`'s `Drop`, here and on every
            // other exit from this scope.
            return result;
        }
        Err(SchedError::Internal(
            "cancel: ownership map contention exceeded retry bound".into(),
        ))
    }

    /// §5.12.4 steps 3-6: re-load under (transient) ownership, terminal
    /// check, C6 write. Does not touch `pending_cancels` or ownership —
    /// the caller ([`Self::cancel_impl`]) owns both.
    async fn cancel_unowned_body(
        &self,
        dag_id: DagId,
        run_id: Option<RunId>,
    ) -> Result<(), SchedError> {
        // Step 3: re-load closes the window a same-generation CAS alone
        // cannot detect (a concurrent same-gen writer that already
        // terminalized between our first probe and now).
        let Some(mut dag) = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| map_store_error_on_load(e, dag_id))?
        else {
            return Err(SchedError::DagNotFound(dag_id));
        };
        if matches!(
            dag.state,
            DagState::Succeeded | DagState::Failed | DagState::Cancelled | DagState::ReplanRequired
        ) {
            return Ok(()); // no write
        }
        // Step 4: missing run binding ⇒ still write C6, warn.
        if run_id.is_none() {
            tracing::warn!(
                dag_id = %dag_id,
                "cancel: no run binding for this dag; writing C6 without run attribution"
            );
        }
        let checkpoint = self.checkpoint();
        let ctx = CheckpointCtx {
            session_id: dag.session_id,
            run_id,
        };
        let (cancelled, skipped) = cancel_targets(&dag);
        checkpoint
            .c6_cancel(&mut dag, ctx, &cancelled, &skipped)
            .await?;
        Ok(())
    }
}

/// Is `state` one of the durable terminals `cancel` must treat as a no-op?
fn terminal_state(state: DagState) -> bool {
    matches!(
        state,
        DagState::Succeeded | DagState::Failed | DagState::Cancelled | DagState::ReplanRequired
    )
}

/// RAII membership in `pending_cancels` (§5.12.1 PC1/PC4).
///
/// PC4 requires the entry to disappear once the cancel intent is consumed or
/// resolved. Removing by hand only covered the one success path, so a store
/// error, a missing run binding, an `AlreadyOwned` retry, or the contention
/// bound each left a live entry behind — and a live entry fires
/// `run_cancel` on the *next* `run()` for that `DagId` (R5), which after a
/// replan is a different, unrelated generation. `Drop` removes on every path
/// including `?` and `continue`.
struct PendingCancelGuard<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<DagId>>,
    dag_id: DagId,
}

impl<'a> PendingCancelGuard<'a> {
    fn insert(
        set: &'a std::sync::Mutex<std::collections::HashSet<DagId>>,
        dag_id: DagId,
    ) -> Result<Self, SchedError> {
        set.lock()
            .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?
            .insert(dag_id);
        Ok(Self { set, dag_id })
    }
}

impl Drop for PendingCancelGuard<'_> {
    fn drop(&mut self) {
        // A poisoned lock still yields the set; dropping the intent is more
        // important than the poison, and leaving it behind is the bug.
        self.set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.dag_id);
    }
}

/// Everything §5.6's dispatch table needs for one attempt, resolved before
/// C3 commits so the values handed to a worker match what was checkpointed.
struct DispatchPlan {
    kind: NodeKind,
    capability: Option<crate::types::ids::CapabilityId>,
    /// Post-escalation tier (ES3: never written back to `TaskNode`).
    effective_tier: crate::types::budget::ModelTier,
    /// The node's own token budget from the topology. `CapabilityExecContext`
    /// documents this as the per-node budget and RFC-0013's workers will
    /// enforce against it — shipping a zeroed one is a broken contract even
    /// while `UnavailableCapabilityExecutor` ignores it.
    budget: crate::types::budget::TokenBudget,
    /// §5.19 node deadline, already clamped by the remaining run budget.
    /// The same value `dispatch_node`'s `tokio::time::timeout` wrapper uses
    /// (DP2), so a worker's own deadline arithmetic agrees with the
    /// scheduler's.
    deadline: Duration,
    input: crate::dag::NodeInputEnvelope,
}

/// Success payload or structured failure from one dispatch (capability or
/// verify adapter).
enum DispatchResult {
    Succeeded(serde_json::Value),
    Failed(FailureIr),
}

fn verify_outcome_to_result(
    outcome: VerifyOutcome,
    class: ErrorClass,
    notes: &str,
) -> DispatchResult {
    if outcome.ok {
        DispatchResult::Succeeded(serde_json::json!({
            "ok": true,
            "diagnostics": outcome.diagnostics,
            "raw_artifact": outcome.raw_artifact.map(|id| id.to_string()),
        })) // OU4
    } else {
        // RL1-RL5 put the raw log for `ok: false` too, but its id only
        // survived on the success path (in the output envelope). On this path
        // nothing durable referenced it, so the artifact existed with no way
        // back to it from the run's records — exactly when an operator most
        // wants the compiler output. `FailureIr` has no artifact-id field, so
        // name it in `notes`, which is what a failed verify surfaces.
        let notes = match outcome.raw_artifact {
            Some(id) => format!("{notes} (raw log: {id})"),
            None => notes.to_string(),
        };
        DispatchResult::Failed(FailureIr {
            node: NodeId::new(), // overwritten by the caller (DP4)
            error_class: class,
            retry: RetryDisposition::NonRetryable,
            diagnostics: outcome.diagnostics, // F2
            notes,
        })
    }
}

/// §5.10 `CapabilityExecError` -> `FailureIr`.
fn failure_from_capability_exec_error(node: NodeId, e: CapabilityExecError) -> FailureIr {
    let (error_class, retry, notes) = match &e {
        CapabilityExecError::Unavailable => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            "capability executor unavailable".to_string(),
        ),
        CapabilityExecError::Worker(m) => (
            ErrorClass::Internal,
            RetryDisposition::Retryable,
            format!("worker error: {m}"),
        ),
        CapabilityExecError::Timeout => (
            ErrorClass::Timeout,
            RetryDisposition::Retryable,
            "worker reported timeout".to_string(),
        ),
        CapabilityExecError::Cancelled => (
            ErrorClass::Cancelled,
            RetryDisposition::NonRetryable,
            "cancelled".to_string(),
        ),
        CapabilityExecError::Internal(m) => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            format!("internal: {m}"),
        ),
    };
    FailureIr {
        node,
        error_class,
        retry,
        diagnostics: vec![],
        notes,
    }
}

/// §5.10 `AdapterError` -> `FailureIr`. `pub(super)`: also used by `gate.rs`
/// for the wait-result's non-gate-specific adapter failures.
pub(super) fn failure_from_adapter_error(node: NodeId, e: AdapterError) -> FailureIr {
    let (error_class, retry, notes) = match &e {
        AdapterError::Unavailable => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            "adapter unavailable".to_string(),
        ),
        AdapterError::Cancelled => (
            ErrorClass::Cancelled,
            RetryDisposition::NonRetryable,
            "cancelled".to_string(),
        ),
        AdapterError::Tool(m) => (
            ErrorClass::Tool,
            RetryDisposition::NonRetryable,
            format!("tool: {m}"),
        ),
        AdapterError::Internal(m) => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            format!("internal: {m}"),
        ),
        AdapterError::ToolFailure(te) => match te {
            crate::types::tools::ToolError::Transient { code, .. } => (
                ErrorClass::Tool,
                RetryDisposition::Retryable,
                format!("tool transient: {code}"),
            ),
            crate::types::tools::ToolError::Permanent { code, .. } => (
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
                format!("tool permanent: {code}"),
            ),
            crate::types::tools::ToolError::InvalidArgs { .. } => (
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                "adapter built invalid args".to_string(),
            ),
            // §5.13.2's exit-code classification is P6's; a reasonable
            // Tool/Retryable default until then.
            crate::types::tools::ToolError::ExecutionFailed { message, .. } => (
                ErrorClass::Tool,
                RetryDisposition::Retryable,
                format!("tool execution failed: {message}"),
            ),
        },
        AdapterError::PermissionDenied(m) => (
            ErrorClass::Tool,
            RetryDisposition::NonRetryable,
            format!("permission denied: {m}"),
        ),
        AdapterError::Timeout => (
            ErrorClass::Timeout,
            RetryDisposition::Retryable,
            "adapter timeout".to_string(),
        ),
        AdapterError::ShuttingDown => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            "mcp host shutting down".to_string(),
        ),
        AdapterError::Artifact(m) => (
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            format!("artifact store: {m}"),
        ),
    };
    FailureIr {
        node,
        error_class,
        retry,
        diagnostics: vec![],
        notes,
    }
}

/// §5.11.4 ES5: escalation only applies to capability-worker node kinds.
/// `VerifyCompile`/`VerifyTest`/`GateHuman`/`Aggregate` have no model call
/// to escalate.
fn is_capability_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Plan | NodeKind::Analyze | NodeKind::Edit | NodeKind::Review
    )
}

/// Every node not `except` and not already terminal (SK3: reachability is
/// not consulted in MVP). `pub(super)`: also used by `gate.rs`'s deny/expiry
/// terminal writes.
pub(super) fn non_terminal_except(dag: &TaskDag, except: NodeId) -> Vec<NodeId> {
    dag.nodes
        .values()
        .filter(|n| {
            n.id != except
                && matches!(
                    n.state,
                    NodeState::Pending
                        | NodeState::Ready
                        | NodeState::Running
                        | NodeState::WaitingApproval
                )
        })
        .map(|n| n.id)
        .collect()
}

/// §5.12.2 step 3: the in-flight node (`Running`/`Ready`/`WaitingApproval`,
/// at most one under K1) becomes `Cancelled`; every other non-terminal node
/// becomes `Skipped`. When nothing is in flight (cancel arrives before the
/// first frontier promotion), every non-terminal node is `Skipped` and none
/// is `Cancelled` — `c6_cancel` still forces `DagState::Cancelled` directly
/// (it does not rely on derive_dag_state's "≥1 Cancelled" rule).
pub(super) fn cancel_targets(dag: &TaskDag) -> (Vec<NodeId>, Vec<NodeId>) {
    let in_flight = dag
        .nodes
        .values()
        .find(|n| {
            matches!(
                n.state,
                NodeState::Running | NodeState::Ready | NodeState::WaitingApproval
            )
        })
        .map(|n| n.id);
    match in_flight {
        Some(id) => (vec![id], non_terminal_except(dag, id)),
        None => {
            let skipped = dag
                .nodes
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
                .collect();
            (vec![], skipped)
        }
    }
}

/// §5.16.3 / T8 shared attribution order: selected Ready (if given) → else
/// lowest `Ready` → else lowest `Pending`.
fn attribution_target(dag: &TaskDag, selected_ready: Option<NodeId>) -> Option<NodeId> {
    if let Some(id) = selected_ready {
        return Some(id);
    }
    if let Some(id) = ready_nodes(dag).into_iter().next() {
        return Some(id); // ready_nodes is already ascending.
    }
    dag.nodes
        .values()
        .filter(|n| n.state == NodeState::Pending)
        .map(|n| n.id)
        .min()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::adapters::{
        Approval, CapabilityExecutor, GateHumanAdapter, VerifyCompileAdapter, VerifyTestAdapter,
    };
    use crate::dag::{
        Backoff, DependencyEdge, EdgeKind, NodeInputEnvelope, NodeInputPayload, PredecessorOutput,
        RetryPolicy, TaskNode,
    };
    use crate::events::EventSink;
    use crate::obs::{ProcessCostMeterFactory, RecordingDecisionLog, RetentionPolicy};
    use crate::runtime::AlloyRuntime;
    use crate::scheduler::linear::{LinearSchedulerDeps, SchedConfig};
    use crate::session::{RunGoalRecord, Session, SessionPlane};
    use crate::storage::{
        install_sqlite_event_sink, AlloyStorage, ArtifactKind, ArtifactPut, ArtifactStore,
        DagStore, EventStore, RunRow, SessionRows, StorageOpenOptions,
    };
    use crate::types::budget::{BudgetPolicy, Goal, ModelTier, TokenBudget};
    use crate::types::ids::{ArtifactId, CapabilityId, GateId, ProfileId, SessionId, Timestamp};

    // ---- test doubles ----

    struct StaticCapability {
        outcomes: StdMutex<VecDeque<Result<CapabilityOutcome, CapabilityExecError>>>,
    }
    impl StaticCapability {
        fn new(outcomes: Vec<Result<CapabilityOutcome, CapabilityExecError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: StdMutex::new(VecDeque::from(outcomes)),
            })
        }
    }
    #[async_trait]
    impl CapabilityExecutor for StaticCapability {
        async fn execute(
            &self,
            _ctx: &CapabilityExecContext,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(CapabilityExecError::Internal("exhausted".into())))
        }
    }

    struct StaticVerify {
        outcomes: StdMutex<VecDeque<Result<VerifyOutcome, AdapterError>>>,
    }
    impl StaticVerify {
        fn new(outcomes: Vec<Result<VerifyOutcome, AdapterError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: StdMutex::new(VecDeque::from(outcomes)),
            })
        }
        fn ok_once() -> Arc<Self> {
            Self::new(vec![Ok(VerifyOutcome {
                ok: true,
                diagnostics: vec![],
                raw_artifact: None,
            })])
        }
    }
    #[async_trait]
    impl VerifyCompileAdapter for StaticVerify {
        async fn check(&self, _ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(AdapterError::Internal("exhausted".into())))
        }
    }
    #[async_trait]
    impl VerifyTestAdapter for StaticVerify {
        async fn test(&self, _ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(AdapterError::Internal("exhausted".into())))
        }
    }

    /// `GateHumanAdapter` test double that also drives the real control plane
    /// (`SessionPlane::register_gate_waiter` + `approve`) for `Ok` outcomes.
    ///
    /// §5.7.10 requires `gate_wait_and_dispatch` to re-scan for a durable
    /// `ApprovalResolved` event after the wait returns, trusting that over the
    /// in-memory decision (crash-tolerant by design). A canned in-memory
    /// return with no matching event would make that scan correctly fail
    /// closed, so this double performs the same two writes a real
    /// `SessionGateHumanAdapter` + operator `approve()` call would.
    struct StaticGate {
        plane: SessionPlane,
        outcomes: StdMutex<VecDeque<Result<Approval, AdapterError>>>,
    }
    impl StaticGate {
        fn new(plane: SessionPlane, outcomes: Vec<Result<Approval, AdapterError>>) -> Arc<Self> {
            Arc::new(Self {
                plane,
                outcomes: StdMutex::new(VecDeque::from(outcomes)),
            })
        }
    }
    #[async_trait]
    impl GateHumanAdapter for StaticGate {
        async fn wait_approval(
            &self,
            ctx: &NodeExecContext,
            gate: GateId,
        ) -> Result<Approval, AdapterError> {
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(AdapterError::Internal("exhausted".into())));
            if let Ok(approval) = outcome {
                // Keep the receiver alive until `approve` fires the sender —
                // dropping it immediately (e.g. via a bare `?` on the `Result`)
                // makes `approve`'s `sender.send` fail closed.
                let _rx = self
                    .plane
                    .register_gate_waiter(ctx.meta.run_id, gate)
                    .await
                    .map_err(|e| AdapterError::Internal(format!("static gate register: {e}")))?;
                self.plane
                    .approve(ctx.meta.run_id, gate, approval)
                    .await
                    .map_err(|e| AdapterError::Internal(format!("static gate approve: {e}")))?;
            }
            outcome
        }
    }

    // ---- fixture ----

    struct Fixture {
        _dir: tempfile::TempDir,
        _rt: AlloyRuntime,
        storage: Arc<AlloyStorage>,
        plane: SessionPlane,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut rt = AlloyRuntime::new();
            rt.configure(crate::config::RuntimeConfig {
                data_dir: dir.path().join("runtime"),
                data_dir_rule: "test",
                profile_path: dir.path().join("profiles/default.toml"),
                router_path: dir.path().join("router.toml"),
                env_file_hint: dir.path().join("example.env"),
                retain_full_prompts: false,
                retain_tool_bodies: false,
                run_timeout: Duration::from_secs(30),
                budget_policy: BudgetPolicy::default(),
                context_profile: crate::context::ContextProfile::v2_defaults(),
            })
            .unwrap();
            let handle = rt.start().await.unwrap();
            let storage = install_sqlite_event_sink(
                &handle,
                Some(StorageOpenOptions::for_data_dir(dir.path().join("storage"))),
            )
            .await
            .unwrap();
            let plane = SessionPlane::new(handle, Arc::clone(&storage));
            Self {
                _dir: dir,
                _rt: rt,
                storage,
                plane,
            }
        }

        async fn close(self) {
            self.storage.close().await.unwrap();
            self._rt.shutdown().await.unwrap();
        }

        async fn seed_session(&self) -> SessionId {
            let session = Session {
                id: SessionId::new(),
                workspace_root: std::path::PathBuf::from("/tmp/alloy-loop-test-ws"),
                profile: ProfileId::new("default").unwrap(),
                budget: BudgetPolicy::default(),
                language_backends: vec![],
                created_at: Timestamp::now(),
            };
            self.storage
                .sessions()
                .upsert_session(&session)
                .await
                .unwrap();
            session.id
        }

        async fn seed_run(&self, session_id: SessionId, dag_id: DagId, state: &str) -> RunId {
            self.seed_run_with_constraints(session_id, dag_id, state, vec![])
                .await
        }

        /// §5.16.1 BG1/BG2 test hook: `seed_run` with an explicit
        /// `goal.constraints` list (e.g. `Constraint::MaxUsd(_)`).
        async fn seed_run_with_constraints(
            &self,
            session_id: SessionId,
            dag_id: DagId,
            state: &str,
            constraints: Vec<crate::types::budget::Constraint>,
        ) -> RunId {
            let run_id = RunId::new();
            let goal = RunGoalRecord {
                goal: Goal {
                    text: "fix it".into(),
                    constraints,
                    attachments: vec![],
                },
                dag_id,
            };
            let row = RunRow {
                id: run_id,
                session_id,
                goal_json: serde_json::to_value(&goal).unwrap(),
                state: state.into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            };
            self.storage.sessions().upsert_run(&row).await.unwrap();
            run_id
        }

        /// Plan-time placeholder input, standing in for whatever the
        /// planner would have written for a non-root node before the
        /// scheduler's own C5 rewrite ever runs.
        async fn put_placeholder_input(
            &self,
            dag_id: DagId,
            node_id: NodeId,
            kind: NodeKind,
            preds: Vec<PredecessorOutput>,
        ) -> ArtifactId {
            let env = NodeInputEnvelope::new(
                dag_id,
                node_id,
                kind,
                1,
                NodeInputPayload::FromPredecessors { preds },
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        }

        /// A plain success-output artifact for a node that already
        /// `Succeeded`, usable as a real `output_ref` (no `pending_pred`
        /// label, so E3 accepts it as a predecessor slot).
        async fn put_node_output(&self, dag_id: DagId, node_id: NodeId) -> ArtifactId {
            let env = crate::dag::NodeOutputEnvelope::new(
                dag_id,
                node_id,
                NodeKind::Edit,
                1,
                1,
                serde_json::json!({"ok": true}),
            );
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes: serde_json::to_vec(&env).unwrap(),
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        }

        async fn put_pending_placeholder_artifact(&self) -> ArtifactId {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"schema_version": 1, "pending": true}))
                    .unwrap();
            let mut labels = serde_json::Map::new();
            labels.insert(
                "alloy.envelope".into(),
                serde_json::Value::String("pending_pred".into()),
            );
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels,
                })
                .await
                .unwrap()
        }

        async fn put_goal_envelope(
            &self,
            dag_id: DagId,
            node_id: NodeId,
            kind: NodeKind,
        ) -> ArtifactId {
            let env = NodeInputEnvelope::new(
                dag_id,
                node_id,
                kind,
                1,
                NodeInputPayload::Goal(Goal {
                    text: "fix".into(),
                    constraints: vec![],
                    attachments: vec![],
                }),
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        }

        #[allow(clippy::too_many_arguments)]
        fn build_scheduler(
            &self,
            sched_dir: std::path::PathBuf,
            capabilities: Arc<dyn CapabilityExecutor>,
            verify_compile: Arc<dyn VerifyCompileAdapter>,
            verify_test: Arc<dyn VerifyTestAdapter>,
            gate_human: Arc<dyn GateHumanAdapter>,
            budget_policy: BudgetPolicy,
            run_timeout: Duration,
        ) -> LinearScheduler {
            self.build_scheduler_full(
                sched_dir,
                capabilities,
                verify_compile,
                verify_test,
                gate_human,
                budget_policy,
                run_timeout,
                CancellationToken::new(),
            )
            .0
        }

        /// Full variant exposing the [`RecordingDecisionLog`] (so tests can
        /// assert on §5.11.2's `Retry` decision records) and the
        /// `runtime_cancel` token (so tests can simulate a process-wide
        /// drain interrupting an in-flight backoff sleep, B3).
        #[allow(clippy::too_many_arguments)]
        fn build_scheduler_full(
            &self,
            sched_dir: std::path::PathBuf,
            capabilities: Arc<dyn CapabilityExecutor>,
            verify_compile: Arc<dyn VerifyCompileAdapter>,
            verify_test: Arc<dyn VerifyTestAdapter>,
            gate_human: Arc<dyn GateHumanAdapter>,
            budget_policy: BudgetPolicy,
            run_timeout: Duration,
            runtime_cancel: CancellationToken,
        ) -> (LinearScheduler, Arc<RecordingDecisionLog>) {
            let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
            let deps = LinearSchedulerDeps {
                dags: self.storage.dags(),
                artifacts: self.storage.artifacts(),
                events: self.storage.events(),
                sessions: self.storage.sessions(),
                session_plane: self.plane.clone(),
                runs: self.plane.runs(),
                verify_compile,
                verify_test,
                gate_human,
                capabilities,
                decisions: Arc::clone(&decisions) as Arc<dyn crate::obs::DecisionLog>,
                cost_meters: Arc::new(ProcessCostMeterFactory::new()),
                runtime_cancel,
                budget_policy,
                run_timeout,
                config: {
                    let mut c = SchedConfig::new(sched_dir);
                    // These are loop-mechanics tests, not validator tests:
                    // relax load validation (require_gates / exactly-one-root
                    // etc.) so hand-built minimal DAGs don't need to satisfy
                    // template-shape rules unrelated to what's under test.
                    c.validate_on_load = false;
                    c
                },
            };
            (LinearScheduler::new_for_test(deps).unwrap(), decisions)
        }

        /// BE4 probe: identical wiring, but every `DecisionLog::record`
        /// fails, so a pre-CAS record cannot be persisted.
        fn build_scheduler_with_failing_decisions(
            &self,
            sched_dir: std::path::PathBuf,
            capabilities: Arc<dyn CapabilityExecutor>,
        ) -> LinearScheduler {
            self.build_scheduler_with_failing_decisions_and_budget(
                sched_dir,
                capabilities,
                BudgetPolicy::default(),
            )
        }

        /// [`Self::build_scheduler_with_failing_decisions`] with an explicit
        /// ceiling, so the budget stop path can be driven too.
        fn build_scheduler_with_failing_decisions_and_budget(
            &self,
            sched_dir: std::path::PathBuf,
            capabilities: Arc<dyn CapabilityExecutor>,
            budget_policy: BudgetPolicy,
        ) -> LinearScheduler {
            let deps = LinearSchedulerDeps {
                dags: self.storage.dags(),
                artifacts: self.storage.artifacts(),
                events: self.storage.events(),
                sessions: self.storage.sessions(),
                session_plane: self.plane.clone(),
                runs: self.plane.runs(),
                verify_compile: Arc::new(crate::adapters::UnavailableVerifyCompile),
                verify_test: Arc::new(crate::adapters::UnavailableVerifyTest),
                gate_human: Arc::new(crate::adapters::UnavailableGateHuman),
                capabilities,
                decisions: Arc::new(AlwaysFailingDecisionLog),
                cost_meters: Arc::new(ProcessCostMeterFactory::new()),
                runtime_cancel: CancellationToken::new(),
                budget_policy,
                run_timeout: Duration::from_secs(30),
                config: {
                    let mut c = SchedConfig::new(sched_dir);
                    c.validate_on_load = false;
                    c
                },
            };
            LinearScheduler::new_for_test(deps).unwrap()
        }
    }

    struct AlwaysFailingDecisionLog;
    #[async_trait]
    impl crate::obs::DecisionLog for AlwaysFailingDecisionLog {
        async fn record(
            &self,
            _rec: DecisionRecord,
        ) -> Result<crate::types::ids::EventSeq, crate::obs::ObsError> {
            Err(crate::obs::ObsError::Invalid("decision sink down".into()))
        }
        async fn record_model_call(
            &self,
            _rec: crate::obs::ModelCallRecord,
        ) -> Result<crate::types::ids::EventSeq, crate::obs::ObsError> {
            Err(crate::obs::ObsError::Invalid("decision sink down".into()))
        }
        async fn record_tool_call(
            &self,
            _rec: crate::obs::ToolCallRecord,
        ) -> Result<crate::types::ids::EventSeq, crate::obs::ObsError> {
            Err(crate::obs::ObsError::Invalid("decision sink down".into()))
        }
    }

    fn adapter_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![],
            escalate_after: None,
            escalate_to_tier: None,
        }
    }

    fn expected_capability(kind: NodeKind) -> &'static str {
        match kind {
            NodeKind::Plan => "planning",
            NodeKind::Analyze => "repair",
            NodeKind::Edit => "edit",
            NodeKind::Review => "review",
            other => panic!("{other:?} has no capability-node mapping"),
        }
    }

    fn llm_node(id: NodeId, kind: NodeKind, input_ref: ArtifactId, retry: RetryPolicy) -> TaskNode {
        TaskNode {
            id,
            kind,
            capability: Some(CapabilityId::new(expected_capability(kind)).unwrap()),
            input_ref,
            output_ref: None,
            state: NodeState::Pending,
            retry,
            cache_key: None,
            budget: TokenBudget {
                max_input: 1000,
                max_output: 1000,
            },
            model_tier: ModelTier::Economy,
            approval: None,
            timeout_ms: 30_000,
        }
    }

    fn adapter_node(id: NodeId, kind: NodeKind, input_ref: ArtifactId) -> TaskNode {
        TaskNode {
            id,
            kind,
            capability: None,
            input_ref,
            output_ref: None,
            state: NodeState::Pending,
            retry: adapter_retry(),
            cache_key: None,
            budget: TokenBudget {
                max_input: 0,
                max_output: 0,
            },
            model_tier: ModelTier::Economy,
            approval: None,
            timeout_ms: 30_000,
        }
    }

    // ---- happy path: 4-node chain (§11.2) ----

    #[tokio::test]
    async fn happy_path_four_node_chain_reaches_succeeded() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();

        let analyze = NodeId::new();
        let edit = NodeId::new();
        let verify = NodeId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();

        let analyze_input = fx
            .put_goal_envelope(dag_id, analyze, NodeKind::Analyze)
            .await;
        let analyze_pending_output = fx.put_pending_placeholder_artifact().await;
        let edit_input = fx
            .put_placeholder_input(
                dag_id,
                edit,
                NodeKind::Edit,
                vec![PredecessorOutput {
                    node_id: analyze,
                    kind: NodeKind::Analyze,
                    output_ref: analyze_pending_output,
                }],
            )
            .await;
        let edit_pending_output = fx.put_pending_placeholder_artifact().await;
        let verify_input = fx
            .put_placeholder_input(
                dag_id,
                verify,
                NodeKind::VerifyCompile,
                vec![PredecessorOutput {
                    node_id: edit,
                    kind: NodeKind::Edit,
                    output_ref: edit_pending_output,
                }],
            )
            .await;
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;

        let mut nodes = BTreeMap::new();
        nodes.insert(
            analyze,
            llm_node(analyze, NodeKind::Analyze, analyze_input, adapter_retry()),
        );
        nodes.insert(
            edit,
            llm_node(edit, NodeKind::Edit, edit_input, adapter_retry()),
        );
        nodes.insert(
            verify,
            adapter_node(verify, NodeKind::VerifyCompile, verify_input),
        );
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "ship it".into(),
        });
        nodes.insert(gate, gate_node_val);

        let edges = vec![
            DependencyEdge {
                from: analyze,
                to: edit,
                kind: EdgeKind::Data,
            },
            DependencyEdge {
                from: analyze,
                to: edit,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: edit,
                to: verify,
                kind: EdgeKind::Data,
            },
            DependencyEdge {
                from: edit,
                to: verify,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: verify,
                to: gate,
                kind: EdgeKind::Sequence,
            },
        ];
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes,
            edges,
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"analysis": "ok"}),
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"patch": "ok"}),
            }),
        ]);
        let verify_compile = StaticVerify::ok_once();
        let gate_human = StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Allow)]);

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            verify_compile,
            Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human,
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        assert_eq!(outcome.failed_node, None);

        let final_dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        for node in final_dag.nodes.values() {
            assert!(
                matches!(node.state, NodeState::Succeeded),
                "node {} ended in {:?}",
                node.id,
                node.state
            );
            assert!(node.output_ref.is_some());
        }
        fx.close().await;
    }

    // ---- R9 fast path ----

    #[tokio::test]
    async fn r9_fast_path_returns_terminal_dag_without_recompute() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.state = NodeState::Succeeded;
        node.output_ref = Some(fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await);
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Succeeded,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "succeeded").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        assert_eq!(outcome.failed_node, None);
        fx.close().await;
    }

    // ---- Appendix F multi-row binding (AC 76) ----

    /// Seed a run row bound to `dag_id` with an explicit state and
    /// `created_at`, for Appendix F multi-candidate tests.
    async fn seed_run_row_at(
        fx: &Fixture,
        session_id: SessionId,
        dag_id: DagId,
        state: &str,
        unix_secs: i64,
    ) -> RunId {
        let run_id = RunId::new();
        let goal = RunGoalRecord {
            goal: Goal {
                text: "fix it".into(),
                constraints: vec![],
                attachments: vec![],
            },
            dag_id,
        };
        let at = Timestamp(time::OffsetDateTime::from_unix_timestamp(unix_secs).unwrap());
        let row = RunRow {
            id: run_id,
            session_id,
            goal_json: serde_json::to_value(&goal).unwrap(),
            state: state.into(),
            created_at: at.clone(),
            updated_at: at,
        };
        fx.storage.sessions().upsert_run(&row).await.unwrap();
        run_id
    }

    /// Minimal stored DAG for binding-resolution tests plus a scheduler to
    /// call [`LinearScheduler::resolve_run_binding`] on directly.
    async fn binding_fixture(fx: &Fixture, session: SessionId) -> (TaskDag, LinearScheduler) {
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-binding"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        (dag, sched)
    }

    /// AC 76 / Appendix F RB6: with several candidate rows, a non-terminal
    /// `Running` row wins over a newer non-`Running` row.
    #[tokio::test]
    async fn rb6_prefers_the_running_row_over_a_newer_non_running_row() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag, sched) = binding_fixture(&fx, session).await;

        let running = seed_run_row_at(&fx, session, dag.id, "running", 1_000).await;
        let _newer_pending = seed_run_row_at(&fx, session, dag.id, "pending", 2_000).await;

        let row = sched.resolve_run_binding(&dag).await.unwrap();
        assert_eq!(row.id, running, "RB6 must prefer the Running row");
        fx.close().await;
    }

    /// AC 76 / Appendix F RB5: with no `Running` row, the last row under
    /// (`created_at` ascending, `run_id` ascending) wins — i.e. the maximum;
    /// on equal `created_at` the higher `run_id` breaks the tie.
    #[tokio::test]
    async fn rb5_orders_candidates_by_created_at_then_run_id() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag, sched) = binding_fixture(&fx, session).await;

        // Distinct created_at: the newer row wins regardless of id order.
        let _older = seed_run_row_at(&fx, session, dag.id, "pending", 1_000).await;
        let newer = seed_run_row_at(&fx, session, dag.id, "pending", 2_000).await;
        let row = sched.resolve_run_binding(&dag).await.unwrap();
        assert_eq!(row.id, newer, "RB5 must pick the max created_at");

        // Equal created_at: run_id ascending breaks the tie, max wins.
        let tie_a = seed_run_row_at(&fx, session, dag.id, "pending", 3_000).await;
        let tie_b = seed_run_row_at(&fx, session, dag.id, "pending", 3_000).await;
        let expected = if tie_a > tie_b { tie_a } else { tie_b };
        let row = sched.resolve_run_binding(&dag).await.unwrap();
        assert_eq!(
            row.id, expected,
            "RB5 tie-break is run_id ascending, last wins"
        );
        fx.close().await;
    }

    // ---- §5.7.9 closed-receiver classification (AC 39) ----

    /// Gate adapter whose waiter always "closes" (`AdapterError::Internal`),
    /// optionally flipping the run row to a target `RunControlState` string
    /// first — so `gate_closed_receiver` observes exactly that durable state.
    struct ClosedReceiverGate {
        sessions: Arc<dyn SessionRows>,
        flip_to: Option<&'static str>,
        calls: StdMutex<u32>,
    }
    impl ClosedReceiverGate {
        fn new(sessions: Arc<dyn SessionRows>, flip_to: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                sessions,
                flip_to,
                calls: StdMutex::new(0),
            })
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }
    #[async_trait]
    impl GateHumanAdapter for ClosedReceiverGate {
        async fn wait_approval(
            &self,
            ctx: &NodeExecContext,
            _gate: GateId,
        ) -> Result<Approval, AdapterError> {
            *self.calls.lock().unwrap() += 1;
            if let Some(state) = self.flip_to {
                let mut row = self
                    .sessions
                    .get_run(ctx.meta.run_id)
                    .await
                    .unwrap()
                    .expect("run row exists");
                row.state = state.into();
                self.sessions.upsert_run(&row).await.unwrap();
            }
            Err(AdapterError::Internal("waiter closed".into()))
        }
    }

    /// Fresh single-node gate DAG (Pending) plus its "running" run row.
    async fn seed_pending_gate_dag(
        fx: &Fixture,
        session: SessionId,
    ) -> (DagId, NodeId, GateId, RunId) {
        let dag_id = DagId::new();
        let node_id = NodeId::new();
        let gate_id = GateId::new();
        let input = fx
            .put_goal_envelope(dag_id, node_id, NodeKind::GateHuman)
            .await;
        let mut node = adapter_node(node_id, NodeKind::GateHuman, input);
        node.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "review".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(node_id, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;
        (dag_id, node_id, gate_id, run_id)
    }

    fn closed_receiver_scheduler(fx: &Fixture, gate: Arc<ClosedReceiverGate>) -> LinearScheduler {
        fx.build_scheduler(
            fx._dir.path().join("s-closed-recv"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            gate,
            BudgetPolicy::default(),
            Duration::from_secs(30),
        )
    }

    /// §5.7.9 active-states row: a closed waiter while the run row stays
    /// `Running` re-registers up to `GATE_REREGISTER_MAX` (3) times, then
    /// becomes `SchedError::Internal`. The call count pins the bound:
    /// 1 initial wait + 3 re-registrations.
    #[tokio::test]
    async fn closed_receiver_while_running_reregisters_thrice_then_internal() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, ..) = seed_pending_gate_dag(&fx, session).await;
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), None);
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(&err, SchedError::Internal(m) if m.contains("gate waiter closed in state Running")),
            "expected Internal(closed in Running), got {err:?}"
        );
        assert_eq!(gate.calls(), 4, "1 initial wait + GATE_REREGISTER_MAX (3)");
        fx.close().await;
    }

    /// §5.7.9 `Cancelling`/`Cancelled` row: a closed waiter with a durable
    /// cancel takes the cancel path and terminalizes the DAG `Cancelled`.
    #[tokio::test]
    async fn closed_receiver_while_cancelling_takes_the_cancel_path() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, ..) = seed_pending_gate_dag(&fx, session).await;
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("cancelling"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Cancelled);
        assert_eq!(gate.calls(), 1, "no re-registration on the cancel row");
        fx.close().await;
    }

    /// §5.7.9 `Failed` row, no durable resolution: the gate terminalizes as
    /// `Expired` with an `Approval`-class failure carrying the closed-waiter
    /// note — never a re-registration loop.
    #[tokio::test]
    async fn closed_receiver_while_failed_without_resolution_expires_the_gate() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, node_id, ..) = seed_pending_gate_dag(&fx, session).await;
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("failed"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node_id));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        assert!(
            failure.notes.contains("gate waiter closed; run failed"),
            "unexpected notes: {}",
            failure.notes
        );
        fx.close().await;
    }

    /// §5.7.9 `Failed` row with a durable `deny`: the durable resolution is
    /// followed (GD3 denied failure), not the closed-waiter expiry.
    #[tokio::test]
    async fn closed_receiver_while_failed_with_durable_deny_applies_deny() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, node_id, gate_id, run_id) = seed_pending_gate_dag(&fx, session).await;
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::ApprovalResolved,
                payload: serde_json::json!({
                    "gate_id": gate_id.to_string(),
                    "decision": "deny",
                    "generation": 1u64,
                }),
            })
            .await
            .unwrap();
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("failed"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node_id));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.notes, "approval denied");
        fx.close().await;
    }

    /// §5.7.9 `Failed` row with a durable `allow`: contradictory durable
    /// state is an `Invariant`, never silently allowed or expired.
    #[tokio::test]
    async fn closed_receiver_while_failed_with_durable_allow_is_invariant() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, _node_id, gate_id, run_id) = seed_pending_gate_dag(&fx, session).await;
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::ApprovalResolved,
                payload: serde_json::json!({
                    "gate_id": gate_id.to_string(),
                    "decision": "allow",
                    "generation": 1u64,
                }),
            })
            .await
            .unwrap();
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("failed"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(&err, SchedError::Invariant(m) if m.contains("resolution is Allow")),
            "expected Invariant(Failed-with-Allow), got {err:?}"
        );
        fx.close().await;
    }

    /// §5.7.9 `Succeeded` row: a run that claims success while its gate is
    /// still pending is an `Invariant`.
    #[tokio::test]
    async fn closed_receiver_while_succeeded_is_invariant() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, ..) = seed_pending_gate_dag(&fx, session).await;
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("succeeded"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(&err, SchedError::Invariant(m) if m.contains("run succeeded while gate pending")),
            "expected Invariant(succeeded-while-pending), got {err:?}"
        );
        fx.close().await;
    }

    /// §5.7.9 `ReplanRequested` row: the replan path wins and the DAG lands
    /// in `ReplanRequired` with no failure attribution.
    #[tokio::test]
    async fn closed_receiver_while_replan_requested_takes_the_replan_path() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, ..) = seed_pending_gate_dag(&fx, session).await;
        let gate = ClosedReceiverGate::new(fx.storage.sessions(), Some("replan_requested"));
        let sched = closed_receiver_scheduler(&fx, Arc::clone(&gate));

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::ReplanRequired);
        assert_eq!(outcome.failed_node, None);
        assert_eq!(outcome.failure, None);
        fx.close().await;
    }

    // ---- gate-before-C3 ordering (AC 71) and no scheduler model/tool
    // ---- emission (AC 38) ----

    /// Run an `Analyze → GateHuman(allow)` chain to `Succeeded` and return
    /// the session events plus the recording decision log.
    async fn run_analyze_gate_chain(
        fx: &Fixture,
        session: SessionId,
    ) -> (
        NodeId,
        Vec<crate::events::SessionEvent>,
        Arc<RecordingDecisionLog>,
    ) {
        let dag_id = DagId::new();
        let analyze = NodeId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let analyze_input = fx
            .put_goal_envelope(dag_id, analyze, NodeKind::Analyze)
            .await;
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "ship it".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([
                (
                    analyze,
                    llm_node(analyze, NodeKind::Analyze, analyze_input, adapter_retry()),
                ),
                (gate, gate_node_val),
            ]),
            edges: vec![DependencyEdge {
                from: analyze,
                to: gate,
                kind: EdgeKind::Sequence,
            }],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Succeeded {
            payload: serde_json::json!({"analysis": "ok"}),
        })]);
        let gate_human = StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Allow)]);
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s-gate-order"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human,
            BudgetPolicy::default(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 200)
            .await
            .unwrap();
        (gate, events, decisions)
    }

    /// AC 71: a `GateHuman` node is never C3-dispatched to `Running` while
    /// its gate is unresolved — every durable event that moves the gate node
    /// to `running` sits *after* the durable `ApprovalResolved` in the log.
    #[tokio::test]
    async fn gate_node_reaches_running_only_after_a_durable_resolution() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (gate, events, _decisions) = run_analyze_gate_chain(&fx, session).await;

        let resolved_seq = events
            .iter()
            .find(|e| e.type_ == crate::events::SessionEventType::ApprovalResolved)
            .map(|e| e.seq)
            .expect("allow must write a durable ApprovalResolved");
        let gate_running: Vec<_> = events
            .iter()
            .filter(|e| {
                e.type_ == crate::events::SessionEventType::NodeState
                    && e.payload.get("node_id").and_then(serde_json::Value::as_str)
                        == Some(gate.to_string().as_str())
                    && e.payload.get("to").and_then(serde_json::Value::as_str) == Some("running")
            })
            .collect();
        assert!(
            !gate_running.is_empty(),
            "the allow fold must move the gate through running"
        );
        for ev in gate_running {
            assert!(
                ev.seq > resolved_seq,
                "gate node moved to running (seq {:?}) before its resolution (seq {resolved_seq:?})",
                ev.seq
            );
        }
        fx.close().await;
    }

    /// AC 38: the scheduler layer itself never records `ModelCall` or
    /// `ToolCall` decisions — those belong to workers/tools (RFC-0004
    /// attribution). A full capability + gate run must leave both recorders
    /// empty while still recording ordinary decisions.
    #[tokio::test]
    async fn scheduler_never_records_model_or_tool_calls() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (_gate, _events, decisions) = run_analyze_gate_chain(&fx, session).await;

        assert!(
            decisions.recorded_model_calls().is_empty(),
            "scheduler must not emit ModelCall records"
        );
        assert!(
            decisions.recorded_tool_calls().is_empty(),
            "scheduler must not emit ToolCall records"
        );
        fx.close().await;
    }

    // ---- gate resume crash points (ACs 33/35/36) ----

    /// Seed a durably `WaitingApproval` single-gate DAG (the pre-crash
    /// state), its `waiting_approval` run row, and the prior process's
    /// durable `ApprovalRequested`. Optionally seed a durable resolution.
    async fn seed_waiting_gate_resume(
        fx: &Fixture,
        session: SessionId,
        resolution: Option<&str>,
    ) -> (DagId, NodeId, GateId, RunId) {
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.state = NodeState::WaitingApproval;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::WaitingApproval,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx
            .seed_run(session, dag_id, RunControlState::WaitingApproval.as_str())
            .await;
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::ApprovalRequested,
                payload: serde_json::json!({
                    "gate_id": gate_id.to_string(),
                    "reason": "risky",
                    "timeout_ms": 30_000u64,
                    "generation": 1u64,
                }),
            })
            .await
            .unwrap();
        if let Some(decision) = resolution {
            fx.storage
                .events()
                .append_session(crate::events::NewSessionEvent {
                    session_id: session,
                    run_id: Some(run_id),
                    type_: crate::events::SessionEventType::ApprovalResolved,
                    payload: serde_json::json!({
                        "gate_id": gate_id.to_string(),
                        "decision": decision,
                        "generation": 1u64,
                    }),
                })
                .await
                .unwrap();
        }
        (dag_id, gate, gate_id, run_id)
    }

    /// AC 36: resume with a durable `expired` resolution terminalizes from
    /// the scan alone — `expire_gate` is not called again (the run row is
    /// left exactly as seeded) and no second `ApprovalResolved` is written.
    #[tokio::test]
    async fn gate_resume_with_durable_expired_terminalizes_without_reexpiring() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, gate, _gate_id, run_id) =
            seed_waiting_gate_resume(&fx, session, Some("expired")).await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s-resume-expired"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman), // must never wait
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);

        // `expire_gate` was not re-driven: the run row is untouched and the
        // seeded resolution is still the only one.
        let row = fx
            .storage
            .sessions()
            .get_run(run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state, "waiting_approval",
            "run row must not be rewritten"
        );
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        let resolved = events
            .iter()
            .filter(|e| e.type_ == crate::events::SessionEventType::ApprovalResolved)
            .count();
        assert_eq!(resolved, 1, "no second ApprovalResolved may be written");
        fx.close().await;
    }

    /// AC 35 (also AC 33's `WaitingApproval` crash point): resume with no
    /// durable resolution re-registers the waiter only — the surviving
    /// `ApprovalRequested` is *not* re-emitted (GR3 repairs only a missing
    /// one) and the gate then resolves normally.
    #[tokio::test]
    async fn gate_resume_waiting_without_resolution_reregisters_without_a_second_request() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, _gate, _gate_id, _run_id) = seed_waiting_gate_resume(&fx, session, None).await;

        let gate_human = StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Allow)]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-resume-waiting"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human,
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        let requested = events
            .iter()
            .filter(|e| e.type_ == crate::events::SessionEventType::ApprovalRequested)
            .count();
        assert_eq!(
            requested, 1,
            "the surviving ApprovalRequested must not be duplicated on resume"
        );
        fx.close().await;
    }

    /// AC 33's post-fold crash point: the allow fold's C9b completed (gate
    /// node durably `Succeeded`) but the process died before the terminal
    /// C7. Resume must finish the DAG naturally without consulting any
    /// adapter.
    #[tokio::test]
    async fn gate_resume_after_completed_fold_finishes_the_dag_naturally() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.state = NodeState::Succeeded;
        gate_node_val.output_ref = Some(fx.put_node_output(dag_id, gate).await);
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::Running, // C7 never landed
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s-resume-folded"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        assert_eq!(outcome.failed_node, None);
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Succeeded, "C7 must land on resume");
        fx.close().await;
    }

    // ---- R4b vs concurrent unowned cancel (AC 88) ----

    /// `DagStore` interposer: serves the first `get` for `target` from the
    /// real store, then immediately writes the RC4-shaped terminal blob a
    /// concurrent unowned `cancel` would have committed — so the R4b re-load
    /// (the second `get`) observes the cancel's write.
    struct CancelBetweenLoadsDagStore {
        inner: Arc<dyn DagStore>,
        target: DagId,
        fired: StdMutex<bool>,
        gets: std::sync::atomic::AtomicU32,
    }
    #[async_trait]
    impl DagStore for CancelBetweenLoadsDagStore {
        async fn put(&self, dag: &TaskDag) -> Result<(), crate::storage::StoreError> {
            self.inner.put(dag).await
        }
        async fn put_if_generation(
            &self,
            dag: &TaskDag,
            expected: Option<u64>,
        ) -> Result<(), crate::storage::StoreError> {
            self.inner.put_if_generation(dag, expected).await
        }
        async fn replace_for_replan(
            &self,
            dag: &TaskDag,
            expected_generation: u64,
        ) -> Result<(), crate::storage::ReplanReplaceError> {
            self.inner
                .replace_for_replan(dag, expected_generation)
                .await
        }
        async fn get(&self, dag_id: DagId) -> Result<Option<TaskDag>, crate::storage::StoreError> {
            let res = self.inner.get(dag_id).await?;
            if dag_id == self.target {
                self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let fire = {
                    let mut fired = self.fired.lock().unwrap();
                    !std::mem::replace(&mut *fired, true)
                };
                if fire {
                    if let Some(mut cancelled) = res.clone() {
                        for n in cancelled.nodes.values_mut() {
                            n.state = NodeState::Cancelled;
                        }
                        cancelled.state = DagState::Cancelled;
                        self.inner.put(&cancelled).await?;
                    }
                }
            }
            Ok(res)
        }
        async fn delete(&self, dag_id: DagId) -> Result<(), crate::storage::StoreError> {
            self.inner.delete(dag_id).await
        }
        async fn list_by_session(
            &self,
            session_id: SessionId,
        ) -> Result<Vec<DagId>, crate::storage::StoreError> {
            self.inner.list_by_session(session_id).await
        }
    }

    /// AC 88 / R4b: when a concurrent unowned `cancel` commits its terminal
    /// write between R1's load and R4b's re-load, the re-load observes it
    /// and short-circuits at R9 — assembling the `Cancelled` outcome instead
    /// of executing over (and overwriting) the cancel's write.
    #[tokio::test]
    async fn r4b_reload_short_circuits_on_a_concurrent_unowned_cancel_write() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let interposed = Arc::new(CancelBetweenLoadsDagStore {
            inner: fx.storage.dags(),
            target: dag_id,
            fired: StdMutex::new(false),
            gets: std::sync::atomic::AtomicU32::new(0),
        });
        let deps = LinearSchedulerDeps {
            dags: Arc::clone(&interposed) as Arc<dyn DagStore>,
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: Arc::new(crate::adapters::UnavailableVerifyCompile),
            verify_test: Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human: Arc::new(crate::adapters::UnavailableGateHuman),
            // Unavailable: any dispatch would fail the run — proof R9 won.
            capabilities: Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()))
                as Arc<dyn crate::obs::DecisionLog>,
            cost_meters: Arc::new(ProcessCostMeterFactory::new()),
            runtime_cancel: CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(30),
            config: {
                let mut c = SchedConfig::new(fx._dir.path().join("s-r4b-race"));
                c.validate_on_load = false;
                c
            },
        };
        let sched = LinearScheduler::new_for_test(deps).unwrap();

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(
            outcome.state,
            DagState::Cancelled,
            "R9 must assemble, not execute"
        );
        assert_eq!(outcome.failed_node, None);
        assert!(
            interposed.gets.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "R4b must re-load under ownership"
        );
        // The cancel's write survives untouched — no fresh execution overwrote it.
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Cancelled);
        assert_eq!(stored.nodes[&a].state, NodeState::Cancelled);
        fx.close().await;
    }

    // ---- T7/T8 timeout attribution (AC 65) ----

    /// Capability that never completes — only a deadline can end it.
    struct HangingCapability;
    #[async_trait]
    impl CapabilityExecutor for HangingCapability {
        async fn execute(
            &self,
            _ctx: &CapabilityExecContext,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            std::future::pending().await
        }
    }

    /// AC 65 / T8: the shared attribution chain in isolation — selected
    /// Ready verbatim, else lowest Ready, else lowest Pending, else None.
    #[test]
    fn attribution_target_follows_the_t8_fallback_chain() {
        let dag_id = DagId::new();
        let mut ids: Vec<NodeId> = (0..4).map(|_| NodeId::new()).collect();
        ids.sort();
        let (p_low, p_high, r_low, r_high) = (ids[0], ids[1], ids[2], ids[3]);
        let mut nodes = BTreeMap::new();
        for (&id, state) in ids.iter().zip([
            NodeState::Pending,
            NodeState::Pending,
            NodeState::Ready,
            NodeState::Ready,
        ]) {
            let mut n = llm_node(id, NodeKind::Analyze, ArtifactId::new(), adapter_retry());
            n.state = state;
            nodes.insert(id, n);
        }
        let mut dag = TaskDag {
            id: dag_id,
            session_id: SessionId::new(),
            generation: 1,
            nodes,
            edges: vec![],
            state: DagState::Running,
        };

        // A selected Ready node is used verbatim, even over a lower Ready.
        assert_eq!(attribution_target(&dag, Some(r_high)), Some(r_high));
        // No selection: lowest Ready wins over every Pending.
        assert_eq!(attribution_target(&dag, None), Some(r_low));
        // No Ready left: lowest Pending.
        for id in [r_low, r_high] {
            dag.nodes.get_mut(&id).unwrap().state = NodeState::Succeeded;
        }
        assert_eq!(attribution_target(&dag, None), Some(p_low));
        // Nothing attributable: None (run_timeout_path turns this into an
        // Invariant rather than inventing a node).
        for id in [p_low, p_high] {
            dag.nodes.get_mut(&id).unwrap().state = NodeState::Skipped;
        }
        assert_eq!(attribution_target(&dag, None), None);
    }

    /// Single hanging-capability DAG for the T7 deadline-attribution tests.
    async fn seed_hanging_node(
        fx: &Fixture,
        session: SessionId,
        node_timeout_ms: u64,
    ) -> (DagId, NodeId) {
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.timeout_ms = node_timeout_ms;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;
        (dag_id, a)
    }

    /// AC 65 / T7: on an exact tie (`remaining_run == node.timeout_ms`) the
    /// run budget is the binding constraint — the elapsed deadline is a run
    /// timeout (NonRetryable), not a retryable node timeout. Pins the `<=`
    /// in `run_attributed`.
    #[tokio::test(start_paused = true)]
    async fn t7_deadline_tie_attributes_to_the_run_not_the_node() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let run_timeout = Duration::from_millis(100);
        let (dag_id, node) = seed_hanging_node(&fx, session, 100).await;

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s-t7-tie"),
            Arc::new(HangingCapability),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            run_timeout,
        ));
        let handle = tokio::spawn({
            let sched = Arc::clone(&sched);
            async move { sched.run(dag_id).await }
        });
        tokio::time::advance(Duration::from_millis(200)).await;
        let outcome = handle.await.unwrap().unwrap();

        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(
            outcome.failed_node,
            Some(node),
            "T7 hint names the in-flight node"
        );
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Timeout);
        assert_eq!(
            failure.retry,
            RetryDisposition::NonRetryable,
            "a tie is a run timeout (T8 shape), never a retryable node timeout"
        );
        assert!(
            failure.notes.contains("run timeout"),
            "unexpected notes: {}",
            failure.notes
        );
        fx.close().await;
    }

    /// AC 65 / T7 converse: a node deadline strictly inside the run budget
    /// stays node-attributed — `ErrorClass::Timeout` and Retryable.
    #[tokio::test(start_paused = true)]
    async fn t7_node_deadline_inside_run_budget_is_a_retryable_node_timeout() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, node) = seed_hanging_node(&fx, session, 50).await;

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s-t7-node"),
            Arc::new(HangingCapability),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));
        let handle = tokio::spawn({
            let sched = Arc::clone(&sched);
            async move { sched.run(dag_id).await }
        });
        tokio::time::advance(Duration::from_millis(200)).await;
        let outcome = handle.await.unwrap().unwrap();

        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Timeout);
        assert_eq!(failure.retry, RetryDisposition::Retryable);
        assert!(
            failure.notes.contains("node timeout"),
            "unexpected notes: {}",
            failure.notes
        );
        fx.close().await;
    }

    // ---- BE4 post-CAS half (AC 82) ----

    /// AC 82's post-CAS half: `record_gate_decision` runs *after*
    /// `c9c_gate_deny`'s CAS, so a decision-sink outage there is logged and
    /// swallowed — the deny still terminalizes the DAG instead of aborting
    /// the already-committed checkpoint.
    #[tokio::test]
    async fn be4_post_cas_gate_decision_failure_is_logged_not_aborted() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, node_id, ..) = seed_pending_gate_dag(&fx, session).await;

        let deps = LinearSchedulerDeps {
            dags: fx.storage.dags(),
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: Arc::new(crate::adapters::UnavailableVerifyCompile),
            verify_test: Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human: StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Deny)]),
            capabilities: Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            decisions: Arc::new(AlwaysFailingDecisionLog),
            cost_meters: Arc::new(ProcessCostMeterFactory::new()),
            runtime_cancel: CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(30),
            config: {
                let mut c = SchedConfig::new(fx._dir.path().join("s-be4-post"));
                c.validate_on_load = false;
                c
            },
        };
        let sched = LinearScheduler::new_for_test(deps).unwrap();

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed, "deny must still land");
        assert_eq!(outcome.failed_node, Some(node_id));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.notes, "approval denied");
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Failed, "the CAS must be durable");
        fx.close().await;
    }

    // ---- R8 meter rebuild on resume (AC 62) ----

    /// AC 62 / R8 (B7/B8): a resumed run's meter is *assigned* from the
    /// durable event rebuild, never added onto a stale in-process meter — a
    /// prior attempt's leftover accumulation must not double the totals.
    #[tokio::test]
    async fn resumed_run_meter_is_rebuilt_not_double_counted() {
        use crate::obs::CostMeterFactory as _;
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        // The durable truth: exactly one model call for this run.
        let log =
            crate::obs::EventDecisionLog::from_handle(fx._rt.handle(), Arc::clone(&fx.storage))
                .unwrap();
        crate::obs::DecisionLog::record_model_call(
            &log,
            crate::obs::ModelCallRecord::new(
                session,
                crate::types::ids::ProviderId::new("p").unwrap(),
                ModelTier::Standard,
            )
            .run(run_id)
            .tokens(Some(10), Some(2))
            .usd(Some(0.1)),
        )
        .await
        .unwrap();

        // The hazard: the process-local meter still holds the prior
        // attempt's in-memory accumulation of that same call.
        let factory = Arc::new(ProcessCostMeterFactory::new());
        factory.meter_for(run_id).with_mut(|m| {
            m.add_model_usage(ModelTier::Standard, Some(10), Some(2), Some(0.1));
        });

        let deps = LinearSchedulerDeps {
            dags: fx.storage.dags(),
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: Arc::new(crate::adapters::UnavailableVerifyCompile),
            verify_test: Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human: Arc::new(crate::adapters::UnavailableGateHuman),
            capabilities: StaticCapability::new(vec![Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"analysis": "ok"}),
            })]),
            decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()))
                as Arc<dyn crate::obs::DecisionLog>,
            cost_meters: Arc::clone(&factory) as Arc<_>,
            runtime_cancel: CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(30),
            config: {
                let mut c = SchedConfig::new(fx._dir.path().join("s-meter"));
                c.validate_on_load = false;
                c
            },
        };
        let sched = LinearScheduler::new_for_test(deps).unwrap();
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        // R8 assigned the rebuilt meter: one durable call, not the durable
        // call plus the stale in-process copy.
        let snap = factory.meter_for(run_id).with_mut(|m| m.snapshot());
        assert_eq!(snap.model_calls, 1, "meter must be rebuilt, not summed");
        assert_eq!(snap.tokens_in, 10);
        assert_eq!(snap.tokens_out, 2);
        assert!((snap.usd_spent.unwrap() - 0.1).abs() < 1e-9);
        fx.close().await;
    }

    // ---- §4.4 ownership release on panic (AC 49) ----

    /// Capability that parks until the test fires `trigger`, then panics —
    /// a worker bug unwinding out of the run body. Deliberately not keyed on
    /// the cancel token: the dispatch `select!` drops the capability future
    /// on cancellation, so a cancel-triggered panic would race with a clean
    /// checkpoint cancel instead of deterministically unwinding.
    struct PanicOnTriggerCapability {
        executing: Arc<std::sync::atomic::AtomicBool>,
        trigger: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl CapabilityExecutor for PanicOnTriggerCapability {
        async fn execute(
            &self,
            _ctx: &CapabilityExecContext,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            let notified = self.trigger.notified();
            self.executing
                .store(true, std::sync::atomic::Ordering::SeqCst);
            notified.await;
            panic!("worker bug: panic mid-execute");
        }
    }

    /// AC 49 / §4.4 G1-G3: a panic unwinding out of the run body still
    /// releases DAG ownership via `OwnedGuard::drop`, records the panic as
    /// the cancel result (so a waiting `cancel` resolves immediately instead
    /// of burning its full drain grace), and leaves the DAG re-acquirable.
    #[tokio::test]
    async fn owned_guard_drop_releases_ownership_when_the_run_body_panics() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let executing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let trigger = Arc::new(tokio::sync::Notify::new());
        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s-panic"),
            Arc::new(PanicOnTriggerCapability {
                executing: Arc::clone(&executing),
                trigger: Arc::clone(&trigger),
            }),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));
        let run_sched = Arc::clone(&sched);
        let run_handle = tokio::spawn(async move { run_sched.run(dag_id).await });

        // Wait until the capability is parked in execute(), then grab the
        // live OwnedDag handle a concurrent `cancel` would be waiting on.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !executing.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "capability never started executing"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let owned = sched
            .lookup_owned(dag_id)
            .unwrap()
            .expect("run must own the DAG while executing");

        // Fire the panic; the run task surfaces it...
        trigger.notify_waiters();
        let join = run_handle.await;
        assert!(
            join.is_err_and(|e| e.is_panic()),
            "run body must have panicked"
        );

        // ...and `OwnedGuard::drop` recorded the panic as the cancel result
        // and notified: a waiter on the pre-panic handle resolves right away
        // with the truthful outcome instead of burning its drain grace.
        let result = owned
            .wait_for_completion(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("drop must have set a result and notified");
        assert!(
            matches!(&result, Err(SchedError::Internal(m)) if m.contains("run body panicked")),
            "expected the recorded panic outcome, got {result:?}"
        );
        // ...and ownership was released by `OwnedGuard::drop` (G1): the map
        // is empty and a fresh `run` is not `AlreadyOwned`.
        assert!(sched.lookup_owned(dag_id).unwrap().is_none());
        let second = sched.run(dag_id).await;
        assert!(
            !matches!(second, Err(SchedError::AlreadyOwned(_))),
            "ownership must not leak after a panic, got {second:?}"
        );
        fx.close().await;
    }

    // ---- §5.7.2 resolution-scan generation filter (AC 79) ----

    /// AC 79: `scan_gate_resolution` ignores an `ApprovalResolved` recorded
    /// against a stale generation — only an event carrying the scanned
    /// generation resolves the gate, and among those the newest wins.
    #[tokio::test]
    async fn scan_gate_resolution_ignores_a_stale_generation_event() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate_id = GateId::new();
        let run_id = RunId::new();
        let ctx = CheckpointCtx {
            session_id: session,
            run_id: Some(run_id),
        };
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-scan-gen"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let append_resolution = |decision: &'static str, generation: u64| {
            let events = fx.storage.events();
            async move {
                events
                    .append_session(crate::events::NewSessionEvent {
                        session_id: session,
                        run_id: Some(run_id),
                        type_: crate::events::SessionEventType::ApprovalResolved,
                        payload: serde_json::json!({
                            "gate_id": gate_id.to_string(),
                            "decision": decision,
                            "generation": generation,
                        }),
                    })
                    .await
                    .unwrap();
            }
        };

        // A generation-1 allow alone must not resolve a generation-2 scan.
        append_resolution("allow", 1).await;
        let got = sched
            .scan_gate_resolution(dag_id, ctx, gate_id, 2)
            .await
            .unwrap();
        assert_eq!(got, None, "stale-generation allow must be ignored");

        // The same event is still visible to its own generation's scan.
        let got = sched
            .scan_gate_resolution(dag_id, ctx, gate_id, 1)
            .await
            .unwrap();
        assert_eq!(got, Some(GateResolution::Allow));

        // With a matching generation-2 deny appended, the generation-2 scan
        // resolves to it — the stale gen-1 allow still never leaks in.
        append_resolution("deny", 2).await;
        let got = sched
            .scan_gate_resolution(dag_id, ctx, gate_id, 2)
            .await
            .unwrap();
        assert_eq!(got, Some(GateResolution::Deny));
        fx.close().await;
    }

    // ---- §5.7.8 expire_gate retry loop (AC 80) ----

    /// Single-gate DAG with a short real-time deadline plus a `NeverGate`
    /// scheduler, for the `expire_gate` retry tests. Returns the spawned run
    /// handle once the plane's gate waiter is durably registered (run row
    /// `waiting_approval`) — the point after which the next run-row upsert
    /// belongs to `expire_gate`.
    async fn spawn_expiring_gate_run(
        fx: &Fixture,
        session: SessionId,
    ) -> (
        DagId,
        NodeId,
        RunId,
        tokio::task::JoinHandle<Result<DagOutcome, SchedError>>,
    ) {
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.timeout_ms = 50;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s-expire-retry"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(NeverGate {
                plane: fx.plane.clone(),
            }),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));
        let handle = tokio::spawn(async move { sched.run(dag_id).await });

        // Wait (bounded) for the waiter registration to land durably; after
        // this there are no plane-side run-row writes until `expire_gate`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let row = fx.storage.sessions().get_run(run_id).await.unwrap();
            if row.is_some_and(|r| r.state == "waiting_approval") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gate waiter never registered"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        (dag_id, gate, run_id, handle)
    }

    /// AC 80 / §5.7.8: a transient `Err(other)` from `expire_gate` is
    /// retried after `EXPIRE_RETRY_BACKOFF`; the second attempt succeeds and
    /// the expiry is durable (ApprovalResolved `expired` + run row `failed`).
    #[tokio::test]
    async fn expire_gate_transient_error_is_retried_then_durable() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (_dag_id, gate_node, run_id, handle) = spawn_expiring_gate_run(&fx, session).await;

        // Exactly one injected failure: attempt 1 eats it, attempt 2 lands.
        fx.plane.fail_next_run_upsert();

        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate_node));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(
            failure.notes, "approval timeout",
            "retry succeeded — not the exhaust shape"
        );

        // The retry made the control-plane expiry durable after all.
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        let resolved = events
            .iter()
            .find(|e| e.type_ == crate::events::SessionEventType::ApprovalResolved)
            .expect("second attempt must write ApprovalResolved");
        assert_eq!(resolved.payload["decision"], serde_json::json!("expired"));
        let row = fx
            .storage
            .sessions()
            .get_run(run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "failed", "persist_expiry moved the run row");
        fx.close().await;
    }

    /// AC 80 / §5.7.8 GT3(iii): when every `expire_gate` attempt fails with
    /// `Err(other)`, the scheduler exhausts `EXPIRE_RETRY_MAX` (3) and
    /// terminalizes locally rather than propagating `Err` or leaving the DAG
    /// `WaitingApproval` — the control plane never records the expiry.
    #[tokio::test]
    async fn expire_gate_exhausts_retries_and_terminalizes_locally() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let (dag_id, gate_node, run_id, handle) = spawn_expiring_gate_run(&fx, session).await;

        // Re-arm the one-shot injector continuously so every attempt's
        // step-5 upsert fails. Only `expire_gate` writes run rows from here
        // on, so the arming cannot hit an unrelated write.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let armer = {
            let plane = fx.plane.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    plane.fail_next_run_upsert();
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        };

        let outcome = handle.await.unwrap().unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        armer.join().unwrap();

        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate_node));
        let failure = outcome.failure.expect("failure ir");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        assert!(
            failure
                .notes
                .contains("expire_gate failed after 3 attempts"),
            "expected the exhaust notes, got: {}",
            failure.notes
        );

        // GT3(iii) is local: no durable ApprovalResolved, run row untouched,
        // but the DAG itself is durably terminal (not WaitingApproval).
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|e| e.type_ == crate::events::SessionEventType::ApprovalResolved),
            "control-plane expiry must never have landed"
        );
        let row = fx
            .storage
            .sessions()
            .get_run(run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "waiting_approval", "run row was never moved");
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Failed);
        fx.close().await;
    }

    // ---- R2/R3 entry errors (ACs 12/13) ----

    /// AC 12: `run` on a DAG with no bound run row fails with
    /// `RunBindingMissing` (§5.1 R3 / Appendix F RB1) before any ownership
    /// or state write.
    #[tokio::test]
    async fn run_without_a_bound_run_row_is_run_binding_missing() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        // Deliberately no `seed_run`: nothing binds this DAG to a run.

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(err, SchedError::RunBindingMissing(d) if d == dag_id),
            "expected RunBindingMissing({dag_id}), got {err:?}"
        );
        // R3 precedes R4 ownership and every checkpoint: the stored blob is
        // untouched and no session event was appended.
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Pending);
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        assert!(events.is_empty(), "no events expected, got {events:?}");
        fx.close().await;
    }

    /// AC 13: with `validate_on_load = true` (the production default), `run`
    /// on an invalid stored blob fails at R2 with `Invariant` and issues no
    /// CAS — the invalid blob is left exactly as stored.
    #[tokio::test]
    async fn run_with_validate_on_load_rejects_invalid_blob_without_cas() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        // V5 violation: a self-loop edge on the only node.
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![DependencyEdge {
                from: a,
                to: a,
                kind: EdgeKind::Sequence,
            }],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let deps = LinearSchedulerDeps {
            dags: fx.storage.dags(),
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: Arc::new(crate::adapters::UnavailableVerifyCompile),
            verify_test: Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human: Arc::new(crate::adapters::UnavailableGateHuman),
            capabilities: Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            decisions: decisions as Arc<dyn crate::obs::DecisionLog>,
            cost_meters: Arc::new(ProcessCostMeterFactory::new()),
            runtime_cancel: CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(30),
            // Production default: validate_on_load stays true.
            config: SchedConfig::new(fx._dir.path().join("s1")),
        };
        let sched = LinearScheduler::new_for_test(deps).unwrap();

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(err, SchedError::Invariant(_)),
            "expected Invariant, got {err:?}"
        );
        // No CAS: the invalid blob is byte-identical in intent — same state,
        // same node state, same edge list — and no session event exists.
        let stored = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.state, DagState::Pending);
        assert_eq!(stored.nodes[&a].state, NodeState::Pending);
        assert_eq!(stored.edges.len(), 1);
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        assert!(events.is_empty(), "no events expected, got {events:?}");
        fx.close().await;
    }

    /// FO1/FO2/FO3 fixture: a durably `Failed` single-node DAG, with the
    /// node's own `NodeState` event and artifact left to each test to seed
    /// (or not) below `Fixture::put_goal_envelope`'s placeholder input.
    async fn seed_failed_single_node(fx: &Fixture, session: SessionId, dag_id: DagId) -> NodeId {
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.state = NodeState::Failed;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Failed,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "failed").await;
        a
    }

    #[tokio::test]
    async fn fo1_r9_recovers_failure_from_the_durable_artifact() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let node_id = seed_failed_single_node(&fx, session, dag_id).await;

        let original = FailureIr {
            node: node_id,
            error_class: ErrorClass::Compile,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "cargo check exited 101".into(),
        };
        let artifact_id = fx
            .storage
            .artifacts()
            .put(crate::storage::ArtifactPut {
                bytes: serde_json::to_vec(&original).unwrap(),
                kind: crate::storage::ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                session_id: Some(session),
                run_id: None,
                labels: serde_json::Map::new(),
            })
            .await
            .unwrap();
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: None,
                type_: crate::events::SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": node_id.to_string(),
                    "from": "running",
                    "to": "failed",
                    "generation": 1u64,
                    "failure_ref": artifact_id.to_string(),
                    "error_class": "compile",
                    "retry": "non_retryable",
                }),
            })
            .await
            .unwrap();

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node_id));
        assert_eq!(outcome.failure, Some(original));
        fx.close().await;
    }

    #[tokio::test]
    async fn fo2_r9_degrades_to_event_fields_when_artifact_is_missing() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let node_id = seed_failed_single_node(&fx, session, dag_id).await;

        // A NodeState event with error_class/retry but no failure_ref at all.
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: None,
                type_: crate::events::SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": node_id.to_string(),
                    "from": "running",
                    "to": "failed",
                    "generation": 1u64,
                    "error_class": "test",
                    "retry": "retryable",
                }),
            })
            .await
            .unwrap();

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node_id));
        let failure = outcome.failure.unwrap();
        assert_eq!(failure.node, node_id);
        assert_eq!(failure.error_class, ErrorClass::Test);
        assert_eq!(failure.retry, RetryDisposition::Retryable);
        assert_eq!(failure.notes, "failure detail unavailable");
        assert!(failure.diagnostics.is_empty());
        fx.close().await;
    }

    #[tokio::test]
    async fn fo3_r9_synthesizes_internal_failure_when_no_event_exists() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        // No NodeState event appended at all — durable state is Failed
        // (e.g. a very old generation whose events aged out, or a corrupted
        // write) but there's nothing to recover from.
        let node_id = seed_failed_single_node(&fx, session, dag_id).await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(node_id));
        let failure = outcome.failure.unwrap();
        assert_eq!(failure.node, node_id);
        assert_eq!(failure.error_class, ErrorClass::Internal);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        assert_eq!(failure.notes, "failure detail unavailable; event missing");
        fx.close().await;
    }

    // ---- FN2: gate deny/expiry attribution on R9 (§5.18, AC 66/92) ----

    /// Gate deny/expiry (C9c) and gate-origin RC4 both leave the gate node
    /// `Cancelled`, never `Failed`. `seeded` controls whether the C9c
    /// `NodeState` event survived the crash.
    async fn seed_gate_denied_dag(
        fx: &Fixture,
        session: SessionId,
        dag_id: DagId,
        seed_event: bool,
    ) -> (NodeId, NodeId) {
        let analyze = NodeId::new();
        let gate = NodeId::new();
        let a_input = fx
            .put_goal_envelope(dag_id, analyze, NodeKind::Analyze)
            .await;
        let g_input = fx
            .put_goal_envelope(dag_id, gate, NodeKind::GateHuman)
            .await;
        let mut a_node = llm_node(analyze, NodeKind::Analyze, a_input, adapter_retry());
        // Every other non-terminal node is Skipped by C9c, never Failed.
        a_node.state = NodeState::Skipped;
        let mut g_node = adapter_node(gate, NodeKind::GateHuman, g_input);
        g_node.state = NodeState::Cancelled;
        g_node.approval = Some(crate::dag::ApprovalSpec {
            gate: GateId::new(),
            reason: "review the diff".into(),
        });

        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(analyze, a_node), (gate, g_node)]),
            edges: vec![],
            state: DagState::Failed,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "failed").await;

        if seed_event {
            let failure = FailureIr {
                node: gate,
                error_class: ErrorClass::Approval,
                retry: RetryDisposition::NonRetryable,
                diagnostics: vec![],
                notes: "approval denied".into(),
            };
            let artifact = fx
                .storage
                .artifacts()
                .put(crate::storage::ArtifactPut {
                    bytes: serde_json::to_vec(&failure).unwrap(),
                    kind: crate::storage::ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: Some(session),
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap();
            fx.storage
                .events()
                .append_session(crate::events::NewSessionEvent {
                    session_id: session,
                    run_id: None,
                    type_: crate::events::SessionEventType::NodeState,
                    payload: serde_json::json!({
                        "node_id": gate.to_string(),
                        "from": "waiting_approval",
                        "to": "cancelled",
                        "generation": 1u64,
                        "decision": "deny",
                        "failure_ref": artifact.to_string(),
                        "error_class": "approval",
                        "retry": "non_retryable",
                    }),
                })
                .await
                .unwrap();
        }
        (analyze, gate)
    }

    #[tokio::test]
    async fn fn2_r9_attributes_a_resumed_gate_denied_dag_to_the_cancelled_gate() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_analyze, gate) = seed_gate_denied_dag(&fx, session, dag_id, true).await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();

        assert_eq!(outcome.state, DagState::Failed);
        // FN1 finds nothing (no node is `Failed`); FN2 must attribute the gate.
        assert_eq!(outcome.failed_node, Some(gate));
        let failure = outcome.failure.expect("FO6: Failed + Some(node) ⇒ Some");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.notes, "approval denied");
        fx.close().await;
    }

    /// AC 92: the gate CAS committed but its `NodeState` event was lost, so
    /// nothing durable says "approval". RF7 must synthesize it *before* FN2
    /// selection, or the attribution is lost entirely.
    #[tokio::test]
    async fn fn2_r9_rf7_repairs_a_lost_gate_event_before_attributing() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_analyze, gate) = seed_gate_denied_dag(&fx, session, dag_id, false).await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();

        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate));
        let failure = outcome.failure.expect("FO6");
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.notes, "repaired after crash");

        // RF7 is a durable repair: the event it synthesized must be there.
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        assert!(
            events.iter().any(|e| {
                e.type_ == crate::events::SessionEventType::NodeState
                    && e.payload["node_id"] == gate.to_string()
                    && e.payload["to"] == "cancelled"
                    && e.payload["repaired"] == true
            }),
            "RF7 must append the lost cancelled event"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn fn1_r9_wins_over_fn2_when_both_shapes_are_present() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (analyze, gate) = seed_gate_denied_dag(&fx, session, dag_id, true).await;

        // Promote the non-gate node to `Failed`: FN1 must now win outright,
        // whichever NodeId sorts lower.
        let mut dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        dag.nodes.get_mut(&analyze).unwrap().state = NodeState::Failed;
        fx.storage.dags().put(&dag).await.unwrap();

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.failed_node, Some(analyze));
        assert_ne!(outcome.failed_node, Some(gate));
        fx.close().await;
    }

    #[tokio::test]
    async fn fn3_r9_does_not_attribute_a_cancelled_node_without_an_approval_failure() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();

        // A `Cancelled` non-gate node on a `Failed` DAG with no durable
        // Approval failure: FN2 must not claim it (FO3's floor is `Internal`).
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.state = NodeState::Cancelled;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Failed,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "failed").await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, None, "FN3");
        assert!(outcome.failure.is_none());
        fx.close().await;
    }

    // ---- R4 ownership ----

    #[tokio::test]
    async fn r4_second_run_for_same_dag_is_already_owned() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // A capability that blocks until released, so the first `run` stays
        // in flight while the second is attempted.
        struct Blocking(tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);
        #[async_trait]
        impl CapabilityExecutor for Blocking {
            async fn execute(
                &self,
                _ctx: &CapabilityExecContext,
            ) -> Result<CapabilityOutcome, CapabilityExecError> {
                let rx = self.0.lock().await.take().unwrap();
                let _ = rx.await;
                Ok(CapabilityOutcome::Succeeded {
                    payload: serde_json::json!({}),
                })
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let capabilities = Arc::new(Blocking(tokio::sync::Mutex::new(Some(rx))));

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));

        let sched2 = Arc::clone(&sched);
        let first = tokio::spawn(async move { sched2.run(dag_id).await });
        // Give the first run time to reach ownership + dispatch.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(matches!(err, SchedError::AlreadyOwned(id) if id == dag_id));

        tx.send(()).unwrap();
        let outcome = first.await.unwrap().unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        fx.close().await;
    }

    // ---- L8 serial invariant ----

    #[tokio::test]
    async fn l8_multiple_ready_nodes_is_invariant_violation() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let input_a = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let input_b = fx.put_goal_envelope(dag_id, b, NodeKind::Analyze).await;
        let mut node_a = llm_node(a, NodeKind::Analyze, input_a, adapter_retry());
        node_a.state = NodeState::Ready;
        let mut node_b = llm_node(b, NodeKind::Analyze, input_b, adapter_retry());
        node_b.state = NodeState::Ready;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node_a), (b, node_b)]),
            edges: vec![],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let err = sched.run(dag_id).await.unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("multiple ready nodes")));
        fx.close().await;
    }

    // ---- gate deny ----

    #[tokio::test]
    async fn gate_deny_terminalizes_failed_with_approval_class() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let gate_human = StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Deny)]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            gate_human,
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate));
        let failure = outcome.failure.unwrap();
        assert_eq!(failure.error_class, ErrorClass::Approval);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        fx.close().await;
    }

    /// `GateHumanAdapter` that registers a real waiter (so the control plane
    /// durably reaches `waiting_approval`, matching what a real
    /// `SessionGateHumanAdapter` does) but then never resolves it — the
    /// scheduler's own `timeout_ms` deadline (GC1) must be what fires, not
    /// the adapter.
    struct NeverGate {
        plane: SessionPlane,
    }
    #[async_trait]
    impl GateHumanAdapter for NeverGate {
        async fn wait_approval(
            &self,
            ctx: &NodeExecContext,
            gate: GateId,
        ) -> Result<Approval, AdapterError> {
            let _rx = self
                .plane
                .register_gate_waiter(ctx.meta.run_id, gate)
                .await
                .map_err(|e| AdapterError::Internal(format!("never gate register: {e}")))?;
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gate_expiry_terminalizes_failed_with_approval_class() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.timeout_ms = 50; // GT: short deadline under the paused clock.
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(NeverGate {
                plane: fx.plane.clone(),
            }),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));
        let sched2 = Arc::clone(&sched);
        let handle = tokio::spawn(async move { sched2.run(dag_id).await });
        tokio::time::advance(Duration::from_millis(200)).await;
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(gate));
        let failure = outcome.failure.unwrap();
        assert_eq!(failure.error_class, ErrorClass::Approval); // GT4
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);

        // The control plane must have durably recorded the expiry (§5.7.8).
        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        let resolved = events
            .iter()
            .find(|e| e.type_ == crate::events::SessionEventType::ApprovalResolved)
            .expect("expiry must write a durable ApprovalResolved");
        assert_eq!(resolved.payload["decision"], serde_json::json!("expired"));
        assert_eq!(
            resolved.payload["gate_id"],
            serde_json::json!(gate_id.to_string())
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn gate_resume_with_durable_resolution_never_calls_adapter() {
        // §5.7.2/§5.7.3 crash recovery: a prior process's `approve()` already
        // landed durably before this process ever starts. R16 must find and
        // apply that resolution via the re-scan alone — the adapter must not
        // be touched at all.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.state = NodeState::WaitingApproval; // durable pre-crash state
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::WaitingApproval,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx
            .seed_run(session, dag_id, RunControlState::WaitingApproval.as_str())
            .await;

        // Simulate the prior process's durable ApprovalResolved write (what
        // `RunController::approve` would have appended) without ever
        // registering a waiter in this process.
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::ApprovalResolved,
                payload: serde_json::json!({
                    "gate_id": gate_id.to_string(),
                    "decision": "allow",
                    "generation": 1u64,
                }),
            })
            .await
            .unwrap();

        struct PanicGate;
        #[async_trait]
        impl GateHumanAdapter for PanicGate {
            async fn wait_approval(
                &self,
                _ctx: &NodeExecContext,
                _gate: GateId,
            ) -> Result<Approval, AdapterError> {
                panic!("adapter must not be called when a durable resolution already exists");
            }
        }

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(PanicGate),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        assert_eq!(outcome.failed_node, None);
        fx.close().await;
    }

    // ---- retry admission stub (P5 seam, exercised end to end) ----

    #[tokio::test]
    async fn soft_failure_retries_then_succeeds() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Failed {
                failure: FailureIr {
                    node: a,
                    error_class: ErrorClass::Model,
                    retry: RetryDisposition::Retryable,
                    diagnostics: vec![],
                    notes: "transient model error".into(),
                },
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"ok": true}),
            }),
        ]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        let retry_events = events
            .iter()
            .filter(|e| {
                e.type_ == crate::events::SessionEventType::NodeState
                    && e.payload.get("next_attempt").is_some()
            })
            .count();
        assert_eq!(retry_events, 1, "exactly one C8 retry cycle expected");
        fx.close().await;
    }

    #[tokio::test]
    async fn scheduler_metrics_count_a_retry_then_success_run() {
        // §9.3: the same shape as `soft_failure_retries_then_succeeds`,
        // asserting on `SchedulerMetrics` instead of the event log — proves
        // the counters wired in P9 actually increment on a real run, not
        // just that they compile.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Failed {
                failure: FailureIr {
                    node: a,
                    error_class: ErrorClass::Model,
                    retry: RetryDisposition::Retryable,
                    diagnostics: vec![],
                    notes: "transient model error".into(),
                },
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"ok": true}),
            }),
        ]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let m = sched.metrics();
        assert_eq!(m.runs_started, 1);
        assert_eq!(m.runs_succeeded, 1);
        assert_eq!(m.runs_failed, 0);
        assert_eq!(m.nodes_dispatched, 2); // C3 fires once per attempt
        assert_eq!(m.nodes_succeeded, 1); // only the final attempt reaches C4
        assert_eq!(m.retries_admitted, 1);
        assert_eq!(m.retries_rejected, 0);
        fx.close().await;
    }

    #[tokio::test]
    async fn soft_failure_non_retryable_terminalizes_failed() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let node = llm_node(a, NodeKind::Analyze, input, adapter_retry()); // retry_on empty
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Failed {
            failure: FailureIr {
                node: a,
                error_class: ErrorClass::Model,
                retry: RetryDisposition::NonRetryable,
                diagnostics: vec![],
                notes: "bad prompt".into(),
            },
        })]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(a));
        assert_eq!(outcome.failure.unwrap().notes, "bad prompt");
        fx.close().await;
    }

    // ---- replan ----

    #[tokio::test]
    async fn replan_requested_checkpoints_c10_and_returns() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "replan_requested").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::ReplanRequired);
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::ReplanRequired);
        fx.close().await;
    }

    // ---- budget ----

    #[tokio::test]
    async fn budget_exhausted_before_dispatch_terminalizes_failed() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let policy = BudgetPolicy {
            max_tokens_per_run: 0, // BG6: exhausted from the first check.
            ..BudgetPolicy::default()
        };
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            policy,
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(a));
        assert_eq!(outcome.failure.unwrap().error_class, ErrorClass::Budget);
        fx.close().await;
    }

    #[tokio::test]
    async fn bg3_goal_max_usd_zero_terminalizes_before_dispatch() {
        // §5.16.1 BG2/BG3: a goal `MaxUsd(0.0)` cap must win the effective-
        // ceiling min even though `deps.budget_policy`'s own ceiling (5.0
        // default) is nowhere near exhausted, and the pre-dispatch BG3 check
        // must catch it before the capability is ever called — proven here
        // by using `UnavailableCapabilityExecutor` (which would fail with a
        // *different* error_class than `Budget` if ever reached).
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run_with_constraints(
            session,
            dag_id,
            "running",
            vec![crate::types::budget::Constraint::MaxUsd(0.0)],
        )
        .await;

        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(a));
        assert_eq!(outcome.failure.unwrap().error_class, ErrorClass::Budget);

        // BE1: a Budget decision lands even though `usd_spent` was still
        // `None` (the pre-dispatch BG3 case `maybe_signal_budget_warning`
        // alone would have missed).
        let recorded = decisions.recorded_decisions();
        let exhaustion = recorded
            .iter()
            .find(|d| d.kind == DecisionKind::Budget && d.node == Some(a))
            .expect("budget exhaustion decision must be recorded");
        assert_eq!(exhaustion.metadata["effective_usd"], 0.0);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        assert!(
            events.iter().any(
                |e| e.type_ == crate::events::SessionEventType::BudgetWarning
                    && e.payload["message"]
                        == serde_json::json!("budget exhausted: effective_usd <= 0 (pre-dispatch)")
            ),
            "BE1's pre-dispatch BudgetWarning must land with the BG3-specific message"
        );
        fx.close().await;
    }

    // BG1 (non-finite goal `MaxUsd` ignored + recorded) has no end-to-end
    // test here: JSON cannot represent NaN/Infinity, so a goal carrying one
    // never survives the `goal_json` round trip at all — `resolve_run_binding`
    // (R3, upstream of anything this phase touches) already fails the run
    // with `RunBindingMissing` before the scheduler's own BG1 logic could
    // ever run. `budget::effective_budget`'s pure unit tests (this file's
    // `budget` module) cover BG1 directly against an in-memory `Goal`,
    // which is the only way a non-finite `MaxUsd` can actually reach it.

    // ---- cancel (unowned path, §5.12.4 simplified) ----

    #[tokio::test]
    async fn cancel_unowned_non_terminal_dag_marks_cancelled() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "accepted").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        sched.cancel(dag_id).await.unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Cancelled);
        assert_eq!(persisted.nodes[&a].state, NodeState::Skipped);
        fx.close().await;
    }

    // ---- cancel (owned path, §4.3-4.4/§5.12.2 O4 race-free wait) ----

    #[tokio::test]
    async fn cancel_owned_run_blocks_until_c6_commits_and_interrupts_dispatch() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // Hangs forever unless interrupted — proves `cancel(dag_id)` reaches
        // the in-flight dispatch's own `select!` (`rc.run_cancel`, now the
        // real per-run token via `OwnedDag`), not just the loop-boundary
        // L1/L2 checks a cancel arriving between dispatches would hit.
        struct Never;
        #[async_trait]
        impl CapabilityExecutor for Never {
            async fn execute(
                &self,
                _ctx: &CapabilityExecContext,
            ) -> Result<CapabilityOutcome, CapabilityExecError> {
                std::future::pending().await
            }
        }

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(Never),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));

        let sched2 = Arc::clone(&sched);
        let run_handle = tokio::spawn(async move { sched2.run(dag_id).await });
        tokio::time::sleep(Duration::from_millis(50)).await; // let it reach dispatch

        // O4/CN2: `cancel` MUST NOT return before the run's own C6 is
        // durable — no sleep/poll after this call, so a premature `Ok(())`
        // (the pre-P8 behavior) would make the very next assertion flaky.
        sched.cancel(dag_id).await.unwrap();

        let final_dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(final_dag.state, DagState::Cancelled);
        assert_eq!(final_dag.nodes[&a].state, NodeState::Cancelled);

        let outcome = run_handle.await.unwrap().unwrap();
        assert_eq!(outcome.state, DagState::Cancelled);
        fx.close().await;
    }

    #[tokio::test]
    async fn cancel_owned_run_already_terminal_is_ok_with_no_rewrite() {
        // §5.12.3: "Owned run; run already terminal ⇒ Ok(())." A cancel that
        // lands after the terminal checkpoint must observe it, not error.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Succeeded {
            payload: serde_json::json!({"ok": true}),
        })]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        // The run already returned — ownership is released — so this is the
        // §5.12.4 unowned-but-terminal path (step 3: terminal ⇒ `Ok(())`,
        // no write).
        sched.cancel(dag_id).await.unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Succeeded);
        fx.close().await;
    }

    #[tokio::test]
    async fn cancel_unknown_dag_errors() {
        let fx = Fixture::new().await;
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let err = sched.cancel(DagId::new()).await.unwrap_err();
        assert!(matches!(err, SchedError::DagNotFound(_)));
        fx.close().await;
    }

    // ---- reconcile_terminal_run (§5.20 RC1-RC8) ----

    async fn seed_pending_single_node(fx: &Fixture, session: SessionId, dag_id: DagId) -> NodeId {
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        a
    }

    fn reconcile_scheduler(fx: &Fixture) -> LinearScheduler {
        fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        )
    }

    #[tokio::test]
    async fn reconcile_rc1_rejects_non_terminal_target() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        seed_pending_single_node(&fx, session, dag_id).await;
        let sched = reconcile_scheduler(&fx);

        let err = sched
            .reconcile_terminal_run(dag_id, DagState::Running)
            .await
            .unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc2_missing_dag_errors() {
        let fx = Fixture::new().await;
        let sched = reconcile_scheduler(&fx);
        let err = sched
            .reconcile_terminal_run(DagId::new(), DagState::Failed)
            .await
            .unwrap_err();
        assert!(matches!(err, SchedError::DagNotFound(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc2_already_terminal_is_idempotent_noop() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Succeeded,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Failed)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Succeeded); // untouched
        assert_eq!(persisted.generation, 1); // no CAS at all
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc3_owned_by_live_run_is_noop() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        struct Never;
        #[async_trait]
        impl CapabilityExecutor for Never {
            async fn execute(
                &self,
                _ctx: &CapabilityExecContext,
            ) -> Result<CapabilityOutcome, CapabilityExecError> {
                std::future::pending().await
            }
        }
        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(Never),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        ));
        let sched2 = Arc::clone(&sched);
        let run_handle = tokio::spawn(async move { sched2.run(dag_id).await });
        tokio::time::sleep(Duration::from_millis(50)).await; // let it reach dispatch

        // RC3: a live run in this process owns terminalization — `Ok(())`,
        // no write, and (unlike `cancel`) the run is left running.
        sched
            .reconcile_terminal_run(dag_id, DagState::Failed)
            .await
            .unwrap();
        let mid = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(mid.state, DagState::Running); // C1 already ran; RC3 left it untouched

        sched.cancel(dag_id).await.unwrap(); // clean up the still-running dispatch
        run_handle.await.unwrap().unwrap();
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc4_cancelled_marks_non_terminal_nodes() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = seed_pending_single_node(&fx, session, dag_id).await;
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Cancelled)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Cancelled);
        assert_eq!(persisted.nodes[&a].state, NodeState::Skipped); // Pending, never in flight
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc4_failed_non_gate_attributes_lowest_non_terminal() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let input_lo = fx.put_goal_envelope(dag_id, lo, NodeKind::Analyze).await;
        let input_hi = fx.put_goal_envelope(dag_id, hi, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([
                (
                    lo,
                    llm_node(lo, NodeKind::Analyze, input_lo, adapter_retry()),
                ),
                (
                    hi,
                    llm_node(hi, NodeKind::Analyze, input_hi, adapter_retry()),
                ),
            ]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Failed)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Failed);
        assert_eq!(persisted.nodes[&lo].state, NodeState::Failed); // FN1: lowest NodeId
        assert_eq!(persisted.nodes[&hi].state, NodeState::Skipped);
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc4_failed_no_non_terminal_node_is_bare_cas() {
        // A stale row: `dag.state` still `Pending` but every node already
        // reached a terminal `NodeState` (a corruption/lag, not a scenario
        // this scheduler's live loop can itself produce, but reconcile MUST
        // NOT crash on it — RC4's "no non-terminal node remains" branch).
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.state = NodeState::Succeeded;
        node.output_ref = Some(fx.put_pending_placeholder_artifact().await);
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending, // stale
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Failed)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Failed);
        assert_eq!(persisted.nodes[&a].state, NodeState::Succeeded); // untouched
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc4_rc5_gate_origin_cancels_gate_with_approval_class() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let gate = NodeId::new();
        let gate_id = GateId::new();
        let gate_input = fx
            .put_placeholder_input(dag_id, gate, NodeKind::GateHuman, vec![])
            .await;
        let mut gate_node_val = adapter_node(gate, NodeKind::GateHuman, gate_input);
        gate_node_val.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "risky".into(),
        });
        gate_node_val.state = NodeState::WaitingApproval;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(gate, gate_node_val)]),
            edges: vec![],
            state: DagState::WaitingApproval,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Failed)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Failed);
        assert_eq!(persisted.nodes[&gate].state, NodeState::Cancelled); // FN2, not Failed

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 50)
            .await
            .unwrap();
        let gate_event = events
            .iter()
            .find(|e| {
                e.type_ == crate::events::SessionEventType::NodeState
                    && e.payload.get("error_class").is_some()
            })
            .expect("gate cancellation must carry an error_class");
        assert_eq!(
            gate_event.payload["error_class"],
            serde_json::json!("approval")
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc6_succeeded_with_non_terminal_nodes_writes_failed_instead() {
        // RC6: reconcile MUST NOT invent success.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = seed_pending_single_node(&fx, session, dag_id).await;
        let sched = reconcile_scheduler(&fx);

        sched
            .reconcile_terminal_run(dag_id, DagState::Succeeded)
            .await
            .unwrap();
        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(persisted.state, DagState::Failed); // never Succeeded
        assert_eq!(persisted.nodes[&a].state, NodeState::Failed);
        fx.close().await;
    }

    #[tokio::test]
    async fn reconcile_rc8_racing_cancel_and_reconcile_agree_on_one_terminal_write() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        seed_pending_single_node(&fx, session, dag_id).await;
        let sched = Arc::new(reconcile_scheduler(&fx));

        // Neither call owns the DAG (no live run) — both `cancel` and
        // `reconcile_terminal_run` take the §5.12.4/RC4 "unowned" transient-
        // ownership path. Whichever wins the race writes once; the other
        // MUST observe the resulting terminal state and return `Ok(())`
        // (RC8), never error or double-write.
        let s1 = Arc::clone(&sched);
        let s2 = Arc::clone(&sched);
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { s1.cancel(dag_id).await }),
            tokio::spawn(async move { s2.reconcile_terminal_run(dag_id, DagState::Failed).await }),
        );
        r1.unwrap().unwrap();
        r2.unwrap().unwrap();

        let persisted = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert!(matches!(
            persisted.state,
            DagState::Cancelled | DagState::Failed
        ));
        fx.close().await;
    }

    #[tokio::test]
    async fn ac48_pending_cancel_before_run_starts_fires_at_r5() {
        // AC 48 / PC1-PC2: "A cancel arriving before `run` starts is
        // captured in `pending_cancels` and cancels the run at R5."
        //
        // Reproducing the exact adversarial timing (`cancel`'s own unowned
        // path racing a concurrent `run`'s R4) deterministically would need
        // new test-only synchronization hooks in production code, which
        // isn't worth adding just to exercise this. Instead this seeds
        // `pending_cancels` directly — reachable from here because the
        // field is `pub(in crate::scheduler::linear)` and this test module
        // is a descendant — which is exactly the durable precondition R5
        // itself consumes, so it tests R5's actual mechanism (not a
        // simulation of it): a never-dispatched node with a pending-cancel
        // entry present at `run` entry must terminalize `Cancelled` via C6
        // before ever reaching L9/dispatch, with no capability call at all.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        seed_pending_single_node(&fx, session, dag_id).await;
        fx.seed_run(session, dag_id, "running").await;
        let sched = reconcile_scheduler(&fx); // Unavailable capability: dispatch must not happen.

        sched.pending_cancels.lock().unwrap().insert(dag_id);

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Cancelled);

        // PC4: consumed at R5, not left behind to wrongly cancel a later run.
        assert!(!sched.pending_cancels.lock().unwrap().contains(&dag_id));
        fx.close().await;
    }

    #[tokio::test]
    async fn pc4_cancel_of_a_missing_dag_leaves_no_pending_entry() {
        // The insert used to happen before the durable checks, and only the
        // success path removed it. A `DagNotFound` therefore leaked an entry
        // that would fire `run_cancel` on the next `run()` for that id.
        let fx = Fixture::new().await;
        let sched = reconcile_scheduler(&fx);
        let dag_id = DagId::new();

        let err = sched.cancel(dag_id).await.unwrap_err();
        assert!(matches!(err, SchedError::DagNotFound(_)), "got {err:?}");
        assert!(
            !sched.pending_cancels.lock().unwrap().contains(&dag_id),
            "PC4: a failed cancel must not leave a cancel intent behind"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn pc4_cancel_of_a_terminal_dag_leaves_no_pending_entry() {
        // PC1 only admits an insert for a *non-terminal* durable DAG; a
        // terminal cancel is a no-op and must not arm anything.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        seed_failed_single_node(&fx, session, dag_id).await;
        let sched = reconcile_scheduler(&fx);

        sched.cancel(dag_id).await.unwrap();
        assert!(
            !sched.pending_cancels.lock().unwrap().contains(&dag_id),
            "PC1/PC4: terminal cancel must not arm a pending entry"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn pc4_a_leaked_intent_would_cancel_the_next_unrelated_run() {
        // Guards the *consequence* PC4 exists to prevent, independent of how
        // the entry got there: a stale intent cancels a later, unrelated run
        // of the same DagId (a replan reuses the id and only bumps the
        // generation). If a future refactor reintroduces a leak, this states
        // plainly what it costs.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        seed_pending_single_node(&fx, session, dag_id).await;
        fx.seed_run(session, dag_id, "running").await;
        let sched = reconcile_scheduler(&fx);

        sched.pending_cancels.lock().unwrap().insert(dag_id);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(
            outcome.state,
            DagState::Cancelled,
            "a live entry cancels the next run — which is why it must never outlive its cancel"
        );
        fx.close().await;
    }

    // ---- §5.3.2 row 4: gate adoption with a durable allow ----

    #[tokio::test]
    async fn adopt_running_gate_without_durable_allow_is_an_invariant() {
        // Row 3 — unchanged behaviour, pinned so the row-4 fix below cannot
        // accidentally widen it.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_gate_id, _node) = seed_running_gate(&fx, session, dag_id, None).await;
        let sched = reconcile_scheduler(&fx);

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(err, SchedError::Invariant(ref m) if m == "gate node running"),
            "got {err:?}"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn adopt_running_gate_with_durable_allow_resumes_the_fold() {
        // Row 4: a crash between GA4's post-allow C3 and the fold's C4 used
        // to strand the run forever — every retry reloaded the same
        // `Running` gate and re-raised `Invariant`.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_gate_id, node_id) = seed_running_gate(&fx, session, dag_id, Some("allow")).await;
        let sched = reconcile_scheduler(&fx);

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(after.nodes[&node_id].state, NodeState::Succeeded);
        assert!(
            after.nodes[&node_id].output_ref.is_some(),
            "GA1: the resumed fold must still put a node_output envelope"
        );
        fx.close().await;
    }

    /// A single `GateHuman` node durably `Running` (post-allow C3 committed,
    /// fold's C4 lost). `decision` seeds a durable `ApprovalResolved`.
    async fn seed_running_gate(
        fx: &Fixture,
        session: SessionId,
        dag_id: DagId,
        decision: Option<&str>,
    ) -> (GateId, NodeId) {
        let node_id = NodeId::new();
        let gate_id = GateId::new();
        let input = fx
            .put_goal_envelope(dag_id, node_id, NodeKind::GateHuman)
            .await;
        let mut node = adapter_node(node_id, NodeKind::GateHuman, input);
        node.state = NodeState::Running;
        node.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "review".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(node_id, node)]),
            edges: vec![],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        // The C3 that moved it to Running is durable (W4a/DP1).
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": node_id.to_string(),
                    "from": "ready",
                    "to": "running",
                    "generation": 1u64,
                    "attempt": 1u64,
                }),
            })
            .await
            .unwrap();

        if let Some(decision) = decision {
            fx.storage
                .events()
                .append_session(crate::events::NewSessionEvent {
                    session_id: session,
                    run_id: Some(run_id),
                    type_: crate::events::SessionEventType::ApprovalResolved,
                    payload: serde_json::json!({
                        "gate_id": gate_id.to_string(),
                        "decision": decision,
                        "generation": 1u64,
                    }),
                })
                .await
                .unwrap();
        }
        (gate_id, node_id)
    }

    // ---- ER4 / ER5 / L14 re-verify (§6.5, ACs 84/90/95) ----

    /// `Edit → verify` plus an optional extra node, all in one generation.
    /// The DAG is seeded `Running` (a resume, not a fresh start), so R15 is
    /// not skipped by `performed_c1`.
    async fn seed_edit_verify_dag(
        fx: &Fixture,
        session: SessionId,
        dag_id: DagId,
        verify_state: NodeState,
        extra: Option<(NodeKind, NodeState)>,
    ) -> (NodeId, NodeId, Option<NodeId>) {
        let edit = NodeId::new();
        let verify = NodeId::new();
        let e_input = fx.put_goal_envelope(dag_id, edit, NodeKind::Edit).await;
        let v_input = fx
            .put_goal_envelope(dag_id, verify, NodeKind::VerifyCompile)
            .await;

        let mut e_node = llm_node(edit, NodeKind::Edit, e_input, adapter_retry());
        e_node.state = NodeState::Succeeded;
        // A real output envelope, not `put_pending_placeholder_artifact` —
        // that one carries `alloy.envelope = pending_pred`, which E3 rejects.
        e_node.output_ref = Some(fx.put_node_output(dag_id, edit).await);
        let mut v_node = adapter_node(verify, NodeKind::VerifyCompile, v_input);
        v_node.state = verify_state;

        let mut nodes = BTreeMap::from([(edit, e_node), (verify, v_node)]);
        let mut edges = vec![crate::dag::DependencyEdge {
            from: edit,
            to: verify,
            kind: crate::dag::EdgeKind::Data,
        }];
        let extra_id = match extra {
            Some((kind, state)) => {
                let id = NodeId::new();
                let input = fx.put_goal_envelope(dag_id, id, kind).await;
                let mut n = if is_capability_kind(kind) {
                    llm_node(id, kind, input, adapter_retry())
                } else {
                    adapter_node(id, kind, input)
                };
                n.state = state;
                if kind == NodeKind::GateHuman {
                    n.approval = Some(crate::dag::ApprovalSpec {
                        gate: GateId::new(),
                        reason: "review".into(),
                    });
                }
                nodes.insert(id, n);
                edges.push(crate::dag::DependencyEdge {
                    from: edit,
                    to: id,
                    kind: crate::dag::EdgeKind::Data,
                });
                Some(id)
            }
            None => None,
        };

        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes,
            edges,
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;
        (edit, verify, extra_id)
    }

    /// AC 84: resume with `Edit=Succeeded`, `VerifyCompile=Pending` must
    /// dispatch the verify next — not ER5-fail, and not stall.
    #[tokio::test]
    async fn er4_resume_dispatches_the_pending_verify_next() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_edit, verify, _) =
            seed_edit_verify_dag(&fx, session, dag_id, NodeState::Pending, None).await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s-er4"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            StaticVerify::ok_once(),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );

        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded, "{outcome:?}");
        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(after.nodes[&verify].state, NodeState::Succeeded);
        fx.close().await;
    }

    /// AC 95: the blocked node is durably `Failed` via C7 — `Ok(Failed)`,
    /// never `Err(Invariant)`.
    #[tokio::test]
    async fn er4_l14_blocked_ready_node_is_durably_failed_not_an_error() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        // Topology: Edit(Succeeded) → Review(Ready) → VerifyCompile(Pending).
        // `needs_reverify` holds (the verify is reachable and non-terminal),
        // and the verify is *not* promotable yet, so `Review` is the only
        // Ready node — exactly ER4's "the only Ready node is blocked" case.
        let edit = NodeId::new();
        let review = NodeId::new();
        let verify = NodeId::new();
        let e_input = fx.put_goal_envelope(dag_id, edit, NodeKind::Edit).await;
        let r_input = fx.put_goal_envelope(dag_id, review, NodeKind::Review).await;
        let v_input = fx
            .put_goal_envelope(dag_id, verify, NodeKind::VerifyCompile)
            .await;
        let mut e_node = llm_node(edit, NodeKind::Edit, e_input, adapter_retry());
        e_node.state = NodeState::Succeeded;
        e_node.output_ref = Some(fx.put_node_output(dag_id, edit).await);
        let mut r_node = llm_node(review, NodeKind::Review, r_input, adapter_retry());
        r_node.state = NodeState::Ready;
        let mut v_node = adapter_node(verify, NodeKind::VerifyCompile, v_input);
        v_node.state = NodeState::Pending;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(edit, e_node), (review, r_node), (verify, v_node)]),
            edges: vec![
                crate::dag::DependencyEdge {
                    from: edit,
                    to: review,
                    kind: crate::dag::EdgeKind::Data,
                },
                crate::dag::DependencyEdge {
                    from: review,
                    to: verify,
                    kind: crate::dag::EdgeKind::Data,
                },
            ],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched
            .run(dag_id)
            .await
            .expect("ER4 must durable-fail, not return Err");
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(review));
        assert_eq!(
            outcome.failure.expect("FO6").notes,
            "blocked by pending re-verify after edit (ER4)"
        );

        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(
            after.nodes[&review].state,
            NodeState::Failed,
            "the block must be durable, not just an in-memory outcome"
        );
        fx.close().await;
    }

    /// AC 84/90: reachable verify terminal-without-success and a `Pending`
    /// target remaining ⇒ ER5 fires.
    #[tokio::test]
    async fn er5_fires_when_a_succeeded_edit_has_only_failed_verifies() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_edit, _verify, target) = seed_edit_verify_dag(
            &fx,
            session,
            dag_id,
            NodeState::Failed,
            Some((NodeKind::Review, NodeState::Pending)),
        )
        .await;
        let target = target.unwrap();

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(target));
        let failure = outcome.failure.expect("FO6");
        assert_eq!(failure.error_class, ErrorClass::Internal);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        assert_eq!(
            failure.notes,
            "edit succeeded without successful verify after restart"
        );
        fx.close().await;
    }

    /// AC 90: a verify that already succeeded clears `needs_reverify`, so
    /// ER5 must not fire even though an `Edit` succeeded.
    #[tokio::test]
    async fn er5_does_not_fire_when_a_reachable_verify_succeeded() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_edit, _verify, review) = seed_edit_verify_dag(
            &fx,
            session,
            dag_id,
            NodeState::Succeeded,
            Some((NodeKind::Review, NodeState::Pending)),
        )
        .await;
        let review = review.unwrap();

        let caps = StaticCapability::new(vec![Ok(CapabilityOutcome::Succeeded {
            payload: serde_json::json!({"ok": true}),
        })]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-er5-ok"),
            caps as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );

        // The Review runs normally (not blocked, not ER5-failed).
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded, "{outcome:?}");
        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(after.nodes[&review].state, NodeState::Succeeded);
        fx.close().await;
    }

    /// AC 84: `Edit → GateHuman` with no reachable verify is a verify-less
    /// DAG — the human is the check, so ER5 must stay out of it.
    #[tokio::test]
    async fn er5_does_not_fire_for_a_verify_less_dag() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();

        let edit = NodeId::new();
        let gate = NodeId::new();
        let e_input = fx.put_goal_envelope(dag_id, edit, NodeKind::Edit).await;
        let g_input = fx
            .put_goal_envelope(dag_id, gate, NodeKind::GateHuman)
            .await;
        let mut e_node = llm_node(edit, NodeKind::Edit, e_input, adapter_retry());
        e_node.state = NodeState::Succeeded;
        e_node.output_ref = Some(fx.put_node_output(dag_id, edit).await);
        let mut g_node = adapter_node(gate, NodeKind::GateHuman, g_input);
        g_node.state = NodeState::Pending;
        let gate_id = GateId::new();
        g_node.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "review".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(edit, e_node), (gate, g_node)]),
            edges: vec![crate::dag::DependencyEdge {
                from: edit,
                to: gate,
                kind: crate::dag::EdgeKind::Data,
            }],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        let sched = fx.build_scheduler(
            fx._dir.path().join("s-er5-gate"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            StaticGate::new(fx.plane.clone(), vec![Ok(Approval::Allow)]),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let _ = (run_id, gate_id);
        let outcome = sched.run(dag_id).await.unwrap();

        assert_ne!(
            outcome.state,
            DagState::Failed,
            "verify-less DAG must not ER5-fail: {outcome:?}"
        );
        fx.close().await;
    }

    /// AC 90: ER5 is resume-only. A DAG still `Pending` at entry gets C1 in
    /// this same call (`performed_c1`), so ER5 must be skipped entirely.
    #[tokio::test]
    async fn er5_is_skipped_when_this_call_performed_c1() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let (_edit, _verify, review) = seed_edit_verify_dag(
            &fx,
            session,
            dag_id,
            NodeState::Failed,
            Some((NodeKind::Review, NodeState::Pending)),
        )
        .await;
        let review = review.unwrap();
        // Rewind the DAG to Pending so this run performs C1.
        let mut dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        dag.state = DagState::Pending;
        fx.storage.dags().put(&dag).await.unwrap();

        let sched = reconcile_scheduler(&fx);
        let outcome = sched.run(dag_id).await.unwrap();
        // Still Failed, but attributed by the ER4 L14 block (the Review is
        // promotable and blocked), not by ER5's restart note.
        assert_eq!(outcome.failed_node, Some(review));
        assert_ne!(
            outcome.failure.expect("FO6").notes,
            "edit succeeded without successful verify after restart",
            "ER5 must not fire on a run that performed C1"
        );
        fx.close().await;
    }

    // ---- R16b: gate left Ready by a C9b→C3 crash ----

    #[tokio::test]
    async fn resume_with_a_ready_gate_rescans_the_durable_approval() {
        // C9b committed `WaitingApproval → Ready` + `DagState::Running`, then
        // the process died before GA4's C3. R16 cannot see it (DAG is no
        // longer WaitingApproval) and `adopt_running` cannot either (nothing
        // is Running). Without R16b the loop takes the *first-schedule* gate
        // path, re-requests approval, and lets the granted one expire.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();

        let node_id = NodeId::new();
        let gate_id = GateId::new();
        let input = fx
            .put_goal_envelope(dag_id, node_id, NodeKind::GateHuman)
            .await;
        let mut node = adapter_node(node_id, NodeKind::GateHuman, input);
        node.state = NodeState::Ready;
        node.approval = Some(crate::dag::ApprovalSpec {
            gate: gate_id,
            reason: "review".into(),
        });
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(node_id, node)]),
            edges: vec![],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        // The human already approved before the crash.
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::ApprovalResolved,
                payload: serde_json::json!({
                    "gate_id": gate_id.to_string(),
                    "decision": "allow",
                    "generation": 1u64,
                }),
            })
            .await
            .unwrap();

        // `NeverGate` guarantees the durable scan is what resolves this: if
        // the code re-registered a waiter instead, the run would hang.
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-r16b"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(NeverGate {
                plane: fx.plane.clone(),
            }),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );

        let outcome = tokio::time::timeout(Duration::from_secs(5), sched.run(dag_id))
            .await
            .expect("must resolve from the durable approval, not wait on a new waiter")
            .unwrap();
        assert_eq!(outcome.state, DagState::Succeeded, "{outcome:?}");

        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(after.nodes[&node_id].state, NodeState::Succeeded);
        assert!(after.nodes[&node_id].output_ref.is_some(), "GA1");

        // And it must not have re-requested approval.
        let requested = fx
            .storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.type_ == crate::events::SessionEventType::ApprovalRequested)
            .count();
        assert_eq!(
            requested, 0,
            "a resumed, already-approved gate must not re-request approval"
        );
        fx.close().await;
    }

    // ---- DS4 stall recovery (§5.17) ----

    #[tokio::test]
    async fn ds4_stalled_dag_is_terminalized_instead_of_wedged() {
        // A `Pending` node whose only Data predecessor is `Skipped` can never
        // be promoted. The loop exits naturally with nothing Ready, and the
        // derive says `Running` — previously an `Err(Invariant)` that left the
        // blob durably `Running` forever.
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();

        let a = NodeId::new();
        let b = NodeId::new();
        let a_input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let b_input = fx.put_goal_envelope(dag_id, b, NodeKind::Review).await;
        let mut a_node = llm_node(a, NodeKind::Analyze, a_input, adapter_retry());
        a_node.state = NodeState::Skipped;
        let mut b_node = llm_node(b, NodeKind::Review, b_input, adapter_retry());
        b_node.state = NodeState::Pending;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, a_node), (b, b_node)]),
            edges: vec![crate::dag::DependencyEdge {
                from: a,
                to: b,
                kind: crate::dag::EdgeKind::Data,
            }],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let sched = reconcile_scheduler(&fx);
        let outcome = sched
            .run(dag_id)
            .await
            .expect("DS4 must terminalize, not Err");
        assert_eq!(outcome.state, DagState::Failed, "D8: all-skipped ⇒ Failed");
        assert_eq!(outcome.failed_node, None, "FO4(i): a stall attributes none");
        assert!(outcome.failure.is_none());

        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(
            after.state,
            DagState::Failed,
            "the terminal must be durable"
        );
        assert_eq!(after.nodes[&b].state, NodeState::Skipped);
        fx.close().await;
    }

    // ---- BE4 on the budget stop path (AC 82) ----

    #[tokio::test]
    async fn be4_pre_cas_budget_decision_failure_aborts_the_stop_checkpoint() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // BG3: `max_usd_per_run = 0` exhausts the budget before dispatch, so
        // L6 takes the stop path and records a pre-C7 Budget decision.
        let policy = BudgetPolicy {
            max_usd_per_run: 0.0,
            ..BudgetPolicy::default()
        };
        let sched = fx.build_scheduler_with_failing_decisions_and_budget(
            fx._dir.path().join("s-be4b"),
            Arc::new(crate::adapters::UnavailableCapabilityExecutor),
            policy,
        );

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(err, SchedError::Store(ref m) if m.contains("decision record failed")),
            "BE4: a pre-CAS ObsError must surface as Store, got {err:?}"
        );
        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_ne!(
            after.nodes[&a].state,
            NodeState::Failed,
            "C7 must not proceed when its Budget record could not be written"
        );
        fx.close().await;
    }

    // ---- Aggregate fold ----

    #[tokio::test]
    async fn aggregate_fold_produces_ascending_preds_payload() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let r1 = NodeId::new();
        let r2 = NodeId::new();
        let agg = NodeId::new();
        let (lo, hi) = if r1 < r2 { (r1, r2) } else { (r2, r1) };

        let r1_input = fx.put_goal_envelope(dag_id, r1, NodeKind::Analyze).await;
        let r2_input = fx.put_goal_envelope(dag_id, r2, NodeKind::Analyze).await;
        let lo_placeholder = fx.put_pending_placeholder_artifact().await;
        let hi_placeholder = fx.put_pending_placeholder_artifact().await;
        let agg_input = fx
            .put_placeholder_input(
                dag_id,
                agg,
                NodeKind::Aggregate,
                vec![
                    PredecessorOutput {
                        node_id: lo,
                        kind: NodeKind::Analyze,
                        output_ref: lo_placeholder,
                    },
                    PredecessorOutput {
                        node_id: hi,
                        kind: NodeKind::Analyze,
                        output_ref: hi_placeholder,
                    },
                ],
            )
            .await;

        let mut nodes = BTreeMap::new();
        nodes.insert(
            r1,
            llm_node(r1, NodeKind::Analyze, r1_input, adapter_retry()),
        );
        nodes.insert(
            r2,
            llm_node(r2, NodeKind::Analyze, r2_input, adapter_retry()),
        );
        nodes.insert(agg, adapter_node(agg, NodeKind::Aggregate, agg_input));
        let edges = vec![
            // Serial MVP: two simultaneously-Pending roots would both
            // become Ready in the same C2 CAS and trip L8's "multiple
            // ready nodes" invariant (correctly — RS5). A real planner
            // always chains same-generation roots via Sequence for
            // exactly this reason.
            DependencyEdge {
                from: lo,
                to: hi,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: r1,
                to: agg,
                kind: EdgeKind::Data,
            },
            DependencyEdge {
                from: r2,
                to: agg,
                kind: EdgeKind::Data,
            },
        ];
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes,
            edges,
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"a": 1}),
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"a": 2}),
            }),
        ]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let final_dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        let agg_output_ref = final_dag.nodes[&agg].output_ref.unwrap();
        let blob = fx.storage.artifacts().get(agg_output_ref).await.unwrap();
        let envelope: crate::dag::NodeOutputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
        assert_eq!(envelope.payload["aggregate"], true);
        let preds = envelope.payload["preds"].as_array().unwrap();
        assert_eq!(preds.len(), 2);
        assert_eq!(preds[0]["node_id"], lo.to_string());
        assert_eq!(preds[1]["node_id"], hi.to_string());
        fx.close().await;
    }

    // ---- run timeout (§5.19, TD1: tokio::time::pause, no real sleep) ----

    #[tokio::test(start_paused = true)]
    async fn run_timeout_terminalizes_failed_with_timeout_class() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, llm_node(a, NodeKind::Analyze, input, adapter_retry()))]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // A capability that never resolves on its own; the run timeout must
        // fire the tokio::select! branch inside dispatch_node instead.
        struct Never;
        #[async_trait]
        impl CapabilityExecutor for Never {
            async fn execute(
                &self,
                _ctx: &CapabilityExecContext,
            ) -> Result<CapabilityOutcome, CapabilityExecError> {
                std::future::pending().await
            }
        }

        let sched = Arc::new(fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::new(Never),
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_millis(100),
        ));
        let sched2 = Arc::clone(&sched);
        let handle = tokio::spawn(async move { sched2.run(dag_id).await });
        tokio::time::advance(Duration::from_millis(200)).await;
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(a));
        let failure = outcome.failure.unwrap();
        assert_eq!(failure.error_class, ErrorClass::Timeout);
        assert_eq!(failure.retry, RetryDisposition::NonRetryable);
        fx.close().await;
    }

    // ---- P5: retry admission, escalation, interruptible backoff ----

    /// Records the `effective_tier` seen on every call, then returns the
    /// queued outcomes in order — lets escalation tests assert what tier
    /// each attempt actually dispatched under.
    struct RecordingCapability {
        seen_tiers: StdMutex<Vec<ModelTier>>,
        outcomes: StdMutex<VecDeque<Result<CapabilityOutcome, CapabilityExecError>>>,
    }
    impl RecordingCapability {
        fn new(outcomes: Vec<Result<CapabilityOutcome, CapabilityExecError>>) -> Arc<Self> {
            Arc::new(Self {
                seen_tiers: StdMutex::new(Vec::new()),
                outcomes: StdMutex::new(VecDeque::from(outcomes)),
            })
        }
        fn tiers(&self) -> Vec<ModelTier> {
            self.seen_tiers.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl CapabilityExecutor for RecordingCapability {
        async fn execute(
            &self,
            ctx: &CapabilityExecContext,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            self.seen_tiers.lock().unwrap().push(ctx.effective_tier);
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(CapabilityExecError::Internal("exhausted".into())))
        }
    }

    fn retryable_model_failure(node: NodeId, notes: &str) -> FailureIr {
        FailureIr {
            node,
            error_class: ErrorClass::Model,
            retry: RetryDisposition::Retryable,
            diagnostics: vec![],
            notes: notes.into(),
        }
    }

    // ---- BE4: pre-CAS decision-record failure aborts the CAS ----

    #[tokio::test]
    async fn be4_pre_cas_decision_failure_aborts_the_retry_checkpoint() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // Attempt 1 soft-fails ⇒ an admitted retry ⇒ a pre-C8 Retry record.
        let caps = StaticCapability::new(vec![Ok(CapabilityOutcome::Failed {
            failure: retryable_model_failure(a, "flaky"),
        })]);
        let sched = fx.build_scheduler_with_failing_decisions(
            fx._dir.path().join("s-be4"),
            caps as Arc<dyn CapabilityExecutor>,
        );

        let err = sched.run(dag_id).await.unwrap_err();
        assert!(
            matches!(err, SchedError::Store(ref m) if m.contains("decision record failed")),
            "BE4: a pre-CAS ObsError must surface as Store, got {err:?}"
        );

        // And the C8 it described must NOT have committed.
        let after = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(
            after.nodes[&a].state,
            NodeState::Running,
            "C8 must not proceed when its decision record could not be written"
        );
        fx.close().await;
    }

    // ---- §5.6 / CE: the capability context carries the real contract ----

    /// Captures the whole `CapabilityExecContext` so the test can assert on
    /// fields `UnavailableCapabilityExecutor` would silently ignore.
    struct ContextCapturingCapability {
        seen: StdMutex<Vec<(TokenBudget, Duration)>>,
    }
    impl ContextCapturingCapability {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: StdMutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl CapabilityExecutor for ContextCapturingCapability {
        async fn execute(
            &self,
            ctx: &CapabilityExecContext,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            self.seen
                .lock()
                .unwrap()
                .push((ctx.budget.clone(), ctx.timeout));
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"ok": true}),
            })
        }
    }

    #[tokio::test]
    async fn dispatch_passes_the_nodes_real_budget_and_clamped_deadline() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.budget = TokenBudget {
            max_input: 4242,
            max_output: 777,
        };
        node.timeout_ms = 5_000;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let caps = ContextCapturingCapability::new();
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-ctx"),
            Arc::clone(&caps) as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            // Run budget far exceeds the node timeout, so the §5.19 clamp
            // resolves to the node's own deadline.
            Duration::from_secs(600),
        );
        sched.run(dag_id).await.unwrap();

        let seen = caps.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        let (budget, timeout) = &seen[0];
        assert_eq!(
            budget.max_input, 4242,
            "worker MUST see the node's own token budget, not a zeroed one"
        );
        assert_eq!(budget.max_output, 777);
        assert_eq!(
            *timeout,
            Duration::from_millis(5_000),
            "worker MUST see the §5.19-clamped node deadline, not zero"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn dispatch_deadline_is_clamped_by_the_remaining_run_budget() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.timeout_ms = 600_000; // far longer than the run budget below
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let caps = ContextCapturingCapability::new();
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-clamp"),
            Arc::clone(&caps) as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        sched.run(dag_id).await.unwrap();

        let (_, timeout) = caps.seen.lock().unwrap()[0].clone();
        assert!(
            timeout <= Duration::from_secs(30) && timeout > Duration::ZERO,
            "deadline must be min(node.timeout_ms, remaining_run), got {timeout:?}"
        );
        fx.close().await;
    }

    // ---- B4: resume re-waits the full backoff before C3 (§5.11.3) ----

    #[tokio::test(start_paused = true)]
    async fn b4_resumed_ready_node_with_prior_attempts_rewaits_full_backoff() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 3,
            backoff: Backoff::Fixed { delay_ms: 10_000 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        // Durably `Ready` — the crash landed after C8 committed.
        node.state = NodeState::Ready;
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let run_id = fx.seed_run(session, dag_id, "running").await;

        // C8 recorded next_attempt = 2 before the crash, so `attempts_started`
        // rebuilds to 1 and attempt 2 owes a fresh 10s wait.
        fx.storage
            .events()
            .append_session(crate::events::NewSessionEvent {
                session_id: session,
                run_id: Some(run_id),
                type_: crate::events::SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": a.to_string(),
                    "from": "failed",
                    "to": "ready",
                    "generation": 1u64,
                    "attempt": 1u64,
                    "next_attempt": 2u64,
                }),
            })
            .await
            .unwrap();

        let caps = ContextCapturingCapability::new();
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-b4"),
            Arc::clone(&caps) as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(600),
        );

        let started = tokio::time::Instant::now();
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        // TD1/TD2: paused clock, so this is the virtual time the scheduler
        // actually slept, not a wall-clock measurement.
        assert!(
            started.elapsed() >= Duration::from_millis(10_000),
            "B4 requires the full backoff re-wait before C3, slept {:?}",
            started.elapsed()
        );
        fx.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn b4_in_loop_retry_does_not_double_wait_its_backoff() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 10_000 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        // Attempt 1 soft-fails (one C8 backoff), attempt 2 succeeds.
        let caps = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Failed {
                failure: retryable_model_failure(a, "flaky"),
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"ok": true}),
            }),
        ]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s-b4b"),
            caps as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(600),
        );

        let started = tokio::time::Instant::now();
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
        // Exactly one 10s backoff, not two: `apply_soft_failure` already
        // served it, so the re-dispatch must not serve it again.
        assert!(
            started.elapsed() < Duration::from_millis(20_000),
            "in-loop retry double-waited its backoff: {:?}",
            started.elapsed()
        );
        fx.close().await;
    }

    fn escalating_retry(max_attempts: u32, escalate_after: u32, tier: ModelTier) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: Some(escalate_after),
            escalate_to_tier: Some(tier),
        }
    }

    #[tokio::test]
    async fn es1_escalation_applies_to_capability_context_after_threshold() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.model_tier = ModelTier::Economy;
        node.retry = escalating_retry(3, 1, ModelTier::Premium); // escalate once attempt > 1
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = RecordingCapability::new(vec![
            Ok(CapabilityOutcome::Failed {
                failure: retryable_model_failure(a, "attempt 1 fails"),
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"ok": true}),
            }),
        ]);
        let sched = fx.build_scheduler(
            fx._dir.path().join("s1"),
            Arc::clone(&capabilities) as Arc<dyn CapabilityExecutor>,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        // Attempt 1 (k=1, not > escalate_after=1) uses the base tier;
        // attempt 2 (k=2 > 1) escalates to Premium (ES1/ES3/ES4).
        assert_eq!(
            capabilities.tiers(),
            vec![ModelTier::Economy, ModelTier::Premium]
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn es2_escalation_skipped_no_target_tier_records_decision() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        // escalate_after already satisfied on the very first dispatch
        // (k=1 > 0), but no target tier is configured.
        node.retry = RetryPolicy {
            max_attempts: 1,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![],
            escalate_after: Some(0),
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Succeeded {
            payload: serde_json::json!({}),
        })]);
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let recorded = decisions.recorded_decisions();
        let skipped = recorded
            .iter()
            .find(|d| d.metadata.get("escalation_skipped").is_some())
            .expect("ES2 decision must be recorded");
        assert_eq!(skipped.metadata["escalation_skipped"], "no target tier");
        fx.close().await;
    }

    #[tokio::test]
    async fn retry_admitted_decision_has_full_511_2_shape() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![
            Ok(CapabilityOutcome::Failed {
                failure: retryable_model_failure(a, "transient"),
            }),
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({}),
            }),
        ]);
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);

        let recorded = decisions.recorded_decisions();
        let admitted = recorded
            .iter()
            .find(|d| d.metadata.get("retry_admitted") == Some(&serde_json::Value::Bool(true)))
            .expect("admitted retry decision must be recorded");
        assert_eq!(admitted.kind, DecisionKind::Retry);
        assert_eq!(admitted.metadata["node_id"], a.to_string());
        assert_eq!(admitted.metadata["attempt"], 1);
        assert_eq!(admitted.metadata["next_attempt"], 2);
        assert_eq!(admitted.metadata["error_class"], "model");
        assert_eq!(admitted.metadata["backoff_ms"], 0);
        assert!(admitted.metadata["escalated_to"].is_null());
        fx.close().await;
    }

    #[tokio::test]
    async fn retry_rejected_decision_has_reason() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let node = llm_node(a, NodeKind::Analyze, input, adapter_retry()); // retry_on empty -> A2 rejects
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Failed {
            failure: retryable_model_failure(a, "will be rejected"),
        })]);
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);

        let recorded = decisions.recorded_decisions();
        let rejected = recorded
            .iter()
            .find(|d| d.metadata.get("retry_admitted") == Some(&serde_json::Value::Bool(false)))
            .expect("rejected retry decision must be recorded");
        assert_eq!(rejected.metadata["reason"], "error_class_not_retryable");
        fx.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a4_cancel_during_backoff_interrupts_immediately() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 5,
            backoff: Backoff::Fixed { delay_ms: 10_000 }, // long enough that only cancel ends it
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Failed {
            failure: retryable_model_failure(a, "will retry then get cancelled mid-backoff"),
        })]);
        let runtime_cancel = CancellationToken::new();
        let (sched, _decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_secs(3600),
            runtime_cancel.clone(),
        );
        let sched = Arc::new(sched);
        let sched2 = Arc::clone(&sched);
        let handle = tokio::spawn(async move { sched2.run(dag_id).await });

        // Give the run task a chance to reach the backoff sleep, then cancel
        // the process-wide token without ever advancing the (paused) clock —
        // if the sleep were not interruptible this would hang forever.
        tokio::task::yield_now().await;
        runtime_cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run must return promptly on cancel, not wait out the 10s backoff")
            .unwrap()
            .unwrap();
        assert_eq!(outcome.state, DagState::Cancelled);
        fx.close().await;
    }

    #[tokio::test]
    async fn a6_insufficient_remaining_budget_rejects_retry() {
        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 5,
            // 1000ms backoff + 250ms RETRY_BUDGET_SLICE > the ~500ms run
            // budget remaining right after the first (near-instant) attempt.
            backoff: Backoff::Fixed { delay_ms: 1000 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = StaticCapability::new(vec![Ok(CapabilityOutcome::Failed {
            failure: retryable_model_failure(a, "would retry but budget is too tight"),
        })]);
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            BudgetPolicy::default(),
            Duration::from_millis(500),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);
        assert_eq!(outcome.failed_node, Some(a));

        let recorded = decisions.recorded_decisions();
        let rejected = recorded
            .iter()
            .find(|d| d.metadata.get("retry_admitted") == Some(&serde_json::Value::Bool(false)))
            .expect("rejected retry decision must be recorded");
        assert_eq!(rejected.metadata["reason"], "insufficient_remaining_budget");
        fx.close().await;
    }

    #[tokio::test]
    async fn a5_budget_exhausted_by_the_attempt_itself_rejects_retry() {
        struct BudgetBustingCapability {
            outcomes: StdMutex<VecDeque<Result<CapabilityOutcome, CapabilityExecError>>>,
        }
        #[async_trait]
        impl CapabilityExecutor for BudgetBustingCapability {
            async fn execute(
                &self,
                ctx: &CapabilityExecContext,
            ) -> Result<CapabilityOutcome, CapabilityExecError> {
                // Blow through the token ceiling as a side effect of this
                // (failed) attempt, so A5 sees an already-exhausted budget
                // when the retry it would otherwise admit is considered.
                ctx.cost_meter
                    .add_model_usage(ModelTier::Economy, Some(1000), Some(1000), None);
                self.outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Err(CapabilityExecError::Internal("exhausted".into())))
            }
        }

        let fx = Fixture::new().await;
        let session = fx.seed_session().await;
        let dag_id = DagId::new();
        let a = NodeId::new();
        let input = fx.put_goal_envelope(dag_id, a, NodeKind::Analyze).await;
        let mut node = llm_node(a, NodeKind::Analyze, input, adapter_retry());
        node.retry = RetryPolicy {
            max_attempts: 5,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        };
        let dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(a, node)]),
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        fx.seed_run(session, dag_id, "running").await;

        let capabilities = Arc::new(BudgetBustingCapability {
            outcomes: StdMutex::new(VecDeque::from(vec![Ok(CapabilityOutcome::Failed {
                failure: retryable_model_failure(a, "used up the budget on the way out"),
            })])),
        });
        let policy = BudgetPolicy {
            max_tokens_per_run: 1500, // survives L6's pre-dispatch check, not the attempt's own usage
            ..BudgetPolicy::default()
        };
        let (sched, decisions) = fx.build_scheduler_full(
            fx._dir.path().join("s1"),
            capabilities,
            Arc::new(crate::adapters::UnavailableVerifyCompile),
            Arc::new(crate::adapters::UnavailableVerifyTest),
            Arc::new(crate::adapters::UnavailableGateHuman),
            policy,
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        let outcome = sched.run(dag_id).await.unwrap();
        assert_eq!(outcome.state, DagState::Failed);

        let recorded = decisions.recorded_decisions();
        let rejected = recorded
            .iter()
            .find(|d| d.metadata.get("retry_admitted") == Some(&serde_json::Value::Bool(false)))
            .expect("rejected retry decision must be recorded");
        assert_eq!(rejected.metadata["reason"], "budget_exhausted");
        fx.close().await;
    }
}
