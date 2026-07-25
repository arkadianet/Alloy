//! [`RunController`] implementation (RFC-0003 §6).
//!
//! Owns run-control transitions, `RunAccepted` / `RunFinished` emission, and the gate
//! waiter lifecycle. DAG execution stays behind [`crate::RuntimeHandle::run_dag`]; DAG
//! topology and node state belong to RFC-0009 / RFC-0010.
//!
//! Author: arkadianet

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::goal_record::RunGoalRecord;
use super::inner::SessionInner;
use super::map_err::{runtime_to_run, store_to_run};
use super::metrics::AtomicSessionMetrics;
use super::run_state::RunControlState;
use super::traits::{ReplanReason, RunController};
use crate::adapters::Approval;
use crate::error::{RunError, RuntimeError, SchedError};
use crate::events::{NewSessionEvent, RuntimeEvent, SessionEventType};
use crate::runtime::RuntimePhase;
use crate::scheduler::{DagOutcome, DagState};
use crate::storage::{RunRow, SessionRows};
use crate::types::ids::{DagId, GateId, RunId, SessionId, Timestamp};

/// Phase gate for `start` / `approve` / `request_replan` / `register_gate_waiter`.
pub(super) fn require_running(inner: &SessionInner, op: &str) -> Result<(), RunError> {
    let phase = inner.handle.phase();
    if phase == RuntimePhase::Running {
        Ok(())
    } else {
        Err(RunError::InvalidPhase(format!("{op} in phase {phase:?}")))
    }
}

/// Phase gate for `cancel` (allowed while draining).
fn require_running_or_draining(inner: &SessionInner, op: &str) -> Result<(), RunError> {
    let phase = inner.handle.phase();
    match phase {
        RuntimePhase::Running | RuntimePhase::Draining => Ok(()),
        _ => Err(RunError::InvalidPhase(format!("{op} in phase {phase:?}"))),
    }
}

/// Load a run row, mapping a missing row to typed [`RunError::NotFound`].
pub(super) async fn load_run(inner: &SessionInner, run: RunId) -> Result<RunRow, RunError> {
    inner
        .storage
        .sessions()
        .get_run(run)
        .await
        .map_err(store_to_run)?
        .ok_or(RunError::NotFound(run))
}

/// Parse the control-plane state vocabulary; unknown strings are not dispatchable.
pub(super) fn parse_state(row: &RunRow) -> Result<RunControlState, RunError> {
    RunControlState::parse(&row.state)
        .ok_or_else(|| RunError::InvalidPhase(format!("unknown run state: {}", row.state)))
}

/// Persist a new control state for `row` (row-first ordering).
pub(super) async fn upsert_state(
    inner: &SessionInner,
    row: &RunRow,
    state: RunControlState,
) -> Result<(), RunError> {
    let next = RunRow {
        state: state.as_str().to_owned(),
        updated_at: Timestamp::now(),
        ..row.clone()
    };
    inner
        .storage
        .sessions()
        .upsert_run(&next)
        .await
        .map_err(store_to_run)
}

/// Deserialize the goal envelope; corrupt JSON is an internal error (§4).
fn parse_goal(row: &RunRow) -> Result<RunGoalRecord, RunError> {
    serde_json::from_value(row.goal_json.clone())
        .map_err(|e| RunError::Internal(format!("corrupt goal_json for run {}: {e}", row.id)))
}

/// Append a session event for a run through the active sink.
async fn append_run_event(
    inner: &SessionInner,
    session: SessionId,
    run: RunId,
    type_: SessionEventType,
    payload: Value,
) -> Result<(), RunError> {
    inner
        .handle
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_,
            payload,
        })
        .await
        .map(|_seq| ())
        .map_err(runtime_to_run)
}

/// Best-effort `Error` event; failures are logged, never masked over the real error.
async fn append_error_event(
    inner: &SessionInner,
    session: SessionId,
    run: RunId,
    class: &str,
    message: Option<String>,
) {
    let payload = match message {
        Some(m) => json!({ "class": class, "message": m }),
        None => json!({ "class": class }),
    };
    if let Err(e) = append_run_event(inner, session, run, SessionEventType::Error, payload).await {
        warn!(run_id = %run, class, error = %e, "failed to append run Error event");
    }
}

/// `RunCompleted` payload carries the DAG state that terminated the run.
async fn append_run_completed(
    inner: &SessionInner,
    session: SessionId,
    run: RunId,
    state: DagState,
    reason: Option<&str>,
) -> Result<(), RunError> {
    let mut payload = json!({ "dag_state": state });
    if let (Some(reason), Some(map)) = (reason, payload.as_object_mut()) {
        map.insert("reason".into(), Value::String(reason.to_owned()));
    }
    append_run_event(inner, session, run, SessionEventType::RunCompleted, payload).await
}

