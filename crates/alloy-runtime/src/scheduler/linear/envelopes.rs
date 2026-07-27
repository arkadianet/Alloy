//! Input envelope assembly and `input_ref` rewrite (RFC-0010 §5.5, E1-E10).
//!
//! Rewrite writes route through [`super::checkpoint::Checkpoint::c5_rewrite_input`]
//! (checkpoint.rs stays the sole CAS caller, rule M3); this module only
//! decides *whether* a rewrite is needed and builds the candidate bytes.

use crate::dag::{NodeInputEnvelope, NodeInputPayload, PredecessorOutput, TaskDag};
use crate::error::SchedError;
use crate::storage::ArtifactStore;
use crate::types::ids::{DagId, NodeId};

use super::checkpoint::{map_store_error, Checkpoint, CheckpointCtx};

/// A node's input shape, classified from its incoming edges (§5.5 table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputShape {
    /// No incoming `Data` or `Sequence` edges: keep the plan-time `Goal`
    /// envelope untouched.
    Root,
    /// ≥1 `Sequence` edge, zero `Data` edges: keep `FromPredecessors { preds:
    /// [] }` untouched.
    SequenceOnly,
    /// ≥1 incoming `Data` edge, from these predecessors in ascending
    /// `NodeId` (E1's rewrite trigger).
    Data(Vec<NodeId>),
}

/// Classify `node_id`'s input shape from `dag.edges` (§5.5, pure). `Hint`
/// edges never participate.
#[must_use]
pub(crate) fn classify_input_shape(dag: &TaskDag, node_id: NodeId) -> InputShape {
    let mut data_preds: Vec<NodeId> = Vec::new();
    let mut has_sequence = false;
    for edge in &dag.edges {
        if edge.to != node_id {
            continue;
        }
        match edge.kind {
            crate::dag::EdgeKind::Data => data_preds.push(edge.from),
            crate::dag::EdgeKind::Sequence => has_sequence = true,
            crate::dag::EdgeKind::Hint => {}
        }
    }
    if !data_preds.is_empty() {
        data_preds.sort();
        data_preds.dedup();
        InputShape::Data(data_preds)
    } else if has_sequence {
        InputShape::SequenceOnly
    } else {
        InputShape::Root
    }
}

/// Assemble (rewriting via C5 when needed), load, and validate the input
/// envelope for `node_id` immediately before dispatch (§5.5, E1-E10).
///
/// Data-shaped nodes are rewritten to carry each Data predecessor's real
/// `output_ref` (E1); Root/Sequence-only nodes keep their plan-time envelope
/// untouched. Either way, the final bytes are decoded and validated (E8-E10)
/// and, for the Data shape, each referenced predecessor artifact is checked
/// for a lingering `pending_pred` placeholder (E3/E4).
pub(crate) async fn assemble_input(
    checkpoint: &Checkpoint,
    artifacts: &dyn ArtifactStore,
    dag: &mut TaskDag,
    ctx: CheckpointCtx,
    node_id: NodeId,
) -> Result<NodeInputEnvelope, SchedError> {
    let shape = classify_input_shape(dag, node_id);
    if let InputShape::Data(preds) = &shape {
        rewrite_data_input(checkpoint, artifacts, dag, ctx, node_id, preds).await?;
    }

    let node = dag.nodes.get(&node_id).ok_or_else(|| {
        SchedError::Invariant(format!("unknown node {node_id} in assemble_input"))
    })?;
    let input_ref = node.input_ref;
    let blob = artifacts
        .get(input_ref)
        .await
        .map_err(|e| map_store_error(e, dag.id))?;
    let envelope: NodeInputEnvelope = serde_json::from_slice(&blob.bytes).map_err(|e| {
        SchedError::Invariant(format!(
            "input envelope decode failed for node {node_id}: {e}"
        ))
    })?; // E8

    if !envelope.is_supported_schema() {
        return Err(SchedError::Invariant(format!(
            "unsupported input envelope schema {} for node {node_id}",
            envelope.schema_version
        ))); // E9
    }
    if envelope.dag_id != dag.id
        || envelope.node_id != node_id
        || envelope.generation != dag.generation
    {
        return Err(SchedError::Invariant(format!(
            "input envelope identity mismatch for node {node_id}"
        ))); // E10
    }

    if let NodeInputPayload::FromPredecessors { preds } = &envelope.payload {
        check_no_pending_placeholder(artifacts, dag.id, node_id, preds).await?; // E3/E4
    }

    Ok(envelope)
}

