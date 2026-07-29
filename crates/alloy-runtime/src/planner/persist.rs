//! `PlanPersistence` — the single validated plan/replan write path
//! (RFC-0017 §3.5b, AM-0009-6).
//!
//! Absorbs `TemplatePlanService`'s private three-phase machinery behind one
//! named, crate-private API so a second plan service (RFC-0017's
//! `LlmPlanService`) can write topology without a second write path. Owns the
//! RFC-0009 order: build → pre-CAS validate with ephemeral input refs →
//! Phase B input puts → re-validate with real refs → CAS
//! (`put_if_generation` for gen 1, `replace_for_replan` for gen N+1) →
//! snapshot artifact → `PlanProduced`.
//!
//! PS3: deliberately `pub(crate)` and never re-exported — a public
//! DAG-writing seam would be a second topology writer in exactly the sense
//! V2 §6.4 forbids. PS4: the SD1–SD10 root-seeding decision lives here, not
//! in either plan service, so every `FailureIr` replan is seeded regardless
//! of which service ran.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dag::{
    allocate_ids, build_topology, encode_json, BuildTopology, DagValidator, EdgeKind,
    NodeInputEnvelope, NodeInputPayload, NodeKind, NodeOutputEnvelope, PendingPredPlaceholder,
    PredecessorOutput, TemplateEdgeSpec, TemplateId, TemplateIdMap, TemplateManifest,
    TemplateNodeSpec, ValidateOpts,
};
use crate::events::{EventSink, NewSessionEvent, SessionEventType};
use crate::session::ReplanReason;
use crate::storage::{
    ArtifactKind, ArtifactPut, ArtifactStore, DagStore, ReplanReplaceError, StoreError,
};
use crate::types::ids::{ArtifactId, NodeId};

use super::seed::project_failure;
use super::template_service::{
    PlanContext, PlanError, PlanProducedPayload, PlanResult, PlanSource,
};

/// A resource-assigned node spec ready for persistence. A template
/// instantiation and a compiled proposal are indistinguishable here by
/// design (AM-0009-6): both arrive as the same spec shape `build_topology`
/// consumes.
pub(crate) type ResolvedNodeSpec = TemplateNodeSpec;

/// A resolved edge spec by local node name (dual Data+Sequence convention).
pub(crate) type ResolvedEdgeSpec = TemplateEdgeSpec;

/// CAS expectation for the durable write.
pub(crate) enum CasExpected {
    /// Generation 1: insert-only (`put_if_generation(dag, None)`).
    InsertOnly,
    /// Generation N+1: atomic replace guarded on the prior generation.
    Replan {
        /// Generation the store must still hold.
        expected_generation: u64,
    },
}

/// What to persist (RFC-0017 §3.5b). `specs`/`edges` are already
/// resource-assigned; `reason` drives the CAS mode's seeding (SD1–SD10) and
/// `probe_kinds` supplies the prior generation's node kinds for SD3's kind
/// lookup.
pub(crate) struct PersistRequest<'a> {
    /// Planning context.
    pub ctx: &'a PlanContext,
    /// Resource-assigned node specs.
    pub specs: &'a [ResolvedNodeSpec],
    /// Edges by local name.
    pub edges: &'a [ResolvedEdgeSpec],
    /// Provenance recorded on `PlanResult` / `PlanProduced`; never affects
    /// the write.
    pub source: PlanSource,
    /// Template identity (fallback identity for compiled proposals, LP5).
    pub template_id: TemplateId,
    /// Raw proposal artifact when `source` is `LlmProposed`.
    pub proposal_artifact: Option<ArtifactId>,
    /// `None` for generation 1; `Some(FailureIr)` additionally drives the
    /// SD1–SD10 root seeding.
    pub reason: Option<&'a ReplanReason>,
    /// Generation to stamp.
    pub generation: u64,
    /// CAS mode.
    pub expected_for_cas: CasExpected,
    /// Prior generation's node kinds (from the replan probe blob) for SD3.
    pub probe_kinds: Option<&'a BTreeMap<NodeId, NodeKind>>,
}

/// The only code in the workspace that writes a DAG row for a plan or replan
/// (PS1; CI grep AC 46).
pub(crate) struct PlanPersistence {
    dags: Arc<dyn DagStore>,
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<dyn EventSink>,
}

