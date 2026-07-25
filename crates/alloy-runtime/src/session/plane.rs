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
use super::metrics::{AtomicSessionMetrics, SessionMetrics};
use super::run_controller::{load_run, parse_state, require_running, upsert_state};
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
}

impl SessionPlane {
    /// Wire the control plane onto a started runtime and an opened store.
    #[must_use]
    pub fn new(handle: RuntimeHandle, storage: Arc<AlloyStorage>) -> Self {
        Self {
            inner: Arc::new(SessionInner::new(handle, storage)),
        }
    }

    /// [`SessionService`] view over the same inner state.
    #[must_use]
    pub fn sessions(&self) -> Arc<dyn SessionService> {
        Arc::new(SessionServiceView::new(Arc::clone(&self.inner)))
    }

    /// [`RunController`] view over the same inner state.
    #[must_use]
    pub fn runs(&self) -> Arc<dyn RunController> {
        Arc::new(super::run_controller::RunControllerView::new(Arc::clone(
            &self.inner,
        )))
    }

    /// Snapshot of the in-process counters (RFC-0003 §13).
    #[must_use]
    pub fn metrics(&self) -> SessionMetrics {
        self.inner.metrics.snapshot()
    }

    /// CLI convenience facade for [`RunController::approve`] (traits stay distinct).
    pub async fn approve(
        &self,
        run: RunId,
        gate: GateId,
        decision: Approval,
    ) -> Result<(), RunError> {
        self.runs().approve(run, gate, decision).await
    }

    /// CLI convenience facade for [`RunController::cancel`].
    pub async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        self.runs().cancel(run).await
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

        AtomicSessionMetrics::inc(&self.inner.metrics.budget_warnings);
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
    /// [`RunError::InvalidPhase`] when it is terminal, cancelling, or not yet started.
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
            | RunControlState::WaitingApproval
            | RunControlState::ReplanRequested => {}
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

        upsert_state(&self.inner, &row, RunControlState::WaitingApproval).await?;
        let rx = self.inner.gates.register(run, gate);
        info!(run_id = %run, gate_id = %gate, "gate waiter registered");
        Ok(rx)
    }
}
