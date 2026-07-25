//! [`SessionService`] implementation (RFC-0003 §5).
//!
//! Owns session lifecycle, run row creation, and event reads. Never executes tools,
//! never mutates DAG topology, never assigns event sequence numbers.
//!
//! Author: arkadianet

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tracing::{info, warn};

use super::goal_record::RunGoalRecord;
use super::inner::SessionInner;
use super::map_err::runtime_to_session;
use super::metrics::AtomicSessionMetrics;
use super::profiles::validate_mvp_profile;
use super::run_state::RunControlState;
use super::traits::{clamp_events_page_limit, Session, SessionService};
use crate::error::SessionError;
use crate::events::{NewSessionEvent, SessionEvent, SessionEventType};
use crate::runtime::RuntimePhase;
use crate::storage::{store_to_session, EventStore, RunRow, SessionRows};
use crate::types::budget::{CreateSession, Goal};
use crate::types::ids::{DagId, EventSeq, RunId, SessionId, Timestamp};

/// Phase gate for mutating session APIs (`create`, `submit_goal`, budget hook).
pub(super) fn require_mutating_phase(inner: &SessionInner) -> Result<(), SessionError> {
    if inner.handle.phase() != RuntimePhase::Running {
        return Err(SessionError::Invalid("runtime not running".into()));
    }
    if inner.handle.cancellation().is_cancelled() {
        return Err(SessionError::Invalid("runtime cancelled".into()));
    }
    Ok(())
}

/// Phase gate for read-only session APIs (`resume`, `events`).
fn require_read_phase(inner: &SessionInner) -> Result<(), SessionError> {
    match inner.handle.phase() {
        RuntimePhase::Running | RuntimePhase::Draining => Ok(()),
        _ => Err(SessionError::Invalid("runtime not available".into())),
    }
}

/// `Arc<dyn SessionService>` view over the shared session plane.
pub(super) struct SessionServiceView {
    inner: Arc<SessionInner>,
}

impl SessionServiceView {
    pub(super) fn new(inner: Arc<SessionInner>) -> Self {
        Self { inner }
    }

    fn rows(&self) -> Arc<dyn SessionRows> {
        self.inner.storage.sessions()
    }

    /// Load a session row, mapping a store miss to [`SessionError::NotFound`].
    ///
    /// RFC-0003 §7: `store_to_session` must not invent `NotFound` from stringly misses.
    async fn load_session(&self, id: SessionId) -> Result<Session, SessionError> {
        self.rows()
            .get_session(id)
            .await
            .map_err(store_to_session)?
            .ok_or(SessionError::NotFound(id))
    }

    /// §5.3 step 9: rewrite one crash-recovery row. Returns the new state when rewritten.
    async fn rearm_run(&self, row: &RunRow, state: RunControlState) -> Result<(), SessionError> {
        let target = match state {
            RunControlState::Running | RunControlState::WaitingApproval => {
                RunControlState::Accepted
            }
            RunControlState::Cancelling => RunControlState::Cancelled,
            _ => return Ok(()),
        };
        let rewritten = RunRow {
            state: target.as_str().to_owned(),
            updated_at: Timestamp::now(),
            ..row.clone()
        };
        self.rows()
            .upsert_run(&rewritten)
            .await
            .map_err(store_to_session)?;
        info!(
            run_id = %row.id,
            from = state.as_str(),
            to = target.as_str(),
            "resume re-armed run control state"
        );
        Ok(())
    }
}

#[async_trait]
impl SessionService for SessionServiceView {
    /// §5.2 — validate, `upsert_session`, then append `SessionCreated`.
    async fn create(&self, req: CreateSession) -> Result<SessionId, SessionError> {
        require_mutating_phase(&self.inner)?;
        validate_mvp_profile(&req.profile)?;

        let Some(root) = req.workspace_root.to_str() else {
            return Err(SessionError::Invalid(
                "workspace_root must be valid UTF-8".into(),
            ));
        };
        if !req.workspace_root.is_absolute() {
            return Err(SessionError::Invalid(format!(
                "workspace_root must be absolute: {root}"
            )));
        }
        if req.language_backends.is_empty() {
            return Err(SessionError::Invalid(
                "language_backends must not be empty (MVP expects [\"rust\"])".into(),
            ));
        }

        let session = Session {
            id: SessionId::new(),
            workspace_root: req.workspace_root.clone(),
            profile: req.profile,
            budget: req.budget,
            language_backends: req.language_backends,
            created_at: Timestamp::now(),
        };
        let id = session.id;

        self.rows().upsert_session(&session).await.map_err(|e| {
            warn!(error = %e, "session create failed");
            store_to_session(e)
        })?;

        let payload = json!({
            "workspace_root": req.workspace_root.to_string_lossy(),
            "profile": session.profile.as_str(),
            "budget": session.budget,
            "language_backends": session.language_backends,
        });
        // Row committed; a failed append leaves a row without its creation event
        // (§5.2 crash window). There is no compensating delete in RFC-0002.
        self.inner
            .handle
            .append_session(NewSessionEvent {
                session_id: id,
                run_id: None,
                type_: SessionEventType::SessionCreated,
                payload,
            })
            .await
            .map_err(|e| {
                warn!(session_id = %id, error = %e, "session row persisted without SessionCreated");
                SessionError::Internal(format!("append SessionCreated for {id}: {e}"))
            })?;

        AtomicSessionMetrics::inc(&self.inner.metrics.sessions_created);
        info!(session_id = %id, profile = session.profile.as_str(), "session created");
        Ok(id)
    }

