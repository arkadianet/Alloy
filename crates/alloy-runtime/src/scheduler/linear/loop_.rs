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

use super::checkpoint::{
    map_store_error, map_store_error_on_load, Checkpoint, CheckpointCtx, GateDecision,
};
use super::envelopes::{self, InputShape};
use super::ready::{derive_dag_state, promotable_nodes, ready_nodes, DeriveFlags};
use super::retry::{self, Admission, Escalation};
use super::LinearScheduler;
use crate::adapters::{
    Approval, CapabilityExecContext, CapabilityExecError, CapabilityOutcome, NodeExecContext,
    NodeExecRef, VerifyOutcome,
};
use crate::dag::{DagValidator, NodeKind, NodeOutputEnvelope, NodeState, TaskDag};
use crate::error::{AdapterError, SchedError};
use crate::obs::{reaccumulate_cost_from_events, DecisionKind, DecisionRecord};
use crate::scheduler::{DagOutcome, DagState, Scheduler};
use crate::session::{RunControlState, RunGoalRecord, Session};
use crate::storage::RunRow;
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{DagId, NodeId, RunId};

/// What a single loop iteration produced.
enum StepOutcome {
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
struct RunCtx<'a> {
    checkpoint: &'a Checkpoint,
    ctx: CheckpointCtx,
    session: &'a Session,
    run_cancel: &'a CancellationToken,
    run_id: RunId,
    run_started: Instant,
    run_timeout: Duration,
    /// §5.19 T1: wall time spent inside gate waits, excluded from the
    /// charged run elapsed. `std::sync::Mutex` (not `Cell`) because `&RunCtx`
    /// crosses `.await` points and the held future must stay `Send`, which
    /// requires `RunCtx: Sync`; only the gate route mutates it.
    gate_wait_total: std::sync::Mutex<Duration>,
}

impl RunCtx<'_> {
    fn gate_wait_total(&self) -> Duration {
        *self
            .gate_wait_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn add_gate_wait(&self, delta: Duration) {
        let mut g = self
            .gate_wait_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g += delta;
    }

    /// §5.19: `remaining_run = run_timeout - (elapsed - gate_wait_total)`.
    fn remaining_run(&self) -> Duration {
        let elapsed_charged = self
            .run_started
            .elapsed()
            .saturating_sub(self.gate_wait_total());
        self.run_timeout.saturating_sub(elapsed_charged)
    }
}

/// RAII membership guard for [`LinearScheduler::owned`] (minimal P4
/// ownership; see module docs for what P8 adds).
struct OwnedMembership<'a> {
    sched: &'a LinearScheduler,
    dag_id: DagId,
}

