//! `PlanningWorker` (id `planning`, kind `Plan`) — RFC-0013 §9.4 as amended
//! by RFC-0017 AM-0013-1/2/3 (PW5's sanctioned enablement path).
//!
//! Two constructed variants, reported truthfully per instance:
//!
//! - **Deterministic** ([`PlanningWorker::new`]) — exactly the pre-0017
//!   body: no model call, no tool call, `Pure`, sole-template selection.
//!   Registered whenever `planner.mode = "template"`.
//! - **Model branch** ([`PlanningWorker::new_model`]) — `uses_model = true`,
//!   `SideEffectClass::ReadOnly` (AM-0013-3: a model completion reads and
//!   never writes), proposes a [`ProposedDagManifest`] via the RFC-0013
//!   house exchange (structured-output-first, one repair turn, bounded by
//!   `max_model_turns`). Reached only through the `CapabilityExecutor` seam
//!   by `LlmPlanService`'s proposer — never via a DAG node (PW3 amended).
//!
//! Topology has exactly one writer (V2 §6.4, ADR F-03): this worker
//! proposes in its payload and never writes a DAG (PW2 retained verbatim).
//! It performs **no clamping** — containment is the proposal compiler's, so
//! it holds even against a compromised worker (RFC-0017 SEC5).

use async_trait::async_trait;
use serde::Deserialize;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::{NodeKind, ProposedDagManifest, ProposedNodeSpec, TemplateId};
use crate::types::budget::ModelTier;
use crate::types::ids::CapabilityId;
use crate::types::tools::ToolSelector;

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::payload::{
    clamp_string, exceeds_total_bound, PlanningProposalPayload, MAX_PAYLOAD_STRING_BYTES,
    PAYLOAD_SCHEMA_VERSION,
};
use super::super::prompt::PLANNING_SYSTEM;
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{finish_attempt, llm_exchange, worker_span, Attempt, WorkerError, WorkerSuccess};

/// Model reply schema (PS5: `deny_unknown_fields`): the manifest fields plus
/// an optional confidence the wire manifest does not carry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanningReply {
    schema_version: u32,
    nodes: Vec<ProposedNodeSpec>,
    rationale: String,
    #[serde(default)]
    confidence: Option<f32>,
}

/// Template-selection / chain-proposal worker.
#[derive(Debug, Clone)]
pub struct PlanningWorker {
    config: WorkerConfig,
    /// `true` only for [`Self::new_model`] — set from `planner.mode` by the
    /// composition root (AM-0013-1), never a flag on this worker's own
    /// config (PW5).
    model_mode: bool,
}

impl PlanningWorker {
    /// Deterministic branch (PW1): no model call, no tool call.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self {
            config,
            model_mode: false,
        }
    }

    /// Model branch (RFC-0017 AM-0013-1): proposes a chain via one bounded
    /// model exchange. Constructed only when `planner.mode = "llm"`.
    #[must_use]
    pub fn new_model(config: WorkerConfig) -> Self {
        Self {
            config,
            model_mode: true,
        }
    }

    const VERSION: CapabilityVersion = CapabilityVersion::new(1, 0, 0);

    fn capability_id() -> CapabilityId {
        CapabilityId::new("planning").expect("static id")
    }
}

#[async_trait]
impl Capability for PlanningWorker {
    fn id(&self) -> CapabilityId {
        Self::capability_id()
    }

    fn version(&self) -> CapabilityVersion {
        Self::VERSION
    }

