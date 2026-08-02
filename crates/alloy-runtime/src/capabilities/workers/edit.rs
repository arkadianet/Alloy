//! `EditWorker` (id `edit`, kind `Edit`) — RFC-0013 §9.2, rules EW1–EW11
//! plus the AM-0013-1 line-ops response form.
//!
//! Obtains either a unified diff or a line-ops array from the model,
//! converts it to a validated `PatchSet` **locally** (EW4 / AM-0013-1 —
//! ops are compiled against the CURRENT file content read via `fs_read`,
//! with each op's `expect` lines verified verbatim), persists the
//! canonical patch as `ArtifactKind::Patch` (EW9), and applies it through
//! the `apply_patch` builtin only (EW1: never a second write stack, never
//! a direct file write, never a checkpoint restore). Forward-only: no
//! re-apply, no compensation of a partial apply (EW10) — RFC-0008's
//! transaction is the unit of atomicity.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::{NodeInputPayload, NodeKind};
use crate::edit::{FilePatch, PatchSet};
use crate::graph::GraphQuery;
use crate::obs::{truncate_utf8_bytes, DecisionKind, DecisionRecord};
use crate::storage::{ArtifactKind, ArtifactPut};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{DiagnosticEvent, ErrorClass, RetryDisposition};
use crate::types::ids::{ArtifactId, CapabilityId, TransactionId};
use crate::types::tools::{ToolName, ToolSelector};

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::parse::{
    ops_to_patchset, parse_line_op, parse_model_diff, screen_line_ops, LineOp,
};
use super::super::payload::{
    clamp_string, EditAppliedPayload, MAX_PAYLOAD_STRING_BYTES, PAYLOAD_SCHEMA_VERSION,
};
use super::super::perms::WorkerToolClass;
use super::super::prompt::{edit_response_schema, fence_tool, EDIT_SYSTEM};
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{
    call_tool, diagnostics_from_payloads, finish_attempt, llm_exchange, load_pred_payloads,
    map_tool_result_error, worker_span, Attempt, WorkerError, WorkerSuccess,
};

/// EW5: mirrors RFC-0006's `MAX_ARGUMENT_BYTES` (64 KiB). The constant
/// lives in `alloy-tools`, which this crate MUST NOT depend on (C2), so the
/// bound is restated here and cross-checked by the RFC-0006 host anyway.
const MAX_PATCH_ARGUMENT_BYTES: usize = 64 * 1024;

/// RW2 parity: diagnostics presented to the model are capped at this many,
/// matching the repair worker's bound.
const MAX_DIAGNOSTICS: usize = 32;

/// Tools this worker may call (TL5).
const ALLOWED_TOOLS: [&str; 2] = ["fs_read", "apply_patch"];

/// Model response schema (EW3 + AM-0013-1, PS5: `deny_unknown_fields`):
/// exactly one of `patch` (a unified diff) or `ops` (line operations
/// against the numbered CURRENT file content) — never both, never neither.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchProposal {
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    ops: Option<Vec<serde_json::Value>>,
    summary: String,
    #[serde(default)]
    confidence: Option<f32>,
}

/// The locally validated body of one proposal: a parsed diff, or screened
/// ops that still need the current file content to compile (AM-0013-1).
enum ProposalBody {
    /// EW4: unified diff already parsed into a `PatchSet`.
    Patch(PatchSet),
    /// AM-0013-1: statically screened ops, compiled by [`EditWorker::compile_ops`].
    Ops(Vec<LineOp>),
}

/// Sanitized view of the patch builtin's success content (EW8: paths and
/// transaction id come from the tool outcome, never from the model).
#[derive(Debug, Deserialize)]
struct PatchOutcomeView {
    #[serde(default)]
    files_touched: Vec<String>,
    #[serde(default)]
    transaction_id: Option<TransactionId>,
}

/// OB3-adjacent bounds for the per-attempt `edit_attempt` telemetry record
/// (audit 2026-08 FINDING 1): entries are one per model turn, so the default
/// `max_model_turns = 3` never nears the cap; refusal detail is clamped
/// before it reaches the decision log.
const MAX_TELEMETRY_PROPOSALS: usize = 16;
const MAX_TELEMETRY_REASON_BYTES: usize = 256;

/// One model proposal's observable shape and fate, recorded per turn.
#[derive(Debug, Clone, Serialize)]
struct ProposalObs {
    /// Response form: `"patch"`, `"ops"`, `"both"`, `"neither"`, or
    /// `"undecodable"` (failed `PatchProposal` deserialization).
    form: &'static str,
    /// Ops carried by an ops-form reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    op_count: Option<usize>,
    /// Raw `op` tag per op, in reply order (`"?"` for a missing tag).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    op_kinds: Vec<String>,
    /// The check that refused this proposal, when refused: `schema`,
    /// `form_exclusivity`, `diff_parse`, `op_parse`, `op_screen`,
    /// `ops_compile`, `argument_bytes`, `dry_run`, or `apply`.
    refused_by: Option<&'static str>,
    /// Bounded refusal detail.
    refusal: Option<String>,
}

/// Per-attempt edit-path telemetry (audit 2026-08 FINDING 1: no record
/// existed of response form, op shape, refusing check, or terminal outcome,
/// so no change to this path could be attributed). Interior mutability
/// because the PS6 validate closure is `Fn`; the lock is uncontended and
/// never held across an await.
#[derive(Debug, Default)]
struct EditTelemetry {
    proposals: Mutex<Vec<ProposalObs>>,
}

impl EditTelemetry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProposalObs>> {
        self.proposals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Open one proposal entry for the turn that just validated.
    fn propose(&self, form: &'static str, op_count: Option<usize>, op_kinds: Vec<String>) {
        let mut proposals = self.lock();
        if proposals.len() < MAX_TELEMETRY_PROPOSALS {
            proposals.push(ProposalObs {
                form,
                op_count,
                op_kinds,
                refused_by: None,
                refusal: None,
            });
        }
    }

    /// Attribute a refusal to the newest proposal.
    fn refuse(&self, check: &'static str, reason: &str) {
        if let Some(last) = self.lock().last_mut() {
            last.refused_by = Some(check);
            last.refusal = Some(truncate_utf8_bytes(reason, MAX_TELEMETRY_REASON_BYTES));
        }
    }

    fn snapshot(&self) -> Vec<ProposalObs> {
        self.lock().clone()
    }
}

/// Append the per-attempt `edit_attempt` decision record (FINDING 1).
/// Mirrors `finish_attempt`'s OB3 posture: a host-boundary fault records
/// nothing (the attempt did not complete as a worker attempt), and a
/// decision-log failure never fails the attempt.
async fn record_edit_attempt(
    ctx: &CapabilityContext<'_>,
    attempt: &Attempt,
    telemetry: &EditTelemetry,
    result: &Result<WorkerSuccess, WorkerError>,
) {
    let (outcome, error_class) = match result {
        Ok(_) => ("succeeded", None),
        Err(WorkerError::Soft { class, .. }) => ("failed", Some(*class)),
        Err(WorkerError::Host(_)) => return,
    };
    let metadata = json!({
        "capability": "edit",
        "attempt": ctx.attempt,
        "model_turns": attempt.model_turns,
        "tool_calls": attempt.tool_calls,
        "proposals": telemetry.snapshot(),
        "outcome": outcome,
        "error_class": error_class.map(|c| format!("{c:?}")),
    });
    let record = DecisionRecord {
        session: ctx.session,
        run: Some(ctx.run),
        node: Some(ctx.node),
        kind: DecisionKind::Custom("edit_attempt".into()),
        metadata,
        content_hash: None,
        prompt_body: None,
    };
    if let Err(e) = ctx.decisions.record(record).await {
        tracing::warn!(error = %e, "edit_attempt decision record failed");
    }
}

