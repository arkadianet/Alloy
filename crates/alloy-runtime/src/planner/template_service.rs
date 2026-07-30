//! Template-backed [`PlanService`] implementation (RFC-0009 §5.2 / §5.6).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::dag::{DagValidationError, NodeKind, TaskDag, TemplateCatalog, TemplateId};
use crate::events::{EventSink, EventSinkError};
use crate::scheduler::DagState;
use crate::session::ReplanReason;
use crate::storage::{ArtifactStore, DagStore, StoreError};
use crate::types::budget::Goal;
use crate::types::ids::{ArtifactId, DagId, Digest, NodeId, RunId, SessionId};

use super::persist::{CasExpected, PersistRequest, PlanPersistence};

/// Provenance of a persisted plan generation (RFC-0017 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanSource {
    /// Instantiated from the closed template catalog.
    Template,
    /// Compiled from an accepted `ProposedDagManifest`.
    LlmProposed,
}

/// Planning context. `dag_id` is pre-minted by RFC-0003 `submit_goal`.
#[derive(Debug, Clone)]
pub struct PlanContext {
    /// Owning session.
    pub session_id: SessionId,
    /// Owning run.
    pub run_id: RunId,
    /// Pre-minted by RFC-0003 (`RunGoalRecord.dag_id`).
    pub dag_id: DagId,
    /// User goal.
    pub goal: Goal,
    /// Optional explicit template. On replan, callers SHOULD pass prior template id.
    pub template_override: Option<TemplateId>,
    /// Policy hash (cache / future).
    pub policy_hash: Digest,
    /// Tool versions digest.
    pub tool_versions: Digest,
    /// Compiler fingerprint.
    pub compiler_fingerprint: Digest,
    /// Prior generation's plan source, carried into `replan` (RFC-0017
    /// AM-0009-7). `None` for first plans and for callers without
    /// provenance — behaviour is then exactly pre-RFC-0017.
    pub prior_source: Option<PlanSource>,
    /// Prior generation's raw proposal artifact when `prior_source` is
    /// [`PlanSource::LlmProposed`] (RFC-0017 AM-0009-7 / GN10).
    pub prior_proposal_artifact: Option<ArtifactId>,
}

/// Successful plan / replan result.
#[derive(Debug, Clone)]
pub struct PlanResult {
    /// Persisted DAG.
    pub dag: TaskDag,
    /// Selected template.
    pub template_id: TemplateId,
    /// CAS snapshot of the DAG JSON.
    pub snapshot_artifact: ArtifactId,
    /// Plan provenance (RFC-0017 AM-0009-4).
    pub source: PlanSource,
    /// Raw proposal artifact when `source` is [`PlanSource::LlmProposed`]
    /// (RFC-0017 AM-0009-4).
    pub proposal_artifact: Option<ArtifactId>,
}

/// Typed PlanProduced payload (session event wire shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanProducedPayload {
    /// DAG id.
    pub dag_id: DagId,
    /// Generation.
    pub generation: u64,
    /// Template used.
    pub template_id: TemplateId,
    /// Snapshot artifact id.
    pub snapshot_artifact: ArtifactId,
    /// Sorted ascending by [`NodeId`].
    pub node_ids: Vec<NodeId>,
    /// Whether this was a replan.
    pub replan: bool,
    /// Replan reason when `replan`.
    pub reason: Option<ReplanReason>,
    /// Plan provenance; absent ⇒ `template` (RFC-0017 AM-0009-3, old-reader
    /// back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PlanSource>,
    /// Raw proposal artifact for compiled proposals (RFC-0017 AM-0009-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_artifact: Option<ArtifactId>,
    /// Whether the replan root was seeded per SD1–SD10 (RFC-0017 AM-0009-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeded_root: Option<bool>,
}