    fn describe(&self) -> CapabilityDescriptor {
        // AM-0013-3: `describe()` is per-instance, so the constructed
        // variant reports truthfully — `Pure` would lie on the model branch
        // (`Pure` is documented as "no tool call, no model call").
        let (summary, uses_model, side_effects) = if self.model_mode {
            (
                "Model-backed linear chain proposal (compiled and clamped by the runtime)",
                true,
                SideEffectClass::ReadOnly,
            )
        } else {
            (
                "Deterministic template-selection proposal (no model call)",
                false,
                SideEffectClass::Pure,
            )
        };
        CapabilityDescriptor {
            id: Self::capability_id(),
            version: Self::VERSION,
            summary: summary.into(),
            uses_model,
            side_effects,
            kinds: vec![NodeKind::Plan],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        Vec::new() // PW-C: stays [] on both branches.
    }

    fn preferred_tier(&self) -> ModelTier {
        // Advisory only (MR2); the planner invokes at Standard (PP2).
        ModelTier::Economy
    }

    fn accepts_kind(&self, kind: NodeKind) -> bool {
        kind == NodeKind::Plan
    }

    async fn execute(
        &self,
        ctx: &CapabilityContext<'_>,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        let span = worker_span(ctx);
        let mut attempt = Attempt::new(self.preferred_tier(), ctx.effective_tier);
        let result = if self.model_mode {
            use tracing::Instrument;
            self.run_model(ctx, &mut attempt)
                .instrument(span.clone())
                .await
        } else {
            self.run_deterministic(ctx, &attempt)
        };
        finish_attempt(ctx, &self.describe(), &attempt, result, &span).await
    }
}

impl PlanningWorker {
    /// Deterministic branch — byte-identical to the pre-0017 body (PW1
    /// re-scoped by AM-0013-1): every goal maps to the sole catalog
    /// template; no model call, no tool call, no filesystem access.
    fn run_deterministic(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &Attempt,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }
        let template = TemplateId::RepairLocalDiagnostic;
        let payload = PlanningProposalPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "planning".into(),
            template_id: template.as_str().to_owned(),
            rationale: "sole MVP catalog template; deterministic selection".into(),
            replan_requested: false, // PW2/PW4.
            truncated: false,
            confidence: 1.0, // deterministic selection (PW4).
            citations: vec![],
            artifacts: vec![],
            metrics: attempt.metrics(ctx, None),
            proposal: None, // AM-0013-2: absent ⇒ deterministic selection.
        };
        let payload = serde_json::to_value(&payload).map_err(|e| {
            // CW10: serializing an owned struct cannot fail; treat as an
            // invariant break.
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "payload serialization failed: {e}"
            )))
        })?;
        Ok(WorkerSuccess {
            payload,
            confidence: 1.0,
        })
    }

    /// Model branch (PW-B/PW-D): one house exchange, no clamping.
    async fn run_model(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }
        let inputs = AssembleInputs {
            run: Some(ctx.run),
            input: Some(ctx.input.clone()),
            diagnostics: vec![],
            budget: Some(ctx.budget.clone()),
            focus_paths: vec![],
        };
        let (reply, _pack) = llm_exchange(
            ctx,
            attempt,
            &self.config,
            PLANNING_SYSTEM,
            &inputs,
            &[],
            |value| {
                let reply: PlanningReply =
                    serde_json::from_value(value.clone()).map_err(|e| format!("schema: {e}"))?;
                Ok(reply)
            },
        )
        .await?;

        // RW7 semantics: model confidence clamped; absent ⇒ 0.5.
        let confidence = reply.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
        let manifest = ProposedDagManifest {
            schema_version: reply.schema_version,
            nodes: reply.nodes,
            rationale: reply.rationale,
        };
        // Audit copy of the rationale, payload-bounded; the manifest itself
        // passes through verbatim — containment is the compiler's (SEC5).
        let mut rationale = manifest.rationale.clone();
        let rationale_cut = clamp_string(&mut rationale, MAX_PAYLOAD_STRING_BYTES);

        let mut payload = PlanningProposalPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "planning".into(),
            // PW-D: the day-1 selector's answer as fallback identity.
            template_id: TemplateId::RepairLocalDiagnostic.as_str().to_owned(),
            rationale,
            replan_requested: false, // PW2.
            truncated: rationale_cut,
            confidence,
            citations: attempt.citations.clone(),
            artifacts: vec![],
            metrics: attempt.metrics(ctx, None),
            proposal: Some(manifest),
        };
        let mut value = serde_json::to_value(&payload).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "payload serialization failed: {e}" // CW10.
            )))
        })?;
        // OC7 total bound, fail closed: drop the proposal rather than ship
        // an oversize payload (the planner then falls back to templates).
        if exceeds_total_bound(&value) {
            payload.proposal = None;
            payload.truncated = true;
            value = serde_json::to_value(&payload).map_err(|e| {
                WorkerError::Host(CapabilityExecError::Internal(format!(
                    "payload serialization failed: {e}"
                )))
            })?;
        }
        Ok(WorkerSuccess {
            payload: value,
            confidence,
        })
    }
}
