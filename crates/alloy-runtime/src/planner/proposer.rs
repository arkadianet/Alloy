//! `PlanProposer` — the seam between `LlmPlanService` and the `planning`
//! capability (RFC-0017 §3.6 / §5.3.1, rules PP1–PP6).
//!
//! The production impl drives the RFC-0010 `CapabilityExecutor` with a
//! synthetic Plan-node context, so router binding, run-scoped metering,
//! budget admission, and cancellation behave exactly as for scheduled nodes
//! (PP4). It holds no prompt: prompts live in the worker (RFC-0013).
//!
//! Author: arkadianet

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::adapters::{
    CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityOutcome, NodeExecRef,
};
use crate::capabilities::PlanningProposalPayload;
use crate::dag::{NodeInputEnvelope, NodeInputPayload, NodeKind, ProposedDagManifest};
use crate::obs::{BudgetCheck, CostMeterFactory};
use crate::types::budget::{BudgetPolicy, ModelTier};
use crate::types::diagnostic::ErrorClass;
use crate::types::ids::{CapabilityId, NodeId};

use super::config::PlannerConfig;
use super::template_service::PlanContext;
use std::sync::Arc;

/// Seam between the plan service and the planning capability. Exactly one
/// production impl ([`CapabilityPlanProposer`]); tests inject scripted
/// proposers.
#[async_trait]
pub trait PlanProposer: Send + Sync {
    /// Obtain a proposal for `ctx.goal`. `Err` values are *fallback
    /// triggers*, never run failures — except [`ProposeError::Cancelled`],
    /// which propagates (FB2b).
    async fn propose(&self, ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError>;
}

/// Why a propose call yielded no manifest (RFC-0017 §3.6).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProposeError {
    /// Registry resolve failure, router down, model 5xx.
    #[error("planning capability unavailable: {0}")]
    Unavailable(String),
    /// Completion error after admission.
    #[error("planning call failed: {0}")]
    Model(String),
    /// Payload missing / undecodable / carrying no proposal.
    #[error("proposal payload malformed: {0}")]
    Malformed(String),
    /// Planning budget denied (FB6; never retried at a lower tier — BG4).
    #[error("planning budget denied")]
    Budget,
    /// Planning call deadline elapsed.
    #[error("planning timed out")]
    Timeout,
    /// The run's token fired — NOT a fallback trigger (FB2b).
    #[error("cancelled")]
    Cancelled,
}

/// Everything `CapabilityExecContext` requires that a [`PlanContext`] cannot
/// supply (RFC-0017 §3.6, blocker 5).
pub struct ProposerDeps {
    /// From `Session.workspace_root` — the same value the scheduler puts on
    /// `NodeExecRef` at dispatch. The proposer MUST NOT read the process
    /// CWD (PP1).
    pub workspace_root: PathBuf,
    /// The run-scoped token (in production, the runtime's token), so a
    /// cancel aborts an in-flight planning call exactly as it aborts a node.
    pub cancellation: CancellationToken,
    /// The **run's** meter source, not a fresh meter: the planning call's
    /// tokens and USD are charged to the run (PP4/FB6). Resolved per
    /// `ctx.run_id` at propose time — the same factory the scheduler and
    /// router share, so RFC-0013 BG1/BG2 hold (the router bound to the
    /// meter records the usage; the proposer never writes it).
    pub cost_meters: Arc<dyn CostMeterFactory>,
    /// Run-level ceilings for the pre-call admission check (FB6).
    pub budget_policy: BudgetPolicy,
}

impl std::fmt::Debug for ProposerDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposerDeps")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

/// Production proposer: drives the `planning` capability through the
/// RFC-0010 `CapabilityExecutor` seam with a synthetic Plan-node context
/// (§5.3.1).
pub struct CapabilityPlanProposer {
    executor: Arc<dyn CapabilityExecutor>,
    deps: ProposerDeps,
    cfg: PlannerConfig,
}

impl CapabilityPlanProposer {
    /// Construct over the production executor and its run-context deps.
    #[must_use]
    pub fn new(
        executor: Arc<dyn CapabilityExecutor>,
        deps: ProposerDeps,
        cfg: PlannerConfig,
    ) -> Self {
        Self {
            executor,
            deps,
            cfg,
        }
    }

