//! `LlmPlanService` — LLM-backed [`PlanService`], fail-closed onto the
//! template catalog (RFC-0017 §3.6 / §5.1, rules LP1–LP11 and FB1–FB7).
//!
//! Orchestrates propose → compile → persist. Everything model-derived is
//! untrusted until the proposal compiler's clamps and `DagValidator` accept
//! it (§2.5); any defect — model unavailable, malformed payload, clamp
//! violation, validation failure, budget denial, timeout — falls back to
//! `TemplatePlanService` with an audited `PlanProposal` decision record.
//! Cancellation is the one exception: a stop request is never answered by
//! starting work (FB2b).
//!
//! All persistence goes through `PlanPersistence::persist_validated` (LP2,
//! AM-0009-6); this service never touches `DagStore` write methods.
//!
//! Author: arkadianet

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::dag::{
    allocate_proposal_ids, compile_proposal, resolve_proposal, CompileArgs, ProposedDagManifest,
    TemplateId,
};
use crate::obs::{DecisionKind, DecisionLog, DecisionRecord};
use crate::scheduler::DagState;
use crate::session::ReplanReason;
use crate::storage::{ArtifactKind, ArtifactPut, ArtifactStore};
use crate::types::ids::ArtifactId;

use super::config::{PlannerConfig, PlannerMode};
use super::persist::{CasExpected, PersistRequest};
use super::proposer::{PlanProposer, ProposeError};
use super::template_service::{
    PlanContext, PlanError, PlanResult, PlanService, PlanSource, TemplatePlanService,
};

#[derive(Default)]
struct AtomicPlannerMetrics {
    proposals_accepted: AtomicU64,
    proposals_rejected: AtomicU64,
}

/// Snapshot of the §9.4 planner counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerMetrics {
    /// `planner.proposals_accepted`.
    pub proposals_accepted: u64,
    /// `planner.proposals_rejected`.
    pub proposals_rejected: u64,
}

/// LLM-backed [`PlanService`]. Delegates template selection and fallback to
/// [`TemplatePlanService`], and *all* persistence to the shared
/// `PlanPersistence` write path (LP2, AM-0009-6).
pub struct LlmPlanService {
    inner: TemplatePlanService,
    proposer: Arc<dyn PlanProposer>,
    artifacts: Arc<dyn ArtifactStore>,
    decisions: Arc<dyn DecisionLog>,
    cfg: PlannerConfig,
    metrics: AtomicPlannerMetrics,
}

impl LlmPlanService {
    /// Construct over the template service it falls back to.
    #[must_use]
    pub fn new(
        inner: TemplatePlanService,
        proposer: Arc<dyn PlanProposer>,
        artifacts: Arc<dyn ArtifactStore>,
        decisions: Arc<dyn DecisionLog>,
        cfg: PlannerConfig,
    ) -> Self {
        Self {
            inner,
            proposer,
            artifacts,
            decisions,
            cfg,
            metrics: AtomicPlannerMetrics::default(),
        }
    }

    /// §9.4 counters (accept/reject paths).
    #[must_use]
    pub fn metrics(&self) -> PlannerMetrics {
        PlannerMetrics {
            proposals_accepted: self.metrics.proposals_accepted.load(Ordering::Relaxed),
            proposals_rejected: self.metrics.proposals_rejected.load(Ordering::Relaxed),
        }
    }

    /// LP8/FB3/FB7 — the one `PlanProposal` decision per plan call.
    /// Best-effort per LP11: a failed record is logged and dropped, never a
    /// `PlanError` and never a fallback suppressor.
    async fn record_proposal_decision(&self, ctx: &PlanContext, metadata: serde_json::Value) {
        let record = DecisionRecord {
            session: ctx.session_id,
            run: Some(ctx.run_id),
            node: None,
            kind: DecisionKind::PlanProposal,
            metadata,
            content_hash: None,
            prompt_body: None, // §9.2 — planner-authored.
        };
        if let Err(e) = self.decisions.record(record).await {
            tracing::warn!(
                run_id = %ctx.run_id,
                dag_id = %ctx.dag_id,
                error = %e,
                "PlanProposal decision record failed (best-effort, LP11)"
            );
        }
    }