/// Emit the host `RunFinished` event (terminal outcomes only).
async fn emit_run_finished(
    inner: &SessionInner,
    run: RunId,
    outcome: DagOutcome,
) -> Result<(), RunError> {
    inner
        .handle
        .emit(RuntimeEvent::RunFinished {
            run_id: run,
            outcome,
        })
        .await
        .map_err(runtime_to_run)
}

/// Outcome recorded when a run terminates without a scheduler-provided outcome.
fn synthetic_outcome(dag_id: DagId, state: DagState) -> DagOutcome {
    DagOutcome {
        dag_id,
        generation: 0,
        state,
        failed_node: None,
        failure: None,
    }
}

/// Control states that a late `run_dag` completion must not clobber (§6.3 step 9a).
fn is_advanced(state: RunControlState) -> bool {
    matches!(
        state,
        RunControlState::ReplanRequested
            | RunControlState::Cancelling
            | RunControlState::Cancelled
            | RunControlState::Succeeded
            | RunControlState::Failed
    )
}

/// `Arc<dyn RunController>` view over the shared session plane.
pub(super) struct RunControllerView {
    inner: Arc<SessionInner>,
}

impl RunControllerView {
    pub(super) fn new(inner: Arc<SessionInner>) -> Self {
        Self { inner }
    }

    /// §6.3 steps 9–10, executed under the re-acquired per-run mutex.
    async fn apply_start_outcome(
        &self,
        run: RunId,
        dag_id: DagId,
        result: Result<DagOutcome, RuntimeError>,
    ) -> Result<(), RunError> {
        let row = load_run(&self.inner, run).await?;
        let session = row.session_id;
        let durable = parse_state(&row)?;

        if is_advanced(durable) {
            warn!(
                run_id = %run,
                state = durable.as_str(),
                "start outcome merged: durable state advanced during run_dag"
            );
            // A cancel or replan landed while we awaited: preserve it, drop this outcome.
            if durable != RunControlState::Succeeded && durable != RunControlState::Failed {
                return Ok(());
            }
            return match result {
                Ok(_) => Err(RunError::InvalidPhase("state advanced during run".into())),
                Err(RuntimeError::Scheduler(SchedError::Cancelled)) => Ok(()),
                Err(e) => Err(runtime_to_run(e)),
            };
        }

        match result {
            Ok(outcome) => self.apply_ok_outcome(&row, session, run, outcome).await,
            Err(RuntimeError::Scheduler(SchedError::Cancelled)) => {
                // Cancellation from the scheduler is a success path, not `runtime_to_run`.
                upsert_state(&self.inner, &row, RunControlState::Cancelled).await?;
                append_run_completed(&self.inner, session, run, DagState::Cancelled, None).await?;
                emit_run_finished(
                    &self.inner,
                    run,
                    synthetic_outcome(dag_id, DagState::Cancelled),
                )
                .await?;
                AtomicSessionMetrics::inc(&self.inner.metrics.runs_cancelled);
                Ok(())
            }
            Err(
                RuntimeError::SchedulerUnavailable
                | RuntimeError::Scheduler(SchedError::Unavailable),
            ) => {
                // Durable state stays `accepted`: the run remains re-dispatchable.
                AtomicSessionMetrics::inc(&self.inner.metrics.runs_start_unavailable);
                warn!(run_id = %run, "start unavailable: no executable scheduler");
                append_error_event(&self.inner, session, run, "scheduler_unavailable", None).await;
                Err(RunError::SchedulerUnavailable)
            }
            Err(RuntimeError::Scheduler(SchedError::DagNotFound(id))) => {
                append_error_event(&self.inner, session, run, "dag_not_found", None).await;
                Err(RunError::InvalidPhase(format!("dag not found: {id}")))
            }
            Err(e @ (RuntimeError::SchedulerBusy | RuntimeError::InvalidPhase { .. })) => {
                Err(runtime_to_run(e))
            }
            Err(e @ (RuntimeError::EventSinkBusy | RuntimeError::EventSink(_))) => {
                warn!(run_id = %run, error = %e, "event sink failure during start");
                Err(runtime_to_run(e))
            }
            Err(other) => {
                let mapped = runtime_to_run(other);
                append_error_event(
                    &self.inner,
                    session,
                    run,
                    "internal",
                    Some(mapped.to_string()),
                )
                .await;
                Err(mapped)
            }
        }
    }

