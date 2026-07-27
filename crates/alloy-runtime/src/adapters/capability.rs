//! Capability-node executor seam (RFC-0010 §3.8). Worker bodies land in RFC-0013;
//! until then the scheduler MUST inject [`UnavailableCapabilityExecutor`].

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::NodeExecRef;
use crate::dag::{NodeInputEnvelope, NodeKind};
use crate::obs::SharedCostMeter;
use crate::types::budget::{ModelTier, TokenBudget};
use crate::types::diagnostic::FailureIr;
use crate::types::ids::CapabilityId;

/// Executes one capability-node attempt (`Plan`/`Analyze`/`Edit`/`Review`).
///
/// Implementations MUST NOT retry, sleep for backoff, escalate tiers, write
/// [`crate::dag::TaskNode`] fields, or write [`crate::dag::NodeState`] events —
/// the scheduler owns all of that (RFC-0010 CE4).
#[async_trait]
pub trait CapabilityExecutor: Send + Sync {
    /// Run one attempt of a capability node.
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError>;
}

/// One capability-node attempt's execution context.
#[derive(Debug, Clone)]
pub struct CapabilityExecContext {
    /// Persistable identity (carries the dispatch attempt).
    pub meta: NodeExecRef,
    /// Cancellation token.
    pub cancellation: CancellationToken,
    /// From `TaskNode.capability` (always `Some` post-validate).
    pub capability: CapabilityId,
    /// Dispatched node kind.
    pub kind: NodeKind,
    /// Effective tier after retry escalation.
    pub effective_tier: ModelTier,
    /// Per-node token budget.
    pub budget: TokenBudget,
    /// Node deadline, already clamped by the remaining run budget.
    pub timeout: Duration,
    /// Decoded input envelope (`schema_version == 1`).
    pub input: NodeInputEnvelope,
    /// Attempt index starting at 1. MUST equal `meta.attempt` (CE3).
    pub attempt: u32,
    /// Run-scoped cost meter. Workers MUST record model usage here and MUST
    /// NOT construct their own meter.
    pub cost_meter: SharedCostMeter,
}

/// Success or structured soft failure. A worker never both succeeds and fails.
#[derive(Debug, Clone)]
pub enum CapabilityOutcome {
    /// The node produced an output payload.
    Succeeded {
        /// Opaque success payload (written verbatim to the output envelope).
        payload: serde_json::Value,
    },
    /// The node failed with a structured, admission-checkable failure.
    Failed {
        /// Structured failure IR. The scheduler overwrites `failure.node`
        /// with the dispatched node id before persisting (CE2).
        failure: FailureIr,
    },
}

/// Capability executor failure (host/worker-boundary, not a node soft-failure).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CapabilityExecError {
    /// No capability executor wired yet (RFC-0013 not landed).
    #[error("unavailable")]
    Unavailable,
    /// Cancelled via token.
    #[error("cancelled")]
    Cancelled,
    /// Worker deadline elapsed.
    #[error("timeout")]
    Timeout,
    /// Worker-reported error (not a structured `FailureIr`).
    #[error("worker: {0}")]
    Worker(String),
    /// Internal executor error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Stub executor returning [`CapabilityExecError::Unavailable`] until RFC-0013 lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableCapabilityExecutor;

#[async_trait]
impl CapabilityExecutor for UnavailableCapabilityExecutor {
    async fn execute(
        &self,
        _ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        Err(CapabilityExecError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::super::{
        CapabilityExecContext, CapabilityExecError, CapabilityExecutor, NodeExecRef,
    };
    use crate::dag::{NodeInputEnvelope, NodeInputPayload, NodeKind, ENVELOPE_SCHEMA_VERSION};
    use crate::obs::SharedCostMeter;
    use crate::types::budget::{Goal, ModelTier, TokenBudget};
    use crate::types::ids::{CapabilityId, DagId, NodeId, RunId, SessionId};
    use crate::UnavailableCapabilityExecutor;

    fn ctx() -> CapabilityExecContext {
        let dag_id = DagId::new();
        let node_id = NodeId::new();
        CapabilityExecContext {
            meta: NodeExecRef {
                session_id: SessionId::new(),
                run_id: RunId::new(),
                dag_id,
                node_id,
                workspace_root: std::path::PathBuf::from("/tmp/ws"),
                attempt: 1,
            },
            cancellation: CancellationToken::new(),
            capability: CapabilityId::new("repair").unwrap(),
            kind: NodeKind::Edit,
            effective_tier: ModelTier::Standard,
            budget: TokenBudget {
                max_input: 1000,
                max_output: 1000,
            },
            timeout: Duration::from_secs(30),
            input: NodeInputEnvelope {
                schema_version: ENVELOPE_SCHEMA_VERSION,
                dag_id,
                node_id,
                kind: NodeKind::Edit,
                generation: 1,
                payload: NodeInputPayload::Goal(Goal {
                    text: "test goal".into(),
                    constraints: vec![],
                    attachments: vec![],
                }),
            },
            attempt: 1,
            cost_meter: SharedCostMeter::new(),
        }
    }

    #[tokio::test]
    async fn unavailable_executor_returns_unavailable() {
        let executor = UnavailableCapabilityExecutor;
        let err = executor.execute(&ctx()).await.unwrap_err();
        assert!(matches!(err, CapabilityExecError::Unavailable));
    }
}