    /// LP4 — CAS the raw manifest JSON *before* compilation so rejected
    /// proposals stay auditable.
    async fn put_proposal_artifact(
        &self,
        ctx: &PlanContext,
        manifest: &ProposedDagManifest,
    ) -> Result<ArtifactId, PlanError> {
        let bytes = serde_json::to_vec(manifest)
            .map_err(|e| PlanError::Internal(format!("proposal serde: {e}")))?;
        let mut labels = serde_json::Map::new();
        labels.insert(
            "alloy.envelope".into(),
            serde_json::Value::String("plan_proposal".into()),
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

    /// FB1–FB7 — record the named trigger, then fall back to the template
    /// path. Fallback plans have `source = Template` (FB5); a fallback
    /// failure propagates template-path semantics unchanged (FB4).
    async fn fall_back(
        &self,
        ctx: PlanContext,
        rejected_reason: String,
        proposal_artifact: Option<ArtifactId>,
    ) -> Result<PlanResult, PlanError> {
        self.metrics
            .proposals_rejected
            .fetch_add(1, Ordering::Relaxed);
        let fallback_template = TemplatePlanService::select(&ctx);
        self.record_proposal_decision(
            &ctx,
            serde_json::json!({
                "dag_id": ctx.dag_id,
                "generation": 1_u64,
                "accepted": false,
                "rejected_reason": rejected_reason,
                "proposal_artifact": proposal_artifact,
                "fallback_template": fallback_template.as_str(),
            }),
        )
        .await;
        self.inner.plan(ctx).await
    }

    /// Compile an accepted manifest and persist it through the single
    /// validated write path (LP5) at `generation` with `reason`/CAS mode.
    // The argument list mirrors `PersistRequest`'s fields one-to-one; a
    // bundling struct would only restate that shape.
    #[allow(clippy::too_many_arguments)]
    async fn compile_and_persist(
        &self,
        ctx: &PlanContext,
        manifest: &ProposedDagManifest,
        proposal_artifact: ArtifactId,
        generation: u64,
        reason: Option<&ReplanReason>,
        expected_for_cas: CasExpected,
        probe_kinds: Option<
            &std::collections::BTreeMap<crate::types::ids::NodeId, crate::dag::NodeKind>,
        >,
    ) -> Result<Result<PlanResult, crate::dag::ProposalRejection>, PlanError> {
        // LP5 — allocate ids, run the full compiler (PC1–PC14 including the
        // pre-CAS `DagValidator` pass over ephemeral refs), then hand the
        // resource-assigned specs to `persist_validated`, which re-runs the
        // three phases and re-validates with real refs before the CAS.
        // §9.1 `planner.compile` — the pipeline is pure/sync, so a plain
        // entered span suffices (no await crosses it before the persist).
        let compile_span = tracing::info_span!(
            "planner.compile",
            dag_id = %ctx.dag_id,
            node_count = manifest.nodes.len(),
            rejection_variant = tracing::field::Empty,
        );
        let compile_guard = compile_span.enter();
        let ids = match allocate_proposal_ids(manifest) {
            Ok(ids) => ids,
            Err(rej) => {
                compile_span.record("rejection_variant", tracing::field::debug(&rej));
                return Ok(Err(rej));
            }
        };
        let mut ephemeral = std::collections::BTreeMap::new();
        for nid in ids.nodes.values() {
            ephemeral.insert(*nid, ArtifactId::new());
        }
        if let Err(rej) = compile_proposal(
            manifest,
            CompileArgs {
                dag_id: ctx.dag_id,
                session_id: ctx.session_id,
                generation,
                ids: &ids,
                input_refs: &ephemeral,
                cfg: &self.cfg,
            },
        ) {
            compile_span.record("rejection_variant", tracing::field::debug(&rej));
            return Ok(Err(rej));
        }
        let (specs, edges) = match resolve_proposal(manifest, &self.cfg) {
            Ok(pair) => pair,
            Err(rej) => {
                compile_span.record("rejection_variant", tracing::field::debug(&rej));
                return Ok(Err(rej));
            }
        };
        drop(compile_guard);
        let template_id = TemplatePlanService::select(ctx);
        let result = self
            .inner
            .persistence()
            .persist_validated(PersistRequest {
                ctx,
                specs: &specs,
                edges: &edges,
                source: PlanSource::LlmProposed,
                template_id,
                proposal_artifact: Some(proposal_artifact),
                reason,
                generation,
                expected_for_cas,
                probe_kinds,
            })
            .await?;
        Ok(Ok(result))
    }
}

/// LP3's outcome classification for the outer bound.
async fn propose_bounded(
    proposer: &Arc<dyn PlanProposer>,
    cfg: &PlannerConfig,
    ctx: &PlanContext,
) -> Result<ProposedDagManifest, ProposeError> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(cfg.planning_timeout_ms),
        proposer.propose(ctx),
    )
    .await
    {
        Ok(result) => result,
        // The proposer's own PP5 mapping classifies token-fired timeouts as
        // `Cancelled`; an outer elapse without a fired token is a timeout.
        Err(_elapsed) => Err(ProposeError::Timeout),
    }
}