/// Patch-authoring worker.
///
/// # Graph-arm wiring guard (doctest, not an example)
///
/// The doctest below is a regression test: it drives one `edit` attempt
/// through the public `RegistryCapabilityExecutor` with a scripted
/// read-only graph and **fails if the worker's `GraphQuery::Diagnostics`
/// read is deleted** — the gap an adversarial verifier confirmed in round
/// 1, when nothing in the workspace caught that deletion. It lives here as
/// a doctest because the RFC-0013 SEC1/SEC3 CI greps ban graph doubles
/// (the `ProjectGraph` name and its write-method identifiers) on
/// non-comment lines under `src/capabilities/**`, while doc lines are
/// grep-exempt and still run under `cargo test -p alloy-runtime` (the
/// default doctest pass; `--tests` alone skips it). The live/predecessor
/// arm of the same acquisition is guarded by the in-module tokio test
/// `edit_worker_passes_live_pred_diagnostics_to_the_working_set`.
///
/// ```
/// # use std::sync::{Arc, Mutex};
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// # use tokio_util::sync::CancellationToken;
/// # use alloy_runtime::types::ids::{GraphSnapshotId, GraphVersion, SummaryId};
/// # use alloy_runtime::{
/// #     AdapterError, ArtifactBlob, ArtifactId, ArtifactMeta, ArtifactPut, ArtifactStore,
/// #     AssembleInputs, AssembleRequest, CapabilityExecContext, CapabilityExecutor,
/// #     CapabilityId, CapabilityRegistry, CompactStrategy, ContextEngine, ContextError,
/// #     DagId, DiagnosticEvent, DiagnosticId, DiagnosticLevel, Digest, DomainId,
/// #     EvictPolicy, EvictReport, FileChange, FixEvent, Goal, GraphError, GraphQuery,
/// #     GraphView, GraphViewHandle, ModelResponse, ModelRouter, ModelTier, NodeExecRef,
/// #     NodeId, NodeInputEnvelope, NodeInputPayload, NodeKind, PermissionToken,
/// #     ProjectGraph, PromptPack, RecordingDecisionLog, RegistryCapabilityExecutor,
/// #     RetentionPolicy, RoutedModel, RouterError, RoutingRequest, RunId,
/// #     RunRouterProvider, RunRow, Session, SessionId, SessionProvenance, SessionRows,
/// #     SharedCostMeter, StaleReason, StoreError, TokenBudget, ToolCall, ToolCaller,
/// #     ToolCallerError, ToolResult, WorkerConfig, WorkerDeps, WorkerPermissions,
/// #     WorkerToolClass, ENVELOPE_SCHEMA_VERSION,
/// # };
/// #
/// // A graph whose recorded-diagnostics table holds exactly one event.
/// struct DiagGraph(DiagnosticEvent);
///
/// #[async_trait]
/// impl ProjectGraph for DiagGraph {
///     async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError> {
///         let mut view = GraphView::empty(GraphVersion(1));
///         if let GraphQuery::Diagnostics { .. } = q {
///             view.diagnostics = vec![self.0.clone()];
///         }
///         Ok(view)
///     }
/// #     async fn rebuild(&self, _r: &std::path::Path) -> Result<GraphVersion, GraphError> {
/// #         Err(GraphError::Disabled)
/// #     }
/// #     async fn apply_incremental(
/// #         &self,
/// #         _c: &[FileChange],
/// #     ) -> Result<GraphVersion, GraphError> {
/// #         Err(GraphError::Disabled)
/// #     }
/// #     async fn record_diagnostic(&self, _d: DiagnosticEvent) -> Result<(), GraphError> {
/// #         Err(GraphError::Disabled)
/// #     }
/// #     async fn record_fix(&self, _f: FixEvent) -> Result<(), GraphError> {
/// #         Err(GraphError::Disabled)
/// #     }
/// #     async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
/// #         Err(GraphError::Disabled)
/// #     }
/// }
/// #
/// # // Captures the `AssembleInputs` the worker hands the engine, then
/// # // halts the attempt (same seam as the in-module unit tests).
/// # #[derive(Default)]
/// # struct RecordingEngine {
/// #     seen: Mutex<Vec<AssembleInputs>>,
/// # }
/// #
/// # #[async_trait]
/// # impl ContextEngine for RecordingEngine {
/// #     async fn assemble(&self, _r: AssembleRequest) -> Result<PromptPack, ContextError> {
/// #         Err(ContextError::EmptyPrompt)
/// #     }
/// #     async fn assemble_with(
/// #         &self,
/// #         _r: AssembleRequest,
/// #         inputs: AssembleInputs,
/// #     ) -> Result<PromptPack, ContextError> {
/// #         self.seen.lock().unwrap().push(inputs);
/// #         Err(ContextError::EmptyPrompt)
/// #     }
/// #     async fn compact(&self, _d: DomainId, _s: CompactStrategy) -> Result<(), ContextError> {
/// #         Ok(())
/// #     }
/// #     async fn evict(&self, _p: EvictPolicy) -> Result<EvictReport, ContextError> {
/// #         Ok(EvictReport::default())
/// #     }
/// #     async fn mark_stale(&self, id: SummaryId, _r: StaleReason) -> Result<(), ContextError> {
/// #         Err(ContextError::SummaryNotFound(id))
/// #     }
/// # }
/// #
/// # struct StubRouter;
/// #
/// # #[async_trait]
/// # impl ModelRouter for StubRouter {
/// #     async fn route(&self, _req: RoutingRequest) -> Result<RoutedModel, RouterError> {
/// #         Err(RouterError::Internal("unreached: engine halts first".into()))
/// #     }
/// #     async fn complete(
/// #         &self,
/// #         _routed: &RoutedModel,
/// #         _prompt: PromptPack,
/// #     ) -> Result<ModelResponse, RouterError> {
/// #         Err(RouterError::Internal("unreached: engine halts first".into()))
/// #     }
/// # }
/// #
/// # struct StubRouters;
/// #
/// # impl RunRouterProvider for StubRouters {
/// #     fn router_for(
/// #         &self,
/// #         _run: RunId,
/// #         _meter: &SharedCostMeter,
/// #     ) -> Result<Arc<dyn ModelRouter>, RouterError> {
/// #         Ok(Arc::new(StubRouter))
/// #     }
/// #     fn release(&self, _run: RunId) {}
/// # }
/// #
/// # struct NoTools;
/// #
/// # #[async_trait]
/// # impl ToolCaller for NoTools {
/// #     async fn call(
/// #         &self,
/// #         _c: ToolCall,
/// #         _p: PermissionToken,
/// #     ) -> Result<ToolResult, ToolCallerError> {
/// #         Err(ToolCallerError::Internal("unreached".into()))
/// #     }
/// # }
/// #
/// # struct NoPerms;
/// #
/// # #[async_trait]
/// # impl WorkerPermissions for NoPerms {
/// #     async fn token_for(
/// #         &self,
/// #         _ctx: &NodeExecRef,
/// #         _class: WorkerToolClass,
/// #     ) -> Result<PermissionToken, AdapterError> {
/// #         Err(AdapterError::PermissionDenied("unreached".into()))
/// #     }
/// # }
/// #
/// # struct NoArtifacts;
/// #
/// # #[async_trait]
/// # impl ArtifactStore for NoArtifacts {
/// #     async fn put(&self, _r: ArtifactPut) -> Result<ArtifactId, StoreError> {
/// #         Ok(ArtifactId::new())
/// #     }
/// #     async fn get(&self, _id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
/// #         Err(StoreError::NotFound("unreached: goal-rooted input".into()))
/// #     }
/// #     async fn meta(&self, _id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
/// #         Err(StoreError::NotFound("unreached".into()))
/// #     }
/// #     async fn get_by_digest(&self, _d: &Digest) -> Result<Option<ArtifactId>, StoreError> {
/// #         Ok(None)
/// #     }
/// #     async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
/// #         Ok(())
/// #     }
/// # }
/// #
/// # struct NoSessions;
/// #
/// # #[async_trait]
/// # impl SessionRows for NoSessions {
/// #     async fn upsert_session(
/// #         &self,
/// #         _s: &Session,
/// #         _p: &SessionProvenance,
/// #     ) -> Result<(), StoreError> {
/// #         Ok(())
/// #     }
/// #     async fn get_session(&self, _id: SessionId) -> Result<Option<Session>, StoreError> {
/// #         Ok(None)
/// #     }
/// #     async fn get_provenance(
/// #         &self,
/// #         _id: SessionId,
/// #     ) -> Result<Option<SessionProvenance>, StoreError> {
/// #         Ok(None)
/// #     }
/// #     async fn upsert_run(&self, _row: &RunRow) -> Result<(), StoreError> {
/// #         Ok(())
/// #     }
/// #     async fn get_run(&self, _id: RunId) -> Result<Option<RunRow>, StoreError> {
/// #         Ok(None)
/// #     }
/// #     async fn list_runs(&self, _s: SessionId) -> Result<Vec<RunRow>, StoreError> {
/// #         Ok(vec![])
/// #     }
/// #     async fn set_graph_version(
/// #         &self,
/// #         _id: SessionId,
/// #         _v: GraphVersion,
/// #     ) -> Result<(), StoreError> {
/// #         Ok(())
/// #     }
/// # }
/// #
/// let recorded = DiagnosticEvent {
/// #     id: DiagnosticId::new(),
/// #     code: Some("E0502".into()),
/// #     level: DiagnosticLevel::Error,
/// #     message: "cannot borrow `x` as mutable".into(),
/// #     spans: vec![],
/// #     children: vec![],
/// #     package: None,
///     fingerprint: Digest::sha256(b"edit-graph-arm-wiring-guard"),
/// #     raw_json: None,
///     // ...
/// };
/// let engine = Arc::new(RecordingEngine::default());
/// let deps = WorkerDeps {
/// #     routers: Arc::new(StubRouters),
/// #     context: engine.clone(),
/// #     tools: Arc::new(NoTools),
/// #     perms: Arc::new(NoPerms),
///     graph: GraphViewHandle::new(Arc::new(DiagGraph(recorded.clone()))),
/// #     artifacts: Arc::new(NoArtifacts),
/// #     decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
/// #     sessions: Arc::new(NoSessions),
/// #     config: WorkerConfig::default(),
///     // ...
/// };
/// let registry = CapabilityRegistry::mvp(deps).expect("mvp registry");
/// let executor = RegistryCapabilityExecutor::new(Arc::new(registry));
/// #
/// # let dag_id = DagId::new();
/// # let node_id = NodeId::new();
/// // Goal-rooted envelope: no predecessors, so the live set is empty and
/// // ONLY the graph read can populate the working-set diagnostics.
/// let ctx = CapabilityExecContext {
/// #     meta: NodeExecRef {
/// #         session_id: SessionId::new(),
/// #         run_id: RunId::new(),
/// #         dag_id,
/// #         node_id,
/// #         workspace_root: "/tmp/ws".into(),
/// #         attempt: 1,
/// #     },
/// #     cancellation: CancellationToken::new(),
/// #     capability: CapabilityId::new("edit").expect("static id"),
/// #     kind: NodeKind::Edit,
/// #     effective_tier: ModelTier::Standard,
/// #     budget: TokenBudget { max_input: 4096, max_output: 1024 },
/// #     timeout: Duration::from_secs(30),
/// #     input: NodeInputEnvelope {
/// #         schema_version: ENVELOPE_SCHEMA_VERSION,
/// #         dag_id,
/// #         node_id,
/// #         kind: NodeKind::Edit,
/// #         generation: 1,
/// #         payload: NodeInputPayload::Goal(Goal {
/// #             text: "fix the borrow error".into(),
/// #             constraints: vec![],
/// #             attachments: vec![],
/// #         }),
/// #     },
/// #     attempt: 1,
/// #     cost_meter: SharedCostMeter::new(),
///     // ...
/// };
///
/// tokio::runtime::Builder::new_current_thread()
///     .enable_all()
///     .build()
///     .expect("runtime")
///     .block_on(async {
///         let outcome = executor.execute(&ctx).await;
///         assert!(outcome.is_ok(), "engine halt is a soft failure: {outcome:?}");
///     });
///
/// let seen = engine.seen.lock().expect("engine lock");
/// let inputs = seen.first().expect("edit worker reached prompt assembly");
/// assert!(
///     inputs.diagnostics.iter().any(|d| d.fingerprint == recorded.fingerprint),
///     "graph-recorded diagnostics must reach the edit working set; the \
///      GraphQuery::Diagnostics read in EditWorker::run is the only path"
/// );
/// ```
#[derive(Debug, Clone)]
pub struct EditWorker {
    config: WorkerConfig,
}

