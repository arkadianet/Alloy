//! Error types for the Alloy runtime host and stub surfaces.

use thiserror::Error;

use crate::events::EventSinkError;
use crate::types::ids::{DagId, GateId, RunId, SessionId};

/// Host-level runtime errors.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Operation not valid in the current phase.
    #[error("invalid phase {current:?} for operation {op}")]
    InvalidPhase {
        /// Current phase.
        current: crate::runtime::RuntimePhase,
        /// Operation name.
        op: &'static str,
    },
    /// Configuration load/validation failure.
    #[error("config: {0}")]
    Config(String),
    /// Scheduler is the MVP [`crate::NullScheduler`] (or equivalent).
    #[error("scheduler unavailable")]
    SchedulerUnavailable,
    /// A DAG `run` is already in flight (MVP single-flight).
    #[error("scheduler busy")]
    SchedulerBusy,
    /// Non-unavailable scheduler failure.
    #[error("scheduler: {0}")]
    Scheduler(#[source] SchedError),
    /// Event sink replace blocked or timed out.
    #[error("event sink busy")]
    EventSinkBusy,
    /// Event sink I/O or internal failure.
    #[error("event sink: {0}")]
    EventSink(#[from] EventSinkError),
    /// Filesystem or other I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Runtime already stopped.
    #[error("already stopped")]
    AlreadyStopped,
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

/// Scheduler errors (RFC-0010 fills behavior; trait compiles today).
#[derive(Debug, Error)]
pub enum SchedError {
    /// No real scheduler registered.
    #[error("unavailable")]
    Unavailable,
    /// Run cancelled.
    #[error("cancelled")]
    Cancelled,
    /// Unknown DAG id.
    #[error("dag not found: {0}")]
    DagNotFound(DagId),
    /// Internal scheduler error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Session service errors (behavior in RFC-0003).
#[derive(Debug, Error)]
pub enum SessionError {
    /// Session missing.
    #[error("not found: {0}")]
    NotFound(SessionId),
    /// Invalid request/state.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Internal error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Run controller errors (behavior in RFC-0003).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    /// Run missing.
    #[error("not found: {0}")]
    NotFound(RunId),
    /// Invalid run phase.
    #[error("invalid phase: {0}")]
    InvalidPhase(String),
    /// Internal error.
    #[error("internal: {0}")]
    Internal(String),
    /// No executable scheduler / NullScheduler / SchedError::Unavailable.
    #[error("scheduler unavailable")]
    SchedulerUnavailable,
    /// `start` called while an in-process live execution is already registered.
    #[error("already started: {0}")]
    AlreadyStarted(RunId),
    /// `approve` for a gate with no pending waiter.
    #[error("unknown gate: {0}")]
    UnknownGate(GateId),
}

/// Runtime adapter errors (impl in RFC-0010 / 0006).
#[derive(Debug, Error)]
pub enum AdapterError {
    /// Adapter not wired yet.
    #[error("unavailable")]
    Unavailable,
    /// Cancelled via token.
    #[error("cancelled")]
    Cancelled,
    /// Underlying tool failure.
    #[error("tool: {0}")]
    Tool(String),
    /// Internal adapter error.
    #[error("internal: {0}")]
    Internal(String),
}