#[async_trait]
impl PlanService for LlmPlanService {
    /// §5.1 LP1–LP11.
    async fn plan(&self, ctx: PlanContext) -> Result<PlanResult, PlanError> {
        // LP1 — template mode delegates unchanged (constructed this way
        // only in tests; production selects the service by mode).
        if self.cfg.mode == PlannerMode::Template {
            return self.inner.plan(ctx).await;
        }
        // §9.1 `planner.propose` — outcome and sizes only; never goal text,
        // never rationale.
        let propose_span = tracing::info_span!(
            "planner.propose",
            session_id = %ctx.session_id,
            run_id = %ctx.run_id,
            dag_id = %ctx.dag_id,
            outcome = tracing::field::Empty,
            node_count = tracing::field::Empty,
            bytes = tracing::field::Empty,
        );
        let proposed = {
            use tracing::Instrument as _;
            propose_bounded(&self.proposer, &self.cfg, &ctx)
                .instrument(propose_span.clone())
                .await
        };
        if let Ok(manifest) = &proposed {
            propose_span.record("node_count", manifest.nodes.len());
            propose_span.record(
                "bytes",
                serde_json::to_vec(manifest).map(|b| b.len()).unwrap_or(0),
            );
        }
        let manifest = match proposed {
            Ok(manifest) => manifest,
            // FB2b — cancellation propagates; no plan, no DAG row, no
            // fallback: a stop request is never answered by starting work.
            // LP8 still holds: the one decision names the stop.
            Err(ProposeError::Cancelled) => {
                propose_span.record("outcome", "unavailable");
                self.record_proposal_decision(
                    &ctx,
                    serde_json::json!({
                        "dag_id": ctx.dag_id,
                        "generation": 1_u64,
                        "accepted": false,
                        "rejected_reason": "cancelled",
                    }),
                )
                .await;
                return Err(PlanError::Internal("cancelled".into()));
            }
            // FB2 — every other ProposeError falls back.
            Err(e) => {
                propose_span.record("outcome", "unavailable");
                return self.fall_back(ctx, e.to_string(), None).await;
            }
        };
        // LP4 — auditable before compilation.
        let proposal_artifact = self.put_proposal_artifact(&ctx, &manifest).await?;
        match self
            .compile_and_persist(
                &ctx,
                &manifest,
                proposal_artifact,
                1,
                None,
                CasExpected::InsertOnly,
                None,
            )
            .await?
        {
            Ok(result) => {
                propose_span.record("outcome", "accepted");
                self.metrics
                    .proposals_accepted
                    .fetch_add(1, Ordering::Relaxed);
                self.record_proposal_decision(
                    &ctx,
                    serde_json::json!({
                        "dag_id": ctx.dag_id,
                        "generation": 1_u64,
                        "accepted": true,
                        "node_count": manifest.nodes.len(),
                        "proposal_artifact": proposal_artifact,
                    }),
                )
                .await;
                Ok(result)
            }
            Err(rejection) => {
                propose_span.record("outcome", "rejected");
                self.fall_back(ctx, rejection.to_string(), Some(proposal_artifact))
                    .await
            }
        }
    }

    /// LP6 — an explicit template request is never second-guessed by a
    /// model.
    async fn load_template(
        &self,
        id: TemplateId,
        ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        self.inner.load_template(id, ctx).await
    }

    /// §5.5 GN10 — source-preserving replan: a proposal-sourced run
    /// re-compiles **the same stored proposal manifest** at the new
    /// generation (repair generations change *inputs*, not *shape*);
    /// everything else delegates to the template path.
    async fn replan(
        &self,
        reason: ReplanReason,
        ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        let (Some(PlanSource::LlmProposed), Some(artifact)) =
            (ctx.prior_source, ctx.prior_proposal_artifact)
        else {
            return self.inner.replan(reason, ctx).await;
        };
        // Fetch and decode the stored manifest. GN10 forbids silent
        // re-selection: a missing or undecodable stored proposal is an
        // internal error, not a quiet template replan.
        let blob = self
            .artifacts
            .get(artifact)
            .await
            .map_err(PlanError::Artifact)?;
        let manifest: ProposedDagManifest = serde_json::from_slice(&blob.bytes).map_err(|e| {
            PlanError::Internal(format!("stored proposal {artifact} undecodable: {e}"))
        })?;

        // Mirror the template replan probe (session check, busy preflight,
        // generation bump, SD3 kind map) — reads only.
        let dags = self.inner.dag_store();
        let probe = dags
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
        let probe_kinds: std::collections::BTreeMap<_, _> = probe
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.kind))
            .collect();

        match self
            .compile_and_persist(
                &ctx,
                &manifest,
                artifact,
                next_gen,
                Some(&reason),
                CasExpected::Replan {
                    expected_generation: probe.generation,
                },
                Some(&probe_kinds),
            )
            .await?
        {
            Ok(result) => Ok(result),
            // The stored manifest compiled when it was accepted; a rejection
            // now means the config or clamps changed underneath it. GN10:
            // never silently re-select — surface it.
            Err(rejection) => Err(PlanError::Internal(format!(
                "stored proposal no longer compiles: {rejection}"
            ))),
        }
    }
}
