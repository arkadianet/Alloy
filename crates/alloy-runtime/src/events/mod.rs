//! Session event envelopes and event sink injection.

mod runtime_event;
mod sink;

pub use runtime_event::RuntimeEvent;
pub use sink::{EventSink, EventSinkError, InMemoryEventSink};

use serde::{Deserialize, Serialize};

use crate::types::ids::{EventSeq, RunId, SessionId, Timestamp};

/// Appendix A session event type strings (`snake_case` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventType {
    /// Session created.
    SessionCreated,
    /// Goal submitted.
    GoalSubmitted,
    /// Plan/DAG produced.
    PlanProduced,
    /// Node state transition.
    NodeState,
    /// Decision record.
    Decision,
    /// Model call record.
    ModelCall,
    /// Tool call record.
    ToolCall,
    /// Edit applied.
    EditApplied,
    /// Approval requested.
    ApprovalRequested,
    /// Approval resolved.
    ApprovalResolved,
    /// Budget warning.
    BudgetWarning,
    /// Replan requested.
    ReplanRequested,
    /// Run completed.
    RunCompleted,
    /// Error event.
    Error,
}

/// Persisted session event envelope (Appendix A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Sequence number within the session.
    pub seq: EventSeq,
    /// Event timestamp.
    pub ts: Timestamp,
    /// Owning session.
    pub session_id: SessionId,
    /// Optional run.
    pub run_id: Option<RunId>,
    /// Event type.
    #[serde(rename = "type")]
    pub type_: SessionEventType,
    /// Type-specific payload.
    pub payload: serde_json::Value,
}

/// New session event before sequence assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionEvent {
    /// Owning session.
    pub session_id: SessionId,
    /// Optional run.
    pub run_id: Option<RunId>,
    /// Event type.
    #[serde(rename = "type")]
    pub type_: SessionEventType,
    /// Type-specific payload.
    pub payload: serde_json::Value,
}
