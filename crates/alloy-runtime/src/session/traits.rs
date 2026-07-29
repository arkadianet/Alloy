//! Control-plane trait stubs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::run_state::RunControlState;
use crate::adapters::Approval;
use crate::error::{RunError, SessionError};
use crate::events::SessionEvent;
use crate::types::budget::{BudgetPolicy, CreateSession, Goal};
use crate::types::diagnostic::FailureIr;
use crate::types::ids::{EventSeq, GateId, LanguageId, ProfileId, RunId, SessionId, Timestamp};

/// Hard cap for [`SessionService::events`] page size (impls must clamp or reject above this).
pub const MAX_EVENTS_PAGE: usize = 1_000;

/// Clamp a requested page size into `1..=MAX_EVENTS_PAGE`.
#[must_use]
pub fn clamp_events_page_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_EVENTS_PAGE)
}

/// Session lifecycle API (behavior in RFC-0003).
#[async_trait]
pub trait SessionService: Send + Sync {
    /// Create a session.
    async fn create(&self, req: CreateSession) -> Result<SessionId, SessionError>;
    /// Resume a session.
    async fn resume(&self, id: SessionId) -> Result<Session, SessionError>;
    /// Submit a goal; returns a run id.
    async fn submit_goal(&self, id: SessionId, goal: Goal) -> Result<RunId, SessionError>;
    /// List session events with an exclusive cursor and page limit.
    ///
    /// - `after: None` — return from the first event (`EventSeq(0)`).
    /// - `after: Some(seq)` — return events with `seq > after` (exclusive).
    /// - `limit` — max events to return; impls must use [`clamp_events_page_limit`]
    ///   (or reject `0` / values above [`MAX_EVENTS_PAGE`] with [`SessionError::Invalid`]).
    async fn events(
        &self,
        id: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError>;
}

/// Run control API (behavior in RFC-0003).
#[async_trait]
pub trait RunController: Send + Sync {
    /// Start a run.
    async fn start(&self, run: RunId) -> Result<(), RunError>;
    /// Cancel a run.
    async fn cancel(&self, run: RunId) -> Result<(), RunError>;
    /// Resolve a human gate.
    async fn approve(&self, run: RunId, gate: GateId, decision: Approval) -> Result<(), RunError>;
    /// Request a replan.
    async fn request_replan(&self, run: RunId, reason: ReplanReason) -> Result<(), RunError>;
    /// Terminalize a gate whose `timeout_ms` elapsed (RFC-0010 §3.15 / §5.7.8,
    /// amendment A4). Mirrors `approve(Deny)` with `decision: "expired"`.
    ///
    /// Idempotent with respect to a missing waiter (amendment A7): a
    /// `(run, gate)` with no registered waiter is not an error.
    async fn expire_gate(&self, run: RunId, gate: GateId) -> Result<(), RunError>;

    /// Re-arm an **externally** replanned run (RFC-0017 AM-0003-1):
    /// `ReplanRequested → Accepted` — not `Running`, so re-entry reuses
    /// §6.3's existing `Accepted` arm and no second `RunAccepted` is
    /// emitted. Requires no live execution lease and a stored DAG not
    /// `Running`. Idempotent from `Accepted` (no second event); every other
    /// state, or a held lease, is [`RunError::InvalidPhase`]. Appends
    /// `ReplanResumed`. The caller then calls [`Self::start`].
    async fn resume_after_replan(&self, run: RunId) -> Result<(), RunError>;

    /// In-run generation bump, step 1 of 2 (RFC-0017 AM-0003-3). Drops all
    /// gate waiters for the run and appends a `ReplanRequested` session
    /// event carrying `reason`. Leaves the row `Running` (rule RC1 — never
    /// writes `RunControlState::ReplanRequested`). Requires a live
    /// execution lease for `run` (callable only from inside `start`'s
    /// dispatch); otherwise [`RunError::InvalidPhase`] (rule RC2).
    async fn begin_repair_generation(
        &self,
        run: RunId,
        reason: &ReplanReason,
    ) -> Result<(), RunError>;

    /// In-run generation bump, step 2 of 2 (RFC-0017 AM-0003-3). Appends
    /// `ReplanResumed` `{ run_id, generation }`. Same lease precondition as
    /// [`Self::begin_repair_generation`]. Leaves the row `Running`.
    async fn complete_repair_generation(&self, run: RunId, generation: u64)
        -> Result<(), RunError>;

    /// Read the durable control state (RFC-0017 AM-0003-3, rule RC4).
    /// Takes the per-run mutex, parses the stored state, writes nothing.
    async fn control_state(&self, run: RunId) -> Result<RunControlState, RunError>;
}

/// Session record snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session id.
    pub id: SessionId,
    /// Workspace root.
    pub workspace_root: std::path::PathBuf,
    /// Profile.
    pub profile: ProfileId,
    /// Budget policy.
    pub budget: BudgetPolicy,
    /// Language backends.
    pub language_backends: Vec<LanguageId>,
    /// Creation time.
    pub created_at: Timestamp,
}

/// Reason for requesting a replan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReplanReason {
    /// Failure IR from a node.
    FailureIr(FailureIr),
    /// User requested.
    UserRequested,
    /// Budget policy triggered.
    BudgetPolicy,
    /// Other.
    Other(String),
}
