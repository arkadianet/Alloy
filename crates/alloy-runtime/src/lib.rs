//! Alloy runtime host and shared intermediate representation.
//!
//! This crate is the foundation defined by **RFC-0001**, extended by **RFC-0002**
//! (durable storage, artifacts, session event log) and **RFC-0003** (session manager
//! and run controller control plane).
//!
//! # Crate map
//!
//! - [`types`] — IDs, budgets, diagnostics, permissions, metrics
//! - [`events`] — session event envelopes and [`EventSink`]
//! - [`storage`] — SQLite event log, artifact CAS, handoff (RFC-0002)
//! - [`runtime`] — [`AlloyRuntime`] host lifecycle
//! - [`scheduler`] — [`Scheduler`] trait + [`NullScheduler`]
//! - [`adapters`] — Verify*/GateHuman stub traits
//! - [`session`] — [`SessionPlane`] control plane: Session/RunController (RFC-0003)
//! - [`dag`] — TaskDag type sketches (store in RFC-0009)
//! - [`config`] — TOML + env load (never writes `.env`)
//!
//! Author: arkadianet

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod adapters;
pub mod config;
pub mod dag;
pub mod error;
pub mod events;
pub mod logging;
pub mod runtime;
pub mod scheduler;
pub mod session;
pub mod storage;
pub mod types;

pub use adapters::{
    Approval, GateHumanAdapter, NodeExecContext, NodeExecRef, UnavailableGateHuman,
    UnavailableVerifyCompile, UnavailableVerifyTest, VerifyCompileAdapter, VerifyOutcome,
    VerifyTestAdapter,
};
pub use config::{ConfigPaths, RuntimeConfig};
pub use dag::{
    ApprovalSpec, Backoff, CacheKey, DependencyEdge, EdgeKind, NodeKind, NodeState, RetryPolicy,
    TaskDag, TaskNode,
};
pub use error::{AdapterError, RunError, RuntimeError, SchedError, SessionError};
pub use events::{
    EventSink, EventSinkError, HandoffSnapshot, InMemoryEventSink, NewSessionEvent, RuntimeEvent,
    SessionEvent, SessionEventType,
};
pub use runtime::{AlloyRuntime, RuntimeHandle, RuntimePhase};
pub use scheduler::{DagOutcome, DagState, NullScheduler, Scheduler};
pub use session::{
    clamp_events_page_limit, ReplanReason, RunControlState, RunController, RunGoalRecord, Session,
    SessionMetrics, SessionPlane, SessionService, MAX_EVENTS_PAGE,
};
pub use storage::{
    install_sqlite_event_sink, store_to_runtime, store_to_session, AlloyStorage, ArtifactBlob,
    ArtifactKind, ArtifactMeta, ArtifactPut, ArtifactStore, EventStore, FsArtifactStore, RunRow,
    SessionRows, SqliteEventStore, SqliteSessionRows, SqliteSynchronous, StorageLayout,
    StorageMetricsSnapshot, StorageOpenOptions, StoreError,
};
pub use types::budget::{
    BudgetPolicy, BudgetSnapshot, Constraint, CreateSession, Goal, ModelTier, TokenBudget,
};
pub use types::diagnostic::{DiagnosticEvent, DiagnosticLevel, ErrorClass, FailureIr, SpanRef};
pub use types::ids::{
    ArtifactId, CapabilityId, CheckpointId, DagId, DiagnosticId, Digest, DigestError, EventSeq,
    GateId, GraphNodeId, GraphVersion, IdError, LanguageId, NodeId, ProfileId, ProviderId, RunId,
    SessionId, Timestamp, TransactionId,
};
pub use types::metrics::{RuntimeMetrics, WorkerMetrics};
pub use types::permission::{ExecAllow, Glob, Grant, HostAllow, PermissionToken};
