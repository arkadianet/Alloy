//! Gate orchestration for `NodeKind::GateHuman` (RFC-0010 §5.7).
//!
//! The scheduler owns `NodeState::WaitingApproval`, `DagState::WaitingApproval`,
//! `ApprovalRequested`, the `timeout_ms` deadline, and DAG terminalization.
//! `RunController::approve`/`expire_gate` (session/run_controller.rs) own
//! `RunControlState`, `ApprovalResolved`, and waiter delivery.
//! `SessionGateHumanAdapter` (adapters/gate.rs) owns registering the waiter
//! and awaiting it — no timer of its own (§5.7.1's ownership table).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::checkpoint::{map_store_error, GateDecision};
use super::loop_::{non_terminal_except, RunCtx, StepOutcome};
use super::LinearScheduler;
use crate::adapters::{NodeExecContext, NodeExecRef};
use crate::dag::{NodeState, TaskDag};
use crate::error::{AdapterError, RunError, SchedError};
use crate::events::SessionEventType;
use crate::obs::{DecisionKind, DecisionRecord};
use crate::scheduler::DagState;
use crate::session::{RunControlState, MAX_EVENTS_PAGE};
use crate::storage::EventStore;
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{DagId, GateId, NodeId, SessionId};

/// `EXPIRE_RETRY_MAX` (§5.7.8): total `expire_gate` attempts, including the
/// first, when it keeps returning an unrecognized `Err(other)`.
const EXPIRE_RETRY_MAX: u32 = 3;
/// Interruptible backoff between `expire_gate` retries (§5.7.8).
const EXPIRE_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Durable resolution for a gate, per the `ApprovalResolved.decision` wire
/// vocabulary (§5.7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateResolution {
    Allow,
    AllowOnce,
    Deny,
    Expired,
}

