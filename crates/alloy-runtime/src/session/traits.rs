//! Control-plane trait stubs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