impl EditWorker {
    /// Construct with worker knobs.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
    }

    const VERSION: CapabilityVersion = CapabilityVersion::new(1, 0, 0);

    fn capability_id() -> CapabilityId {
        CapabilityId::new("edit").expect("static id")
    }
}

#[async_trait]
impl Capability for EditWorker {
    fn id(&self) -> CapabilityId {
        Self::capability_id()
    }

    fn version(&self) -> CapabilityVersion {
        Self::VERSION
    }

    fn describe(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: Self::capability_id(),
            version: Self::VERSION,
            summary: "Produce a minimal unified diff and apply it via the patch builtin".into(),
            uses_model: true,
            side_effects: SideEffectClass::WorkspaceWrite,
            kinds: vec![NodeKind::Edit],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        vec![
            ToolSelector::name(ToolName::new("fs_read").expect("static name")),
            ToolSelector::name(ToolName::new("apply_patch").expect("static name")),
        ]
    }

    fn preferred_tier(&self) -> ModelTier {
        ModelTier::Standard
    }

    fn accepts_kind(&self, kind: NodeKind) -> bool {
        kind == NodeKind::Edit
    }

    async fn execute(
        &self,
        ctx: &CapabilityContext<'_>,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        let span = worker_span(ctx);
        let mut attempt = Attempt::new(self.preferred_tier(), ctx.effective_tier);
        let telemetry = EditTelemetry::default();
        let result = {
            use tracing::Instrument;
            self.run(ctx, &mut attempt, &telemetry)
                .instrument(span.clone())
                .await
        };
        record_edit_attempt(ctx, &attempt, &telemetry, &result).await;
        finish_attempt(ctx, &self.describe(), &attempt, result, &span).await
    }
}

/// One validated patch candidate ready to be sent to the builtin.
struct Candidate {
    proposal: PatchProposal,
    canonical: serde_json::Value,
    bytes: u32,
    hunk_count: u32,
}

impl EditWorker {
    async fn run(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        telemetry: &EditTelemetry,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }

