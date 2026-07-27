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

/// Scheduler errors (RFC-0010 §3.2).
///
/// `Err(SchedError)` is reserved for "no durable [`crate::DagOutcome`] was
/// written" — planned failures (compile/test exhaustion, gate deny/expiry,
/// budget/run timeout, cancellation) return `Ok(DagOutcome)` instead.
///
/// `Clone`: `OwnedDag::cancel_result` (§4.3 O3) needs to store a copy of the
/// terminal `Err` alongside the one `run` actually returns. Every variant is
/// plain data (`String`/`DagId`), so this is free.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum SchedError {
    /// No real scheduler registered (`NullScheduler` only).
    #[error("unavailable")]
    Unavailable,
    /// Run cancelled without a durable outcome (prefer `Ok(DagOutcome)`).
    #[error("cancelled")]
    Cancelled,
    /// Unknown DAG id.
    #[error("dag not found: {0}")]
    DagNotFound(DagId),
    /// Internal scheduler error.
    #[error("internal: {0}")]
    Internal(String),
    /// Invalid construction / parallelism / `data_dir` configuration.
    #[error("config: {0}")]
    Config(String),
    /// Generation CAS conflict — the scheduler MUST stop checkpointing.
    #[error("generation conflict for dag {dag_id}")]
    Conflict {
        /// The DAG whose generation moved under the scheduler.
        dag_id: DagId,
    },
    /// Contract violation (multiple Ready nodes, corrupt DAG, impossible state).
    #[error("invariant: {0}")]
    Invariant(String),
    /// Store / artifact / event I/O failure after mapping.
    #[error("store: {0}")]
    Store(String),
    /// Another in-process run already owns this DAG.
    #[error("dag already owned: {0}")]
    AlreadyOwned(DagId),
    /// No run row binds this DAG.
    #[error("no run bound to dag {0}")]
    RunBindingMissing(DagId),
    /// Scheduler ownership could not be established (OS lock, poisoned map).
    #[error("ownership: {0}")]
    Ownership(String),
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

/// Runtime adapter errors (RFC-0010 §3.3).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// Adapter not wired yet.
    #[error("unavailable")]
    Unavailable,
    /// Cancelled via token.
    #[error("cancelled")]
    Cancelled,
    /// Legacy free-form tool failure (retained; new code prefers `ToolFailure`).
    #[error("tool: {0}")]
    Tool(String),
    /// Internal adapter error.
    #[error("internal: {0}")]
    Internal(String),
    /// A tool ran and failed, carrying the merged RFC-0006 taxonomy.
    #[error("tool failure: {0}")]
    ToolFailure(#[source] crate::types::tools::ToolError),
    /// Sandbox / token / disclosure denial. NOT a compile or test failure.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Adapter-observed deadline (host `call_timeout`, sandbox `exec_timeout`).
    #[error("timeout")]
    Timeout,
    /// MCP host draining or stopped.
    #[error("shutting down")]
    ShuttingDown,
    /// Artifact store failure while persisting a raw log.
    #[error("artifact: {0}")]
    Artifact(String),
}