fn parse_gate_resolution(s: &str) -> Result<GateResolution, SchedError> {
    match s {
        "allow" => Ok(GateResolution::Allow),
        "allow_once" => Ok(GateResolution::AllowOnce),
        "deny" => Ok(GateResolution::Deny),
        "expired" => Ok(GateResolution::Expired),
        other => Err(SchedError::Invariant(format!(
            "unknown approval decision: {other}"
        ))),
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

fn approval_denied_failure(node_id: NodeId) -> FailureIr {
    FailureIr {
        node: node_id,
        error_class: ErrorClass::Approval,
        retry: RetryDisposition::NonRetryable, // GD3
        diagnostics: vec![],
        notes: "approval denied".into(),
    }
}

/// GT4: expiry is always `ErrorClass::Approval` / `NonRetryable`, never
/// `ErrorClass::Timeout` (that's for execution deadlines).
fn approval_timeout_failure(node_id: NodeId, notes: impl Into<String>) -> FailureIr {
    FailureIr {
        node: node_id,
        error_class: ErrorClass::Approval,
        retry: RetryDisposition::NonRetryable,
        diagnostics: vec![],
        notes: notes.into(),
    }
}

impl LinearScheduler {
    // -----------------------------------------------------------------
    // Entry point (replaces the P4 placeholder)
    // -----------------------------------------------------------------

    /// §5.7 entry point. `resuming = true` when called from R16 (the DAG was
    /// durably `WaitingApproval` on `run` entry); `false` for a fresh L13
    /// dispatch of a `Ready` gate node.
    pub(super) async fn gate_route(
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
        let gate_id = approval.gate;

        if !resuming {
            // §5.7.7 steps 1-3: first schedule.
            rc.checkpoint
                .c9a_gate_schedule(
                    dag,
                    rc.ctx,
                    node_id,
                    gate_id,
                    &approval.reason,
                    dag.nodes[&node_id].timeout_ms,
                )
                .await?;
            let deadline = Duration::from_millis(dag.nodes[&node_id].timeout_ms);
            return self
                .gate_wait_and_dispatch(dag, rc, node_id, gate_id, deadline)
                .await;
        }

        // §5.7.2: resume always scans for a durable resolution first.
        if let Some(resolution) = self
            .scan_gate_resolution(dag.id, rc.ctx.session_id, gate_id, dag.generation)
            .await?
        {
            return self
                .gate_apply_resolution(dag, rc, node_id, resolution)
                .await;
        }

        // §5.7.3: unresolved — recompute the remaining deadline (GR4) and
        // wait again. GR3: RF6-repair a missing ApprovalRequested here, the
        // only place the scheduler may re-emit it.
        let remaining = self
            .gate_remaining_deadline(dag, rc, node_id, gate_id)
            .await?;
        if remaining == Duration::ZERO {
            return self.gate_expire(dag, rc, node_id, gate_id).await;
        }
        self.gate_wait_and_dispatch(dag, rc, node_id, gate_id, remaining)
            .await
    }

    /// §5.7.10 wait-result dispatch, wrapped in the scheduler-owned deadline
    /// (GC1). `deps.gate_human.wait_approval` never sees the timeout.
    ///
    /// Boxed: part of the `gate_wait_and_dispatch <-> gate_closed_receiver
    /// <-> gate_reregister_or_internal` mutual-recursion cycle (see
    /// [`Self::gate_expire`]'s doc comment for why one indirection is
    /// required).
    fn gate_wait_and_dispatch<'a>(
        &'a self,
        dag: &'a mut TaskDag,
        rc: &'a RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
        deadline: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<StepOutcome, SchedError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let meta = NodeExecRef {
                session_id: rc.session.id,
                run_id: rc.run_id,
                dag_id: dag.id,
                node_id,
                workspace_root: rc.session.workspace_root.clone(),
                attempt: 0, // NX1: gate wait contexts (unresolved, no C3 yet) use 0.
            };
            let ctx = NodeExecContext {
                meta,
                cancellation: rc.run_cancel.clone(),
            };
            let wait_started = tokio::time::Instant::now();
            let wait_result =
                tokio::time::timeout(deadline, self.deps.gate_human.wait_approval(&ctx, gate_id))
                    .await;
            rc.add_gate_wait(wait_started.elapsed()); // T1: excluded from the run budget (GT2).

            match wait_result {
                // §5.7.8: the scheduler's own timeout wrapper elapsed — never
                // AdapterError::Timeout, which the adapter must not produce (GC1).
                Err(_elapsed) => self.gate_expire(dag, rc, node_id, gate_id).await,
                // Resolution is durable before the waiter fires on `main`
                // (§5.7.10) — re-scan and trust the durable payload, not the
                // in-memory `Approval` value, regardless of which way it went.
                Ok(Ok(_approval)) => {
                    match self
                        .scan_gate_resolution(dag.id, rc.ctx.session_id, gate_id, dag.generation)
                        .await?
                    {
                        Some(resolution) => self.gate_apply_resolution(dag, rc, node_id, resolution).await,
                        None => Err(SchedError::Invariant(format!(
                            "gate {gate_id} wait returned a decision but no durable ApprovalResolved was found"
                        ))),
                    }
                }
                Ok(Err(AdapterError::Cancelled)) => self.cancel_path(dag, rc).await,
                Ok(Err(e)) => {
                    self.gate_closed_receiver(dag, rc, node_id, gate_id, Some(e))
                        .await
                }
            }
        })
    }

    fn gate_apply_resolution<'a>(
        &'a self,
        dag: &'a mut TaskDag,
        rc: &'a RunCtx<'_>,
        node_id: NodeId,
        resolution: GateResolution,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<StepOutcome, SchedError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match resolution {
                GateResolution::Allow => {
                    self.gate_allow_fold(dag, rc, node_id, GateDecision::Allow)
                        .await
                }
                GateResolution::AllowOnce => {
                    self.gate_allow_fold(dag, rc, node_id, GateDecision::AllowOnce)
                        .await
                }
                GateResolution::Deny => {
                    self.gate_terminal(
                        dag,
                        rc,
                        node_id,
                        GateDecision::Deny,
                        approval_denied_failure(node_id),
                    )
                    .await
                }
                GateResolution::Expired => {
                    self.gate_terminal(
                        dag,
                        rc,
                        node_id,
                        GateDecision::Expired,
                        approval_timeout_failure(node_id, "approval timeout"),
                    )
                    .await
                }
            }
        })
    }

    // -----------------------------------------------------------------
    // §5.7.6 allow path
    // -----------------------------------------------------------------

    /// GA1-GA4: `WaitingApproval -> Ready -> Running -> Succeeded`, skipping
    /// whatever the durable node state already shows (crash-tolerant).
    async fn gate_allow_fold(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
    ) -> Result<StepOutcome, SchedError> {
        match dag.nodes[&node_id].state {
            NodeState::WaitingApproval => {
                rc.checkpoint
                    .c9b_gate_allow(dag, rc.ctx, node_id, decision)
                    .await?; // GA3
                self.gate_allow_dispatch(dag, rc, node_id, decision).await
            }
            NodeState::Ready => self.gate_allow_dispatch(dag, rc, node_id, decision).await,
            NodeState::Running => {
                // GA4 already committed in a prior attempt; only the fold + C4 remain.
                let attempt = rc
                    .checkpoint
                    .rebuild_attempts_started(dag.id, rc.session.id, node_id, dag.generation, true)
                    .await?;
                self.gate_fold_and_succeed(dag, rc, node_id, attempt, decision)
                    .await
            }
            NodeState::Succeeded => Ok(StepOutcome::Continue),
            other => Err(SchedError::Invariant(format!(
                "gate resolved allow but node is {other:?}"
            ))),
        }
    }

    /// GA4: the post-allow `Ready -> Running` step is C3.
    async fn gate_allow_dispatch(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
    ) -> Result<StepOutcome, SchedError> {
        let attempts_started = rc
            .checkpoint
            .rebuild_attempts_started(dag.id, rc.session.id, node_id, dag.generation, false)
            .await?;
        let attempt = (attempts_started + 1).max(1);
        rc.checkpoint
            .c3_dispatch(dag, rc.ctx, node_id, attempt)
            .await?;
        self.gate_fold_and_succeed(dag, rc, node_id, attempt, decision)
            .await
    }

    /// GA1: the gate "execution" is a deterministic fold, never a
    /// worker/adapter call.
    async fn gate_fold_and_succeed(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        attempt: u32,
        decision: GateDecision,
    ) -> Result<StepOutcome, SchedError> {
        let payload = serde_json::json!({
            "approved": true,
            "decision": gate_decision_str(decision),
            "gate_id": dag.nodes[&node_id].approval.as_ref().map(|a| a.gate.to_string()),
        });
        self.apply_success(dag, rc, node_id, attempt, payload).await
    }

    // -----------------------------------------------------------------
    // §5.7.4 / §5.7.5 terminal (deny / expiry share this shape)
    // -----------------------------------------------------------------

    /// Ordered single-CAS terminal write shared by deny (§5.7.4) and expiry
    /// (§5.7.5): gate node `Cancelled`, everyone else non-terminal
    /// `Skipped`, `DagState -> Failed` (GD1). Also records the
    /// `DecisionKind::Gate` record §5.7.4 step 4 requires.
    async fn gate_terminal(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        decision: GateDecision,
        failure: FailureIr,
    ) -> Result<StepOutcome, SchedError> {
        let gate_id = dag.nodes[&node_id].approval.as_ref().map(|a| a.gate);
        let skipped = non_terminal_except(dag, node_id);
        let _failure_ref = rc
            .checkpoint
            .c9c_gate_deny(dag, rc.ctx, node_id, decision, &failure, &skipped)
            .await?;

        if let Some(gate_id) = gate_id {
            self.record_gate_decision(rc, node_id, gate_id, decision)
                .await;
        }

        Ok(StepOutcome::Terminal(crate::scheduler::DagOutcome {
            dag_id: dag.id,
            generation: dag.generation,
            state: DagState::Failed,
            failed_node: Some(node_id), // FN2 / GD2
            failure: Some(failure),
        }))
    }

    async fn record_gate_decision(
        &self,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
        decision: GateDecision,
    ) {
        let metadata = serde_json::json!({
            "gate_id": gate_id.to_string(),
            "decision": gate_decision_str(decision),
        });
        let rec = DecisionRecord {
            session: rc.ctx.session_id,
            run: rc.ctx.run_id,
            node: Some(node_id),
            kind: DecisionKind::Gate,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        if let Err(e) = self.deps.decisions.record(rec).await {
            tracing::warn!(error = %e, %node_id, %gate_id, "gate decision record failed");
        }
    }

    // -----------------------------------------------------------------
    // §5.7.8 timeout and `expire_gate`
    // -----------------------------------------------------------------

    /// Boxed: part of the `gate_expire <-> gate_reclassify_after_invalid_phase
    /// <-> gate_closed_receiver <-> gate_reregister_or_internal` mutual-recursion
    /// cycle (bounded by `EXPIRE_RETRY_MAX` / `GATE_REREGISTER_MAX`, but Rust's
    /// opaque `async fn` return type can't express a cycle without one `Box::pin`
    /// indirection somewhere in it).
    fn gate_expire<'a>(
        &'a self,
        dag: &'a mut TaskDag,
        rc: &'a RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<StepOutcome, SchedError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // GT3: a later loop iteration for the same key skips further calls.
            if rc.gate_already_expired(gate_id) {
                if let Some(GateResolution::Expired) = self
                    .scan_gate_resolution(dag.id, rc.ctx.session_id, gate_id, dag.generation)
                    .await?
                {
                    return self
                        .gate_terminal(
                            dag,
                            rc,
                            node_id,
                            GateDecision::Expired,
                            approval_timeout_failure(node_id, "approval timeout"),
                        )
                        .await;
                }
                return Err(SchedError::Invariant(format!(
                    "gate {gate_id} already resolved locally this run with no durable resolution"
                )));
            }

            let mut last_err: Option<RunError> = None;
            for attempt in 1..=EXPIRE_RETRY_MAX {
                match self.deps.runs.expire_gate(rc.run_id, gate_id).await {
                    Ok(()) => {
                        rc.mark_gate_expired(gate_id); // GT3(i)
                        return self
                            .gate_terminal(
                                dag,
                                rc,
                                node_id,
                                GateDecision::Expired,
                                approval_timeout_failure(node_id, "approval timeout"),
                            )
                            .await;
                    }
                    Err(RunError::InvalidPhase(_) | RunError::UnknownGate(_)) => {
                        // UnknownGate MUST NOT happen (A7); treat identically to
                        // InvalidPhase per §5.7.8's table.
                        return self
                            .gate_reclassify_after_invalid_phase(dag, rc, node_id, gate_id)
                            .await;
                    }
                    Err(RunError::NotFound(run)) => {
                        return Err(SchedError::Internal(format!(
                            "run row vanished during gate expiry: {run}"
                        )));
                    }
                    Err(other) => {
                        last_err = Some(other);
                        if attempt < EXPIRE_RETRY_MAX {
                            tokio::select! {
                                () = tokio::time::sleep(EXPIRE_RETRY_BACKOFF) => {}
                                () = rc.run_cancel.cancelled() => return self.cancel_path(dag, rc).await,
                            }
                        }
                    }
                }
            }

            // Exhausted EXPIRE_RETRY_MAX with only Err(other) responses: the
            // scheduler terminalizes locally (GT3(iii)) rather than leaving
            // WaitingApproval durable (§5.7.8's note on why Err must not
            // propagate here).
            rc.mark_gate_expired(gate_id);
            let notes = match last_err {
                Some(e) => format!(
                    "approval timeout; expire_gate failed after {EXPIRE_RETRY_MAX} attempts: {e}"
                ),
                None => "approval timeout".into(),
            };
            self.gate_terminal(
                dag,
                rc,
                node_id,
                GateDecision::Expired,
                approval_timeout_failure(node_id, notes),
            )
            .await
        })
    }

    /// §5.7.8's `InvalidPhase`/`UnknownGate` row: re-scan; a resolution now
    /// present is followed, otherwise classify via §5.7.9 — never an
    /// automatic gate expiry.
    async fn gate_reclassify_after_invalid_phase(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
    ) -> Result<StepOutcome, SchedError> {
        rc.mark_gate_expired(gate_id); // this key's expiry question is now settled either way.
        if let Some(resolution) = self
            .scan_gate_resolution(dag.id, rc.ctx.session_id, gate_id, dag.generation)
            .await?
        {
            return self
                .gate_apply_resolution(dag, rc, node_id, resolution)
                .await;
        }
        self.gate_closed_receiver(dag, rc, node_id, gate_id, None)
            .await
    }

    // -----------------------------------------------------------------
    // §5.7.9 closed-receiver / ambiguous-wait classification
    // -----------------------------------------------------------------

    async fn gate_closed_receiver(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
        _adapter_err: Option<AdapterError>,
    ) -> Result<StepOutcome, SchedError> {
        let run_row = self
            .deps
            .sessions
            .get_run(rc.run_id)
            .await
            .map_err(|e| map_store_error(e, dag.id))?
            .ok_or_else(|| {
                SchedError::Internal(format!(
                    "run row vanished during gate closed-receiver classification: {}",
                    rc.run_id
                ))
            })?;
        let control_state = RunControlState::parse(&run_row.state).ok_or_else(|| {
            SchedError::Invariant(format!("unknown run state: {}", run_row.state))
        })?;

        match control_state {
            RunControlState::WaitingApproval
            | RunControlState::Created
            | RunControlState::Accepted
            | RunControlState::Running => {
                self.gate_reregister_or_internal(dag, rc, node_id, gate_id, control_state)
                    .await
            }
            RunControlState::Cancelling | RunControlState::Cancelled => {
                self.cancel_path(dag, rc).await
            }
            RunControlState::Failed => {
                match self
                    .scan_gate_resolution(dag.id, rc.ctx.session_id, gate_id, dag.generation)
                    .await?
                {
                    Some(resolution @ (GateResolution::Deny | GateResolution::Expired)) => {
                        self.gate_apply_resolution(dag, rc, node_id, resolution)
                            .await
                    }
                    Some(other) => Err(SchedError::Invariant(format!(
                        "gate {gate_id}: durable Failed control state but resolution is {other:?}"
                    ))),
                    None => {
                        self.gate_terminal(
                            dag,
                            rc,
                            node_id,
                            GateDecision::Expired,
                            approval_timeout_failure(node_id, "gate waiter closed; run failed"),
                        )
                        .await
                    }
                }
            }
            RunControlState::Succeeded => Err(SchedError::Invariant(
                "run succeeded while gate pending".into(),
            )),
            RunControlState::ReplanRequested => self.replan_path(dag, rc).await,
        }
    }

    /// §5.7.9's "re-register once (bounded by `GATE_REREGISTER_MAX`), then
    /// treat as `Internal`" rows.
    async fn gate_reregister_or_internal(
        &self,
        dag: &mut TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
        control_state: RunControlState,
    ) -> Result<StepOutcome, SchedError> {
        if !rc.try_consume_gate_reregister(gate_id) {
            return Err(SchedError::Internal(format!(
                "gate waiter closed in state {control_state:?}"
            )));
        }
        let remaining = self
            .gate_remaining_deadline(dag, rc, node_id, gate_id)
            .await?;
        if remaining == Duration::ZERO {
            return self.gate_expire(dag, rc, node_id, gate_id).await;
        }
        self.gate_wait_and_dispatch(dag, rc, node_id, gate_id, remaining)
            .await
    }

    // -----------------------------------------------------------------
    // §5.7.2 durable resolution scan / GR4 remaining deadline
    // -----------------------------------------------------------------

    async fn scan_gate_resolution(
        &self,
        dag_id: DagId,
        session_id: SessionId,
        gate_id: GateId,
        generation: u64,
    ) -> Result<Option<GateResolution>, SchedError> {
        let gate_id_str = gate_id.to_string();
        let mut after = None;
        let mut newest: Option<GateResolution> = None;
        loop {
            let page = self
                .deps
                .events
                .list_session_events(session_id, after, MAX_EVENTS_PAGE)
                .await
                .map_err(|e| map_store_error(e, dag_id))?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for ev in &page {
                after = Some(ev.seq);
                if ev.type_ != SessionEventType::ApprovalResolved {
                    continue;
                }
                let matches_gate =
                    ev.payload.get("gate_id").and_then(Value::as_str) == Some(gate_id_str.as_str());
                let matches_gen =
                    ev.payload.get("generation").and_then(Value::as_u64) == Some(generation);
                // A8: resume-repair emitters without gate_id/generation are
                // out of scope for this scan and never match.
                if !matches_gate || !matches_gen {
                    continue;
                }
                let Some(decision) = ev.payload.get("decision").and_then(Value::as_str) else {
                    continue;
                };
                newest = Some(parse_gate_resolution(decision)?);
            }
            if page_len < MAX_EVENTS_PAGE {
                break;
            }
        }
        Ok(newest)
    }

    /// GR4: `timeout_ms - elapsed_since(first generation-matched
    /// ApprovalRequested.ts)`, clamped at `>= 0`. GR3/RF6: repairs a missing
    /// `ApprovalRequested` (the only place the scheduler may re-emit it)
    /// rather than failing closed, treating the repair's `now` as the
    /// request time (never under-waits — B4's posture).
    async fn gate_remaining_deadline(
        &self,
        dag: &TaskDag,
        rc: &RunCtx<'_>,
        node_id: NodeId,
        gate_id: GateId,
    ) -> Result<Duration, SchedError> {
        let node = &dag.nodes[&node_id];
        let timeout_ms = node.timeout_ms;
        let events: Arc<dyn EventStore> = Arc::clone(&self.deps.events);
        let gate_id_str = gate_id.to_string();
        let mut after = None;
        let mut first_ts: Option<crate::types::ids::Timestamp> = None;
        'scan: loop {
            let page = events
                .list_session_events(rc.ctx.session_id, after, MAX_EVENTS_PAGE)
                .await
                .map_err(|e| map_store_error(e, dag.id))?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for ev in &page {
                after = Some(ev.seq);
                if ev.type_ != SessionEventType::ApprovalRequested {
                    continue;
                }
                let matches_gate =
                    ev.payload.get("gate_id").and_then(Value::as_str) == Some(gate_id_str.as_str());
                let matches_gen =
                    ev.payload.get("generation").and_then(Value::as_u64) == Some(dag.generation);
                if matches_gate && matches_gen {
                    first_ts = Some(ev.ts.clone());
                    break 'scan; // oldest match: pages are chronological.
                }
            }
            if page_len < MAX_EVENTS_PAGE {
                break;
            }
        }

        let first_ts = match first_ts {
            Some(ts) => ts,
            None => {
                // GR3/RF6: repair the missing request rather than failing
                // closed. Treat the repair moment as the request time.
                let reason = node
                    .approval
                    .as_ref()
                    .map(|a| a.reason.clone())
                    .unwrap_or_default();
                rc.checkpoint
                    .repair_approval_requested(
                        dag.id,
                        rc.ctx,
                        node_id,
                        gate_id,
                        &reason,
                        timeout_ms,
                        dag.generation,
                    )
                    .await?;
                return Ok(Duration::from_millis(timeout_ms));
            }
        };
        let elapsed_ms = (crate::types::ids::Timestamp::now().0 - first_ts.0)
            .whole_milliseconds()
            .max(0) as u64;
        Ok(Duration::from_millis(timeout_ms).saturating_sub(Duration::from_millis(elapsed_ms)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- helpers -----

    fn node() -> NodeId {
        NodeId::new()
    }

    // ----- happy path -----

    #[test]
    fn parse_gate_resolution_round_trips_every_wire_decision() {
        assert_eq!(
            parse_gate_resolution("allow").unwrap(),
            GateResolution::Allow
        );
        assert_eq!(
            parse_gate_resolution("allow_once").unwrap(),
            GateResolution::AllowOnce
        );
        assert_eq!(parse_gate_resolution("deny").unwrap(), GateResolution::Deny);
        assert_eq!(
            parse_gate_resolution("expired").unwrap(),
            GateResolution::Expired
        );
    }

    #[test]
    fn gate_decision_str_round_trips_every_checkpoint_decision() {
        assert_eq!(gate_decision_str(GateDecision::Allow), "allow");
        assert_eq!(gate_decision_str(GateDecision::AllowOnce), "allow_once");
        assert_eq!(gate_decision_str(GateDecision::Deny), "deny");
        assert_eq!(gate_decision_str(GateDecision::Expired), "expired");
    }

    #[test]
    fn gate_decision_str_and_parse_gate_resolution_agree() {
        // The checkpoint layer writes `gate_decision_str`; the re-scan reads
        // it back through `parse_gate_resolution` — the two vocabularies must
        // never drift (a checkpoint decision the scan can't parse would wedge
        // every resume/re-register path behind it).
        for d in [
            GateDecision::Allow,
            GateDecision::AllowOnce,
            GateDecision::Deny,
            GateDecision::Expired,
        ] {
            parse_gate_resolution(gate_decision_str(d)).unwrap();
        }
    }

    // ----- error paths -----

    #[test]
    fn parse_gate_resolution_unknown_decision_is_invariant() {
        let err = parse_gate_resolution("maybe").unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("maybe")));
    }

    // ----- oracle parity -----
    // (not applicable — pure vocabulary/shape helpers, not a wire codec that
    // must byte-match the Scala reference node.)

    #[test]
    fn approval_denied_failure_is_approval_class_nonretryable() {
        let n = node();
        let f = approval_denied_failure(n);
        assert_eq!(f.node, n);
        assert_eq!(f.error_class, ErrorClass::Approval); // GD3
        assert_eq!(f.retry, RetryDisposition::NonRetryable);
        assert!(f.diagnostics.is_empty());
    }

    #[test]
    fn approval_timeout_failure_is_approval_not_timeout_class() {
        // GT4: expiry must never surface as ErrorClass::Timeout — that class
        // is reserved for execution deadlines, not the human-gate deadline.
        let n = node();
        let f = approval_timeout_failure(n, "gate waiter closed; run failed");
        assert_eq!(f.node, n);
        assert_eq!(f.error_class, ErrorClass::Approval);
        assert_eq!(f.retry, RetryDisposition::NonRetryable);
        assert_eq!(f.notes, "gate waiter closed; run failed");
    }
}