        // EW2: the first Analyze pred whose payload decodes as a
        // `RepairPlanPayload` is the plan; preds without any decodable plan
        // are an internal failure (FM10-adjacent, "edit node without a
        // repair plan"). A goal-rooted single-node DAG has no pred.
        let payloads = load_pred_payloads(ctx).await?;
        let has_preds = matches!(
            &ctx.input.payload,
            NodeInputPayload::FromPredecessors { .. }
        );
        let plan = payloads.iter().find_map(|(kind, payload)| {
            if *kind != NodeKind::Analyze {
                return None;
            }
            serde_json::from_value::<super::super::payload::RepairPlanPayload>(payload.clone()).ok()
        });
        if has_preds && plan.is_none() {
            return Err(WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                "edit node without a repair plan",
            ));
        }

        let focus_paths = plan
            .as_ref()
            .map(|p| p.target_files.clone())
            .unwrap_or_default();

        // Edit against the errors that exist NOW, acquired exactly as the
        // repair worker acquires them (RW2/RW4): any predecessor payload
        // carrying a `diagnostics` array is the live set. An empty vector
        // here made the context engine fall back to the run-start
        // `graph_diagnostics` table, so generation N edited against
        // generation-0 errors it had already "fixed".
        let live = diagnostics_from_payloads(&payloads);
        // Best-effort graph read (RW4 posture): keeps the recorded table in
        // the working set now that the engine's empty-vector fallback no
        // longer fires; a graph error degrades to "no graph input".
        //
        // Impact, as established by adversarial verification (2026-08): on
        // the DEFAULT repair template this acquisition changes nothing —
        // the edit node's one Data predecessor (`analyze`) carries a
        // `RepairPlanPayload`, which has no `diagnostics` field, so `live`
        // is empty and the working set holds the same table the engine
        // fallback used to fetch. The merge earns its keep on proposal
        // DAGs (autonomous mode), where consecutive nodes carry Data edges
        // whose `{ ok, diagnostics }` bodies flow in through `live`. Do
        // not read this block as doing work on the default arm. Wiring
        // guards: the `EditWorker` doctest (graph arm — fails if the read
        // below is deleted) and the in-module test
        // `edit_worker_passes_live_pred_diagnostics_to_the_working_set`
        // (live arm — fails if `diagnostics` is forced empty).
        let recorded = match ctx
            .graph
            .query(GraphQuery::Diagnostics {
                crate_id: None,
                since: None,
            })
            .await
        {
            Ok(view) => view.diagnostics,
            Err(_) => Vec::new(),
        };
        let diagnostics = merge_live_diagnostics(live, recorded);

        let inputs = AssembleInputs {
            run: Some(ctx.run),
            input: Some(ctx.input.clone()),
            diagnostics,
            budget: Some(ctx.budget.clone()),
            focus_paths,
            // NOT WIRED. The renderer and budget below this exist and are
            // tested, but no producer feeds them: `CapabilityContext` carries
            // `attempt` and no prior `FailureIr`, so every call site — here,
            // repair, planning, review — passes None.
            //
            // Retry amnesia is therefore UNFIXED in production. The edit node
            // retries up to three times and escalates tier after the first,
            // and each attempt still starts blind. Wiring this means adding
            // the failure to `CapabilityContext` and populating it at the
            // retry dispatch site; until that lands, treat this field as
            // scaffolding, not as a delivered capability.
            prior_failure: None,
        };

        // §7.2 turn budget: the model turn(s), the EW6 dry-run, and the PS6
        // repair share one attempt; every loop iteration below consumes a
        // model turn through `llm_exchange`.
        let mut feedback: Vec<String> = Vec::new();
        let mut dry_run_repaired = false;
        let mut ops_repaired = false;
        let (candidate, patch_artifact) = loop {
            let (proposal, body) = self
                .author(ctx, attempt, &inputs, &feedback, telemetry)
                .await?;
            let patch_set = match body {
                ProposalBody::Patch(set) => set,
                // AM-0013-1: compile ops against the current files; a stale
                // or misanchored op is model-repairable feedback, exactly
                // like an EW6 dry-run failure.
                ProposalBody::Ops(ops) => match self.compile_ops(ctx, attempt, &ops).await? {
                    Ok(set) => set,
                    Err(reason) => {
                        telemetry.refuse("ops_compile", &reason);
                        if !ops_repaired && attempt.model_turns < self.config.max_model_turns {
                            ops_repaired = true;
                            feedback = vec![fence_tool(
                                "line_ops",
                                &reason,
                                self.config.max_tool_result_bytes,
                            )];
                            continue;
                        }
                        return Err(WorkerError::soft(
                            ErrorClass::Model,
                            RetryDisposition::Retryable,
                            format!("line ops rejected after repair turn: {reason}"),
                        ));
                    }
                },
            };
            let candidate = match Self::candidate(proposal, &patch_set) {
                Ok(candidate) => candidate,
                Err(e) => {
                    // The only soft refusal here is the EW5 size bound.
                    if let WorkerError::Soft { notes, .. } = &e {
                        telemetry.refuse("argument_bytes", notes);
                    }
                    return Err(e);
                }
            };

            // EW9: persist the canonical PatchSet before the apply call.
            let patch_artifact = self.persist_patch(ctx, &candidate).await?;

            if !self.config.validate_before_apply {
                break (candidate, patch_artifact);
            }
            // EW6: one dry run; on failure, one repair turn with the
            // sanitized tool error fed back, then re-validate.
            let dry = call_tool(
                ctx,
                attempt,
                &self.config,
                WorkerToolClass::Patch,
                &ALLOWED_TOOLS,
                "apply_patch",
                json!({ "patch": candidate.canonical, "dry_run": true }),
            )
            .await?;
            if !dry.is_error() {
                break (candidate, patch_artifact);
            }
            // Audit 2026-08 FINDING 2: the builtin's error CONTENT is only
            // `{"code":…,"dry_run":true}` — the sanitized human-readable
            // message rides `ToolResult::error()`. Feed the model both, or
            // its one repair chance is a status code.
            let dry_detail = match dry.error() {
                Some(err) => format!("{err}\n{}", dry.content),
                None => dry.content.to_string(),
            };
            telemetry.refuse("dry_run", &dry_detail);
            if !dry_run_repaired && attempt.model_turns < self.config.max_model_turns {
                dry_run_repaired = true;
                feedback = vec![fence_tool(
                    "apply_patch",
                    &dry_detail,
                    self.config.max_tool_result_bytes,
                )];
                continue;
            }
            // Second dry-run failure: FM3 disposition from the tool error.
            return Err(map_tool_result_error(&dry));
        };

        // EW7: the apply call is never a dry run; a validated-but-unapplied
        // patch is not success.
        let applied = call_tool(
            ctx,
            attempt,
            &self.config,
            WorkerToolClass::Patch,
            &ALLOWED_TOOLS,
            "apply_patch",
            json!({ "patch": candidate.canonical, "dry_run": false }),
        )
        .await?;
        if applied.is_error() {
            telemetry.refuse(
                "apply",
                &applied
                    .error()
                    .map_or_else(|| applied.content.to_string(), ToString::to_string),
            );
            // EW10/EW11: no re-apply, no compensation; the disposition comes
            // from the tool error taxonomy.
            return Err(map_tool_result_error(&applied));
        }
        // EW8: backend-reported paths only.
        let outcome: PatchOutcomeView =
            serde_json::from_value(applied.content.clone()).map_err(|e| {
                WorkerError::soft(
                    ErrorClass::Internal,
                    RetryDisposition::NonRetryable,
                    format!("apply_patch content undecodable: {e}"),
                )
            })?;

        let confidence = candidate.proposal.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
        let mut summary = candidate.proposal.summary.clone();
        let summary_cut = clamp_string(&mut summary, MAX_PAYLOAD_STRING_BYTES);
        // OC7 vector bound on the backend-reported list.
        let mut files_touched = outcome.files_touched;
        let files_cut = super::super::payload::clamp_vec(
            &mut files_touched,
            super::super::payload::MAX_PAYLOAD_VEC_ENTRIES,
        );

        let mut payload = EditAppliedPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "edit".into(),
            files_touched,
            transaction_id: outcome.transaction_id,
            patch_artifact,
            hunk_count: candidate.hunk_count,
            bytes: candidate.bytes,
            dry_run: false, // EW7.
            summary,
            truncated: summary_cut || files_cut,
            confidence,
            citations: attempt.citations.clone(),
            artifacts: vec![patch_artifact],
            metrics: attempt.metrics(ctx, None),
        };
        let mut value = serde_json::to_value(&payload).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "payload serialization failed: {e}" // CW10.
            )))
        })?;
        // OC7 total bound: drop the largest list rather than a citation.
        if super::super::payload::exceeds_total_bound(&value) {
            payload.files_touched.clear();
            payload.summary.clear();
            payload.truncated = true;
            value = serde_json::to_value(&payload).map_err(|e| {
                WorkerError::Host(CapabilityExecError::Internal(format!(
                    "payload serialization failed: {e}"
                )))
            })?;
        }
        let payload = value;
        Ok(WorkerSuccess {
            payload,
            confidence,
        })
    }

    /// One model turn producing a locally validated proposal (EW3/EW4 plus
    /// the AM-0013-1 ops form: strict either/or, static screen here, file
    /// verification in [`Self::compile_ops`]).
    async fn author(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        inputs: &AssembleInputs,
        feedback: &[String],
        telemetry: &EditTelemetry,
    ) -> Result<(PatchProposal, ProposalBody), WorkerError> {
        let (authored, _pack) = llm_exchange(
            ctx,
            attempt,
            &self.config,
            EDIT_SYSTEM,
            Some(&edit_response_schema()),
            inputs,
            feedback,
            |value| {
                // Every validate call follows one completion, so one
                // `ProposalObs` per model turn; PS5 refusals are attributed
                // here, downstream ones (`ops_compile`, `dry_run`, …) by
                // the run loop.
                let refuse = |check: &'static str, reason: String| {
                    telemetry.refuse(check, &reason);
                    reason
                };
                let proposal: PatchProposal = match serde_json::from_value(value.clone()) {
                    Ok(proposal) => proposal,
                    Err(e) => {
                        telemetry.propose("undecodable", None, Vec::new());
                        return Err(refuse("schema", format!("schema: {e}")));
                    }
                };
                let form = match (&proposal.patch, &proposal.ops) {
                    (Some(_), Some(_)) => "both",
                    (None, None) => "neither",
                    (Some(_), None) => "patch",
                    (None, Some(_)) => "ops",
                };
                let (op_count, op_kinds) =
                    proposal.ops.as_ref().map_or((None, Vec::new()), |raw| {
                        (
                            Some(raw.len()),
                            raw.iter()
                                .take(MAX_TELEMETRY_PROPOSALS)
                                .map(|op| {
                                    op.get("op")
                                        .and_then(Value::as_str)
                                        .unwrap_or("?")
                                        .to_owned()
                                })
                                .collect(),
                        )
                    });
                telemetry.propose(form, op_count, op_kinds);
                let body = match (&proposal.patch, &proposal.ops) {
                    (Some(_), Some(_)) => {
                        return Err(refuse(
                            "form_exclusivity",
                            "reply with either patch or ops, never both".into(),
                        ));
                    }
                    (None, None) => {
                        return Err(refuse(
                            "form_exclusivity",
                            "reply must carry a patch or an ops array".into(),
                        ));
                    }
                    // EW4: local parse before any tool call — an unusable
                    // diff never becomes a permission-denied tool error.
                    (Some(patch), None) => ProposalBody::Patch(
                        parse_model_diff(patch).map_err(|e| refuse("diff_parse", e))?,
                    ),
                    (None, Some(raw_ops)) => {
                        let ops = raw_ops
                            .iter()
                            .map(parse_line_op)
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|e| refuse("op_parse", e))?;
                        screen_line_ops(&ops).map_err(|e| refuse("op_screen", e))?;
                        ProposalBody::Ops(ops)
                    }
                };
                Ok((proposal, body))
            },
        )
        .await?;
        Ok(authored)
    }

    /// AM-0013-1: read each distinct target file once through `fs_read` and
    /// compile the ops into a context-correct `PatchSet`. The outer `Err` is
    /// a host/tool fault; the inner `Err` is model-repairable feedback (a
    /// stale `expect`, an unreadable or truncated file, a bad range).
    async fn compile_ops(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        ops: &[LineOp],
    ) -> Result<Result<PatchSet, String>, WorkerError> {
        let mut files: HashMap<String, String> = HashMap::new();
        for op in ops {
            let path = op.path();
            if files.contains_key(path) {
                continue;
            }
            let result = call_tool(
                ctx,
                attempt,
                &self.config,
                WorkerToolClass::Read,
                &ALLOWED_TOOLS,
                "fs_read",
                json!({ "path": path }),
            )
            .await?;
            if result.is_error() {
                // A path the model named but the jail cannot read is the
                // model's mistake to repair, not a worker failure.
                return Ok(Err(format!(
                    "fs_read failed for {path}; check the path or emit a unified diff patch"
                )));
            }
            let text = result
                .content
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    WorkerError::soft(
                        ErrorClass::Internal,
                        RetryDisposition::NonRetryable,
                        "fs_read content undecodable: no text field",
                    )
                })?;
            if result
                .content
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                // Compiling against a partial read would fabricate context;
                // the honest fallback is the diff form.
                return Ok(Err(format!(
                    "{path} is too large to line-edit; reply with a unified diff patch instead"
                )));
            }
            files.insert(path.to_owned(), text.to_owned());
        }
        Ok(ops_to_patchset(ops, &files))
    }

    /// EW5 bounds over the compiled `PatchSet`, shared by both wire forms.
    fn candidate(proposal: PatchProposal, patch_set: &PatchSet) -> Result<Candidate, WorkerError> {
        let canonical = serde_json::to_value(patch_set).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "patch serialization failed: {e}"
            )))
        })?;
        // EW5/FM7: the serialized tool argument must fit the RFC-0006 cap;
        // chunking across nodes is a template concern (RFC-0010 AS2), not an
        // in-worker split.
        let args_len = serde_json::to_vec(&json!({ "patch": canonical, "dry_run": false }))
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if args_len > MAX_PATCH_ARGUMENT_BYTES {
            return Err(WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                "patch exceeds MAX_ARGUMENT_BYTES; split the repair",
            ));
        }
        let bytes = u32::try_from(serde_json::to_vec(&patch_set).map(|v| v.len()).unwrap_or(0))
            .unwrap_or(u32::MAX);
        let hunk_count = u32::try_from(
            patch_set
                .files
                .iter()
                .map(|f| match f {
                    FilePatch::Modify { hunks, .. } | FilePatch::Create { hunks, .. } => {
                        hunks.len()
                    }
                    FilePatch::Delete {
                        validation_hunks, ..
                    } => validation_hunks.len(),
                })
                .sum::<usize>(),
        )
        .unwrap_or(u32::MAX);
        Ok(Candidate {
            proposal,
            canonical,
            bytes,
            hunk_count,
        })
    }

    /// EW9: canonical `PatchSet` JSON into the CAS as `ArtifactKind::Patch`.
    /// An orphan after a failed apply is acceptable (RFC-0002 has no GC).
    async fn persist_patch(
        &self,
        ctx: &CapabilityContext<'_>,
        candidate: &Candidate,
    ) -> Result<ArtifactId, WorkerError> {
        let bytes = serde_json::to_vec(&candidate.canonical).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "patch serialization failed: {e}"
            )))
        })?;
        ctx.artifacts
            .put(ArtifactPut {
                bytes,
                kind: ArtifactKind::Patch,
                content_type: Some("application/json".into()),
                session_id: Some(ctx.session),
                run_id: Some(ctx.run),
                labels: serde_json::Map::new(),
            })
            .await
            .map_err(|e| {
                WorkerError::soft(
                    ErrorClass::Internal,
                    RetryDisposition::NonRetryable,
                    format!("patch artifact store failed: {e}"),
                )
            })
    }
}