/// E1/E2/E5/E6: build the rewritten envelope for a Data-shaped node and
/// commit it via C5, unless the candidate is byte-identical to what is
/// already stored (E6 SHOULD-skip, implemented as a MUST here: redundant
/// writes are wasted I/O with no behavioural difference).
async fn rewrite_data_input(
    checkpoint: &Checkpoint,
    artifacts: &dyn ArtifactStore,
    dag: &mut TaskDag,
    ctx: CheckpointCtx,
    node_id: NodeId,
    data_preds: &[NodeId],
) -> Result<(), SchedError> {
    let node = dag.nodes.get(&node_id).ok_or_else(|| {
        SchedError::Invariant(format!("unknown node {node_id} in rewrite_data_input"))
    })?;
    let kind = node.kind;
    let current_input_ref = node.input_ref;

    let mut preds = Vec::with_capacity(data_preds.len());
    for &pred_id in data_preds {
        let pred = dag.nodes.get(&pred_id).ok_or_else(|| {
            SchedError::Invariant(format!(
                "unknown Data predecessor {pred_id} of node {node_id}"
            ))
        })?;
        let output_ref = pred.output_ref.ok_or_else(|| {
            // RS3 (RFC-0009 §5.3.2 invariant): a Succeeded predecessor
            // without output_ref must never reach dispatch.
            SchedError::Invariant(format!("succeeded node {pred_id} has no output_ref"))
        })?;
        preds.push(PredecessorOutput {
            node_id: pred_id,
            kind: pred.kind,
            output_ref,
        });
    }

    let candidate = NodeInputEnvelope::new(
        dag.id,
        node_id,
        kind,
        dag.generation,
        NodeInputPayload::FromPredecessors { preds },
    );
    let candidate_bytes = serde_json::to_vec(&candidate)
        .map_err(|e| SchedError::Internal(format!("encode input envelope: {e}")))?;

    let current_bytes = artifacts
        .get(current_input_ref)
        .await
        .map_err(|e| map_store_error(e, dag.id))?
        .bytes;
    if current_bytes == candidate_bytes {
        return Ok(()); // E6
    }

    checkpoint
        .c5_rewrite_input(dag, ctx, node_id, candidate_bytes)
        .await?;
    Ok(())
}