    /// §6.3 step 10 `Ok(outcome)` rows.
    async fn apply_ok_outcome(
        &self,
        row: &RunRow,
        session: SessionId,
        run: RunId,
        outcome: DagOutcome,
    ) -> Result<(), RunError> {
        let control = match outcome.state {
            DagState::Succeeded => RunControlState::Succeeded,
            DagState::Failed => RunControlState::Failed,
            DagState::Cancelled => RunControlState::Cancelled,
            DagState::WaitingApproval => RunControlState::WaitingApproval,
            DagState::Running => RunControlState::Running,
            DagState::ReplanRequired => RunControlState::ReplanRequested,
            // `Pending` means the host admitted work that never started, which is a
            // scheduler contract violation; durable state stays `accepted`.
            DagState::Pending => {
                return Err(RunError::Internal("unexpected pending outcome".into()));
            }
        };
        upsert_state(&self.inner, row, control).await?;

        if !control.is_terminal() {
            info!(run_id = %run, state = control.as_str(), "run not terminal after run_dag");
            return Ok(());
        }

        let state = outcome.state;
        append_run_completed(&self.inner, session, run, state, None).await?;
        emit_run_finished(&self.inner, run, outcome).await?;
        if control == RunControlState::Cancelled {
            AtomicSessionMetrics::inc(&self.inner.metrics.runs_cancelled);
        }
        info!(run_id = %run, dag_state = ?state, "run finished");
        Ok(())
    }

    /// §6.5 durable writes for a resolved gate, performed before notifying the waiter.
    async fn persist_approval(
        &self,
        row: &RunRow,
        gate: GateId,
        decision: Approval,
    ) -> Result<(), RunError> {
        let session = row.session_id;
        let run = row.id;
        let resolved = json!({ "gate_id": gate, "decision": decision });

        if decision == Approval::Deny {
            upsert_state(&self.inner, row, RunControlState::Failed).await?;
            self.inner.gates.clear_run(run);
            append_run_event(
                &self.inner,
                session,
                run,
                SessionEventType::ApprovalResolved,
                resolved,
            )
            .await?;
            append_run_completed(
                &self.inner,
                session,
                run,
                DagState::Failed,
                Some("approval_denied"),
            )
            .await?;
            match parse_goal(row) {
                Ok(record) if self.inner.was_accepted_emitted(run) => {
                    emit_run_finished(
                        &self.inner,
                        run,
                        synthetic_outcome(record.dag_id, DagState::Failed),
                    )
                    .await?;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(run_id = %run, error = %e, "corrupt goal_json: skipping RunFinished for denied gate");
                }
            }
            return Ok(());
        }

        upsert_state(&self.inner, row, RunControlState::Running).await?;
        append_run_event(
            &self.inner,
            session,
            run,
            SessionEventType::ApprovalResolved,
            resolved,
        )
        .await
    }
}

#[async_trait]
impl RunController for RunControllerView {
    /// §6.3 — accept (once), dispatch through the host forwarder, then merge the outcome.
    ///
    /// The per-run mutex is released for the duration of the `run_dag` await so
    /// `approve` / `cancel` stay responsive; the execution lease in
    /// `live_execution` is what makes a concurrent `start` fail with
    /// [`RunError::AlreadyStarted`].
    async fn start(&self, run: RunId) -> Result<(), RunError> {
        require_running(&self.inner, "start")?;
        let lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        let state = parse_state(&row)?;
        let dag_id = parse_goal(&row)?.dag_id;
        let live = self.inner.has_live(run);

        match state {
            RunControlState::Created | RunControlState::Accepted if live => {
                return Err(RunError::AlreadyStarted(run));
            }
            RunControlState::Created | RunControlState::Accepted => {}
            RunControlState::Running | RunControlState::WaitingApproval => {
                return Err(RunError::AlreadyStarted(run));
            }
            RunControlState::Cancelling => {
                return Err(RunError::InvalidPhase("cancelling".into()));
            }
            RunControlState::Cancelled | RunControlState::Succeeded | RunControlState::Failed => {
                return Err(RunError::InvalidPhase("terminal".into()));
            }
            RunControlState::ReplanRequested => {
                return Err(RunError::InvalidPhase("replan pending".into()));
            }
        }

        let first_dispatch = state == RunControlState::Created;
        if first_dispatch {
            upsert_state(&self.inner, &row, RunControlState::Accepted).await?;
            self.inner
                .handle
                .emit(RuntimeEvent::RunAccepted {
                    run_id: run,
                    dag_id,
                })
                .await
                .map_err(runtime_to_run)?;
            self.inner.mark_accepted_emitted(run);
        }

        self.inner.set_live(run, true);
        AtomicSessionMetrics::inc(&self.inner.metrics.runs_started);
        info!(run_id = %run, dag_id = %dag_id, redispatch = !first_dispatch, "run start");

        let ticket = lock.unlock();
        let result = self.inner.handle.run_dag(dag_id).await;
        let lock = ticket.relock().await;

        let applied = self.apply_start_outcome(run, dag_id, result).await;
        // Lease is released only after the durable transition for this result.
        self.inner.set_live(run, false);
        drop(lock);
        applied
    }