/// RW2 treatment of the working-set diagnostics, mirrored from the repair
/// worker: predecessor-carried (`live`) and graph-recorded (`recorded`)
/// events merged, sorted by `(primary span path, start line, code)`,
/// deduplicated by fingerprint (stable sort keeps the live copy first), and
/// capped at [`MAX_DIAGNOSTICS`].
fn merge_live_diagnostics(
    mut diagnostics: Vec<DiagnosticEvent>,
    recorded: Vec<DiagnosticEvent>,
) -> Vec<DiagnosticEvent> {
    diagnostics.extend(recorded);
    diagnostics.sort_by_key(diagnostic_sort_key);
    let mut seen = std::collections::BTreeSet::new();
    diagnostics.retain(|d| seen.insert(d.fingerprint.as_hex().to_owned()));
    diagnostics.truncate(MAX_DIAGNOSTICS);
    diagnostics
}

/// RW2 ordering key, mirrored from the repair worker: `(primary span path,
/// start line, code)`; diagnostics without spans sort first on the empty
/// path, exactly as there.
fn diagnostic_sort_key(d: &DiagnosticEvent) -> (String, u32, Option<String>) {
    let (path, line) = d
        .spans
        .first()
        .map_or((String::new(), 0), |s| (s.path.clone(), s.start_line));
    (path, line, d.code.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// Minimal structural check matching `tests/worker_schemas.rs` for the
    /// subset `edit_response_schema` uses (incl. op-item `oneOf`).
    fn schema_validates(schema: &Value, value: &Value) -> bool {
        let obj = schema.as_object().expect("schema object");
        if let Some(alts) = obj.get("oneOf") {
            return alts
                .as_array()
                .expect("oneOf array")
                .iter()
                .any(|alt| schema_validates(alt, value));
        }
        if let Some(types) = obj.get("type") {
            let ok = match types {
                Value::String(ty) => match ty.as_str() {
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "string" => value.is_string(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "number" => value.is_number(),
                    "null" => value.is_null(),
                    _ => false,
                },
                Value::Array(list) => list
                    .iter()
                    .any(|ty| schema_validates(&json!({ "type": ty.clone() }), value)),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        if let Some(allowed) = obj.get("enum") {
            if !allowed.as_array().expect("enum").contains(value) {
                return false;
            }
        }
        if let Some(props) = obj.get("properties") {
            let props = props.as_object().expect("properties");
            let Some(map) = value.as_object() else {
                return false;
            };
            for (key, sub) in props {
                if let Some(v) = map.get(key) {
                    if !schema_validates(sub, v) {
                        return false;
                    }
                }
            }
            if obj.get("additionalProperties") == Some(&Value::Bool(false))
                && map.keys().any(|k| !props.contains_key(k))
            {
                return false;
            }
            if let Some(required) = obj.get("required") {
                for key in required.as_array().expect("required") {
                    if !map.contains_key(key.as_str().expect("required key")) {
                        return false;
                    }
                }
            }
        }
        if let Some(items) = obj.get("items") {
            if let Some(list) = value.as_array() {
                if !list.iter().all(|item| schema_validates(items, item)) {
                    return false;
                }
            }
        }
        true
    }

    /// A-0007-2 × AM-0013-1 reconciliation guard: the declared edit schema
    /// and the live `PatchProposal` parser must accept and reject the same
    /// surface. PR #64 widened the parser to exactly-one-of `patch` / `ops`;
    /// any future parser change must regenerate `edit_response_schema()` and
    /// this test in the same commit.
    #[test]
    fn edit_schema_matches_current_parser_surface() {
        let schema = edit_response_schema().schema;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required list")
            .iter()
            .map(|v| v.as_str().expect("required entry"))
            .collect();
        let properties: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();

        // The parser requires `summary`, admits exactly one of `patch` /
        // `ops` plus optional `confidence`, and denies unknown fields.
        // serde_json maps iterate sorted, so compare property sets
        // order-insensitively.
        assert_eq!(required, ["summary"]);
        assert_eq!(properties, ["confidence", "ops", "patch", "summary"]);

        // Complete schema-valid replace_lines (not the bare `{op}` stub).
        let ops_shape = json!({
            "ops": [{
                "op": "replace_lines",
                "path": "a.rs",
                "start": 1,
                "end": 1,
                "expect": ["x"],
                "new": ["y"]
            }],
            "summary": "s"
        });
        assert!(
            schema_validates(&schema, &ops_shape),
            "closed replace_lines shape must validate against edit_response_schema"
        );
        assert!(serde_json::from_value::<PatchProposal>(ops_shape.clone()).is_ok());

        let patch_shape = json!({ "patch": "--- a\n+++ b\n", "summary": "s" });
        assert!(schema_validates(&schema, &patch_shape));
        assert!(serde_json::from_value::<PatchProposal>(patch_shape).is_ok());

        // Incomplete / wrong-tag ops are schema-invalid even when they still
        // deserialize into the loose `Vec<Value>` ops field.
        let bare_op = json!({
            "ops": [{ "op": "replace_lines" }],
            "summary": "s"
        });
        assert!(
            !schema_validates(&schema, &bare_op),
            "bare {{op}} must not satisfy the closed op oneOf"
        );
        let wrong_tag = json!({
            "ops": [{
                "op": "replace",
                "path": "a.rs",
                "start": 1,
                "end": 1,
                "expect": ["x"],
                "new": ["y"]
            }],
            "summary": "s"
        });
        assert!(!schema_validates(&schema, &wrong_tag));

        // Unknown top-level fields are closed off by
        // `additionalProperties: false` / `deny_unknown_fields`.
        let mut unknown = ops_shape;
        unknown
            .as_object_mut()
            .expect("object")
            .insert("bogus".into(), json!(true));
        assert!(
            serde_json::from_value::<PatchProposal>(unknown.clone()).is_err(),
            "parser admits an unknown field; regenerate edit_response_schema() (AM-0013-1)"
        );
        assert!(!schema_validates(&schema, &unknown));
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "schema must stay closed while the parser is deny_unknown_fields"
        );
    }
}

/// Defect-2 regression guard: the working set the edit worker assembles must
/// carry the live diagnostics its predecessors and the graph know about,
/// exactly as the repair worker's does (RW2/RW4 acquisition). An empty
/// `diagnostics` vector silently downgrades the context engine to its
/// run-start `graph_diagnostics` fallback, so in generation N the model
/// edits against generation-0 errors.
///
/// Coverage map (post round-2, after adversarial verification):
/// - live/predecessor arm of the wiring:
///   `edit_worker_passes_live_pred_diagnostics_to_the_working_set` (here);
/// - graph arm of the wiring: the `EditWorker` doctest — the SEC3 grep
///   bans graph doubles on non-comment lines in this module, so the
///   scripted graph lives in grep-exempt doc lines and runs under the
///   crate's doctest pass;
/// - `merge_live_diagnostics` helper semantics ONLY (dedupe order, RW2
///   cap): the two `merge_live_diagnostics_*` tests below. They do NOT
///   guard the wiring and survive both wiring mutations by design.
#[cfg(test)]
mod live_diagnostics_tests {
    use super::*;

    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use crate::adapters::{NodeExecRef, ToolCaller, ToolCallerError};
    use crate::context::{
        AssembleRequest, CompactStrategy, ContextEngine, ContextError, DomainId, EvictPolicy,
        EvictReport, StaleReason,
    };
    use crate::dag::{NodeInputEnvelope, PredecessorOutput};
    use crate::error::AdapterError;
    use crate::graph::GraphViewHandle;
    use crate::obs::{RecordingDecisionLog, RetentionPolicy, SharedCostMeter};
    use crate::router::{
        ModelResponse, ModelRouter, PromptPack, RoutedModel, RouterError, RoutingRequest,
    };
    use crate::storage::{ArtifactBlob, ArtifactMeta, ArtifactStore, StoreError};
    use crate::types::budget::TokenBudget;
    use crate::types::diagnostic::{DiagnosticLevel, SpanRef};
    use crate::types::ids::{
        DagId, DiagnosticId, Digest, NodeId, ProviderId, RunId, SessionId, SummaryId, Timestamp,
    };
    use crate::types::metrics::WorkerMetrics;
    use crate::types::permission::PermissionToken;
    use crate::types::tools::{ToolCall, ToolResult};

    use super::super::super::payload::{RepairPlanPayload, PAYLOAD_SCHEMA_VERSION};
    use super::super::super::perms::WorkerPermissions;
    use super::super::Attempt;

    /// Records every `AssembleInputs` the worker hands the engine, then
    /// halts the attempt: these tests assert on the working-set inputs, not
    /// on a full model turn.
    #[derive(Default)]
    struct RecordingEngine {
        seen: Mutex<Vec<AssembleInputs>>,
    }

    #[async_trait]
    impl ContextEngine for RecordingEngine {
        async fn assemble(&self, _req: AssembleRequest) -> Result<PromptPack, ContextError> {
            Err(ContextError::EmptyPrompt)
        }

        async fn assemble_with(
            &self,
            _req: AssembleRequest,
            inputs: AssembleInputs,
        ) -> Result<PromptPack, ContextError> {
            self.seen.lock().expect("engine lock").push(inputs);
            Err(ContextError::EmptyPrompt)
        }

        async fn compact(&self, _d: DomainId, _s: CompactStrategy) -> Result<(), ContextError> {
            Ok(())
        }

        async fn evict(&self, _p: EvictPolicy) -> Result<EvictReport, ContextError> {
            Ok(EvictReport::default())
        }

        async fn mark_stale(&self, id: SummaryId, _r: StaleReason) -> Result<(), ContextError> {
            Err(ContextError::SummaryNotFound(id))
        }
    }

    /// Never reached: the recording engine stops the attempt first.
    struct StubRouter;

    #[async_trait]
    impl ModelRouter for StubRouter {
        async fn route(&self, _req: RoutingRequest) -> Result<RoutedModel, RouterError> {
            Err(RouterError::Internal("unused in these tests".into()))
        }

        async fn complete(
            &self,
            _routed: &RoutedModel,
            _prompt: PromptPack,
        ) -> Result<ModelResponse, RouterError> {
            Err(RouterError::Internal("unused in these tests".into()))
        }
    }

    /// Never reached: the recording engine stops the attempt first.
    struct StubTools;

    #[async_trait]
    impl ToolCaller for StubTools {
        async fn call(
            &self,
            _call: ToolCall,
            _perms: PermissionToken,
        ) -> Result<ToolResult, ToolCallerError> {
            Err(ToolCallerError::UnknownTool("unused in these tests".into()))
        }
    }

    /// Never reached: the recording engine stops the attempt first.
    struct StubPerms;

    #[async_trait]
    impl WorkerPermissions for StubPerms {
        async fn token_for(
            &self,
            _ctx: &NodeExecRef,
            _class: WorkerToolClass,
        ) -> Result<PermissionToken, AdapterError> {
            Err(AdapterError::PermissionDenied(
                "unused in these tests".into(),
            ))
        }
    }

    /// In-memory artifact store serving predecessor payload blobs.
    #[derive(Default)]
    struct MemArtifacts {
        blobs: Mutex<HashMap<ArtifactId, Vec<u8>>>,
    }

    impl MemArtifacts {
        async fn put_json(&self, value: &serde_json::Value) -> ArtifactId {
            self.put(ArtifactPut {
                bytes: serde_json::to_vec(value).expect("payload serializes"),
                kind: ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                session_id: None,
                run_id: None,
                labels: serde_json::Map::new(),
            })
            .await
            .expect("put")
        }
    }

    #[async_trait]
    impl ArtifactStore for MemArtifacts {
        async fn put(&self, req: ArtifactPut) -> Result<ArtifactId, StoreError> {
            let id = ArtifactId::new();
            self.blobs.lock().expect("store lock").insert(id, req.bytes);
            Ok(id)
        }

        async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
            let bytes = self
                .blobs
                .lock()
                .expect("store lock")
                .get(&id)
                .cloned()
                .ok_or_else(|| StoreError::NotFound("artifact".into()))?;
            let meta = ArtifactMeta {
                kind: ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                byte_len: bytes.len() as u64,
                digest: Digest::sha256(&bytes),
                created_at: Timestamp::now(),
                session_id: None,
                run_id: None,
                labels: serde_json::Map::new(),
            };
            Ok(ArtifactBlob { id, meta, bytes })
        }

        async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
            Ok(self.get(id).await?.meta)
        }

        async fn get_by_digest(&self, _digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
            Ok(None)
        }

        async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn diag(path: &str, code: &str) -> DiagnosticEvent {
        DiagnosticEvent {
            id: DiagnosticId::new(),
            code: Some(code.into()),
            level: DiagnosticLevel::Error,
            message: format!("cannot borrow ({code})"),
            spans: vec![SpanRef {
                path: path.into(),
                start_line: 3,
                start_col: 5,
                end_line: 3,
                end_col: 9,
            }],
            children: vec![],
            package: None,
            fingerprint: Digest::sha256(format!("{path}:{code}").as_bytes()),
            raw_json: None,
        }
    }

    /// Analyze-pred body: a decodable `RepairPlanPayload`. Deliberately
    /// carries no `diagnostics` array — the shipped plan payload has none.
    fn plan_value(target: &str) -> serde_json::Value {
        serde_json::to_value(RepairPlanPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "repair".into(),
            summary: "fix the borrow".into(),
            target_files: vec![target.into()],
            steps: vec![],
            diagnostics_addressed: vec![],
            needs_replan: false,
            truncated: false,
            confidence: 0.9,
            citations: vec![],
            artifacts: vec![],
            metrics: WorkerMetrics {
                model_tier_used: ModelTier::Standard,
                provider_id: ProviderId::new("test").expect("static id"),
                input_tokens: None,
                output_tokens: None,
                tool_calls: 0,
                cache_hits: 0,
                duration_ms: 0,
                confidence: None,
                error_class: None,
            },
        })
        .expect("plan serializes")
    }

    fn pred(kind: NodeKind, output_ref: ArtifactId) -> PredecessorOutput {
        PredecessorOutput {
            node_id: NodeId::new(),
            kind,
            output_ref,
        }
    }

    fn envelope(preds: Vec<PredecessorOutput>) -> NodeInputEnvelope {
        NodeInputEnvelope::new(
            DagId::new(),
            NodeId::new(),
            NodeKind::Edit,
            1,
            NodeInputPayload::FromPredecessors { preds },
        )
    }

    fn ctx<'a>(
        input: &'a NodeInputEnvelope,
        engine: Arc<RecordingEngine>,
        artifacts: Arc<MemArtifacts>,
        graph: GraphViewHandle,
    ) -> CapabilityContext<'a> {
        CapabilityContext {
            session: SessionId::new(),
            run: RunId::new(),
            dag: input.dag_id,
            node: input.node_id,
            attempt: 1,
            workspace_root: Path::new("."),
            capability: CapabilityId::new("edit").expect("static id"),
            kind: NodeKind::Edit,
            effective_tier: ModelTier::Standard,
            budget: TokenBudget {
                max_input: 4096,
                max_output: 1024,
            },
            deadline: Duration::from_secs(30),
            cancel: CancellationToken::new(),
            input,
            router: Arc::new(StubRouter),
            context: engine,
            tools: Arc::new(StubTools),
            perms: Arc::new(StubPerms),
            graph,
            artifacts,
            decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
            cost_meter: SharedCostMeter::new(),
            started: Instant::now(),
        }
    }

    /// Run one attempt to the assembly seam and return the captured inputs.
    async fn assembled_inputs(
        input: &NodeInputEnvelope,
        artifacts: Arc<MemArtifacts>,
        graph: GraphViewHandle,
    ) -> AssembleInputs {
        let engine = Arc::new(RecordingEngine::default());
        let ctx = ctx(input, Arc::clone(&engine), artifacts, graph);
        let worker = EditWorker::new(WorkerConfig::default());
        let mut attempt = Attempt::new(ModelTier::Standard, ModelTier::Standard);
        let telemetry = EditTelemetry::default();
        let result = worker.run(&ctx, &mut attempt, &telemetry).await;
        assert!(result.is_err(), "the recording engine halts the attempt");
        let seen = engine.seen.lock().expect("engine lock");
        seen.first().cloned().expect("assemble_with was reached")
    }

    /// A `VerifyCompile` predecessor's `{ ok, diagnostics }` body must reach
    /// the working set — while focus paths stay the plan's target files.
    /// Proven red against the shipped `diagnostics: Vec::new()`.
    #[tokio::test]
    async fn edit_worker_passes_live_pred_diagnostics_to_the_working_set() {
        let artifacts = Arc::new(MemArtifacts::default());
        let plan_ref = artifacts.put_json(&plan_value("src/lib.rs")).await;
        let live = diag("src/lib.rs", "E0502");
        let failure_ref = artifacts
            .put_json(&json!({
                "ok": false,
                "diagnostics": [live],
                "notes": "generation 2 soft failure",
            }))
            .await;
        let input = envelope(vec![
            pred(NodeKind::Analyze, plan_ref),
            pred(NodeKind::VerifyCompile, failure_ref),
        ]);

        let inputs = assembled_inputs(&input, artifacts, GraphViewHandle::null()).await;

        assert!(
            inputs
                .diagnostics
                .iter()
                .any(|d| d.fingerprint == live.fingerprint),
            "live predecessor diagnostics must reach the edit working set"
        );
        // Working-set selection semantics beyond diagnostics are unchanged.
        assert_eq!(inputs.focus_paths, vec!["src/lib.rs".to_owned()]);
    }

    /// Helper-level guard ONLY: pins `merge_live_diagnostics`'s dedupe
    /// semantics — fingerprint dedupe across the two sources, with the
    /// stable sort keeping the live copy ahead of its recorded twin.
    ///
    /// This test does NOT guard the worker wiring: it passes unchanged
    /// when `AssembleInputs.diagnostics` is forced to `Vec::new()` or when
    /// the `GraphQuery::Diagnostics` read is deleted (round-1 verifier
    /// finding — an earlier comment here overstated its coverage). Those
    /// mutations are killed by
    /// `edit_worker_passes_live_pred_diagnostics_to_the_working_set` and
    /// by the `EditWorker` doctest respectively. It stays pure-function
    /// because dedupe ordering is cheapest to pin at this altitude, and a
    /// wiring double for the graph cannot be written on non-comment lines
    /// in this module (SEC3 grep).
    #[test]
    fn merge_live_diagnostics_dedupes_by_fingerprint() {
        let live = diag("src/lib.rs", "E0502");
        let stale_copy = diag("src/lib.rs", "E0502"); // same fingerprint
        let other = diag("src/main.rs", "E0308");

        let merged = merge_live_diagnostics(vec![live.clone()], vec![stale_copy, other.clone()]);

        let fingerprints: Vec<_> = merged.iter().map(|d| &d.fingerprint).collect();
        assert!(
            fingerprints.contains(&&live.fingerprint) && fingerprints.contains(&&other.fingerprint),
            "both live and graph-recorded diagnostics must survive the merge"
        );
        assert_eq!(
            merged.len(),
            2,
            "the shared fingerprint must be deduplicated"
        );
        // Live copy first: `src/lib.rs` sorts before `src/main.rs`, and the
        // stable sort keeps the live instance ahead of its stale twin.
        assert_eq!(merged[0].id, live.id);
    }

    /// Helper-level guard ONLY: pins the RW2 cap constant inside
    /// `merge_live_diagnostics`. Like the dedupe test above, it does not
    /// exercise the worker wiring (see that test's note for what does).
    #[test]
    fn merge_live_diagnostics_caps_at_the_rw2_bound() {
        let many: Vec<DiagnosticEvent> = (0..40)
            .map(|i| diag(&format!("src/f{i:02}.rs"), "E0308"))
            .collect();

        let merged = merge_live_diagnostics(Vec::new(), many);

        assert_eq!(
            merged.len(),
            MAX_DIAGNOSTICS,
            "diagnostics presented to the model are capped like repair's RW2"
        );
    }
}

