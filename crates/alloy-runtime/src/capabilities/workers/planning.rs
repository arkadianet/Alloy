//! `PlanningWorker` (id `planning`, kind `Plan`) — deterministic, LLM-free
//! (RFC-0013 §9.4, rules PW1–PW5).
//!
//! Registered-but-unreached in the MVP path (PW3): no MVP template contains
//! a `Plan` node; it exists so kind resolution succeeds rather than failing
//! closed if a future template adds one. Topology has exactly one writer
//! (V2 §6.4, ADR F-03): this worker proposes a template id in its payload
//! and never writes a DAG (PW2, AM-0009-1).

use async_trait::async_trait;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::dag::{NodeKind, TemplateId};
use crate::types::budget::ModelTier;
use crate::types::ids::CapabilityId;
use crate::types::tools::ToolSelector;

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::payload::{PlanningProposalPayload, PAYLOAD_SCHEMA_VERSION};
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{finish_attempt, worker_span, Attempt, WorkerError, WorkerSuccess};

/// Deterministic template-selection proposal worker (PW1).
#[derive(Debug, Clone)]
pub struct PlanningWorker {
    #[allow(dead_code)] // knobs are unused by the deterministic MVP body.
    config: WorkerConfig,
}

impl PlanningWorker {
    /// Construct with worker knobs (unused in the deterministic MVP body,
    /// kept for constructor symmetry with the LLM workers).
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
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
        CapabilityDescriptor {
            id: Self::capability_id(),
            version: Self::VERSION,
            summary: "Deterministic template-selection proposal (no model call)".into(),
            uses_model: false, // PW1.
            side_effects: SideEffectClass::Pure,
            kinds: vec![NodeKind::Plan],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        Vec::new() // PW1: no tools.
    }

    fn preferred_tier(&self) -> ModelTier {
        // Advisory only (MR2); irrelevant while `uses_model == false`.
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
        let attempt = Attempt::new(self.preferred_tier(), ctx.effective_tier);
        let result = self.run(ctx, &attempt);
        finish_attempt(ctx, &self.describe(), &attempt, result, &span).await
    }
}

impl PlanningWorker {
    /// No model call, no tool call, no filesystem access (PW1/CW6): the
    /// selection mirrors the RFC-0009 MVP rule — every goal maps to the sole
    /// catalog template. Enabling an LLM planner is an RFC amendment, not a
    /// flag (PW5).
    fn run(
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
}
