//! [`SessionPlane`] — process control plane wiring (RFC-0003 §3.6).
//!
//! Construct after `install_sqlite_event_sink` so session events go through the
//! durable sink. Holds no ownership of [`crate::AlloyRuntime`].
//!
//! Author: arkadianet

use std::sync::Arc;

use tokio::sync::oneshot;
use tracing::{info, warn};

use super::inner::SessionInner;
use super::metrics::SessionMetrics;
use super::run_controller::{
    load_run, parse_state, require_running, upsert_state, RunControllerView,
};
use super::run_state::RunControlState;
use super::service::require_mutating_phase;
use super::service::SessionServiceView;
use super::traits::{RunController, SessionService};
use crate::adapters::Approval;
use crate::error::{RunError, SessionError};
use crate::events::{NewSessionEvent, SessionEventType};
use crate::runtime::RuntimeHandle;
use crate::storage::{store_to_session, AlloyStorage, SessionRows};
use crate::types::budget::BudgetSnapshot;
use crate::types::ids::{EventSeq, GateId, RunId, SessionId};

/// Process session/run control plane. Cheap to clone (`Arc` inner).
///
/// # Requirements
///
/// - `handle.phase()` is [`crate::RuntimePhase::Running`] for production wiring.
/// - `storage` is the same `Arc` returned by
///   [`crate::install_sqlite_event_sink`], so rows and events share one store.
#[derive(Clone)]
pub struct SessionPlane {
    inner: Arc<SessionInner>,
    sessions: Arc<dyn SessionService>,
    runs: Arc<dyn RunController>,
}

impl SessionPlane {
    /// Wire the control plane onto a started runtime and an opened store.
    #[must_use]
    pub fn new(handle: RuntimeHandle, storage: Arc<AlloyStorage>) -> Self {
        let inner = Arc::new(SessionInner::new(handle, storage));
        let sessions = Arc::new(SessionServiceView::new(Arc::clone(&inner))) as Arc<_>;
        let runs = Arc::new(RunControllerView::new(Arc::clone(&inner))) as Arc<_>;
        Self {
            inner,
            sessions,
            runs,
        }
    }

    /// [`SessionService`] view over the same inner state.
    #[must_use]
    pub fn sessions(&self) -> Arc<dyn SessionService> {
        Arc::clone(&self.sessions)
    }

    /// [`RunController`] view over the same inner state.
    #[must_use]
    pub fn runs(&self) -> Arc<dyn RunController> {
        Arc::clone(&self.runs)
    }

    /// Snapshot of the in-process counters (RFC-0003 §13).
    #[must_use]
    pub fn metrics(&self) -> SessionMetrics {
        self.inner.metrics.snapshot()
    }

    /// Inject the §6.3 step-8 executor (RFC-0017 AM-0003-2, rule RX4).
    ///
    /// The plane starts with [`crate::DirectRunExecutor`] — today's
    /// single-generation dispatch. The composition root calls this once,
    /// before dispatching runs, to install the repair-generation driver.
    /// The CLI's own call sequence is unchanged (RFC-0015 B1/SQ2): this is
    /// construct-and-inject, not a new execution entry point.
    pub fn set_executor(&self, executor: Arc<dyn super::run_executor::RunExecutor>) {
        self.inner.set_executor(executor);
    }

    /// CLI convenience facade for [`RunController::approve`] (traits stay distinct).
    pub async fn approve(
        &self,
        run: RunId,
        gate: GateId,
        decision: Approval,
    ) -> Result<(), RunError> {
        self.runs.approve(run, gate, decision).await
    }

    /// CLI convenience facade for [`RunController::cancel`].
    pub async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        self.runs.cancel(run).await
    }

    /// Budget exhaustion hook (RFC-0003 §5.6).
    ///
    /// RFC-0004 metering calls this after computing spend; the hook itself neither
    /// meters nor decides policy. Callers MAY follow up with
    /// [`RunController::request_replan`] or [`Self::cancel`].
    pub async fn signal_budget_warning(
        &self,
        session: SessionId,
        run: Option<RunId>,
        snapshot: BudgetSnapshot,
        message: impl Into<String> + Send,
    ) -> Result<EventSeq, SessionError> {
        require_mutating_phase(&self.inner)?;
        let _lock = self.inner.lock_session(session).await;

        if self
            .inner
            .storage
            .sessions()
            .get_session(session)
            .await
            .map_err(store_to_session)?
            .is_none()
        {
            return Err(SessionError::NotFound(session));
        }

        let message = message.into();
        warn!(session_id = %session, run_id = ?run, "budget warning");
        let seq = self
            .inner
            .handle
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: run,
                type_: SessionEventType::BudgetWarning,
                payload: serde_json::json!({ "snapshot": snapshot, "message": message }),
            })
            .await
            .map_err(super::map_err::runtime_to_session)?;

        self.inner.metrics.bump_budget_warnings();
        Ok(seq)
    }

    /// Register an in-process gate waiter and persist `waiting_approval` (§6.7).
    ///
    /// RFC-0010 `GateHumanAdapter` MUST call this before awaiting the receiver.
    /// Replaces any prior waiter for `(run, gate)`; the prior receiver then errs
    /// because its sender was dropped. Waiters are not durable — re-register after
    /// [`SessionService::resume`].
    ///
    /// Returns [`RunError::NotFound`] when the run is missing and
    /// [`RunError::InvalidPhase`] when it is terminal, cancelling, replan-pending, or not
    /// yet started.
    pub async fn register_gate_waiter(
        &self,
        run: RunId,
        gate: GateId,
    ) -> Result<oneshot::Receiver<Approval>, RunError> {
        require_running(&self.inner, "register_gate_waiter")?;
        let _lock = self.inner.lock_run(run).await;

        let row = load_run(&self.inner, run).await?;
        match parse_state(&row)? {
            RunControlState::Accepted
            | RunControlState::Running
            | RunControlState::WaitingApproval => {}
            RunControlState::Created => {
                return Err(RunError::InvalidPhase("not started".into()));
            }
            RunControlState::Cancelling => {
                return Err(RunError::InvalidPhase("cancelling".into()));
            }
            // A replan discards the DAG that owns this gate, so re-registering a waiter
            // would rewrite `replan_requested` back to `waiting_approval` (§6.6).
            RunControlState::ReplanRequested => {
                return Err(RunError::InvalidPhase("replan pending".into()));
            }
            RunControlState::Cancelled | RunControlState::Succeeded | RunControlState::Failed => {
                return Err(RunError::InvalidPhase("terminal".into()));
            }
        }

        upsert_state(&self.inner, &row, RunControlState::WaitingApproval).await?;
        let rx = self.inner.gates.register(run, gate);
        info!(run_id = %run, gate_id = %gate, "gate waiter registered");
        Ok(rx)
    }

    /// Test-only: make the next control-plane run-row upsert fail once.
    #[cfg(test)]
    pub(crate) fn fail_next_run_upsert(&self) {
        self.inner
            .fail_next_run_upsert
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only: make the next control-plane session-event append fail once.
    #[cfg(test)]
    pub(crate) fn fail_next_append(&self) {
        self.inner
            .fail_next_append
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only: number of retained per-run mutexes (must return to zero when idle).
    #[cfg(test)]
    pub(crate) fn run_lock_map_len(&self) -> usize {
        self.inner.run_lock_map_len()
    }

    /// Test-only: number of retained per-session mutexes.
    #[cfg(test)]
    pub(crate) fn session_lock_map_len(&self) -> usize {
        self.inner.session_lock_map_len()
    }
}
