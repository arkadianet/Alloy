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
use super::map_err::{run_to_session, runtime_to_session};
use super::profiles::validate_mvp_profile;
use super::run_controller::{finalize_cancelled, has_run_accepted, upsert_state};
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

    /// §5.3 step 9: rewrite one crash-recovery row, holding the per-run mutex.
    ///
    /// `running` / `waiting_approval` go back to `accepted` so the run is re-dispatchable.
    /// A `cancelling` row is instead **finalized**: the cancel that wrote it died with the
    /// process that owned it, so resume owes the durable terminal state plus the
    /// `RunCompleted` / `RunFinished` pair that no other writer will ever produce.
    ///
    /// Cancel finalization writes terminal events **before** the `Cancelled` upsert so a
    /// failed append/emit cannot leave the row permanently `Cancelled` with those events
    /// missing. Existence checks keep a retry from duplicating events already written by a
    /// prior attempt that failed on the upsert. `RunFinished` is gated on a durable
    /// `RunAccepted` (not on `cancelling` alone), because `created → cancelling` never
    /// announced acceptance.
    async fn rearm_run(
        &self,
        row: &RunRow,
        state: RunControlState,
        dag_id: Option<DagId>,
    ) -> Result<(), SessionError> {
        let target = match state {
            RunControlState::Running | RunControlState::WaitingApproval => {
                RunControlState::Accepted
            }
            RunControlState::Cancelling => RunControlState::Cancelled,
            _ => return Ok(()),
        };

        if target == RunControlState::Cancelled {
            let accepted = has_run_accepted(&self.inner, row.id)
                .await
                .map_err(run_to_session)?;
            finalize_cancelled(
                &self.inner,
                row,
                dag_id,
                accepted,
                Some("resume_finalized_cancel"),
            )
            .await
            .map_err(run_to_session)?;
            info!(
                run_id = %row.id,
                from = state.as_str(),
                to = target.as_str(),
                "resume finalized abandoned cancel"
            );
            return Ok(());
        }

        upsert_state(&self.inner, row, target)
            .await
            .map_err(run_to_session)?;
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
                runtime_to_session(e)
            })?;

        self.inner.metrics.bump_sessions_created();
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
    ///
    /// Each row is re-read under its per-run mutex, so resume serializes against a
    /// concurrent `cancel` / `start` instead of racing the listing snapshot.
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

        let listed = self.rows().list_runs(id).await.map_err(store_to_session)?;
        for run in listed.into_iter().map(|row| row.id) {
            // The listing is a snapshot. Every run-control write is serialized on the
            // per-run mutex, so take it and re-read the row before deciding anything:
            // otherwise a concurrent `cancel` can be rewritten from a stale state.
            let _lock = self.inner.lock_run(run).await;
            let Some(row) = self.rows().get_run(run).await.map_err(store_to_session)? else {
                continue;
            };
            let Some(state) = RunControlState::parse(&row.state) else {
                warn!(run_id = %run, state = %row.state, "skipping run with unknown control state");
                continue;
            };
            let dag_id = match serde_json::from_value::<RunGoalRecord>(row.goal_json.clone()) {
                Ok(record) => Some(record.dag_id),
                Err(e) => {
                    warn!(run_id = %run, error = %e, "skipping run binding: corrupt goal_json");
                    None
                }
            };
            // A corrupt run is never dispatched, so re-arming it would only hide the
            // problem — except for `cancelling`, which MUST still be finalized (§5.3).
            if dag_id.is_none() && state != RunControlState::Cancelling {
                continue;
            }
            if self.inner.has_live(run) {
                continue;
            }
            if let Err(e) = self.rearm_run(&row, state, dag_id).await {
                // Match corrupt-row handling: one bad re-arm must not abort the rest of
                // the session (or the sessions_resumed metric/log path below).
                warn!(run_id = %run, error = %e, "resume failed to re-arm run; continuing");
                continue;
            }
        }

        self.inner.metrics.bump_sessions_resumed();
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

        self.inner.metrics.bump_goals_submitted();
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
