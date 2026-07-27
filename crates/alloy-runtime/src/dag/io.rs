//! Node I/O artifact envelopes shared with RFC-0010 (RFC-0009 §3.11 / §5.3.0).

use serde::{Deserialize, Serialize};

use crate::dag::types::NodeKind;
use crate::types::budget::Goal;
use crate::types::ids::{ArtifactId, DagId, NodeId};

/// Wire schema version for node I/O envelopes (MUST be 1).
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// Plan-time / rewrite input envelope (`schema_version` MUST be 1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeInputEnvelope {
    /// Schema version (MUST be [`ENVELOPE_SCHEMA_VERSION`]).
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

impl NodeInputEnvelope {
    /// Construct with [`ENVELOPE_SCHEMA_VERSION`].
    #[must_use]
    pub fn new(
        dag_id: DagId,
        node_id: NodeId,
        kind: NodeKind,
        generation: u64,
        payload: NodeInputPayload,
    ) -> Self {
        Self {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            dag_id,
            node_id,
            kind,
            generation,
            payload,
        }
    }

    /// Returns true when `schema_version == ENVELOPE_SCHEMA_VERSION`.
    #[must_use]
    pub fn is_supported_schema(&self) -> bool {
        self.schema_version == ENVELOPE_SCHEMA_VERSION
    }
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
    /// Schema version (MUST be [`ENVELOPE_SCHEMA_VERSION`]).
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

impl NodeOutputEnvelope {
    /// Construct with [`ENVELOPE_SCHEMA_VERSION`].
    #[must_use]
    pub fn new(
        dag_id: DagId,
        node_id: NodeId,
        kind: NodeKind,
        generation: u64,
        attempt: u32,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            dag_id,
            node_id,
            kind,
            generation,
            attempt,
            payload,
        }
    }

    /// Returns true when `schema_version == ENVELOPE_SCHEMA_VERSION`.
    #[must_use]
    pub fn is_supported_schema(&self) -> bool {
        self.schema_version == ENVELOPE_SCHEMA_VERSION
    }
}

/// Pending predecessor placeholder blob body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PendingPredPlaceholder {
    /// Schema version (MUST be 1).
    pub schema_version: u32,
    /// Always true for placeholders.
    pub pending: bool,
}

impl PendingPredPlaceholder {
    /// Day-1 pending placeholder.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            schema_version: ENVELOPE_SCHEMA_VERSION,
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
pub(crate) fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::Goal;

    #[test]
    fn goal_envelope_round_trip() {
        let env = NodeInputEnvelope::new(
            DagId::new(),
            NodeId::new(),
            NodeKind::Analyze,
            1,
            NodeInputPayload::Goal(Goal {
                text: "fix".into(),
                constraints: vec![],
                attachments: vec![],
            }),
        );
        assert!(env.is_supported_schema());
        let bytes = encode_json(&env).unwrap();
        let back: NodeInputEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn pending_placeholder() {
        let p = PendingPredPlaceholder::new();
        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        assert_eq!(v["schema_version"], ENVELOPE_SCHEMA_VERSION);
        assert_eq!(v["pending"], true);
    }
}