    /// §6.4 — mark `cancelling`, drop waiters, cancel the DAG, finalize `cancelled`.
    async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        require_running_or_draining(&self.inner, "cancel")?;
        let lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        let session = row.session_id;
        let state = parse_state(&row)?;
        let dag_id = match parse_goal(&row) {
            Ok(record) => Some(record.dag_id),
            Err(e) => {
                warn!(run_id = %run, error = %e, "cancel with corrupt goal_json: skipping cancel_dag");
                None
            }
        };
        if state.is_terminal() {
            return Ok(());
        }
        let left_created = state != RunControlState::Created;

        upsert_state(&self.inner, &row, RunControlState::Cancelling).await?;
        self.inner.gates.clear_run(run);

        let ticket = lock.unlock();
        let cancelled = match dag_id {
            Some(dag_id) => self.inner.handle.cancel_dag(dag_id).await,
            None => Ok(()),
        };
        let lock = ticket.relock().await;

        if let Err(e) = cancelled {
            // Durable state stays `cancelling`; a later `cancel` completes idempotently.
            warn!(run_id = %run, error = %e, "cancel_dag failed; run left cancelling");
            drop(lock);
            return Err(runtime_to_run(e));
        }

        upsert_state(&self.inner, &row, RunControlState::Cancelled).await?;
        self.inner.set_live(run, false);
        append_run_completed(&self.inner, session, run, DagState::Cancelled, None).await?;

        if let Some(dag_id) = dag_id {
            if self.inner.was_accepted_emitted(run) || left_created {
                emit_run_finished(
                    &self.inner,
                    run,
                    synthetic_outcome(dag_id, DagState::Cancelled),
                )
                .await?;
            }
        }

        AtomicSessionMetrics::inc(&self.inner.metrics.runs_cancelled);
        info!(run_id = %run, "run cancelled");
        drop(lock);
        Ok(())
    }

    /// §6.5 — state guards first, then persist the decision, then notify the waiter.
    async fn approve(&self, run: RunId, gate: GateId, decision: Approval) -> Result<(), RunError> {
        require_running(&self.inner, "approve")?;
        let _lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        match parse_state(&row)? {
            RunControlState::WaitingApproval => {}
            RunControlState::Cancelled | RunControlState::Succeeded | RunControlState::Failed => {
                return Err(RunError::InvalidPhase("terminal".into()));
            }
            RunControlState::Cancelling => {
                return Err(RunError::InvalidPhase("cancelling".into()));
            }
            RunControlState::Created
            | RunControlState::Accepted
            | RunControlState::Running
            | RunControlState::ReplanRequested => {
                return Err(RunError::InvalidPhase("not waiting approval".into()));
            }
        }

        let sender = self
            .inner
            .gates
            .take(run, gate)
            .ok_or(RunError::UnknownGate(gate))?;

        if let Err(e) = self.persist_approval(&row, gate, decision).await {
            // The gate must not be consumed as approved when persistence failed.
            self.inner.gates.restore(run, gate, sender);
            return Err(e);
        }

        sender
            .send(decision)
            .map_err(|_| RunError::Internal("gate waiter dropped".into()))?;

        AtomicSessionMetrics::inc(&self.inner.metrics.approvals_resolved);
        info!(run_id = %run, gate_id = %gate, decision = ?decision, "approval resolved");
        Ok(())
    }

    /// §6.6 — record the replan request; DAG mutation belongs to RFC-0009 / RFC-0010.
    async fn request_replan(&self, run: RunId, reason: ReplanReason) -> Result<(), RunError> {
        require_running(&self.inner, "request_replan")?;
        let _lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        let session = row.session_id;
        let state = parse_state(&row)?;
        parse_goal(&row)?;

        match state {
            RunControlState::Accepted
            | RunControlState::Running
            | RunControlState::WaitingApproval => {}
            RunControlState::ReplanRequested => return Ok(()),
            RunControlState::Created => {
                return Err(RunError::InvalidPhase("not started".into()));
            }
            RunControlState::Cancelling => {
                return Err(RunError::InvalidPhase("cancelling".into()));
            }
            RunControlState::Cancelled | RunControlState::Succeeded | RunControlState::Failed => {
                return Err(RunError::InvalidPhase("terminal".into()));
            }
        }

        self.inner.gates.clear_run(run);
        upsert_state(&self.inner, &row, RunControlState::ReplanRequested).await?;
        append_run_event(
            &self.inner,
            session,
            run,
            SessionEventType::ReplanRequested,
            json!({ "reason": reason }),
        )
        .await?;

        AtomicSessionMetrics::inc(&self.inner.metrics.replans_requested);
        info!(run_id = %run, "replan requested");
        Ok(())
    }
}
