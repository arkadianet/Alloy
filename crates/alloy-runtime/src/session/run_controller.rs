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
use super::run_state::RunControlState;
use super::traits::{ReplanReason, RunController, MAX_EVENTS_PAGE};
use crate::adapters::Approval;
use crate::error::{RunError, RuntimeError, SchedError};
use crate::events::{NewSessionEvent, RuntimeEvent, SessionEventType};
use crate::runtime::RuntimePhase;
use crate::scheduler::{DagOutcome, DagState};
use crate::storage::{EventStore, RunRow, SessionRows};
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
///
/// A terminal write also prunes process-local run tracking via
/// [`SessionInner::on_terminal`], so no caller has to remember to release the
/// execution lease or the `RunAccepted` marker itself.
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
    if inner.take_fail_run_upsert() {
        return Err(RunError::Internal("injected upsert failure".into()));
    }
    inner
        .storage
        .sessions()
        .upsert_run(&next)
        .await
        .map_err(store_to_run)?;
    if state.is_terminal() {
        inner.on_terminal(row.id);
    }
    Ok(())
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
    if inner.take_fail_append() {
        return Err(RunError::Internal("injected append failure".into()));
    }
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
pub(super) async fn append_run_completed(
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
pub(super) async fn emit_run_finished(
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

/// True if this run already has a durable `RunCompleted` (retry idempotency).
pub(super) async fn has_run_completed(
    inner: &SessionInner,
    session: SessionId,
    run: RunId,
) -> Result<bool, RunError> {
    let store = inner.storage.events();
    let mut after = None;
    loop {
        let page = store
            .list_session_events(session, after, MAX_EVENTS_PAGE)
            .await
            .map_err(store_to_run)?;
        if page
            .iter()
            .any(|e| e.run_id == Some(run) && e.type_ == SessionEventType::RunCompleted)
        {
            return Ok(true);
        }
        match page.last() {
            Some(last) if page.len() == MAX_EVENTS_PAGE => after = Some(last.seq),
            _ => return Ok(false),
        }
    }
}

/// True if this run already has a host `RunFinished` (retry idempotency).
pub(super) async fn has_run_finished(inner: &SessionInner, run: RunId) -> Result<bool, RunError> {
    runtime_event_for_run(
        inner,
        run,
        |ev| matches!(ev, RuntimeEvent::RunFinished { run_id, .. } if *run_id == run),
    )
    .await
}

/// True if this run already has a host `RunAccepted`.
///
/// Used when durable state is `cancelling` / being finalized: that state is reachable from
/// `created` without acceptance, so `was_accepted(run, Cancelling)` would lie.
pub(super) async fn has_run_accepted(inner: &SessionInner, run: RunId) -> Result<bool, RunError> {
    if inner.was_accepted(run, RunControlState::Created) {
        return Ok(true);
    }
    runtime_event_for_run(
        inner,
        run,
        |ev| matches!(ev, RuntimeEvent::RunAccepted { run_id, .. } if *run_id == run),
    )
    .await
}

async fn runtime_event_for_run(
    inner: &SessionInner,
    _run: RunId,
    mut pred: impl FnMut(&RuntimeEvent) -> bool,
) -> Result<bool, RunError> {
    let store = inner.storage.events();
    let mut after = None;
    loop {
        let page = store
            .list_runtime_events(after, MAX_EVENTS_PAGE)
            .await
            .map_err(store_to_run)?;
        if page.iter().any(|(_, ev)| pred(ev)) {
            return Ok(true);
        }
        match page.last() {
            Some((rowid, _)) if page.len() == MAX_EVENTS_PAGE => after = Some(*rowid),
            _ => return Ok(false),
        }
    }
}

/// Write cancel terminal events, then the `Cancelled` row (events-before-upsert).
pub(super) async fn finalize_cancelled(
    inner: &SessionInner,
    row: &RunRow,
    dag_id: Option<DagId>,
    accepted: bool,
    completed_reason: Option<&str>,
) -> Result<(), RunError> {
    let session = row.session_id;
    let run = row.id;
    if !has_run_completed(inner, session, run).await? {
        append_run_completed(inner, session, run, DagState::Cancelled, completed_reason).await?;
    }
    if let Some(dag_id) = dag_id.filter(|_| accepted) {
        if !has_run_finished(inner, run).await? {
            emit_run_finished(inner, run, synthetic_outcome(dag_id, DagState::Cancelled)).await?;
        }
    }
    upsert_state(inner, row, RunControlState::Cancelled).await?;
    inner.metrics.bump_runs_cancelled();
    Ok(())
}

/// Failure from [`RunControllerView::persist_approval`], noting whether the control row left
/// `waiting_approval` so the caller can decide whether to restore the gate sender.
struct PersistApprovalError {
    err: RunError,
    /// `true` when the durable state write already committed.
    row_committed: bool,
}

/// Outcome recorded when a run terminates without a scheduler-provided outcome.
pub(super) fn synthetic_outcome(dag_id: DagId, state: DagState) -> DagOutcome {
    DagOutcome {
        dag_id,
        generation: 0,
        state,
        failed_node: None,
        failure: None,
    }
}

/// States written by an explicit control call — `cancel`, `request_replan`, or
/// `register_gate_waiter` — that a late `run_dag` completion must not clobber (§6.3 step
/// 9a). Losing this race is the designed outcome, not an anomaly.
fn is_control_protected(state: RunControlState) -> bool {
    matches!(
        state,
        RunControlState::ReplanRequested
            | RunControlState::Cancelling
            | RunControlState::Cancelled
            | RunControlState::WaitingApproval
    )
}

/// Terminal states no control call in §6 writes without going through `start` itself, so
/// finding one here means a second writer finalized the run mid-`run_dag`.
fn is_terminal_durable(state: RunControlState) -> bool {
    matches!(state, RunControlState::Succeeded | RunControlState::Failed)
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

        if is_control_protected(durable) {
            // A cancel, replan, or gate registration landed while we awaited. Preserving
            // it is the contract, so this is an expected merge rather than a fault.
            info!(
                run_id = %run,
                state = durable.as_str(),
                "start outcome merged: control transition took precedence"
            );
            if matches!(
                result,
                Err(RuntimeError::SchedulerUnavailable
                    | RuntimeError::Scheduler(SchedError::Unavailable))
            ) {
                self.inner.metrics.bump_runs_start_unavailable();
            }
            return Ok(());
        }

        if is_terminal_durable(durable) {
            warn!(
                run_id = %run,
                state = durable.as_str(),
                "start outcome merged: durable state already terminal"
            );
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
                finalize_cancelled(&self.inner, &row, Some(dag_id), true, None).await
            }
            Err(
                RuntimeError::SchedulerUnavailable
                | RuntimeError::Scheduler(SchedError::Unavailable),
            ) => {
                // Durable state stays `accepted`: the run remains re-dispatchable.
                self.inner.metrics.bump_runs_start_unavailable();
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

        if !control.is_terminal() {
            upsert_state(&self.inner, row, control).await?;
            info!(run_id = %run, state = control.as_str(), "run not terminal after run_dag");
            return Ok(());
        }

        // Terminal pair before the row write: a crash after upsert would leave a terminal
        // row that resume skips, with no other writer to emit the events.
        let state = outcome.state;
        append_run_completed(&self.inner, session, run, state, None).await?;
        emit_run_finished(&self.inner, run, outcome).await?;
        upsert_state(&self.inner, row, control).await?;
        if control == RunControlState::Cancelled {
            self.inner.metrics.bump_runs_cancelled();
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
    ) -> Result<(), PersistApprovalError> {
        let session = row.session_id;
        let run = row.id;
        let resolved = json!({ "gate_id": gate, "decision": decision });

        if decision == Approval::Deny {
            // `row` is still `waiting_approval`, so acceptance is implied even when the
            // `RunAccepted` emission happened in an earlier process. Sample it before the
            // terminal write prunes the process-local marker.
            let accepted = self.inner.was_accepted(
                run,
                parse_state(row).map_err(|err| PersistApprovalError {
                    err,
                    row_committed: false,
                })?,
            );
            if let Err(err) = upsert_state(&self.inner, row, RunControlState::Failed).await {
                return Err(PersistApprovalError {
                    err,
                    row_committed: false,
                });
            }
            self.inner.gates.clear_run(run);
            let after = async {
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
                    Ok(record) if accepted => {
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
                Ok(())
            };
            return after.await.map_err(|err| PersistApprovalError {
                err,
                row_committed: true,
            });
        }

        if let Err(err) = upsert_state(&self.inner, row, RunControlState::Running).await {
            return Err(PersistApprovalError {
                err,
                row_committed: false,
            });
        }
        append_run_event(
            &self.inner,
            session,
            run,
            SessionEventType::ApprovalResolved,
            resolved,
        )
        .await
        .map_err(|err| PersistApprovalError {
            err,
            row_committed: true,
        })
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
        let live = self.inner.has_live(run);

        // State guards run before the goal envelope is parsed: a live, cancelling,
        // replan-pending, or terminal run is not dispatchable whatever its payload says,
        // and the state error is the one the caller can act on.
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

        let dag_id = parse_goal(&row)?.dag_id;
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

        // The lease is held across the await so a cancelled/aborted `start` future still
        // releases it via `Drop`; a completed dispatch releases it explicitly below.
        let lease = self.inner.acquire_lease(run);
        self.inner.metrics.bump_runs_started();
        info!(run_id = %run, dag_id = %dag_id, redispatch = !first_dispatch, "run start");

        let ticket = lock.unlock();
        let result = self.inner.handle.run_dag(dag_id).await;
        let lock = ticket.relock().await;

        let applied = self.apply_start_outcome(run, dag_id, result).await;
        // The durable transition for this dispatch is recorded, so the run may be
        // re-dispatched. `disarm` keeps `Drop` from repeating a release we just did.
        self.inner.clear_lease(run);
        lease.disarm();
        drop(lock);
        applied
    }

    /// §6.4 — mark `cancelling`, drop waiters, cancel the DAG, finalize `cancelled`.
    async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        require_running_or_draining(&self.inner, "cancel")?;
        let lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        let state = parse_state(&row)?;
        if state.is_terminal() {
            return Ok(());
        }
        let dag_id = match parse_goal(&row) {
            Ok(record) => Some(record.dag_id),
            Err(e) => {
                warn!(run_id = %run, error = %e, "cancel with corrupt goal_json: skipping cancel_dag");
                None
            }
        };

        // Never-started runs were never admitted: finalize in one shot so we never write
        // `cancelling` (which would erase the "never accepted" fact for retries/resume).
        if state == RunControlState::Created {
            self.inner.gates.clear_run(run);
            finalize_cancelled(&self.inner, &row, None, false, None).await?;
            info!(run_id = %run, "run cancelled");
            drop(lock);
            return Ok(());
        }

        // `cancelling` is reachable from `created`, so durable state alone cannot decide
        // whether `RunAccepted` was announced — consult the event log / process marker.
        let accepted = if state == RunControlState::Cancelling {
            has_run_accepted(&self.inner, run).await?
        } else {
            self.inner.was_accepted(run, state)
        };

        if state != RunControlState::Cancelling {
            upsert_state(&self.inner, &row, RunControlState::Cancelling).await?;
        }
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

        // The row was writable by others while `cancel_dag` was awaited (a resume
        // finalizing this cancel, for instance), so finalize from a fresh read.
        let fresh = load_run(&self.inner, run).await?;
        if parse_state(&fresh)?.is_terminal() {
            info!(run_id = %run, "cancel found the run already finalized");
            drop(lock);
            return Ok(());
        }

        finalize_cancelled(&self.inner, &fresh, dag_id, accepted, None).await?;
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
            // Restore only when the row write itself failed. If the state already left
            // `waiting_approval`, putting the sender back permanently strands Deny waiters
            // (terminal guards block every release path) — drop it so they observe closure.
            if !e.row_committed {
                self.inner.gates.restore(run, gate, sender);
            }
            return Err(e.err);
        }

        sender
            .send(decision)
            .map_err(|_| RunError::Internal("gate waiter dropped".into()))?;

        self.inner.metrics.bump_approvals_resolved();
        info!(run_id = %run, gate_id = %gate, decision = ?decision, "approval resolved");
        Ok(())
    }

    /// §6.6 — record the replan request; DAG mutation belongs to RFC-0009 / RFC-0010.
    async fn request_replan(&self, run: RunId, reason: ReplanReason) -> Result<(), RunError> {
        require_running(&self.inner, "request_replan")?;
        let _lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        let session = row.session_id;

        // Replan records intent only; it never dispatches, so the goal envelope is
        // irrelevant here and a corrupt one must not block recording the request.
        match parse_state(&row)? {
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

        self.inner.metrics.bump_replans_requested();
        info!(run_id = %run, "replan requested");
        Ok(())
    }
}