/// PlanService errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// Unknown template name at a string boundary.
    #[error("unknown template: {0}")]
    UnknownTemplate(String),

    /// No template matched the goal (unused day-1).
    #[error("no template matched goal")]
    NoTemplateMatch,

    /// LLM planner disabled.
    #[error("LLM planner disabled")]
    PlannerDisabled,

    /// Validation failed.
    #[error("validation failed: {0}")]
    Validation(#[from] DagValidationError),

    /// DAG store failure.
    #[error("store: {0}")]
    Store(StoreError),

    /// Artifact store failure.
    #[error("artifact: {0}")]
    Artifact(StoreError),

    /// Event sink failure after durable DAG write.
    #[error("event sink: {0}")]
    Event(#[from] EventSinkError),

    /// DAG missing on replan.
    #[error("dag not found: {0}")]
    DagNotFound(DagId),

    /// Session mismatch on replan.
    #[error("session mismatch: dag session {dag_session} != context {context_session}")]
    SessionMismatch {
        /// Session on the stored DAG.
        dag_session: SessionId,
        /// Session from context.
        context_session: SessionId,
    },

    /// CAS generation mismatch.
    #[error("generation mismatch: expected {expected}, store has {actual}")]
    GenerationMismatch {
        /// Expected generation (`0` for insert-only conflicts).
        expected: u64,
        /// Actual stored generation.
        actual: u64,
    },

    /// Replan rejected while DAG is Running.
    #[error("dag busy in state {state:?}; replan not permitted")]
    DagBusy {
        /// Observed state.
        state: DagState,
    },

    /// Generation would overflow u64 or exceed i64::MAX.
    #[error("generation overflow")]
    GenerationOverflow,

    /// Internal invariant / serde failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// Planner trait.
#[async_trait]
pub trait PlanService: Send + Sync {
    /// Instantiate + validate + CAS-insert generation 1 + snapshot + PlanProduced.
    async fn plan(&self, ctx: PlanContext) -> Result<PlanResult, PlanError>;

    /// Instantiate a specific template (ignores `ctx.template_override`).
    async fn load_template(
        &self,
        id: TemplateId,
        ctx: PlanContext,
    ) -> Result<PlanResult, PlanError>;

    /// Replan: atomic replace, re-instantiate, snapshot, PlanProduced.
    ///
    /// Production callers MUST invoke [`crate::RunController::request_replan`]
    /// first so gate waiters clear. The owning scheduler MUST checkpoint a
    /// non-[`crate::DagState::Running`] state (typically `ReplanRequired`) at the
    /// same generation before this call; otherwise [`PlanError::DagBusy`] is
    /// permanent (RFC-0009 Appendix C).
    async fn replan(&self, reason: ReplanReason, ctx: PlanContext)
        -> Result<PlanResult, PlanError>;
}

/// Template-backed planner (day-1 production implementation of [`PlanService`]).
///
/// # Production wiring
///
/// Construct via [`Self::from_storage`] (or [`Self::new`]) after
/// [`crate::AlloyStorage::open`], then inject as `Arc<dyn PlanService>` into the
/// **CLI / host** (`alloy run`, RFC-0015) — never into a capability worker:
/// a worker holding a `PlanService` could write topology from inside a node,
/// breaking the single-writer rule (V2 §6.4, ADR F-03; RFC-0013 AM-0009-1 /
/// rule PW2). This crate does not
/// change [`crate::RunController`] signatures (RFC-0009 §2.4); callers build
/// [`PlanContext`] from [`crate::RunGoalRecord`] and call [`PlanService::plan`].
pub struct TemplatePlanService {
    dags: Arc<dyn DagStore>,
    persist: PlanPersistence,
}

impl TemplatePlanService {
    /// Construct with required event sink (not optional).
    #[must_use]
    pub fn new(
        dags: Arc<dyn DagStore>,
        artifacts: Arc<dyn ArtifactStore>,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            dags: Arc::clone(&dags),
            persist: PlanPersistence::new(dags, artifacts, events),
        }
    }

    /// Production helper: wire dags + artifacts + durable SQLite event sink.
    #[must_use]
    pub fn from_storage(storage: &crate::storage::AlloyStorage) -> Self {
        Self::new(
            storage.dags() as Arc<dyn DagStore>,
            storage.artifacts() as Arc<dyn ArtifactStore>,
            storage.events() as Arc<dyn EventSink>,
        )
    }

    /// Day-1 selector (RFC-0009 MVP rule), shared with `LlmPlanService`'s
    /// fallback-identity computation (RFC-0017 LP5).
    pub(crate) fn select(ctx: &PlanContext) -> TemplateId {
        ctx.template_override
            .unwrap_or(TemplateId::RepairLocalDiagnostic)
    }

    /// The single validated write path, shared with `LlmPlanService` (LP2 —
    /// both services persist through the same `PlanPersistence`).
    pub(crate) fn persistence(&self) -> &PlanPersistence {
        &self.persist
    }

    /// Read-only DAG store handle for the replan probe (never a write path).
    pub(crate) fn dag_store(&self) -> Arc<dyn DagStore> {
        Arc::clone(&self.dags)
    }

    /// Thin caller of [`PlanPersistence::persist_validated`] (AM-0009-6):
    /// template selection stays here, every write happens there.
    async fn instantiate_and_persist(
        &self,
        template_id: TemplateId,
        ctx: &PlanContext,
        generation: u64,
        reason: Option<ReplanReason>,
        expected_for_cas: CasExpected,
        probe_kinds: Option<&std::collections::BTreeMap<NodeId, NodeKind>>,
    ) -> Result<PlanResult, PlanError> {
        // Day-1 never enables cache; PlanContext fingerprints are reserved for RFC-0010
        // (see dag/cache.rs module docs). They are intentionally unread here.
        let manifest = TemplateCatalog::get(template_id);
        self.persist
            .persist_validated(PersistRequest {
                ctx,
                specs: &manifest.nodes,
                edges: &manifest.edges,
                source: PlanSource::Template,
                template_id,
                proposal_artifact: None,
                reason: reason.as_ref(),
                generation,
                expected_for_cas,
                probe_kinds,
            })
            .await
    }
}

#[async_trait]
impl PlanService for TemplatePlanService {
    #[tracing::instrument(
        skip(self, ctx),
        fields(
            session_id = %ctx.session_id,
            run_id = %ctx.run_id,
            dag_id = %ctx.dag_id,
            template = tracing::field::Empty
        ),
        name = "planner.plan"
    )]
    async fn plan(&self, ctx: PlanContext) -> Result<PlanResult, PlanError> {
        let template_id = Self::select(&ctx);
        tracing::Span::current().record("template", template_id.as_str());
        self.instantiate_and_persist(template_id, &ctx, 1, None, CasExpected::InsertOnly, None)
            .await
    }

    #[tracing::instrument(
        skip(self, ctx),
        fields(
            session_id = %ctx.session_id,
            run_id = %ctx.run_id,
            dag_id = %ctx.dag_id,
            template = tracing::field::Empty
        ),
        name = "planner.load_template"
    )]
    async fn load_template(
        &self,
        id: TemplateId,
        ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        tracing::Span::current().record("template", id.as_str());
        self.instantiate_and_persist(id, &ctx, 1, None, CasExpected::InsertOnly, None)
            .await
    }

    #[tracing::instrument(
        skip(self, ctx, reason),
        fields(
            dag_id = %ctx.dag_id,
            generation_from = tracing::field::Empty,
            generation_to = tracing::field::Empty,
            reason_variant = tracing::field::Empty
        ),
        name = "planner.replan"
    )]
    async fn replan(
        &self,
        reason: ReplanReason,
        ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        let reason_variant = match &reason {
            ReplanReason::FailureIr(_) => "failure_ir",
            ReplanReason::UserRequested => "user_requested",
            ReplanReason::BudgetPolicy => "budget_policy",
            ReplanReason::Other(_) => "other",
        };
        tracing::Span::current().record("reason_variant", reason_variant);

        let probe = self
            .dags
            .get(ctx.dag_id)
            .await
            .map_err(PlanError::Store)?
            .ok_or(PlanError::DagNotFound(ctx.dag_id))?;

        if probe.session_id != ctx.session_id {
            return Err(PlanError::SessionMismatch {
                dag_session: probe.session_id,
                context_session: ctx.session_id,
            });
        }

        // Cheap preflight: avoid orphan CAS blobs when scheduler hasn't checkpointed yet.
        // Atomic `replace_for_replan` still rejects Running under the race.
        if probe.state == DagState::Running {
            return Err(PlanError::DagBusy {
                state: DagState::Running,
            });
        }

        let next_gen = probe
            .generation
            .checked_add(1)
            .ok_or(PlanError::GenerationOverflow)?;
        if next_gen > i64::MAX as u64 {
            return Err(PlanError::GenerationOverflow);
        }

        tracing::Span::current().record("generation_from", probe.generation);
        tracing::Span::current().record("generation_to", next_gen);

        let template_id = Self::select(&ctx);

        // SD3: the seeder looks the failed node's kind up in the replan
        // probe blob; an absent node defaults to `VerifyCompile`.
        let probe_kinds: std::collections::BTreeMap<NodeId, NodeKind> = probe
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.kind))
            .collect();

        self.instantiate_and_persist(
            template_id,
            &ctx,
            next_gen,
            Some(reason),
            CasExpected::Replan {
                expected_generation: probe.generation,
            },
            Some(&probe_kinds),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{
        compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, NodeInputEnvelope,
        NodeInputPayload, NodeKind, PendingPredPlaceholder,
    };
    use crate::events::{InMemoryEventSink, NewSessionEvent, SessionEventType};
    use crate::storage::{
        AlloyStorage, ArtifactKind, ArtifactStore, DagStore, EventStore, StorageOpenOptions,
    };
    use std::sync::Mutex;

    fn fingerprints() -> (Digest, Digest, Digest) {
        let toolchain = crate::types::toolchain::ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1 (test)".into(),
            cargo_version: "cargo 1.97.1 (test)".into(),
        };
        (
            policy_hash_digest(
                &crate::types::ids::ProfileId::new("default").unwrap(),
                &crate::types::budget::BudgetPolicy::default(),
            ),
            tool_versions_digest(&toolchain),
            compiler_fingerprint_digest(&toolchain, "x86_64-unknown-linux-gnu"),
        )
    }

    fn plan_ctx(session: SessionId, run: RunId, dag: DagId) -> PlanContext {
        let (policy, tools, compiler) = fingerprints();
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id: dag,
            goal: Goal {
                text: "fix E0382".into(),
                constraints: vec![],
                attachments: vec![],
            },
            template_override: None,
            policy_hash: policy,
            tool_versions: tools,
            compiler_fingerprint: compiler,
            prior_source: None,
            prior_proposal_artifact: None,
        }
    }

    async fn service() -> (
        tempfile::TempDir,
        AlloyStorage,
        TemplatePlanService,
        Arc<InMemoryEventSink>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let events = Arc::new(InMemoryEventSink::new());
        let svc = TemplatePlanService::new(
            storage.dags() as Arc<dyn DagStore>,
            storage.artifacts() as Arc<dyn ArtifactStore>,
            events.clone() as Arc<dyn EventSink>,
        );
        (dir, storage, svc, events)
    }

    /// The read-only `review_diff` template plans and persists although it
    /// carries no `GateHuman`: V11 guards mutation, and this template makes
    /// none. Its single root node receives the goal (diff) envelope.
    #[tokio::test]
    async fn plan_review_diff_needs_no_gate() {
        let (_dir, storage, svc, _events) = service().await;
        let dag_id = DagId::new();
        let mut ctx = plan_ctx(SessionId::new(), RunId::new(), dag_id);
        ctx.template_override = Some(TemplateId::ReviewDiff);
        ctx.goal.text = "review this diff".into();

        let result = svc.plan(ctx).await.unwrap();
        assert_eq!(result.template_id, TemplateId::ReviewDiff);
        assert_eq!(result.dag.nodes.len(), 1);
        let node = result.dag.nodes.values().next().unwrap();
        assert_eq!(node.kind, NodeKind::Review);
        assert_eq!(node.capability.as_ref().unwrap().as_str(), "review");

        let blob = storage.artifacts().get(node.input_ref).await.unwrap();
        let env: NodeInputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
        match env.payload {
            NodeInputPayload::Goal(goal) => assert_eq!(goal.text, "review this diff"),
            other => panic!("review root must carry the goal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_uses_pre_minted_dag_id() {
        let (_dir, storage, svc, events) = service().await;
        let session = SessionId::new();
        let run = RunId::new();
        let dag_id = DagId::new();
        let result = svc.plan(plan_ctx(session, run, dag_id)).await.unwrap();
        assert_eq!(result.dag.id, dag_id);
        assert_eq!(result.dag.generation, 1);
        assert_eq!(result.template_id, TemplateId::RepairLocalDiagnostic);

        let stored = storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(stored.id, dag_id);

        let evs = events.session_events(session);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].type_, SessionEventType::PlanProduced);
        assert_eq!(evs[0].run_id, Some(run));
        let payload: PlanProducedPayload = serde_json::from_value(evs[0].payload.clone()).unwrap();
        assert!(!payload.replan);
        assert!(payload.reason.is_none());
        let mut sorted = payload.node_ids.clone();
        sorted.sort();
        assert_eq!(payload.node_ids, sorted);

        // Every input_ref resolves with required attribution / labels
        for node in result.dag.nodes.values() {
            let blob = storage.artifacts().get(node.input_ref).await.unwrap();
            assert_eq!(blob.meta.kind, ArtifactKind::Blob);
            assert_eq!(blob.meta.content_type.as_deref(), Some("application/json"));
            assert_eq!(blob.meta.session_id, Some(session));
            assert_eq!(blob.meta.run_id, Some(run));
            assert_eq!(
                blob.meta
                    .labels
                    .get("alloy.envelope")
                    .and_then(|v| v.as_str()),
                Some("node_input")
            );
            assert_eq!(
                blob.meta
                    .labels
                    .get("alloy.dag_id")
                    .and_then(|v| v.as_str()),
                Some(dag_id.to_string().as_str())
            );

            // Non-root FromPredecessors pending_pred slots carry the same attribution.
            let env: NodeInputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
            if let NodeInputPayload::FromPredecessors { preds } = env.payload {
                for pred in preds {
                    let pending = storage.artifacts().get(pred.output_ref).await.unwrap();
                    assert_eq!(pending.meta.kind, ArtifactKind::Blob);
                    assert_eq!(
                        pending.meta.content_type.as_deref(),
                        Some("application/json")
                    );
                    assert_eq!(pending.meta.session_id, Some(session));
                    assert_eq!(pending.meta.run_id, Some(run));
                    assert_eq!(
                        pending
                            .meta
                            .labels
                            .get("alloy.envelope")
                            .and_then(|v| v.as_str()),
                        Some("pending_pred")
                    );
                    assert_eq!(
                        pending
                            .meta
                            .labels
                            .get("alloy.dag_id")
                            .and_then(|v| v.as_str()),
                        Some(dag_id.to_string().as_str())
                    );
                }
            }
        }
        let snap = storage
            .artifacts()
            .get(result.snapshot_artifact)
            .await
            .unwrap();
        assert_eq!(snap.meta.kind, ArtifactKind::Blob);
        assert_eq!(snap.meta.content_type.as_deref(), Some("application/json"));
        assert_eq!(snap.meta.session_id, Some(session));
        assert_eq!(snap.meta.run_id, Some(run));
        assert_eq!(
            snap.meta
                .labels
                .get("alloy.envelope")
                .and_then(|v| v.as_str()),
            Some("dag_snapshot")
        );
        assert_eq!(
            snap.meta
                .labels
                .get("alloy.dag_id")
                .and_then(|v| v.as_str()),
            Some(dag_id.to_string().as_str())
        );
        let round: TaskDag = serde_json::from_slice(&snap.bytes).unwrap();
        assert_eq!(round.id, dag_id);
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn load_template_ignores_override() {
        let (_dir, storage, svc, _) = service().await;
        let mut ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        // Even if override were somehow set to the only template, load_template
        // must use the explicit id argument (day-1 catalog has one entry).
        ctx.template_override = Some(TemplateId::RepairLocalDiagnostic);
        let result = svc
            .load_template(TemplateId::RepairLocalDiagnostic, ctx)
            .await
            .unwrap();
        assert_eq!(result.template_id, TemplateId::RepairLocalDiagnostic);
        assert_eq!(result.dag.generation, 1);
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn from_storage_wires_durable_events() {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let svc = TemplatePlanService::from_storage(&storage);
        let session = SessionId::new();
        let run = RunId::new();
        let dag_id = DagId::new();
        let result = svc.plan(plan_ctx(session, run, dag_id)).await.unwrap();
        assert_eq!(result.dag.id, dag_id);
        let evs = storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].type_, SessionEventType::PlanProduced);
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn replan_session_mismatch_and_not_found() {
        let (_dir, storage, svc, _) = service().await;
        let session = SessionId::new();
        let run = RunId::new();
        let dag_id = DagId::new();
        svc.plan(plan_ctx(session, run, dag_id)).await.unwrap();

        let mut bad = plan_ctx(SessionId::new(), run, dag_id);
        let err = svc
            .replan(ReplanReason::UserRequested, bad.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::SessionMismatch { .. }));

        bad.dag_id = DagId::new();
        bad.session_id = session;
        let err = svc
            .replan(ReplanReason::UserRequested, bad)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::DagNotFound(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn second_plan_generation_mismatch() {
        let (_dir, storage, svc, _) = service().await;
        let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        svc.plan(ctx.clone()).await.unwrap();
        let err = svc.plan(ctx).await.unwrap_err();
        assert!(matches!(
            err,
            PlanError::GenerationMismatch {
                expected: 0,
                actual: 1
            }
        ));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn replan_bumps_and_sets_pending() {
        let (_dir, storage, svc, events) = service().await;
        let session = SessionId::new();
        let run = RunId::new();
        let dag_id = DagId::new();
        let ctx = plan_ctx(session, run, dag_id);
        let first = svc.plan(ctx.clone()).await.unwrap();

        // Mark Failed then replan
        let mut failed = first.dag.clone();
        failed.state = DagState::Failed;
        storage.dags().put(&failed).await.unwrap();

        let mut replan_ctx = ctx;
        replan_ctx.template_override = Some(first.template_id);
        let second = svc
            .replan(ReplanReason::UserRequested, replan_ctx)
            .await
            .unwrap();
        assert_eq!(second.dag.generation, 2);
        assert_eq!(second.dag.state, DagState::Pending);
        for n in second.dag.nodes.values() {
            assert_eq!(n.state, crate::dag::NodeState::Pending);
        }

        let evs = events.session_events(session);
        assert_eq!(evs.len(), 2);
        let payload: PlanProducedPayload = serde_json::from_value(evs[1].payload.clone()).unwrap();
        assert!(payload.replan);
        assert_eq!(payload.reason, Some(ReplanReason::UserRequested));
        assert_eq!(payload.generation, 2);

        // Prior gen only via snapshot, not dag_blobs history
        assert_eq!(
            storage
                .dags()
                .get(dag_id)
                .await
                .unwrap()
                .unwrap()
                .generation,
            2
        );
        let old_snap = storage
            .artifacts()
            .get(first.snapshot_artifact)
            .await
            .unwrap();
        let old_dag: TaskDag = serde_json::from_slice(&old_snap.bytes).unwrap();
        assert_eq!(old_dag.generation, 1);
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn replan_dag_busy_when_running() {
        let (_dir, storage, svc, _) = service().await;
        let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        let first = svc.plan(ctx.clone()).await.unwrap();
        let mut running = first.dag.clone();
        running.state = DagState::Running;
        storage.dags().put(&running).await.unwrap();
        let err = svc
            .replan(ReplanReason::UserRequested, ctx)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PlanError::DagBusy {
                state: DagState::Running
            }
        ));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn replan_generation_overflow_at_i64_max() {
        let (_dir, storage, svc, _) = service().await;
        let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        let first = svc.plan(ctx.clone()).await.unwrap();
        let mut maxed = first.dag.clone();
        maxed.generation = i64::MAX as u64;
        storage.dags().put(&maxed).await.unwrap();
        let err = svc
            .replan(ReplanReason::UserRequested, ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::GenerationOverflow));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn root_input_is_goal_nonroot_is_preds() {
        let (_dir, storage, svc, _) = service().await;
        let result = svc
            .plan(plan_ctx(SessionId::new(), RunId::new(), DagId::new()))
            .await
            .unwrap();
        let analyze = result
            .dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Analyze)
            .unwrap();
        let blob = storage.artifacts().get(analyze.input_ref).await.unwrap();
        let env: NodeInputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
        assert!(matches!(env.payload, NodeInputPayload::Goal(_)));

        let edit = result
            .dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Edit)
            .unwrap();
        let blob = storage.artifacts().get(edit.input_ref).await.unwrap();
        let env: NodeInputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
        match env.payload {
            NodeInputPayload::FromPredecessors { preds } => {
                assert_eq!(preds.len(), 1);
                let pending = storage.artifacts().get(preds[0].output_ref).await.unwrap();
                let ph: PendingPredPlaceholder = serde_json::from_slice(&pending.bytes).unwrap();
                assert!(ph.pending);
            }
            _ => panic!("expected FromPredecessors"),
        }
        storage.close().await.unwrap();
    }

    /// Event sink that fails append_session.
    struct FailEventSink {
        inner: InMemoryEventSink,
        fail: Mutex<bool>,
    }

    #[async_trait]
    impl EventSink for FailEventSink {
        async fn append_runtime(
            &self,
            ev: crate::events::RuntimeEvent,
        ) -> Result<(), EventSinkError> {
            self.inner.append_runtime(ev).await
        }

        async fn append_session(
            &self,
            ev: NewSessionEvent,
        ) -> Result<crate::types::ids::EventSeq, EventSinkError> {
            if *self.fail.lock().unwrap() {
                return Err(EventSinkError::Io("injected".into()));
            }
            self.inner.append_session(ev).await
        }
    }

    #[tokio::test]
    async fn event_failure_after_cas_retains_row() {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let sink = Arc::new(FailEventSink {
            inner: InMemoryEventSink::new(),
            fail: Mutex::new(true),
        });
        let svc = TemplatePlanService::new(
            storage.dags() as Arc<dyn DagStore>,
            storage.artifacts() as Arc<dyn ArtifactStore>,
            sink as Arc<dyn EventSink>,
        );
        let dag_id = DagId::new();
        let err = svc
            .plan(plan_ctx(SessionId::new(), RunId::new(), dag_id))
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::Event(_)));
        assert!(storage.dags().get(dag_id).await.unwrap().is_some());
        storage.close().await.unwrap();
    }

    use crate::dag::NodeOutputEnvelope;
    use crate::types::diagnostic::{
        DiagnosticEvent, DiagnosticLevel, ErrorClass, FailureIr, SpanRef,
    };
    use crate::types::ids::DiagnosticId;

    fn compile_failure(node: NodeId) -> FailureIr {
        FailureIr {
            node,
            error_class: ErrorClass::Compile,
            retry: Default::default(),
            diagnostics: vec![DiagnosticEvent {
                id: DiagnosticId::new(),
                code: Some("E0308".into()),
                level: DiagnosticLevel::Error,
                message: "mismatched types: expected `u32`, found `String`".into(),
                spans: vec![SpanRef {
                    path: "src/lib.rs".into(),
                    start_line: 42,
                    start_col: 9,
                    end_line: 42,
                    end_col: 21,
                }],
                children: vec![],
                package: Some("alloy-runtime".into()),
                fingerprint: crate::obs::hash_prompt("e0308"),
                raw_json: Some(serde_json::json!({ "sentinel": "RAWJSON_SENTINEL_AC39" })),
            }],
            notes: "cargo check failed".into(),
        }
    }

    /// Plan generation 1, flip the DAG `Failed`, and return the verify node
    /// id so a replan can name a real failed node.
    async fn plan_and_fail(
        storage: &AlloyStorage,
        svc: &TemplatePlanService,
        ctx: &PlanContext,
    ) -> NodeId {
        let first = svc.plan(ctx.clone()).await.unwrap();
        let verify = first
            .dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::VerifyCompile)
            .unwrap()
            .id;
        let mut failed = first.dag.clone();
        failed.state = DagState::Failed;
        storage.dags().put(&failed).await.unwrap();
        verify
    }

    async fn root_envelope(storage: &AlloyStorage, result: &PlanResult) -> NodeInputEnvelope {
        let analyze = result
            .dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Analyze)
            .unwrap();
        let blob = storage.artifacts().get(analyze.input_ref).await.unwrap();
        serde_json::from_slice(&blob.bytes).unwrap()
    }

    /// AC 17: a `FailureIr` replan seeds the root — `FromPredecessors` with
    /// one synthetic pred whose `output_ref` decodes as the SD3 seed
    /// envelope (`ok: false`, prior generation, failed node id/kind).
    #[tokio::test]
    async fn ac17_failure_ir_replan_seeds_root_envelope() {
        let (_dir, storage, svc, events) = service().await;
        let session = SessionId::new();
        let ctx = plan_ctx(session, RunId::new(), DagId::new());
        let verify = plan_and_fail(&storage, &svc, &ctx).await;

        let mut replan_ctx = ctx.clone();
        replan_ctx.template_override = Some(TemplateId::RepairLocalDiagnostic);
        let second = svc
            .replan(ReplanReason::FailureIr(compile_failure(verify)), replan_ctx)
            .await
            .unwrap();
        assert_eq!(second.dag.generation, 2);

        let env = root_envelope(&storage, &second).await;
        assert_eq!(env.generation, 2);
        let NodeInputPayload::FromPredecessors { preds } = env.payload else {
            panic!("seeded root must be FromPredecessors");
        };
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].node_id, verify);
        assert_eq!(preds[0].kind, NodeKind::VerifyCompile);

        let seed_blob = storage.artifacts().get(preds[0].output_ref).await.unwrap();
        assert_eq!(
            seed_blob
                .meta
                .labels
                .get("alloy.envelope")
                .and_then(|v| v.as_str()),
            Some("replan_seed")
        );
        let seed: NodeOutputEnvelope = serde_json::from_slice(&seed_blob.bytes).unwrap();
        assert_eq!(seed.node_id, verify);
        assert_eq!(seed.kind, NodeKind::VerifyCompile);
        assert_eq!(seed.generation, 1);
        assert_eq!(seed.attempt, 1);
        assert_eq!(seed.payload["ok"], false);
        assert_eq!(seed.payload["error_class"], "compile");
        assert_eq!(seed.payload["diagnostics"][0]["code"], "E0308");

        // AC 39 (seed half): serialized seed bytes carry no raw_json.
        let text = String::from_utf8(seed_blob.bytes.clone()).unwrap();
        assert!(!text.contains("RAWJSON_SENTINEL_AC39"));
        assert!(!text.contains("raw_json"));

        // AM-0009-3: PlanProduced carries seeded_root = true.
        let evs = events.session_events(session);
        let payload: PlanProducedPayload =
            serde_json::from_value(evs.last().unwrap().payload.clone()).unwrap();
        assert!(payload.replan);
        assert_eq!(payload.seeded_root, Some(true));
        assert_eq!(payload.source, Some(PlanSource::Template));
        storage.close().await.unwrap();
    }

    /// AC 18: non-`FailureIr` replans leave the root envelope byte-identical
    /// to the pre-RFC-0017 shape (bare `Goal`).
    #[tokio::test]
    async fn ac18_non_failure_replan_root_is_goal() {
        for reason in [
            ReplanReason::UserRequested,
            ReplanReason::BudgetPolicy,
            ReplanReason::Other("operator".into()),
        ] {
            let (_dir, storage, svc, _events) = service().await;
            let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
            plan_and_fail(&storage, &svc, &ctx).await;
            let second = svc.replan(reason, ctx.clone()).await.unwrap();
            let env = root_envelope(&storage, &second).await;
            match env.payload {
                NodeInputPayload::Goal(goal) => assert_eq!(goal.text, ctx.goal.text),
                other => panic!("non-FailureIr replan root must be Goal, got {other:?}"),
            }
            storage.close().await.unwrap();
        }
    }

    /// AC 19: a failed node absent from the probe blob falls back to
    /// `VerifyCompile`; the seed is still written.
    #[tokio::test]
    async fn ac19_kind_lookup_miss_defaults_verify_compile() {
        let (_dir, storage, svc, _events) = service().await;
        let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        plan_and_fail(&storage, &svc, &ctx).await;

        let phantom = NodeId::new(); // belongs to no generation
        let second = svc
            .replan(ReplanReason::FailureIr(compile_failure(phantom)), ctx)
            .await
            .unwrap();
        let env = root_envelope(&storage, &second).await;
        let NodeInputPayload::FromPredecessors { preds } = env.payload else {
            panic!("seeded root must be FromPredecessors");
        };
        assert_eq!(preds[0].node_id, phantom);
        assert_eq!(preds[0].kind, NodeKind::VerifyCompile);
        let seed_blob = storage.artifacts().get(preds[0].output_ref).await.unwrap();
        let seed: NodeOutputEnvelope = serde_json::from_slice(&seed_blob.bytes).unwrap();
        assert_eq!(seed.kind, NodeKind::VerifyCompile);
        storage.close().await.unwrap();
    }

    /// AC 20: every `input_ref` of a seeded generation resolves and the DAG
    /// validates under default opts.
    #[tokio::test]
    async fn ac20_seeded_generation_input_refs_resolve_and_validate() {
        let (_dir, storage, svc, _events) = service().await;
        let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
        let verify = plan_and_fail(&storage, &svc, &ctx).await;
        let second = svc
            .replan(ReplanReason::FailureIr(compile_failure(verify)), ctx)
            .await
            .unwrap();
        for node in second.dag.nodes.values() {
            storage.artifacts().get(node.input_ref).await.unwrap();
        }
        crate::dag::DagValidator::validate(&second.dag, crate::dag::ValidateOpts::default())
            .unwrap();
        storage.close().await.unwrap();
    }

    /// AC 35: a pre-RFC-0017 `PlanProduced` payload (no `source` /
    /// `proposal_artifact` / `seeded_root`) still decodes.
    #[test]
    fn ac35_old_plan_produced_payload_decodes() {
        let old = serde_json::json!({
            "dag_id": DagId::new(),
            "generation": 1,
            "template_id": "repair_local_diagnostic",
            "snapshot_artifact": crate::types::ids::ArtifactId::new(),
            "node_ids": [],
            "replan": false,
            "reason": null,
        });
        let payload: PlanProducedPayload = serde_json::from_value(old).unwrap();
        assert!(payload.source.is_none());
        assert!(payload.proposal_artifact.is_none());
        assert!(payload.seeded_root.is_none());
    }

    /// AC 36 (planner half): `PlanSource` wire vocabulary.
    #[test]
    fn plan_source_serde_golden() {
        assert_eq!(
            serde_json::to_value(PlanSource::Template).unwrap(),
            serde_json::json!("template")
        );
        assert_eq!(
            serde_json::to_value(PlanSource::LlmProposed).unwrap(),
            serde_json::json!("llm_proposed")
        );
    }
}