impl Drop for OwnedMembership<'_> {
    fn drop(&mut self) {
        if let Ok(mut owned) = self.sched.owned.lock() {
            owned.remove(&self.dag_id);
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
}

impl LinearScheduler {
    fn checkpoint(&self) -> Checkpoint {
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
        let mut dag = self
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

        // R4: minimal insert-if-absent ownership (see module docs).
        {
            let mut owned = self
                .owned
                .lock()
                .map_err(|_| SchedError::Ownership("ownership map poisoned".into()))?;
            if !owned.insert(dag_id) {
                return Err(SchedError::AlreadyOwned(dag_id));
            }
        }
        let _guard = OwnedMembership {
            sched: self,
            dag_id,
        };

        // R4b: re-load under ownership.
        dag = self
            .deps
            .dags
            .get(dag_id)
            .await
            .map_err(|e| map_store_error_on_load(e, dag_id))?
            .ok_or(SchedError::DagNotFound(dag_id))?;
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

        // R5
        {
            let mut pending = self
                .pending_cancels
                .lock()
                .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
            pending.remove(&dag_id);
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

        // R8 (rebuild before any budget check, B7/B8).
        let rebuilt = reaccumulate_cost_from_events(&*self.deps.events, session.id, Some(run_id))
            .await
            .map_err(|e| SchedError::Store(e.to_string()))?; // B10
        let meter = self.deps.cost_meters.meter_for(run_id);
        meter.with_mut(|m| *m = rebuilt);

        let run_cancel = self.deps.runtime_cancel.child_token();
        let ctx = CheckpointCtx {
            session_id: session.id,
            run_id: Some(run_id),
        };
        let rc = RunCtx {
            checkpoint: &checkpoint,
            ctx,
            session: &session,
            run_cancel: &run_cancel,
            run_id,
            run_started: Instant::now(), // R12
            run_timeout: self.deps.run_timeout,
            gate_wait_total: std::sync::Mutex::new(Duration::ZERO),
        };

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

        // R16: minimal WaitingApproval resume (P7 owns the real durable
        // resolution scan / re-register-only semantics, §5.7.2/§5.7.3).
        if dag.state == DagState::WaitingApproval {
            if let Some(gate_node) = dag
                .nodes
                .values()
                .find(|n| n.state == NodeState::WaitingApproval)
                .map(|n| n.id)
            {
                match self.dispatch_gate(&mut dag, &rc, gate_node, true).await? {
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

        // L6 (P9 seam: effective policy is deps.budget_policy verbatim).
        let meter = self.deps.cost_meters.meter_for(rc.run_id);
        if meter.check_budget(&self.deps.budget_policy).is_exhausted() {
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
            return self.dispatch_gate(dag, rc, selected, false).await;
        }

        self.dispatch_node(dag, rc, selected).await
    }

    /// L11-L15 for a non-gate node.
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
                Escalation::To(tier) => tier,
                Escalation::SkippedNoTarget => {
                    self.record_escalation_skipped(rc, node_id, attempt).await;
                    dag.nodes[&node_id].model_tier
                }
                Escalation::None => dag.nodes[&node_id].model_tier,
            }
        } else {
            dag.nodes[&node_id].model_tier // ES5: adapter kinds never escalate.
        };

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
                            self.apply_soft_failure(dag, rc, node_id, attempt, failure).await
                        };
                    }
                }
            }
            () = rc.run_cancel.cancelled() => {
                return self.cancel_path(dag, rc).await;
            }
        };

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
            NodeKind::VerifyCompile => {
                let ctx = NodeExecContext {
                    meta: meta.clone(),
                    cancellation: rc.run_cancel.clone(),
                };
                match self.deps.verify_compile.check(&ctx).await {
                    Ok(outcome) => Ok(verify_outcome_to_result(
                        outcome,
                        ErrorClass::Compile,
                        "cargo check failed",
                    )),
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
                match self.deps.verify_test.test(&ctx).await {
                    Ok(outcome) => Ok(verify_outcome_to_result(
                        outcome,
                        ErrorClass::Test,
                        "cargo test failed",
                    )),
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

    async fn apply_success(
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
        // A5 (§5.16.3): the same check_budget mechanism L6 uses. Full
        // effective-ceiling computation (BG1-BG6) is P9's charter; here we
        // only need "is the run budget already exhausted", which
        // deps.budget_policy already answers.
        let budget_exhausted = meter.check_budget(&self.deps.budget_policy).is_exhausted();
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

    // -----------------------------------------------------------------
    // §5.7 gate route (minimal placeholder — see module docs)
    // -----------------------------------------------------------------

    /// **P7 seam.** Mechanical `C9a -> wait_approval -> C9b/C9c` happy
    /// path: no deadline wrapping (§5.7.8), no durable-resolution resume
    /// scan (§5.7.2/§5.7.3), no closed-receiver reclassification (§5.7.9).
    /// `resuming = true` skips C9a (the node is already `WaitingApproval`).
    async fn dispatch_gate(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        resuming: bool,
    ) -> Result<StepOutcome, SchedError> {
        let approval = dag.nodes[&node_id]
            .approval
            .clone()
            .ok_or_else(|| SchedError::Invariant(format!("gate node {node_id} has no approval")))?;

        if !resuming {
            rc.checkpoint
                .c9a_gate_schedule(
                    dag,
                    rc.ctx,
                    node_id,
                    approval.gate,
                    &approval.reason,
                    dag.nodes[&node_id].timeout_ms,
                )
                .await?;
        }

        let meta = NodeExecRef {
            session_id: rc.session.id,
            run_id: rc.run_id,
            dag_id: dag.id,
            node_id,
            workspace_root: rc.session.workspace_root.clone(),
            attempt: 0, // NX1: gate wait contexts use 0 (no C3 yet).
        };
        let wait_started = Instant::now();
        let result = self
            .deps
            .gate_human
            .wait_approval(
                &NodeExecContext {
                    meta,
                    cancellation: rc.run_cancel.clone(),
                },
                approval.gate,
            )
            .await;
        rc.add_gate_wait(wait_started.elapsed()); // T1

        match result {
            Ok(Approval::Allow) => self.gate_allow(dag, rc, node_id, GateDecision::Allow).await,
            Ok(Approval::AllowOnce) => {
                self.gate_allow(dag, rc, node_id, GateDecision::AllowOnce)
                    .await
            }
            Ok(Approval::Deny) => self.gate_deny(dag, rc, node_id, GateDecision::Deny).await,
            Err(e) => {
                let failure = failure_from_adapter_error(node_id, e);
                self.gate_deny_with_failure(dag, rc, node_id, GateDecision::Deny, failure)
                    .await
            }
        }
    }

    /// GA1-GA4: `WaitingApproval -> Ready -> Running -> Succeeded`.
    async fn gate_allow(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
    ) -> Result<StepOutcome, SchedError> {
        rc.checkpoint
            .c9b_gate_allow(dag, rc.ctx, node_id, decision)
            .await?; // GA3

        // GA4: the post-allow Ready -> Running step is C3.
        let attempts_started = rc
            .checkpoint
            .rebuild_attempts_started(dag.id, rc.session.id, node_id, dag.generation, false)
            .await?;
        let attempt = (attempts_started + 1).max(1);
        rc.checkpoint
            .c3_dispatch(dag, rc.ctx, node_id, attempt)
            .await?;

        // GA1: deterministic fold, not a worker/adapter call.
        let payload = serde_json::json!({
            "approved": true,
            "decision": gate_decision_str(decision),
            "gate_id": dag.nodes[&node_id]
                .approval
                .as_ref()
                .map(|a| a.gate.to_string()),
        });
        self.apply_success(dag, rc, node_id, attempt, payload).await
    }

    async fn gate_deny(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
    ) -> Result<StepOutcome, SchedError> {
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Approval,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "approval denied".into(),
        };
        self.gate_deny_with_failure(dag, rc, node_id, decision, failure)
            .await
    }

    async fn gate_deny_with_failure(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
        failure: FailureIr,
    ) -> Result<StepOutcome, SchedError> {
        let skipped = non_terminal_except(dag, node_id);
        let _failure_ref = rc
            .checkpoint
            .c9c_gate_deny(dag, rc.ctx, node_id, decision, &failure, &skipped)
            .await?;
        Ok(StepOutcome::Terminal(DagOutcome {
            dag_id: dag.id,
            generation: dag.generation,
            state: DagState::Failed,
            failed_node: Some(node_id), // FN2
            failure: Some(failure),
        }))
    }

    // -----------------------------------------------------------------
    // Cancel / replan / budget / run-timeout terminal paths
    // -----------------------------------------------------------------

    /// §5.12.2 owned cancel (run-side): in-flight node (`Running`/`Ready`/
    /// `WaitingApproval`) → `Cancelled`; every other non-terminal node →
    /// `Skipped`.
    async fn cancel_path(
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

    async fn replan_path(
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
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Budget,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: format!(
                "budget exhausted: {:?}",
                self.deps
                    .cost_meters
                    .meter_for(rc.run_id)
                    .check_budget(&self.deps.budget_policy)
            ),
        };
        Ok(StepOutcome::Terminal(
            self.terminal_failed(dag, rc, node_id, None, failure)
                .await?,
        ))
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
    // `cancel` (minimal — see module docs)
    // -----------------------------------------------------------------

    async fn cancel_impl(&self, dag_id: DagId) -> Result<(), SchedError> {
        let is_owned = {
            let owned = self
                .owned
                .lock()
                .map_err(|_| SchedError::Ownership("ownership map poisoned".into()))?;
            owned.contains(&dag_id)
        };
        if is_owned {
            // PC1/PC2: mark pending; the live loop's L1/L2 observes it on
            // its next iteration. P4 does not yet block until that run's
            // own C6 commits (§4.3 O4 / §5.12.2 step 7 — P8).
            let mut pending = self
                .pending_cancels
                .lock()
                .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
            pending.insert(dag_id);
            return Ok(());
        }

        // Unowned: PC1 — insert into pending_cancels so a `run` starting
        // moments later cancels immediately at R5, then best-effort C6 now
        // if the DAG is durably non-terminal (§5.12.4, simplified: no
        // transient-ownership re-load race handling — P8).
        {
            let mut pending = self
                .pending_cancels
                .lock()
                .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
            pending.insert(dag_id);
        }
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
            return Ok(());
        }
        let checkpoint = self.checkpoint();
        let ctx = CheckpointCtx {
            session_id: dag.session_id,
            run_id: None,
        };
        let (cancelled, skipped) = cancel_targets(&dag);
        checkpoint
            .c6_cancel(&mut dag, ctx, &cancelled, &skipped)
            .await?;
        let mut pending = self
            .pending_cancels
            .lock()
            .map_err(|_| SchedError::Ownership("pending_cancels poisoned".into()))?;
        pending.remove(&dag_id);
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

/// §5.10 `AdapterError` -> `FailureIr`.
fn failure_from_adapter_error(node: NodeId, e: AdapterError) -> FailureIr {
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

fn gate_decision_str(d: GateDecision) -> &'static str {
    match d {
        GateDecision::Allow => "allow",
        GateDecision::AllowOnce => "allow_once",
        GateDecision::Deny => "deny",
        GateDecision::Expired => "expired",
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
/// not consulted in MVP).
fn non_terminal_except(dag: &TaskDag, except: NodeId) -> Vec<NodeId> {
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
fn cancel_targets(dag: &TaskDag) -> (Vec<NodeId>, Vec<NodeId>) {
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
        CapabilityExecutor, GateHumanAdapter, VerifyCompileAdapter, VerifyTestAdapter,
    };
    use crate::dag::{
        Backoff, DependencyEdge, EdgeKind, NodeInputEnvelope, NodeInputPayload, PredecessorOutput,
        RetryPolicy, TaskNode,
    };
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

    struct StaticGate {
        outcomes: StdMutex<VecDeque<Result<Approval, AdapterError>>>,
    }
    impl StaticGate {
        fn new(outcomes: Vec<Result<Approval, AdapterError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: StdMutex::new(VecDeque::from(outcomes)),
            })
        }
    }
    #[async_trait]
    impl GateHumanAdapter for StaticGate {
        async fn wait_approval(
            &self,
            _ctx: &NodeExecContext,
            _gate: GateId,
        ) -> Result<Approval, AdapterError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(AdapterError::Internal("exhausted".into())))
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
            let run_id = RunId::new();
            let goal = RunGoalRecord {
                goal: Goal {
                    text: "fix it".into(),
                    constraints: vec![],
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
        let gate_human = StaticGate::new(vec![Ok(Approval::Allow)]);

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

        let gate_human = StaticGate::new(vec![Ok(Approval::Deny)]);
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