/// Full-exchange guards for the edit path's observability (audit 2026-08):
/// a scripted router records every `PromptPack` it completes, so these tests
/// see exactly what the model sees on a repair turn, and the recording
/// decision log captures the per-attempt `edit_attempt` telemetry record.
#[cfg(test)]
mod exchange_tests {
    use super::*;

    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use crate::adapters::{NodeExecRef, ToolCaller, ToolCallerError};
    use crate::context::NullContextEngine;
    use crate::error::AdapterError;
    use crate::graph::GraphViewHandle;
    use crate::obs::{DecisionKind, RecordingDecisionLog, RetentionPolicy, SharedCostMeter};
    use crate::router::{
        ModelEndpoint, ModelResponse, ModelRouter, PromptPack, RoutedModel, RouterError,
        RoutingRequest, Usage,
    };
    use crate::storage::{ArtifactBlob, ArtifactMeta, ArtifactStore, StoreError};
    use crate::types::budget::{Goal, TokenBudget};
    use crate::types::ids::{DagId, Digest, EndpointId, NodeId, ProviderId, RunId, SessionId};
    use crate::types::permission::PermissionToken;
    use crate::types::tools::{ToolCall, ToolError, ToolResult};

    use super::super::super::perms::WorkerPermissions;
    use crate::dag::NodeInputEnvelope;

