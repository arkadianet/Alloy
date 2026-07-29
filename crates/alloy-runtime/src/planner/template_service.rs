//! Template-backed [`PlanService`] implementation (RFC-0009 §5.2 / §5.6).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::dag::{
    allocate_ids, build_topology, encode_json, BuildTopology, DagValidationError, DagValidator,
    EdgeKind, NodeInputEnvelope, NodeInputPayload, PendingPredPlaceholder, PredecessorOutput,
    TaskDag, TemplateCatalog, TemplateId, TemplateIdMap, TemplateManifest, ValidateOpts,
};
use crate::events::{EventSink, EventSinkError, NewSessionEvent, SessionEventType};
use crate::scheduler::DagState;
use crate::session::ReplanReason;
use crate::storage::{
    ArtifactKind, ArtifactPut, ArtifactStore, DagStore, ReplanReplaceError, StoreError,
};
use crate::types::budget::Goal;
use crate::types::ids::{ArtifactId, DagId, Digest, NodeId, RunId, SessionId};

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
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<dyn EventSink>,
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
            dags,
            artifacts,
            events,
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

    fn select(ctx: &PlanContext) -> TemplateId {
        ctx.template_override
            .unwrap_or(TemplateId::RepairLocalDiagnostic)
    }

    async fn instantiate_and_persist(
        &self,
        template_id: TemplateId,
        ctx: &PlanContext,
        generation: u64,
        reason: Option<ReplanReason>,
        expected_for_cas: CasExpected,
    ) -> Result<PlanResult, PlanError> {
        let replan = matches!(expected_for_cas, CasExpected::Replan { .. });
        // Day-1 never enables cache; PlanContext fingerprints are reserved for RFC-0010
        // (see dag/cache.rs module docs). They are intentionally unread here.
        let manifest = TemplateCatalog::get(template_id);

        // Phase A
        let ids = allocate_ids(manifest);

        // Pre-CAS validate with ephemeral placeholders
        let mut placeholders = BTreeMap::new();
        for nid in ids.nodes.values() {
            placeholders.insert(*nid, ArtifactId::new());
        }
        let candidate = build_topology(BuildTopology {
            manifest,
            dag_id: ctx.dag_id,
            session_id: ctx.session_id,
            generation,
            ids: &ids,
            input_refs: &placeholders,
        });
        if let Err(e) = DagValidator::validate(&candidate, ValidateOpts::default()) {
            tracing::warn!(error = %e, "plan validation failed");
            return Err(PlanError::Validation(e));
        }

        // Phase B — real CAS puts
        let input_refs = self
            .put_input_artifacts(manifest, &ids, ctx, generation)
            .await?;

        // Phase C
        let dag = build_topology(BuildTopology {
            manifest,
            dag_id: ctx.dag_id,
            session_id: ctx.session_id,
            generation,
            ids: &ids,
            input_refs: &input_refs,
        });
        // Re-validate after real input_refs are bound. Validator does not inspect
        // ArtifactId values, so this cannot fail if Phase A passed; kept as insurance.
        DagValidator::validate(&dag, ValidateOpts::default())?;

        // Snapshot
        let snapshot_bytes = serde_json::to_vec(&dag)
            .map_err(|e| PlanError::Internal(format!("dag snapshot serde: {e}")))?;
        let snapshot_artifact = self
            .put_labeled(snapshot_bytes, ctx, "dag_snapshot")
            .await?;

        // Persist
        // Reconciliation (deferred): a GC/retention sweep SHOULD reclaim
        // node_input / pending_pred / dag_snapshot artifacts left unreferenced
        // after failed or conflicting CAS attempts (blobs may already exist).
        match expected_for_cas {
            CasExpected::InsertOnly => match self.dags.put_if_generation(&dag, None).await {
                Ok(()) => {}
                Err(StoreError::Conflict(_)) => {
                    let existing = self.dags.get(ctx.dag_id).await.map_err(PlanError::Store)?;
                    match existing {
                        Some(e) => {
                            return Err(PlanError::GenerationMismatch {
                                expected: 0,
                                actual: e.generation,
                            });
                        }
                        None => {
                            return Err(PlanError::Internal(
                                "conflict on insert but get returned None".into(),
                            ));
                        }
                    }
                }
                Err(e) => return Err(PlanError::Store(e)),
            },
            CasExpected::Replan {
                expected_generation,
            } => {
                match self
                    .dags
                    .replace_for_replan(&dag, expected_generation)
                    .await
                {
                    Ok(()) => {}
                    Err(ReplanReplaceError::NotFound) => {
                        return Err(PlanError::DagNotFound(ctx.dag_id));
                    }
                    Err(ReplanReplaceError::GenerationMismatch { actual }) => {
                        return Err(PlanError::GenerationMismatch {
                            expected: expected_generation,
                            actual,
                        });
                    }
                    Err(ReplanReplaceError::DagBusy { state }) => {
                        return Err(PlanError::DagBusy { state });
                    }
                    Err(ReplanReplaceError::Store(e)) => return Err(PlanError::Store(e)),
                }
            }
        }

        // PlanProduced event
        // Reconciliation (deferred): an outbox / replay path SHOULD append a
        // missing PlanProduced for an already-persisted DAG generation when
        // append_session fails after a durable CAS write (row is retained).
        let mut node_ids: Vec<NodeId> = dag.nodes.keys().copied().collect();
        node_ids.sort();
        let payload = PlanProducedPayload {
            dag_id: dag.id,
            generation: dag.generation,
            template_id,
            snapshot_artifact,
            node_ids,
            replan,
            reason,
        };
        let payload_value = serde_json::to_value(&payload)
            .map_err(|e| PlanError::Internal(format!("PlanProducedPayload serde: {e}")))?;

        if let Err(e) = self
            .events
            .append_session(NewSessionEvent {
                session_id: ctx.session_id,
                run_id: Some(ctx.run_id),
                type_: SessionEventType::PlanProduced,
                payload: payload_value,
            })
            .await
        {
            tracing::warn!(error = %e, "PlanProduced append failed after durable DAG write");
            return Err(PlanError::Event(e));
        }

        tracing::info!(
            dag_id = %dag.id,
            generation = dag.generation,
            template = template_id.as_str(),
            replan,
            "plan produced"
        );

        Ok(PlanResult {
            dag,
            template_id,
            snapshot_artifact,
        })
    }

    async fn put_labeled(
        &self,
        bytes: Vec<u8>,
        ctx: &PlanContext,
        envelope: &str,
    ) -> Result<ArtifactId, PlanError> {
        let mut labels = serde_json::Map::new();
        labels.insert(
            "alloy.envelope".into(),
            serde_json::Value::String(envelope.into()),
        );
        labels.insert(
            "alloy.dag_id".into(),
            serde_json::Value::String(ctx.dag_id.to_string()),
        );
        self.artifacts
            .put(ArtifactPut {
                bytes,
                kind: ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                session_id: Some(ctx.session_id),
                run_id: Some(ctx.run_id),
                labels,
            })
            .await
            .map_err(PlanError::Artifact)
    }

    async fn put_input_artifacts(
        &self,
        manifest: &TemplateManifest,
        ids: &TemplateIdMap,
        ctx: &PlanContext,
        generation: u64,
    ) -> Result<BTreeMap<NodeId, ArtifactId>, PlanError> {
        // Collect Data predecessors per local name
        let mut data_preds: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &manifest.edges {
            if e.kind == EdgeKind::Data {
                data_preds
                    .entry(e.to.as_str())
                    .or_default()
                    .push(e.from.as_str());
            }
        }

        let mut input_refs = BTreeMap::new();
        for spec in &manifest.nodes {
            let node_id = *ids
                .nodes
                .get(&spec.name)
                .ok_or_else(|| PlanError::Internal(format!("missing id for {}", spec.name)))?;
            let preds = data_preds
                .get(spec.name.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            let payload = if preds.is_empty() {
                let has_sched_pred = manifest.edges.iter().any(|e| {
                    e.to == spec.name && matches!(e.kind, EdgeKind::Data | EdgeKind::Sequence)
                });
                if has_sched_pred {
                    NodeInputPayload::FromPredecessors { preds: vec![] }
                } else {
                    NodeInputPayload::Goal(ctx.goal.clone())
                }
            } else {
                let mut pred_outs = Vec::with_capacity(preds.len());
                for from_name in preds {
                    let from_id = *ids.nodes.get(*from_name).ok_or_else(|| {
                        PlanError::Internal(format!("missing pred id {from_name}"))
                    })?;
                    let from_kind = manifest
                        .nodes
                        .iter()
                        .find(|n| n.name == *from_name)
                        .map(|n| n.kind)
                        .ok_or_else(|| {
                            PlanError::Internal(format!("missing pred kind {from_name}"))
                        })?;
                    let pending_bytes = encode_json(&PendingPredPlaceholder::new())
                        .map_err(|e| PlanError::Internal(e.to_string()))?;
                    let pending_id = self.put_labeled(pending_bytes, ctx, "pending_pred").await?;
                    pred_outs.push(PredecessorOutput {
                        node_id: from_id,
                        kind: from_kind,
                        output_ref: pending_id,
                    });
                }
                NodeInputPayload::FromPredecessors { preds: pred_outs }
            };

            let envelope =
                NodeInputEnvelope::new(ctx.dag_id, node_id, spec.kind, generation, payload);
            let bytes = encode_json(&envelope)
                .map_err(|e| PlanError::Internal(format!("input envelope serde: {e}")))?;
            let art_id = self.put_labeled(bytes, ctx, "node_input").await?;
            input_refs.insert(node_id, art_id);
        }

        if input_refs.len() != ids.nodes.len() {
            return Err(PlanError::Internal(
                "Phase B input_refs missing coverage".into(),
            ));
        }
        Ok(input_refs)
    }
}

enum CasExpected {
    InsertOnly,
    Replan { expected_generation: u64 },
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
        self.instantiate_and_persist(template_id, &ctx, 1, None, CasExpected::InsertOnly)
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
        self.instantiate_and_persist(id, &ctx, 1, None, CasExpected::InsertOnly)
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

        self.instantiate_and_persist(
            template_id,
            &ctx,
            next_gen,
            Some(reason),
            CasExpected::Replan {
                expected_generation: probe.generation,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{
        mvp_compiler_fingerprint_digest, mvp_policy_hash_digest, mvp_tool_versions_digest,
        NodeInputEnvelope, NodeInputPayload, NodeKind, PendingPredPlaceholder,
    };
    use crate::events::InMemoryEventSink;
    use crate::storage::{
        AlloyStorage, ArtifactKind, ArtifactStore, DagStore, EventStore, StorageOpenOptions,
    };
    use std::sync::Mutex;

    fn fingerprints() -> (Digest, Digest, Digest) {
        (
            mvp_policy_hash_digest(),
            mvp_tool_versions_digest(),
            mvp_compiler_fingerprint_digest(),
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
}