    /// PP5 mapping for `Failed` capability outcomes.
    fn map_failure_class(class: ErrorClass, notes: String) -> ProposeError {
        match class {
            ErrorClass::Budget => ProposeError::Budget,
            ErrorClass::Timeout => ProposeError::Timeout,
            ErrorClass::Cancelled => ProposeError::Cancelled,
            _ => ProposeError::Model(notes),
        }
    }
}

#[async_trait]
impl PlanProposer for CapabilityPlanProposer {
    async fn propose(&self, ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError> {
        let meter = self.deps.cost_meters.meter_for(ctx.run_id);
        // FB6 — pre-call admission: a denied budget never reaches the model
        // and is never retried at a lower tier (BG4).
        if meter.check_budget(&self.deps.budget_policy) != BudgetCheck::Ok {
            return Err(ProposeError::Budget);
        }
        // PP1: fresh NodeId (the node exists in no DAG — `Plan` nodes stay
        // absent from all persisted topologies), attempt = 1, workspace root
        // from deps.
        let node_id = NodeId::new();
        let meta = NodeExecRef {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            dag_id: ctx.dag_id,
            node_id,
            workspace_root: self.deps.workspace_root.clone(),
            attempt: 1,
        };
        // PP3: the same root shape workers already parse.
        let input = NodeInputEnvelope::new(
            ctx.dag_id,
            node_id,
            NodeKind::Plan,
            1,
            NodeInputPayload::Goal(ctx.goal.clone()),
        );
        // PP1b/PP2 — every field supplied, none defaulted or fabricated.
        let exec_ctx = CapabilityExecContext {
            meta,
            cancellation: self.deps.cancellation.clone(),
            capability: CapabilityId::new("planning").expect("static id"),
            kind: NodeKind::Plan,
            effective_tier: ModelTier::Standard,
            budget: self.cfg.planning_budget.clone(),
            timeout: Duration::from_millis(self.cfg.planning_timeout_ms),
            input,
            attempt: 1,
            cost_meter: meter,
            // Always attempt 1 (PP1): there is no prior attempt to remember.
            prior_failure: None,
        };

        let outcome = self.executor.execute(&exec_ctx).await;
        // PP5: a fired token classifies as Cancelled even when the
        // observable failure was a timeout.
        let cancelled = self.deps.cancellation.is_cancelled();
        match outcome {
            Err(CapabilityExecError::Cancelled) => Err(ProposeError::Cancelled),
            Err(CapabilityExecError::Timeout) if cancelled => Err(ProposeError::Cancelled),
            Err(CapabilityExecError::Timeout) => Err(ProposeError::Timeout),
            Err(e @ (CapabilityExecError::Unavailable | CapabilityExecError::Internal(_))) => {
                Err(ProposeError::Unavailable(e.to_string()))
            }
            Err(e @ CapabilityExecError::Worker(_)) => {
                Err(ProposeError::Unavailable(e.to_string()))
            }
            Ok(CapabilityOutcome::Failed { failure }) if cancelled => {
                let _ = failure;
                Err(ProposeError::Cancelled)
            }
            Ok(CapabilityOutcome::Failed { failure }) => Err(Self::map_failure_class(
                failure.error_class,
                failure.notes.clone(),
            )),
            Ok(CapabilityOutcome::Succeeded { payload }) => {
                // PC2 — the raw payload-borne proposal bytes are bounded
                // before decode; the compiler re-checks on its canonical
                // serialization.
                if let Some(proposal) = payload.get("proposal") {
                    let bytes = serde_json::to_vec(proposal).map(|v| v.len()).unwrap_or(0);
                    if bytes > self.cfg.proposal_max_bytes as usize {
                        return Err(ProposeError::Malformed(format!(
                            "proposal exceeds {} bytes",
                            self.cfg.proposal_max_bytes
                        )));
                    }
                }
                let decoded: PlanningProposalPayload = serde_json::from_value(payload)
                    .map_err(|e| ProposeError::Malformed(format!("payload: {e}")))?;
                // PP6: `proposal: None` while mode == Llm is malformed.
                decoded
                    .proposal
                    .ok_or_else(|| ProposeError::Malformed("payload carries no proposal".into()))
            }
        }
    }
}