/// E3/E4: reject a still-pending predecessor slot at dispatch time.
async fn check_no_pending_placeholder(
    artifacts: &dyn ArtifactStore,
    dag_id: DagId,
    node_id: NodeId,
    preds: &[PredecessorOutput],
) -> Result<(), SchedError> {
    for pred in preds {
        let meta = artifacts
            .meta(pred.output_ref)
            .await
            .map_err(|e| map_store_error(e, dag_id))?;
        let is_pending =
            meta.labels.get("alloy.envelope").and_then(|v| v.as_str()) == Some("pending_pred");
        if is_pending {
            return Err(SchedError::Invariant(format!(
                "pending predecessor slot for node {node_id}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::dag::{
        Backoff, DependencyEdge, EdgeKind, NodeKind, NodeState, RetryPolicy, TaskNode,
    };
    use crate::scheduler::linear::metrics::SchedulerCounters;
    use crate::scheduler::DagState;
    use crate::storage::{AlloyStorage, ArtifactKind, ArtifactPut, DagStore, StorageOpenOptions};
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::ids::{ArtifactId, RunId, SessionId};

    struct Fixture {
        _dir: tempfile::TempDir,
        storage: AlloyStorage,
        checkpoint: Checkpoint,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
                .await
                .unwrap();
            let metrics = Arc::new(SchedulerCounters::new());
            let checkpoint = Checkpoint::new(
                storage.dags(),
                storage.artifacts(),
                storage.events(),
                metrics,
            );
            Self {
                _dir: dir,
                storage,
                checkpoint,
            }
        }

        async fn close(self) {
            self.storage.close().await.unwrap();
        }

        fn ctx(&self, session_id: SessionId) -> CheckpointCtx {
            CheckpointCtx {
                session_id,
                run_id: Some(RunId::new()),
            }
        }

        async fn put_pending_placeholder(&self, dag_id: DagId) -> ArtifactId {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "pending": true
            }))
            .unwrap();
            let mut labels = serde_json::Map::new();
            labels.insert(
                "alloy.envelope".into(),
                serde_json::Value::String("pending_pred".into()),
            );
            labels.insert(
                "alloy.dag_id".into(),
                serde_json::Value::String(dag_id.to_string()),
            );
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels,
                })
                .await
                .unwrap()
        }

        async fn put_goal_envelope(
            &self,
            dag_id: DagId,
            node_id: NodeId,
            generation: u64,
        ) -> ArtifactId {
            let env = NodeInputEnvelope::new(
                dag_id,
                node_id,
                NodeKind::Analyze,
                generation,
                NodeInputPayload::Goal(crate::types::budget::Goal {
                    text: "fix".into(),
                    constraints: vec![],
                    attachments: vec![],
                }),
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            self.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        }
    }

    fn node(id: NodeId, state: NodeState, kind: NodeKind, input_ref: ArtifactId) -> TaskNode {
        TaskNode {
            id,
            kind,
            capability: None,
            input_ref,
            output_ref: None,
            state,
            retry: RetryPolicy {
                max_attempts: 3,
                backoff: Backoff::Fixed { delay_ms: 0 },
                retry_on: vec![],
                escalate_after: None,
                escalate_to_tier: None,
            },
            cache_key: None,
            budget: TokenBudget {
                max_input: 0,
                max_output: 0,
            },
            model_tier: ModelTier::Economy,
            approval: None,
            timeout_ms: 1000,
        }
    }

    // ---- classify_input_shape ----

    #[test]
    fn classify_root_has_no_edges() {
        let dag = TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: BTreeMap::new(),
            edges: vec![],
            state: DagState::Running,
        };
        let n = NodeId::new();
        assert_eq!(classify_input_shape(&dag, n), InputShape::Root);
    }

    #[test]
    fn classify_sequence_only_has_no_data_edges() {
        let a = NodeId::new();
        let b = NodeId::new();
        let dag = TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: BTreeMap::new(),
            edges: vec![DependencyEdge {
                from: a,
                to: b,
                kind: EdgeKind::Sequence,
            }],
            state: DagState::Running,
        };
        assert_eq!(classify_input_shape(&dag, b), InputShape::SequenceOnly);
    }

    #[test]
    fn classify_data_collects_ascending_predecessors() {
        let hi = NodeId::new();
        let lo = NodeId::new();
        let (hi, lo) = if hi > lo { (hi, lo) } else { (lo, hi) };
        let n = NodeId::new();
        let dag = TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: BTreeMap::new(),
            edges: vec![
                DependencyEdge {
                    from: hi,
                    to: n,
                    kind: EdgeKind::Data,
                },
                DependencyEdge {
                    from: lo,
                    to: n,
                    kind: EdgeKind::Data,
                },
            ],
            state: DagState::Running,
        };
        assert_eq!(
            classify_input_shape(&dag, n),
            InputShape::Data(vec![lo, hi])
        );
    }

    #[test]
    fn classify_hint_edges_never_participate() {
        let p = NodeId::new();
        let n = NodeId::new();
        let dag = TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes: BTreeMap::new(),
            edges: vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Hint,
            }],
            state: DagState::Running,
        };
        assert_eq!(classify_input_shape(&dag, n), InputShape::Root);
    }

    // ---- assemble_input ----

    #[tokio::test]
    async fn root_node_keeps_plan_time_goal_envelope_untouched() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let n = NodeId::new();
        let goal_ref = fx.put_goal_envelope(dag_id, n, 1).await;
        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(n, node(n, NodeState::Ready, NodeKind::Analyze, goal_ref))]),
            edges: vec![],
            state: DagState::Running,
        };
        let ctx = fx.ctx(session);

        let env = assemble_input(&fx.checkpoint, &*fx.storage.artifacts(), &mut dag, ctx, n)
            .await
            .unwrap();
        assert!(matches!(env.payload, NodeInputPayload::Goal(_)));
        assert_eq!(
            dag.nodes[&n].input_ref, goal_ref,
            "root node input_ref must not be rewritten"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn data_node_rewrites_to_real_predecessor_output_ref() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let pred = NodeId::new();
        let succ = NodeId::new();
        let pending = fx.put_pending_placeholder(dag_id).await;
        let real_output = fx.put_goal_envelope(dag_id, pred, 1).await; // any artifact stands in for a real output

        let succ_input_placeholder = {
            let env = NodeInputEnvelope::new(
                dag_id,
                succ,
                NodeKind::Edit,
                1,
                NodeInputPayload::FromPredecessors {
                    preds: vec![PredecessorOutput {
                        node_id: pred,
                        kind: NodeKind::Analyze,
                        output_ref: pending,
                    }],
                },
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            fx.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        };

        let mut pred_node = node(pred, NodeState::Succeeded, NodeKind::Analyze, real_output);
        pred_node.output_ref = Some(real_output);
        let succ_node = node(
            succ,
            NodeState::Ready,
            NodeKind::Edit,
            succ_input_placeholder,
        );

        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(pred, pred_node), (succ, succ_node)]),
            edges: vec![DependencyEdge {
                from: pred,
                to: succ,
                kind: EdgeKind::Data,
            }],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let ctx = fx.ctx(session);

        let env = assemble_input(
            &fx.checkpoint,
            &*fx.storage.artifacts(),
            &mut dag,
            ctx,
            succ,
        )
        .await
        .unwrap();
        match &env.payload {
            NodeInputPayload::FromPredecessors { preds } => {
                assert_eq!(preds.len(), 1);
                assert_eq!(preds[0].output_ref, real_output);
            }
            other => panic!("expected FromPredecessors, got {other:?}"),
        }
        assert_ne!(
            dag.nodes[&succ].input_ref, succ_input_placeholder,
            "input_ref must be rewritten off the placeholder"
        );
        fx.close().await;
    }

    #[tokio::test]
    async fn data_node_second_call_is_byte_identical_skip() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let pred = NodeId::new();
        let succ = NodeId::new();
        let real_output = fx.put_goal_envelope(dag_id, pred, 1).await;
        let placeholder = fx.put_pending_placeholder(dag_id).await;

        let succ_input_placeholder = {
            let env = NodeInputEnvelope::new(
                dag_id,
                succ,
                NodeKind::Edit,
                1,
                NodeInputPayload::FromPredecessors {
                    preds: vec![PredecessorOutput {
                        node_id: pred,
                        kind: NodeKind::Analyze,
                        output_ref: placeholder,
                    }],
                },
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            fx.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        };
        let mut pred_node = node(pred, NodeState::Succeeded, NodeKind::Analyze, real_output);
        pred_node.output_ref = Some(real_output);
        let succ_node = node(
            succ,
            NodeState::Ready,
            NodeKind::Edit,
            succ_input_placeholder,
        );
        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(pred, pred_node), (succ, succ_node)]),
            edges: vec![DependencyEdge {
                from: pred,
                to: succ,
                kind: EdgeKind::Data,
            }],
            state: DagState::Running,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        let ctx = fx.ctx(session);

        assemble_input(
            &fx.checkpoint,
            &*fx.storage.artifacts(),
            &mut dag,
            ctx,
            succ,
        )
        .await
        .unwrap();
        let rewritten_ref = dag.nodes[&succ].input_ref;

        // Second call over the same (already-rewritten) state must not rewrite again.
        assemble_input(
            &fx.checkpoint,
            &*fx.storage.artifacts(),
            &mut dag,
            ctx,
            succ,
        )
        .await
        .unwrap();
        assert_eq!(dag.nodes[&succ].input_ref, rewritten_ref);
        fx.close().await;
    }

    #[tokio::test]
    async fn succeeded_predecessor_without_output_ref_fails_closed() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let pred = NodeId::new();
        let succ = NodeId::new();
        let placeholder = fx.put_pending_placeholder(dag_id).await;
        let succ_input_placeholder = {
            let env = NodeInputEnvelope::new(
                dag_id,
                succ,
                NodeKind::Edit,
                1,
                NodeInputPayload::FromPredecessors {
                    preds: vec![PredecessorOutput {
                        node_id: pred,
                        kind: NodeKind::Analyze,
                        output_ref: placeholder,
                    }],
                },
            );
            let bytes = serde_json::to_vec(&env).unwrap();
            fx.storage
                .artifacts()
                .put(ArtifactPut {
                    bytes,
                    kind: ArtifactKind::Blob,
                    content_type: Some("application/json".into()),
                    session_id: None,
                    run_id: None,
                    labels: serde_json::Map::new(),
                })
                .await
                .unwrap()
        };
        // pred is Succeeded but (corrupt state) has no output_ref.
        let pred_node = node(
            pred,
            NodeState::Succeeded,
            NodeKind::Analyze,
            ArtifactId::new(),
        );
        let succ_node = node(
            succ,
            NodeState::Ready,
            NodeKind::Edit,
            succ_input_placeholder,
        );
        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(pred, pred_node), (succ, succ_node)]),
            edges: vec![DependencyEdge {
                from: pred,
                to: succ,
                kind: EdgeKind::Data,
            }],
            state: DagState::Running,
        };
        let ctx = fx.ctx(session);

        let err = assemble_input(
            &fx.checkpoint,
            &*fx.storage.artifacts(),
            &mut dag,
            ctx,
            succ,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("no output_ref")));
        fx.close().await;
    }

    #[tokio::test]
    async fn pending_placeholder_still_referenced_fails_closed() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let n = NodeId::new();
        let placeholder = fx.put_pending_placeholder(dag_id).await;
        let env = NodeInputEnvelope::new(
            dag_id,
            n,
            NodeKind::Edit,
            1,
            NodeInputPayload::FromPredecessors {
                preds: vec![PredecessorOutput {
                    node_id: NodeId::new(),
                    kind: NodeKind::Analyze,
                    output_ref: placeholder,
                }],
            },
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let input_ref = fx
            .storage
            .artifacts()
            .put(ArtifactPut {
                bytes,
                kind: ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                session_id: None,
                run_id: None,
                labels: serde_json::Map::new(),
            })
            .await
            .unwrap();
        // Sequence/root shape (no edges in this dag) so no rewrite happens —
        // this simulates a node whose stored envelope still points at an
        // unresolved placeholder (E3/E4 defensive check).
        let node = node(n, NodeState::Ready, NodeKind::Edit, input_ref);
        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(n, node)]),
            edges: vec![],
            state: DagState::Running,
        };
        let ctx = fx.ctx(session);

        let err = assemble_input(&fx.checkpoint, &*fx.storage.artifacts(), &mut dag, ctx, n)
            .await
            .unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("pending predecessor slot")));
        fx.close().await;
    }

    #[tokio::test]
    async fn identity_mismatch_fails_closed() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let dag_id = DagId::new();
        let n = NodeId::new();
        // Envelope's own dag_id disagrees with the dag we pass in.
        let wrong_ref = fx.put_goal_envelope(DagId::new(), n, 1).await;
        let node = node(n, NodeState::Ready, NodeKind::Analyze, wrong_ref);
        let mut dag = TaskDag {
            id: dag_id,
            session_id: session,
            generation: 1,
            nodes: BTreeMap::from([(n, node)]),
            edges: vec![],
            state: DagState::Running,
        };
        let ctx = fx.ctx(session);

        let err = assemble_input(&fx.checkpoint, &*fx.storage.artifacts(), &mut dag, ctx, n)
            .await
            .unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("identity mismatch")));
        fx.close().await;
    }
}
