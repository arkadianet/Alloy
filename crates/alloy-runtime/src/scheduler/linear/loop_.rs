//! The serial scheduling loop (RFC-0010 §5.1 R1-R18, §5.2 L1-L16, §5.6
//! dispatch table, §5.9 success path, §5.14 `Aggregate` fold).
//!
//! [`LinearScheduler::run`]/[`LinearScheduler::cancel`] land on the
//! [`crate::Scheduler`] trait here for the first time. `admit_retry` (§5.11
//! A1-A6 admission, tier escalation, interruptible backoff) is complete as
//! of P5 — see `retry.rs` for the pure decision logic it calls into. Two
//! seams remain, deferred to later phases; each is a private method with a
//! doc comment pointing at the RFC section that completes it:
//!
//! - [`LinearScheduler::dispatch_gate`] — §5.7's full state machine:
//!   deadline/expiry (§5.7.8), durable-resolution resume scans
//!   (§5.7.2/§5.7.3), and closed-receiver reclassification (§5.7.9)
//!   (**P7**). This phase implements the mechanical
//!   `C9a -> wait_approval -> C9b/C9c` happy path with no deadline wrapping.
//! - DAG ownership release — the race-free `Notify`-based cancel wait and
//!   forced-C6-after-grace machinery (§4.3-4.4, §5.12.3) (**P8**). This
//!   phase uses [`LinearScheduler::owned`], a minimal insert-if-absent set,
//!   so R4/`AlreadyOwned` and the `pending_cancels`-driven L1/L2 cancel path
//!   are real, but `cancel` does not yet block until the run's own C6
//!   commits, and a `cancel(dag_id)` call cannot interrupt an
//!   already-in-flight dispatch (only `deps.runtime_cancel` can, since it is
//!   the only cancellation token reachable from outside `run`).
//!
//! Two further gaps are flagged (not phase-assigned in RFC-0010's own
//! P1-P10 breakdown) rather than silently skipped:
//!
//! - R15 (edit-tx resume, `needs_reverify`/ER4/ER5) is a no-op: `TaskNode`
//!   on `main` has no `needs_reverify` field, so RFC-0008's resume-repair
//!   bit does not exist yet to read.
//! - R7/L6 use `deps.budget_policy` directly as the "effective" ceiling (no
//!   goal-constraint folding, BG1-BG6); full effective-ceiling computation
//!   is P9's charter (§5.16.1).

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::budget;
use super::checkpoint::{map_store_error, map_store_error_on_load, Checkpoint, CheckpointCtx};
use super::envelopes::{self, InputShape};
use super::ready::{derive_dag_state, promotable_nodes, ready_nodes, DeriveFlags};
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
            return self.assemble_already_terminal_outcome(&dag, run_id).await;
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
        };
        // BG1/BG5: record once, now that `rc` exists to record through.
        self.record_budget_ignored(&rc, &effective).await;

        // R13: adopt any node durably Running (crash resume).
        if let Some(outcome) = self.adopt_running(&mut dag, &rc).await? {
            return Ok(outcome);
        }

        // R14
        if dag.state == DagState::Pending {
            checkpoint.c1_start(&mut dag).await?;
        }

        // R15: edit-tx resume is a no-op (see module docs — `needs_reverify`
        // does not exist on `TaskNode` yet).

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
    /// the persisted blob without any CAS (§5.18 FO1-FO6, simplified: P4
    /// handles the `Succeeded`/plain-`Cancelled` cases in full; a durable
    /// `Failed` blob's `failed_node` is resolved via FN1 over the node map
    /// (the durable event/failure_ref reconstruction in FO1/FO2 is P7/P8
    /// territory — gate-origin `Cancelled+Approval` reconciliation, RF7).
    async fn assemble_already_terminal_outcome(
        &self,
        dag: &TaskDag,
        _run_id: RunId,
    ) -> Result<DagOutcome, SchedError> {
        match dag.state {
            DagState::Succeeded => Ok(DagOutcome {
                dag_id: dag.id,
                generation: dag.generation,
                state: DagState::Succeeded,
                failed_node: None,
                failure: None,
            }),
            DagState::Cancelled => Ok(DagOutcome {
                dag_id: dag.id,
                generation: dag.generation,
                state: DagState::Cancelled,
                failed_node: None,
                failure: None,
            }),
            DagState::Failed => {
                // FN1: lowest NodeId in Failed.
                let failed_node = dag
                    .nodes
                    .iter()
                    .find(|(_, n)| n.state == NodeState::Failed)
                    .map(|(id, _)| *id);
                Ok(DagOutcome {
                    dag_id: dag.id,
                    generation: dag.generation,
                    state: DagState::Failed,
                    failed_node,
                    failure: None, // FO1/FO2 durable reconstruction deferred.
                })
            }
            other => Err(SchedError::Invariant(format!(
                "assemble_already_terminal_outcome called for non-terminal state {other:?}"
            ))),
        }
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
            // Table row 3/4: gate resume needs a durable-allow scan (P7).
            return Err(SchedError::Invariant("gate node running".into()));
        }
        let attempts_started = rc
            .checkpoint
            .rebuild_attempts_started(dag.id, rc.session.id, node_id, dag.generation, true)
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
            .rebuild_attempts_started(dag.id, rc.session.id, node_id, dag.generation, false)
            .await?;
        let attempt = attempts_started + 1;

        // L12 (§5.11.4 ES1-ES6): decided before C3 (ES4).
        let kind_for_escalation = dag.nodes[&node_id].kind;
        let effective_tier = if is_capability_kind(kind_for_escalation) {
            match retry::escalation_for_attempt(&dag.nodes[&node_id].retry, attempt) {
                Escalation::To(tier) => {
                    self.metrics.inc_escalations();
                    tier
                }
                Escalation::SkippedNoTarget => {
                    self.record_escalation_skipped(rc, node_id, attempt).await;
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

        let kind = dag.nodes[&node_id].kind;
        let capability = dag.nodes[&node_id].capability.clone();
        let meta = NodeExecRef {
            session_id: rc.session.id,
            run_id: rc.run_id,
            dag_id: dag.id,
            node_id,
            workspace_root: rc.session.workspace_root.clone(),
            attempt,
        };

        let outcome = tokio::select! {
            res = tokio::time::timeout(node_deadline, self.dispatch_kind(kind, capability, &meta, effective_tier, input, rc)) => {
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
        kind: NodeKind,
        capability: Option<crate::types::ids::CapabilityId>,
        meta: &NodeExecRef,
        effective_tier: crate::types::budget::ModelTier,
        input: crate::dag::NodeInputEnvelope,
        rc: &RunCtx<'_>,
    ) -> Result<DispatchResult, SchedError> {
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
                    budget: crate::types::budget::TokenBudget {
                        max_input: 0,
                        max_output: 0,
                    },
                    timeout: Duration::from_millis(0),
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
                self.record_retry_rejected(rc, node_id, attempt, &failure, reason)
                    .await;
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
                self.record_retry_admitted(
                    rc,
                    node_id,
                    attempt,
                    next_attempt,
                    &failure,
                    backoff_ms,
                    escalated_to,
                )
                .await;

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
    /// Best-effort (BE4-style posture): a logging failure here MUST NOT
    /// undo the C7/C8 checkpoint it describes.
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
    ) {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "next_attempt": next_attempt,
            "error_class": failure.error_class,
            "retry_admitted": true,
            "backoff_ms": backoff_ms,
            "escalated_to": escalated_to,
        });
        self.record_decision(rc, node_id, metadata).await;
    }

    async fn record_retry_rejected(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        failure: &FailureIr,
        reason: retry::RejectReason,
    ) {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "error_class": failure.error_class,
            "retry_admitted": false,
            "reason": reason.as_str(),
        });
        self.record_decision(rc, node_id, metadata).await;
    }

    /// ES2: `escalate_after` is due but no `escalate_to_tier` is configured.
    async fn record_escalation_skipped(&self, rc: &RunCtx<'_>, node_id: NodeId, attempt: u32) {
        let metadata = serde_json::json!({
            "node_id": node_id.to_string(),
            "attempt": attempt,
            "escalation_skipped": "no target tier",
        });
        self.record_decision(rc, node_id, metadata).await;
    }

    async fn record_decision(&self, rc: &RunCtx<'_>, node_id: NodeId, metadata: serde_json::Value) {
        let rec = DecisionRecord {
            session: rc.ctx.session_id,
            run: rc.ctx.run_id,
            node: Some(node_id),
            kind: DecisionKind::Retry,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        if let Err(e) = self.deps.decisions.record(rec).await {
            tracing::warn!(error = %e, %node_id, "retry/escalation decision record failed");
        }
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
        self.signal_budget_exhaustion(rc, Some(node_id), check)
            .await;
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
    async fn signal_budget_exhaustion(
        &self,
        rc: &RunCtx<'_>,
        node_id: Option<NodeId>,
        check: crate::obs::BudgetCheck,
    ) {
        let metadata = serde_json::json!({
            "check": format!("{check:?}"),
            "effective_usd": rc.effective_budget.max_usd_per_run,
            "effective_tokens": rc.effective_budget.max_tokens_per_run,
        });
        self.record_budget_decision(rc, node_id, metadata).await;

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
        self.record_budget_decision(rc, None, metadata).await;
    }

    /// Shared `DecisionKind::Budget` recorder (BE1/BG1/BG5). `node_id` is
    /// `None` for run-level notes (BG1/BG5), `Some` for an exhaustion
    /// attribution (BE1).
    async fn record_budget_decision(
        &self,
        rc: &RunCtx<'_>,
        node_id: Option<NodeId>,
        metadata: serde_json::Value,
    ) {
        let rec = DecisionRecord {
            session: rc.ctx.session_id,
            run: rc.ctx.run_id,
            node: node_id,
            kind: DecisionKind::Budget,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        if let Err(e) = self.deps.decisions.record(rec).await {
            tracing::warn!(error = %e, ?node_id, "budget decision record failed");
        }
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
        match state {
            DagState::Succeeded => {
                rc.checkpoint.c7_terminal_succeeded(dag).await?;
                Ok(DagOutcome {
                    dag_id: dag.id,
                    generation: dag.generation,
                    state: DagState::Succeeded,
                    failed_node: None,
                    failure: None,
                })
            }
            other => Err(SchedError::Invariant(format!(
                "natural loop exit produced unexpected state {other:?} (DS4 stall path not \
                 reachable given §5.15 SK3's bulk-terminalize design; flagged for follow-up \
                 if this ever fires)"
            ))),
        }
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
            {
                let mut pending = self
                    .pending_cancels
                    .lock()
                    .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
                pending.insert(dag_id); // PC1
            }

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
            // Step 8: consumed either way — a later `run` should not inherit
            // a stale cancel intent from this call.
            if let Ok(mut pending) = self.pending_cancels.lock() {
                pending.remove(&dag_id);
            }
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
        DispatchResult::Failed(FailureIr {
            node: NodeId::new(), // overwritten by the caller (DP4)
            error_class: class,
            retry: RetryDisposition::NonRetryable,
            diagnostics: outcome.diagnostics, // F2
            notes: notes.to_string(),
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
