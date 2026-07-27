//! Node I/O artifact envelopes shared with RFC-0010 (RFC-0009 §3.11 / §5.3.0).

use serde::{Deserialize, Serialize};

use crate::dag::types::NodeKind;
use crate::types::budget::Goal;
use crate::types::ids::{ArtifactId, DagId, NodeId};

/// Plan-time / rewrite input envelope (`schema_version` MUST be 1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeInputEnvelope {
    /// Schema version (MUST be 1).
    pub schema_version: u32,
    /// Owning DAG.
    pub dag_id: DagId,
    /// Owning node.
    pub node_id: NodeId,
    /// Node kind.
    pub kind: NodeKind,
    /// DAG generation at write time.
    pub generation: u64,
    /// Payload body.
    pub payload: NodeInputPayload,
}

/// Input payload variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeInputPayload {
    /// Root input — embeds the merged [`Goal`] type.
    Goal(Goal),
    /// Non-root: predecessor output refs (Data edges).
    FromPredecessors {
        /// Predecessor outputs.
        preds: Vec<PredecessorOutput>,
    },
}

/// One predecessor output slot in a non-root input envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PredecessorOutput {
    /// Predecessor node id.
    pub node_id: NodeId,
    /// Predecessor kind.
    pub kind: NodeKind,
    /// Predecessor `output_ref` (pending placeholder at plan time).
    pub output_ref: ArtifactId,
}

/// Success / cache-hit output body (`schema_version` MUST be 1).
///
/// Failure logging artifacts are RFC-0010’s concern and MUST NOT be written
/// into `TaskNode.output_ref` on failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeOutputEnvelope {
    /// Schema version (MUST be 1).
    pub schema_version: u32,
    /// Owning DAG.
    pub dag_id: DagId,
    /// Owning node.
    pub node_id: NodeId,
    /// Node kind.
    pub kind: NodeKind,
    /// DAG generation.
    pub generation: u64,
    /// Attempt index starting at 1 (writer: RFC-0010).
    pub attempt: u32,
    /// Opaque success payload.
    pub payload: serde_json::Value,
}

/// Pending predecessor placeholder blob body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingPredPlaceholder {
    /// Schema version (MUST be 1).
    pub schema_version: u32,
    /// Always true for placeholders.
    pub pending: bool,
}

impl PendingPredPlaceholder {
    /// Day-1 pending placeholder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: 1,
            pending: true,
        }
    }
}

impl Default for PendingPredPlaceholder {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a value as JSON bytes for CAS put.
pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::Goal;

    #[test]
    fn goal_envelope_round_trip() {
        let env = NodeInputEnvelope {
            schema_version: 1,
            dag_id: DagId::new(),
            node_id: NodeId::new(),
            kind: NodeKind::Analyze,
            generation: 1,
            payload: NodeInputPayload::Goal(Goal {
                text: "fix".into(),
                constraints: vec![],
                attachments: vec![],
            }),
        };
        let bytes = encode_json(&env).unwrap();
        let back: NodeInputEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn pending_placeholder() {
        let p = PendingPredPlaceholder::new();
        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["pending"], true);
    }
}