impl PlanPersistence {
    /// Construct over the plan stores.
    pub(crate) fn new(
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

    /// Validates (`DagValidator::validate`) before every write and returns
    /// `PlanError::Validation` on failure. There is no argument that skips
    /// validation and no constructor that accepts a pre-built `TaskDag`
    /// (RFC-0017 §8.3).
    pub(crate) async fn persist_validated(
        &self,
        req: PersistRequest<'_>,
    ) -> Result<PlanResult, PlanError> {
        let replan = matches!(req.expected_for_cas, CasExpected::Replan { .. });
        // The manifest is a carrier for `build_topology`; the description is
        // never consumed downstream.
        let manifest = TemplateManifest {
            id: req.template_id,
            description: String::new(),
            nodes: req.specs.to_vec(),
            edges: req.edges.to_vec(),
        };
        let ctx = req.ctx;
        let generation = req.generation;

        // Phase A
        let ids = allocate_ids(&manifest);

        // Pre-CAS validate with ephemeral placeholders
        let mut placeholders = BTreeMap::new();
        for nid in ids.nodes.values() {
            placeholders.insert(*nid, ArtifactId::new());
        }
        let candidate = build_topology(BuildTopology {
            manifest: &manifest,
            dag_id: ctx.dag_id,
            session_id: ctx.session_id,
            generation,
            ids: &ids,
            input_refs: &placeholders,
        });
        let opts = validate_opts_for(&manifest);
        if let Err(e) = DagValidator::validate(&candidate, opts) {
            tracing::warn!(error = %e, "plan validation failed");
            return Err(PlanError::Validation(e));
        }

        // Phase B — real CAS puts
        let (input_refs, seeded_root) = self.put_input_artifacts(&manifest, &ids, &req).await?;

        // Phase C
        let dag = build_topology(BuildTopology {
            manifest: &manifest,
            dag_id: ctx.dag_id,
            session_id: ctx.session_id,
            generation,
            ids: &ids,
            input_refs: &input_refs,
        });
        // Re-validate after real input_refs are bound. Validator does not inspect
        // ArtifactId values, so this cannot fail if Phase A passed; kept as insurance.
        DagValidator::validate(&dag, opts)?;

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
        match req.expected_for_cas {
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
            template_id: req.template_id,
            snapshot_artifact,
            node_ids,
            replan,
            reason: req.reason.cloned(),
            source: Some(req.source),
            proposal_artifact: req.proposal_artifact,
            seeded_root: if replan { Some(seeded_root) } else { None },
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
            template = req.template_id.as_str(),
            replan,
            seeded_root,
            "plan produced"
        );

        Ok(PlanResult {
            dag,
            template_id: req.template_id,
            snapshot_artifact,
            source: req.source,
            proposal_artifact: req.proposal_artifact,
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

    /// SD3/SD5: put the sanitized seed predecessor artifact and return the
    /// root's `FromPredecessors` payload for a `FailureIr` replan.
    async fn put_seed(
        &self,
        req: &PersistRequest<'_>,
        f: &crate::types::diagnostic::FailureIr,
    ) -> Result<NodeInputPayload, PlanError> {
        // SD3: failed node's kind from the replan probe blob; an absent node
        // (failure from an older generation) defaults to `VerifyCompile`.
        let kind = req
            .probe_kinds
            .and_then(|kinds| kinds.get(&f.node).copied())
            .unwrap_or(NodeKind::VerifyCompile);
        let projected = project_failure(f);
        let payload_value = serde_json::to_value(&projected)
            .map_err(|e| PlanError::Internal(format!("seed payload serde: {e}")))?;
        // SD3: generation = the prior generation, attempt = 1.
        let prior_generation = req.generation.saturating_sub(1);
        let envelope = NodeOutputEnvelope::new(
            req.ctx.dag_id,
            f.node,
            kind,
            prior_generation,
            1,
            payload_value,
        );
        let bytes = encode_json(&envelope)
            .map_err(|e| PlanError::Internal(format!("seed envelope serde: {e}")))?;
        let seed_id = self.put_labeled(bytes, req.ctx, "replan_seed").await?;
        // SD5: exactly one synthetic predecessor; readers never resolve its
        // node id against the current node map.
        Ok(NodeInputPayload::FromPredecessors {
            preds: vec![PredecessorOutput {
                node_id: f.node,
                kind,
                output_ref: seed_id,
            }],
        })
    }

    /// Phase B: put every node's plan-time input artifact. Returns the ref
    /// map and whether the root was seeded (SD1–SD10).
    async fn put_input_artifacts(
        &self,
        manifest: &TemplateManifest,
        ids: &TemplateIdMap,
        req: &PersistRequest<'_>,
    ) -> Result<(BTreeMap<NodeId, ArtifactId>, bool), PlanError> {
        let ctx = req.ctx;
        let generation = req.generation;

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

        let mut seeded_root = false;
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
                } else if let Some(ReplanReason::FailureIr(f)) = req.reason {
                    // SD1/SD5: the root of a `FailureIr` replan receives the
                    // seed envelope instead of the bare goal. SD6: root
                    // identification is unchanged (zero Data∪Sequence
                    // predecessors), so exactly one node is seeded.
                    seeded_root = true;
                    self.put_seed(req, f).await?
                } else {
                    // SD2: non-FailureIr reasons are byte-identical to the
                    // pre-RFC-0017 shape.
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
        Ok((input_refs, seeded_root))
    }
}

/// Validation options for a manifest: a read-only template carries no gate
/// to require (see [`TemplateManifest::is_read_only`]).
pub(crate) fn validate_opts_for(manifest: &TemplateManifest) -> ValidateOpts {
    ValidateOpts {
        enforce_linear_mvp: true,
        require_gates: !manifest.is_read_only(),
    }
}
