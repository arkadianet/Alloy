//! Alloy runtime host and shared intermediate representation.
//!
//! This crate is the foundation defined by **RFC-0001**, extended by **RFC-0002**
//! (durable storage, artifacts, session event log), **RFC-0003** (session manager
//! and run controller), **RFC-0004** (observability & cost metering), tool IR
//! from **RFC-0006**, model routing/provider support from **RFC-0007**, Task DAG
//! store/templates/planner from **RFC-0009**, and EditEngine IR/trait from
//! **RFC-0008**.
//!
//! # Crate map
//!
//! - [`types`] — IDs, budgets, diagnostics, permissions, metrics, tool IR (RFC-0006)
//! - [`events`] — session event envelopes and [`EventSink`]
//! - [`storage`] — SQLite event log, artifact CAS, DAG blobs, handoff (RFC-0002/0009)
//! - [`runtime`] — [`AlloyRuntime`] host lifecycle
//! - [`scheduler`] — [`Scheduler`] trait, [`LinearScheduler`] (RFC-0010), [`NullScheduler`]
//! - [`adapters`] — Verify/GateHuman/Capability seams plus the MCP-backed
//!   verify adapters (RFC-0010)
//! - [`session`] — [`SessionPlane`] control plane: Session/RunController (RFC-0003)
//! - [`obs`] — DecisionLog, CostMeter, redaction/query helpers (RFC-0004)
//! - [`router`] — sealed model routing, provider traits, and HTTP provider (RFC-0007)
//! - [`dag`] — TaskDag types, validation, templates, cache, I/O envelopes (RFC-0009)
//! - [`planner`] — [`PlanService`] / [`TemplatePlanService`] (RFC-0009)
//! - [`edit`] — EditEngine trait + TextPatch / SemanticOps IR (RFC-0008)
//! - [`lang`] — [`LanguageBackend`] seam, toolchain runner, registry (RFC-0014)
//! - [`config`] — TOML + env load (never writes `.env`)
//!
//! Author: arkadianet

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod adapters;
pub mod capabilities;
pub mod config;
pub mod context;
pub mod dag;
pub mod edit;
pub mod error;
pub mod events;
pub mod graph;
pub mod lang;
pub mod logging;
pub mod obs;
pub mod planner;
pub mod router;
pub mod runtime;
pub mod scheduler;
pub mod session;
pub mod storage;
pub mod types;