    /// §5.3 — snapshot the stored session and re-arm run control rows.
    ///
    /// Run→DAG bindings are derived from `RunRow.goal_json` on demand (§4 ID sourcing);
    /// resume validates them so a corrupt row is warned about and left undispatched
    /// instead of being re-armed. Gate waiters and `live_execution` are process-local
    /// and start empty in a fresh process; a resume in a live process leaves in-flight
    /// leases alone because in-process outcomes are not crash recovery.
    async fn resume(&self, id: SessionId) -> Result<Session, SessionError> {
        require_read_phase(&self.inner)?;
        let session = match self.load_session(id).await {
            Ok(s) => s,
            Err(SessionError::NotFound(_)) => {
                info!(session_id = %id, "resume miss");
                return Err(SessionError::NotFound(id));
            }
            Err(e) => return Err(e),
        };

        for row in self.rows().list_runs(id).await.map_err(store_to_session)? {
            let Some(state) = RunControlState::parse(&row.state) else {
                warn!(run_id = %row.id, state = %row.state, "skipping run with unknown control state");
                continue;
            };
            let goal_ok = match serde_json::from_value::<RunGoalRecord>(row.goal_json.clone()) {
                Ok(_) => true,
                Err(e) => {
                    warn!(run_id = %row.id, error = %e, "skipping run binding: corrupt goal_json");
                    false
                }
            };
            // A corrupt run is never dispatched, so re-arming it would only hide the
            // problem — except for `cancelling`, which MUST still be finalized (§5.3).
            if !goal_ok && state != RunControlState::Cancelling {
                continue;
            }
            if self.inner.has_live(row.id) {
                continue;
            }
            self.rearm_run(&row, state).await?;
        }

        AtomicSessionMetrics::inc(&self.inner.metrics.sessions_resumed);
        info!(session_id = %id, "session resumed");
        Ok(session)
    }

    /// §5.4 — mint `RunId` / `DagId`, persist the run row, then append `GoalSubmitted`.
    async fn submit_goal(&self, id: SessionId, goal: Goal) -> Result<RunId, SessionError> {
        require_mutating_phase(&self.inner)?;
        let _lock = self.inner.lock_session(id).await;

        let session = self.load_session(id).await?;
        if goal.text.trim().is_empty() {
            return Err(SessionError::Invalid("goal text must not be empty".into()));
        }

        let run = RunId::new();
        let dag_id = DagId::new();
        let now = Timestamp::now();
        let record = RunGoalRecord { goal, dag_id };
        let goal_json = serde_json::to_value(&record)
            .map_err(|e| SessionError::Internal(format!("serialize goal record: {e}")))?;

        self.rows()
            .upsert_run(&RunRow {
                id: run,
                session_id: id,
                goal_json,
                state: RunControlState::Created.as_str().to_owned(),
                created_at: now.clone(),
                updated_at: now,
            })
            .await
            .map_err(store_to_session)?;

        self.inner
            .handle
            .append_session(NewSessionEvent {
                session_id: id,
                run_id: Some(run),
                type_: SessionEventType::GoalSubmitted,
                payload: json!({
                    "goal": record.goal,
                    "dag_id": dag_id,
                    "budget": session.budget,
                }),
            })
            .await
            .map_err(runtime_to_session)?;

        AtomicSessionMetrics::inc(&self.inner.metrics.goals_submitted);
        info!(session_id = %id, run_id = %run, dag_id = %dag_id, "goal submitted");
        Ok(run)
    }

    /// §5.5 — exclusive cursor page over the durable event log.
    async fn events(
        &self,
        id: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        require_read_phase(&self.inner)?;
        self.load_session(id).await?;
        let limit = clamp_events_page_limit(limit);
        self.inner
            .storage
            .events()
            .list_session_events(id, after, limit)
            .await
            .map_err(store_to_session)
    }
}
