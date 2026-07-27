//! DAG sketch types for compile-time sharing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::scheduler::DagState;
use crate::types::budget::{ModelTier, TokenBudget};
use crate::types::diagnostic::ErrorClass;
use crate::types::ids::{ArtifactId, CapabilityId, DagId, Digest, GateId, NodeId, SessionId};

/// Explicit task DAG (validated/planned in RFC-0009; executed in RFC-0010).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDag {
    /// DAG id.
    pub id: DagId,
    /// Owning session.
    pub session_id: SessionId,
    /// Generation for replans.
    pub generation: u64,
    /// Nodes.
    pub nodes: BTreeMap<NodeId, TaskNode>,
    /// Edges.
    pub edges: Vec<DependencyEdge>,
    /// DAG state.
    pub state: DagState,
}

/// Single DAG node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskNode {
    /// Node id.
    pub id: NodeId,
    /// Node kind.
    pub kind: NodeKind,
    /// Optional capability.
    pub capability: Option<CapabilityId>,
    /// Input artifact.
    pub input_ref: ArtifactId,
    /// Output artifact.
    pub output_ref: Option<ArtifactId>,
    /// Node state.
    pub state: NodeState,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Optional cache key.
    pub cache_key: Option<CacheKey>,
    /// Token budget.
    pub budget: TokenBudget,
    /// Model tier hint.
    pub model_tier: ModelTier,
    /// Optional approval spec.
    pub approval: Option<ApprovalSpec>,
    /// Timeout in milliseconds (avoids Duration serde issues).
    pub timeout_ms: u64,
}

/// Node kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Planning.
    Plan,
    /// Analysis.
    Analyze,
    /// Edit.
    Edit,
    /// Compile verify (runtime adapter).
    VerifyCompile,
    /// Test verify (runtime adapter).
    VerifyTest,
    /// Review.
    Review,
    /// Human gate.
    GateHuman,
    /// Aggregate.
    Aggregate,
}

/// Node state machine (Appendix C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Pending.
    Pending,
    /// Ready.
    Ready,
    /// Running.
    Running,
    /// Succeeded.
    Succeeded,
    /// Failed.
    Failed,
    /// Skipped.
    Skipped,
    /// Cancelled.
    Cancelled,
    /// Waiting approval.
    WaitingApproval,
    /// Cache hit.
    CachedHit,
}

/// Edge kind. `Hint` is schema-only in MVP (ignored by scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Data dependency.
    Data,
    /// Sequence dependency.
    Sequence,
    /// Deferred hint edge.
    Hint,
}

/// DAG edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    /// Predecessor.
    pub from: NodeId,
    /// Successor.
    pub to: NodeId,
    /// Edge kind.
    pub kind: EdgeKind,
}

/// Retry policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Max attempts.
    pub max_attempts: u32,
    /// Backoff strategy.
    pub backoff: Backoff,
    /// Error classes eligible for retry.
    pub retry_on: Vec<ErrorClass>,
    /// Escalate after N failures.
    pub escalate_after: Option<u32>,
    /// Escalation tier.
    pub escalate_to_tier: Option<ModelTier>,
}

/// Backoff policy using millisecond fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    /// Fixed delay.
    Fixed {
        /// Delay milliseconds.
        delay_ms: u64,
    },
    /// Exponential backoff.
    Exponential {
        /// Base delay milliseconds.
        base_ms: u64,
        /// Multiplier.
        factor: f64,
    },
}

/// Cache key wrapping a digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey(pub Digest);

/// Approval requirement on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSpec {
    /// Gate id.
    pub gate: GateId,
    /// Human-readable reason.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::TokenBudget;
    use crate::types::ids::{ArtifactId, DagId, NodeId, SessionId};

    #[test]
    fn task_dag_serde_round_trip() {
        let node_id = NodeId::new();
        let dag = TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: BTreeMap::from([(
                node_id,
                TaskNode {
                    id: node_id,
                    kind: NodeKind::Edit,
                    capability: None,
                    input_ref: ArtifactId::new(),
                    output_ref: None,
                    state: NodeState::Pending,
                    retry: RetryPolicy {
                        max_attempts: 1,
                        backoff: Backoff::Fixed { delay_ms: 0 },
                        retry_on: vec![],
                        escalate_after: None,
                        escalate_to_tier: None,
                    },
                    cache_key: None,
                    budget: TokenBudget {
                        max_input: 1,
                        max_output: 1,
                    },
                    model_tier: ModelTier::Standard,
                    approval: None,
                    timeout_ms: 1000,
                },
            )]),
            edges: vec![],
            state: DagState::Pending,
        };
        let json = serde_json::to_string(&dag).unwrap();
        let back: TaskDag = serde_json::from_str(&json).unwrap();
        assert_eq!(back.generation, 1);
        assert_eq!(back.nodes.len(), 1);
    }
}
