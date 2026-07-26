//! Alloy runtime host and shared intermediate representation.
//!
//! This crate is the foundation defined by **RFC-0001**, extended by **RFC-0002**
//! (durable storage, artifacts, session event log), **RFC-0003** (session manager
//! and run controller), **RFC-0004** (observability & cost metering), tool IR
//! from **RFC-0006**, and model routing/provider support from **RFC-0007**.
//!
//! # Crate map
//!
//! - [`types`] — IDs, budgets, diagnostics, permissions, metrics, tool IR (RFC-0006)
//! - [`events`] — session event envelopes and [`EventSink`]
//! - [`storage`] — SQLite event log, artifact CAS, handoff (RFC-0002)
//! - [`runtime`] — [`AlloyRuntime`] host lifecycle
//! - [`scheduler`] — [`Scheduler`] trait + [`NullScheduler`]
//! - [`adapters`] — Verify*/GateHuman stub traits
//! - [`session`] — [`SessionPlane`] control plane: Session/RunController (RFC-0003)
//! - [`obs`] — DecisionLog, CostMeter, redaction/query helpers (RFC-0004)
//! - [`router`] — sealed model routing, provider traits, and HTTP provider (RFC-0007)
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
pub mod obs;
pub mod router;
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
pub use obs::{
    apply_prompt_retention, apply_tool_retention, hash_content, hash_prompt, hash_tool_body,
    list_decision_events, maybe_signal_budget_warning, parse_decision_event,
    parse_model_call_event, parse_tool_call_event, reaccumulate_cost_from_events,
    redact_json_strings, redact_secrets, BudgetCheck, CostByTier, CostMeter, CostSnapshot,
    DecisionKind, DecisionLog, DecisionPage, DecisionRecord, EventDecisionLog, ModelCallRecord,
    ModelUsdSource, ObsError, RecordingDecisionLog, RetentionPolicy, SharedCostMeter, TierCost,
    ToolCallRecord,
};
pub use router::{
    classify_provider_error, classify_router_error, ChatMessage, ChatRole, Citation,
    ClassifiedRouterFailure, CompletionRequest, ComplexityScore, EndpointConfig, Health,
    ModelEndpoint, ModelProvider, ModelResponse, ModelRouter, PromptPack, ProviderConfig,
    ProviderError, ProviderKind, RecordingModelProvider, ResponseFormat, RoutedModel, RouterConfig,
    RouterError, RouterMetricsSnapshot, RouterPolicy, RouterShutdownReport, RoutingRequest,
    ScoringWeights, SecretString, TomlModelRouter, TomlModelRouterParts, ToolChoice, Usage,
};
#[cfg(feature = "http-provider")]
pub use router::{OpenAiCompatibleProvider, OpenAiCompatibleSpec};
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
pub use types::diagnostic::{
    DiagnosticEvent, DiagnosticLevel, ErrorClass, FailureIr, RetryDisposition, SpanRef,
};
pub use types::ids::{
    ArtifactId, CapabilityId, CheckpointId, DagId, DiagnosticId, Digest, DigestError, EndpointId,
    EventSeq, GateId, GraphNodeId, GraphVersion, IdError, LanguageId, NodeId, ProfileId,
    ProviderId, RunId, ServerId, SessionId, Timestamp, TransactionId,
};
pub use types::metrics::{RuntimeMetrics, WorkerMetrics};
pub use types::permission::{ExecAllow, Glob, Grant, HostAllow, PermissionToken};
pub use types::tools::{
    token_expired, McpServerSpec, McpTransport, ToolCall, ToolError, ToolName, ToolResult,
    ToolSelector, ToolView,
};