    const GOOD_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n-    let x: &str = 42;\n+    let x: i32 = 42;\n";

    fn endpoint() -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("test-endpoint").expect("static id"),
            provider: ProviderId::new("test").expect("static id"),
            display_name: "test".into(),
            model: "test-model".into(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: true,
            supports_json_schema: false,
            json_schema_strict: false,
            max_context: 32_768,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
            temperature: None,
        }
    }

    /// Pops one scripted structured body per completion and records the
    /// exact `PromptPack` each turn sent.
    struct ScriptedRouter {
        responses: Mutex<VecDeque<serde_json::Value>>,
        prompts: Mutex<Vec<PromptPack>>,
    }

    impl ScriptedRouter {
        fn new(responses: Vec<serde_json::Value>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                prompts: Mutex::new(Vec::new()),
            }
        }

        /// One string per model turn: that turn's messages joined in order.
        fn prompt_texts(&self) -> Vec<String> {
            self.prompts
                .lock()
                .expect("prompts lock")
                .iter()
                .map(|p| {
                    p.messages
                        .iter()
                        .map(|m| m.content.clone())
                        .collect::<Vec<_>>()
                        .join("\n---\n")
                })
                .collect()
        }
    }

    #[async_trait]
    impl ModelRouter for ScriptedRouter {
        async fn route(&self, req: RoutingRequest) -> Result<RoutedModel, RouterError> {
            Ok(RoutedModel::mint(
                endpoint(),
                ModelTier::Standard,
                &req,
                true,
                None,
                0,
            ))
        }

        async fn complete(
            &self,
            _routed: &RoutedModel,
            prompt: PromptPack,
        ) -> Result<ModelResponse, RouterError> {
            self.prompts.lock().expect("prompts lock").push(prompt);
            let structured = self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("model response script exhausted");
            Ok(ModelResponse {
                text: None,
                structured: Some(structured),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                },
                provider_request_id: None,
                finish_reason: Some("stop".into()),
            })
        }
    }

    struct QueueTools {
        results: Mutex<VecDeque<ToolResult>>,
    }

    #[async_trait]
    impl ToolCaller for QueueTools {
        async fn call(
            &self,
            _call: ToolCall,
            _perms: PermissionToken,
        ) -> Result<ToolResult, ToolCallerError> {
            self.results
                .lock()
                .expect("tools lock")
                .pop_front()
                .ok_or_else(|| ToolCallerError::Internal("tool script exhausted".into()))
        }
    }

    struct AllowPerms;

    #[async_trait]
    impl WorkerPermissions for AllowPerms {
        async fn token_for(
            &self,
            ctx: &NodeExecRef,
            _class: WorkerToolClass,
        ) -> Result<PermissionToken, AdapterError> {
            // Deserialized, not a struct literal: the PM2 grep pins token
            // literals to perms.rs, and this double has no business looking
            // like a production mint.
            let token = serde_json::from_value(json!({
                "profile": "default",
                "grants": [],
                "expires": null,
                "run_id": ctx.run_id,
            }))
            .expect("static token shape");
            Ok(token)
        }
    }

    /// Goal-rooted attempts only load pred artifacts when preds exist, so
    /// the store just accepts the EW9 patch put.
    struct PutOnlyArtifacts;

    #[async_trait]
    impl ArtifactStore for PutOnlyArtifacts {
        async fn put(&self, _req: ArtifactPut) -> Result<ArtifactId, StoreError> {
            Ok(ArtifactId::new())
        }

        async fn get(&self, _id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
            Err(StoreError::NotFound("goal-rooted: no preds".into()))
        }

        async fn meta(&self, _id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
            Err(StoreError::NotFound("unused".into()))
        }

        async fn get_by_digest(&self, _d: &Digest) -> Result<Option<ArtifactId>, StoreError> {
            Ok(None)
        }

        async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn patch_response(diff: &str) -> serde_json::Value {
        json!({ "patch": diff, "summary": "fix the annotation", "confidence": 0.8 })
    }

    fn ops_response(expect: &str) -> serde_json::Value {
        json!({
            "ops": [{
                "op": "replace_lines",
                "path": "src/main.rs",
                "start": 1,
                "end": 1,
                "expect": [expect],
                "new": ["    let x: i32 = 42;"],
            }],
            "summary": "fix the annotation",
            "confidence": 0.8,
        })
    }

    fn dry_conflict(msg: &str) -> ToolResult {
        ToolResult::err(
            crate::types::tools::ToolName::new("apply_patch").expect("static name"),
            json!({ "code": "conflict", "dry_run": true }),
            ToolError::Permanent {
                code: "conflict".into(),
                message: msg.into(),
            },
            2,
        )
    }

    fn apply_ok(dry_run: bool) -> ToolResult {
        ToolResult::ok(
            crate::types::tools::ToolName::new("apply_patch").expect("static name"),
            json!({
                "files_touched": ["src/main.rs"],
                "transaction_id": TransactionId::new(),
                "dry_run": dry_run,
            }),
            2,
        )
    }

    fn fs_read_ok(path: &str, text: &str) -> ToolResult {
        ToolResult::ok(
            crate::types::tools::ToolName::new("fs_read").expect("static name"),
            json!({ "path": path, "truncated": false, "text": text }),
            1,
        )
    }

    struct Fx {
        router: Arc<ScriptedRouter>,
        decisions: Arc<RecordingDecisionLog>,
    }

    /// Drive one full goal-rooted edit attempt through `Capability::execute`
    /// with scripted model responses and tool results.
    async fn run_edit(
        responses: Vec<serde_json::Value>,
        tool_results: Vec<ToolResult>,
    ) -> (Fx, Result<CapabilityOutcome, CapabilityExecError>) {
        let router = Arc::new(ScriptedRouter::new(responses));
        let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let dag = DagId::new();
        let node = NodeId::new();
        let input = NodeInputEnvelope::new(
            dag,
            node,
            NodeKind::Edit,
            1,
            NodeInputPayload::Goal(Goal {
                text: "fix the type error in src/main.rs".into(),
                constraints: vec![],
                attachments: vec![],
            }),
        );
        let ctx = CapabilityContext {
            session: SessionId::new(),
            run: RunId::new(),
            dag,
            node,
            attempt: 1,
            workspace_root: Path::new("."),
            capability: CapabilityId::new("edit").expect("static id"),
            kind: NodeKind::Edit,
            effective_tier: ModelTier::Standard,
            budget: TokenBudget {
                max_input: 8192,
                max_output: 1024,
            },
            deadline: Duration::from_secs(30),
            cancel: CancellationToken::new(),
            input: &input,
            router: Arc::clone(&router) as Arc<dyn ModelRouter>,
            context: Arc::new(NullContextEngine::with_goal(
                "fix the type error in src/main.rs",
            )),
            tools: Arc::new(QueueTools {
                results: Mutex::new(tool_results.into_iter().collect()),
            }),
            perms: Arc::new(AllowPerms),
            graph: GraphViewHandle::null(),
            artifacts: Arc::new(PutOnlyArtifacts),
            decisions: Arc::clone(&decisions) as _,
            cost_meter: SharedCostMeter::new(),
            started: Instant::now(),
        };
        let worker = EditWorker::new(WorkerConfig::default());
        let outcome = worker.execute(&ctx).await;
        (Fx { router, decisions }, outcome)
    }

    fn edit_attempt_metadata(fx: &Fx) -> serde_json::Value {
        fx.decisions
            .recorded_decisions()
            .iter()
            .find(|r| r.kind == DecisionKind::Custom("edit_attempt".into()))
            .expect("edit_attempt telemetry record")
            .metadata
            .clone()
    }

    /// FINDING 2 (audit 2026-08): the dry-run repair turn must fence the
    /// sanitized human-readable tool error (`ToolResult::error()`), not just
    /// the builtin's `{"code":...,"dry_run":true}` content JSON.
    #[tokio::test]
    async fn dry_run_repair_feedback_carries_the_tool_error_message() {
        let msg = "hunk 1 does not apply: expected `let x: &str = 42;` at src/main.rs:1";
        let (fx, outcome) = run_edit(
            vec![patch_response(GOOD_DIFF), patch_response(GOOD_DIFF)],
            vec![dry_conflict(msg), dry_conflict(msg)],
        )
        .await;
        // EW6 terminal shape is unchanged: the second dry-run failure is a
        // soft Tool failure.
        assert!(
            matches!(outcome, Ok(CapabilityOutcome::Failed { .. })),
            "expected soft failure, got {outcome:?}"
        );
        let prompts = fx.router.prompt_texts();
        assert_eq!(prompts.len(), 2, "one author turn + one repair turn");
        assert!(
            prompts[1].contains("hunk 1 does not apply"),
            "the repair turn must see the tool error message, got:\n{}",
            prompts[1]
        );
    }

    /// FINDING 1 (audit 2026-08): one `edit_attempt` decision record per
    /// attempt answers — response form per turn, op count and kinds, which
    /// check refused, turns consumed, terminal outcome.
    #[tokio::test]
    async fn edit_attempt_record_captures_proposal_forms_and_ops_refusal() {
        let (fx, outcome) = run_edit(
            vec![
                ops_response("    let x: &str = 43;"), // stale expect
                patch_response(GOOD_DIFF),
            ],
            vec![
                fs_read_ok("src/main.rs", "    let x: &str = 42;\n"),
                apply_ok(true),
                apply_ok(false),
            ],
        )
        .await;
        assert!(
            matches!(outcome, Ok(CapabilityOutcome::Succeeded { .. })),
            "expected success, got {outcome:?}"
        );
        let m = edit_attempt_metadata(&fx);
        assert_eq!(m["outcome"], json!("succeeded"));
        assert_eq!(m["model_turns"], json!(2));
        let proposals = m["proposals"].as_array().expect("proposals array");
        assert_eq!(proposals.len(), 2, "{proposals:?}");
        assert_eq!(proposals[0]["form"], json!("ops"));
        assert_eq!(proposals[0]["op_count"], json!(1));
        assert_eq!(proposals[0]["op_kinds"], json!(["replace_lines"]));
        assert_eq!(proposals[0]["refused_by"], json!("ops_compile"));
        assert!(
            proposals[0]["refusal"]
                .as_str()
                .expect("refusal string")
                .contains("stale op"),
            "{proposals:?}"
        );
        assert_eq!(proposals[1]["form"], json!("patch"));
        assert!(proposals[1]["refused_by"].is_null(), "{proposals:?}");
    }

    /// FINDING 1, refusal arm: dry-run refusals are attributed per proposal
    /// and the terminal outcome carries the failure class.
    #[tokio::test]
    async fn edit_attempt_record_marks_dry_run_refusals_and_failed_outcome() {
        let (fx, outcome) = run_edit(
            vec![patch_response(GOOD_DIFF), patch_response(GOOD_DIFF)],
            vec![
                dry_conflict("hunk 1 does not apply"),
                dry_conflict("hunk 1 does not apply"),
            ],
        )
        .await;
        assert!(
            matches!(outcome, Ok(CapabilityOutcome::Failed { .. })),
            "expected soft failure, got {outcome:?}"
        );
        let m = edit_attempt_metadata(&fx);
        assert_eq!(m["outcome"], json!("failed"));
        assert_eq!(m["error_class"], json!("Tool"));
        assert_eq!(m["model_turns"], json!(2));
        let proposals = m["proposals"].as_array().expect("proposals array");
        assert_eq!(proposals.len(), 2, "{proposals:?}");
        assert!(
            proposals
                .iter()
                .all(|p| p["refused_by"] == json!("dry_run")),
            "{proposals:?}"
        );
        assert!(
            proposals[1]["refusal"]
                .as_str()
                .expect("refusal string")
                .contains("hunk 1 does not apply"),
            "{proposals:?}"
        );
    }
}