pub use adapters::{
    cargo_exit_verdict, diagnostic_fingerprint, parse_rustc_diagnostics, seed_graph_diagnostics,
    Approval, CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityOutcome,
    GateHumanAdapter, McpVerifyCompileAdapter, McpVerifyTestAdapter, NodeExecContext, NodeExecRef,
    SeedReport, SessionGateHumanAdapter, SessionVerifyPermissions, ToolCaller, ToolCallerError,
    UnavailableCapabilityExecutor, UnavailableGateHuman, UnavailableVerifyCompile,
    UnavailableVerifyTest, Verdict, VerdictOutcome, Verifier, VerifyClass, VerifyPermissions,
};
// RFC-0013 exports. The `edit` capability payload (`capabilities::
// EditAppliedPayload`) is deliberately not re-exported here: the crate root
// already exports RFC-0008's session-event payload of the same name.
pub use capabilities::{
    system_instruction_digest, Capability, CapabilityContext, CapabilityDescriptor,
    CapabilityRegistry, CapabilityVersion, EditWorker, PlanningProposalPayload, PlanningWorker,
    ProcessRunRouterProvider, RegError, RegistryCapabilityExecutor, RepairPlanPayload, RepairStep,
    RepairWorker, ResolveHints, ReviewFinding, ReviewPayload, ReviewSeverity, ReviewVerdict,
    ReviewWorker, RunRouterProvider, SessionWorkerPermissions, SideEffectClass, WorkerConfig,
    WorkerDeps, WorkerPermissions, WorkerToolClass, CAPABILITY_CATALOG, EDIT_SYSTEM,
    MAX_LLM_CAPABILITIES, PAYLOAD_SCHEMA_VERSION, REPAIR_SYSTEM, REVIEW_SYSTEM,
};
pub use config::{default_router_toml, ConfigPaths, GatesConfig, RuntimeConfig, SandboxEcho};
pub use context::{
    AssembleInputs, AssembleRequest, BytesPerTokenEstimator, CompactStrategy, ContextEngine,
    ContextError, ContextHandle, ContextMetricsSnapshot, ContextProfile, DefaultContextEngine,
    Degradation, DegradationReason, DomainId, DomainWeights, EvictPolicy, EvictReport, FileExcerpt,
    GraphProjection, NullContextEngine, StaleReason, TokenEstimator, WorkingSet,
};
pub use dag::{
    allocate_ids, build_topology, compiler_fingerprint_digest, compute_cache_key,
    goal_content_digest, policy_hash_digest, tool_versions_digest, ApprovalSpec, Backoff,
    BuildTopology, CacheKey, CacheKeyMaterials, DagValidationError, DagValidator, DependencyEdge,
    EdgeKind, NodeInputEnvelope, NodeInputPayload, NodeKind, NodeOutputEnvelope, NodeState,
    PredecessorOutput, RetryIncoherence, RetryPolicy, TaskDag, TaskNode, TemplateApprovalSpec,
    TemplateCatalog, TemplateEdgeSpec, TemplateId, TemplateIdMap, TemplateManifest,
    TemplateNodeSpec, ValidateOpts, ENVELOPE_SCHEMA_VERSION,
};
pub use edit::{
    rollback_run_edits, transactions_of_run, DeclinedRollback, EditAppliedPayload, EditContext,
    EditEngine, EditError, EditRequest, EditRequestKind, EditTransaction, EditValidation,
    FilePatch, Hunk, PatchSet, RollbackReport, SemanticEditOp, TxState, WorkspaceDigest,
    EDIT_APPLIED_SCHEMA,
};
pub use error::{AdapterError, RunError, RuntimeError, SchedError, SessionError};
pub use events::{
    EventSink, EventSinkError, HandoffSnapshot, InMemoryEventSink, NewSessionEvent, RuntimeEvent,
    SessionEvent, SessionEventType,
};
pub use graph::{
    derive_node_id, FileChange, FileChangeKind, FixEvent, GraphEdge, GraphEdgeKind, GraphError,
    GraphFidelity, GraphNode, GraphNodeKind, GraphQuery, GraphView, GraphViewHandle, IngestReport,
    NullProjectGraph, ProjectGraph,
};
pub use lang::{
    scope_package, selector_args, LangError, LanguageBackend, LanguageManifest, LanguageRegistry,
    McpToolchainRunner, RustToolchain, Scope, TestReport, TestSelector, TextEdit, ToolchainRunner,
};
pub use obs::{
    apply_prompt_retention, apply_tool_retention, hash_content, hash_prompt, hash_tool_body,
    list_decision_events, maybe_signal_budget_warning, parse_decision_event,
    parse_model_call_event, parse_tool_call_event, reaccumulate_cost_from_events,
    redact_json_strings, redact_secrets, BudgetCheck, CostByTier, CostMeter, CostMeterFactory,
    CostSnapshot, DecisionKind, DecisionLog, DecisionPage, DecisionRecord, EventDecisionLog,
    ModelCallRecord, ModelUsdSource, ObsError, ProcessCostMeterFactory, RecordingDecisionLog,
    RetentionPolicy, SharedCostMeter, TierCost, ToolCallRecord,
};
pub use planner::{
    DisabledLlmPlanService, PlanContext, PlanError, PlanProducedPayload, PlanResult, PlanService,
    TemplatePlanService,
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
pub use scheduler::{
    backoff_delay, derive_dag_state, promotable_nodes, ready_nodes, DagOutcome, DagState,
    DeriveFlags, LinearScheduler, LinearSchedulerDeps, NullScheduler, SchedConfig, Scheduler,
    SchedulerMetrics,
};
pub use session::{
    clamp_events_page_limit, validate_mvp_profile, ReplanReason, RunControlState, RunController,
    RunGoalRecord, Session, SessionMetrics, SessionPlane, SessionService, MAX_EVENTS_PAGE,
    MVP_PROFILES, TRAJECTORY_SCHEMA_VERSION,
};
pub use storage::{
    install_sqlite_event_sink, store_to_runtime, store_to_session, AlloyStorage, ArtifactBlob,
    ArtifactKind, ArtifactMeta, ArtifactPut, ArtifactStore, DagStore, EventStore, FsArtifactStore,
    ReplanReplaceError, RunRow, SessionRows, SqliteDagStore, SqliteEventStore, SqliteSessionRows,
    SqliteSynchronous, StorageLayout, StorageMetricsSnapshot, StorageOpenOptions, StoreError,
};
pub use types::budget::{
    BudgetPolicy, BudgetSnapshot, Constraint, CreateSession, Goal, ModelTier, TokenBudget,
};
pub use types::diagnostic::{
    DiagnosticEvent, DiagnosticLevel, ErrorClass, FailureIr, RetryDisposition, SpanRef,
};
pub use types::ids::{
    ArtifactId, CapabilityId, CheckpointId, CrateId, DagId, DiagnosticId, Digest, DigestError,
    DigestHasher, EndpointId, EventSeq, GateId, GraphNodeId, GraphSnapshotId, GraphVersion,
    IdError, LanguageId, NodeId, ProfileId, ProviderId, RunId, ServerId, SessionId, SummaryId,
    Timestamp, TrajectoryId, TransactionId,
};
pub use types::metrics::{RuntimeMetrics, WorkerMetrics};
pub use types::permission::{ExecAllow, Glob, Grant, HostAllow, PermissionToken};
pub use types::provenance::{ConsentRecord, SessionProvenance, PROVENANCE_SCHEMA_VERSION};
pub use types::toolchain::ToolchainRecord;
pub use types::tools::{
    token_expired, McpServerSpec, McpTransport, ToolCall, ToolError, ToolName, ToolResult,
    ToolSelector, ToolView,
};
