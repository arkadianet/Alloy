//! Checkpoint catalog (C1-C10, Appendix A) and CAS-before-events crash
//! repair (RF1-RF8, §5.3.3).
//!
//! This module is the **sole** caller of [`DagStore::put_if_generation`]
//! (RFC-0010 §4.1 rule M3): every durable DAG-blob write, from every other
//! scheduler module, MUST route through one of the `cN_*` methods here so
//! the artifacts → CAS → events write order (§5.8.1) and Appendix A's
//! per-id catalog stay in exactly one place. `cN_*` methods only mutate the
//! fields CA1 allows (`state`, `input_ref`, `output_ref`, the DAG's `state`)
//! and never touch `generation`, `nodes` membership, `edges`, or any other
//! planner-owned field.
//!
//! Which nodes get which transition (which node is "the" failing node,
//! which nodes get skipped, which node is in flight for a cancel) is the
//! serial loop's decision (`loop_.rs`, RFC-0010 P4) and the cancel/gate
//! modules' decision (P7/P8) — this module only executes a transition set
//! it is handed, and enforces that the set is legal and durably ordered.

use std::sync::Arc;

use serde_json::Value;

use super::metrics::SchedulerCounters;
use crate::dag::{NodeState, TaskDag};
use crate::error::SchedError;
use crate::events::{NewSessionEvent, SessionEventType};
use crate::session::MAX_EVENTS_PAGE;
use crate::storage::{ArtifactKind, ArtifactPut, ArtifactStore, DagStore, EventStore, StoreError};
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{ArtifactId, DagId, EventSeq, GateId, NodeId, RunId, SessionId};

// ---------------------------------------------------------------------
// §8.2 StoreError -> SchedError mapping
// ---------------------------------------------------------------------

/// Map a [`StoreError`] encountered while *loading* a DAG blob (RFC-0010
/// §8.2): `NotFound` becomes [`SchedError::DagNotFound`] only here: every
/// other checkpoint-time occurrence of `NotFound` (a CAS racing a delete,
/// which production code never does) maps to `Store` via
/// [`map_store_error`].
#[must_use]
pub(crate) fn map_store_error_on_load(err: StoreError, dag_id: DagId) -> SchedError {
    match err {
        StoreError::NotFound(_) => SchedError::DagNotFound(dag_id),
        other => map_store_error(other, dag_id),
    }
}

/// Total `StoreError` → `SchedError` mapping (RFC-0010 §8.2) for every
/// failure that is not a load-time lookup. `StoreError` is exhaustive (not
/// `#[non_exhaustive]`) on `main`, so this match has no catch-all arm: a new
/// variant fails the build here rather than silently falling through.
#[must_use]
pub(crate) fn map_store_error(err: StoreError, dag_id: DagId) -> SchedError {
    match err {
        StoreError::Conflict(_) => SchedError::Conflict { dag_id },
        StoreError::NotFound(m) => SchedError::Store(m),
        StoreError::Corrupt(m) => SchedError::Invariant(format!("corrupt dag blob: {m}")),
        StoreError::Busy => SchedError::Store("busy".into()),
        StoreError::Internal(m) => SchedError::Store(m),
        StoreError::Io(m) => SchedError::Store(m),
        StoreError::Migration(m) => SchedError::Invariant(format!("store migration: {m}")),
        StoreError::DigestMismatch => SchedError::Invariant("artifact digest mismatch".into()),
        StoreError::Closed => SchedError::Store("store closed".into()),
    }
}

// ---------------------------------------------------------------------
// Shared attribution context
// ---------------------------------------------------------------------

/// Session/run attribution every checkpoint call needs beyond the DAG blob
/// itself (artifact labels and event `session_id`/`run_id`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckpointCtx {
    /// Owning session (mirrors `dag.session_id`; not re-derived so callers
    /// cannot accidentally attribute an artifact/event to the wrong DAG's
    /// session under a bug).
    pub session_id: SessionId,
    /// Owning run, when the caller has resolved one (Appendix F). `None` is
    /// legal for tests and for checkpoints that fire before a run binding
    /// exists.
    pub run_id: Option<RunId>,
}

/// Gate resolution decision as it appears on the wire (`NodeState.decision`,
/// H.1). Distinct from [`crate::adapters::Approval`], which has no
/// `Expired` case — the checkpoint layer needs all four wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateDecision {
    /// Ongoing approval.
    Allow,
    /// Single-use approval.
    AllowOnce,
    /// Explicit denial.
    Deny,
    /// `timeout_ms` elapsed unresolved.
    ///
    /// Not yet produced: the real deadline/`expire_gate` path (RFC-0010
    /// §5.7.8) lands in P7. `c9c_gate_deny` and `gate_decision_str` already
    /// handle it correctly; only the caller that would select it is missing.
    #[allow(dead_code)]
    Expired,
}

impl GateDecision {
    fn as_str(self) -> &'static str {
        match self {
            GateDecision::Allow => "allow",
            GateDecision::AllowOnce => "allow_once",
            GateDecision::Deny => "deny",
            GateDecision::Expired => "expired",
        }
    }
}

// ---------------------------------------------------------------------
// Checkpoint: the sole put_if_generation caller
// ---------------------------------------------------------------------

/// Owns the three durable stores and executes every DAG-blob write in the
/// crate (M3).
pub(crate) struct Checkpoint {
    dags: Arc<dyn DagStore>,
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<dyn EventStore>,
    metrics: Arc<SchedulerCounters>,
}

impl Checkpoint {
    pub(crate) fn new(
        dags: Arc<dyn DagStore>,
        artifacts: Arc<dyn ArtifactStore>,
        events: Arc<dyn EventStore>,
        metrics: Arc<SchedulerCounters>,
    ) -> Self {
        Self {
            dags,
            artifacts,
            events,
            metrics,
        }
    }

    // ---- primitives (W1-W6) ----

    /// W1: put one artifact, labeled per Appendix G.1. `node_id` is omitted
    /// from labels when `None` (dag-level artifacts have none yet).
    #[allow(clippy::too_many_arguments)]
    async fn put_artifact(
        &self,
        dag_id: DagId,
        node_id: Option<NodeId>,
        ctx: CheckpointCtx,
        generation: u64,
        envelope: &'static str,
        kind: ArtifactKind,
        content_type: Option<&'static str>,
        bytes: Vec<u8>,
    ) -> Result<ArtifactId, SchedError> {
        let mut labels = serde_json::Map::new();
        labels.insert("alloy.envelope".into(), Value::String(envelope.to_string()));
        labels.insert("alloy.dag_id".into(), Value::String(dag_id.to_string()));
        if let Some(id) = node_id {
            labels.insert("alloy.node_id".into(), Value::String(id.to_string()));
        }
        labels.insert("alloy.generation".into(), Value::from(generation));
        self.artifacts
            .put(ArtifactPut {
                bytes,
                kind,
                content_type: content_type.map(str::to_string),
                session_id: Some(ctx.session_id),
                run_id: ctx.run_id,
                labels,
            })
            .await
            .map_err(|e| map_store_error(e, dag_id))
    }

    /// W2/W6/M3: the **only** call site of `DagStore::put_if_generation` in
    /// the crate. `expected = Some(dag.generation)`, so `dag.generation`
    /// must already equal the value durably stored (CA1: checkpoints never
    /// change `generation`). Increments `cas_conflicts` on `Conflict`
    /// (§5.8.4 step 4).
    async fn cas(&self, dag: &TaskDag) -> Result<(), SchedError> {
        self.dags
            .put_if_generation(dag, Some(dag.generation))
            .await
            .map_err(|e| {
                if matches!(e, StoreError::Conflict(_)) {
                    self.metrics.inc_cas_conflicts();
                }
                map_store_error(e, dag.id)
            })
    }

    /// W2-W4: append an event that is not load-bearing. Failure is logged
    /// at `warn` and left for RF3 to repair on the next pass; it MUST NOT
    /// fail the checkpoint that already committed its CAS.
    async fn append_best_effort(&self, ev: NewSessionEvent) {
        let dag_id_label = ev
            .payload
            .get("dag_id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        if let Err(e) = self.events.append_session(ev).await {
            tracing::warn!(
                error = %e,
                dag_id = %dag_id_label,
                "event append failed after committed CAS; RF3 will repair"
            );
        }
    }

    /// W4a: append a load-bearing event (C3's `to:running`, C8's `(d)
    /// to:ready`). Failure MUST propagate so the caller does not dispatch /
    /// does not start backoff; the blob stays as committed and resume
    /// adopts it.
    async fn append_load_bearing(
        &self,
        ev: NewSessionEvent,
        dag_id: DagId,
    ) -> Result<EventSeq, SchedError> {
        self.events.append_session(ev).await.map_err(|e| {
            SchedError::Store(format!(
                "load-bearing event append failed for dag {dag_id}: {e}"
            ))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn node_state_event(
        ctx: CheckpointCtx,
        node_id: NodeId,
        from: NodeState,
        to: NodeState,
        generation: u64,
        extra: serde_json::Map<String, Value>,
    ) -> NewSessionEvent {
        let mut payload = serde_json::Map::new();
        payload.insert("node_id".into(), Value::String(node_id.to_string()));
        payload.insert("from".into(), state_json(from));
        payload.insert("to".into(), state_json(to));
        payload.insert("generation".into(), Value::from(generation));
        payload.extend(extra);
        NewSessionEvent {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            type_: SessionEventType::NodeState,
            payload: Value::Object(payload),
        }
    }

    // ---- C1: run start / adopt ----

    /// C1: `Pending → Running` DAG state on first start. No node
    /// transitions, no events (Appendix A). Adopting an already-`Running`
    /// DAG (crash resume) is not a checkpoint — it is a no-op read.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c1", generation = dag.generation, nodes_changed = 0)
    )]
    pub(crate) async fn c1_start(&self, dag: &mut TaskDag) -> Result<(), SchedError> {
        dag.state = crate::scheduler::DagState::Running;
        self.cas(dag).await
    }

    // ---- C2: frontier promotion ----

    /// C2: `Pending → Ready` for every node in `promote`, one CAS, one
    /// `NodeState` event per node (RS6: single CAS covering the whole
    /// frontier so a crash cannot half-promote it).
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c2", generation = dag.generation, nodes_changed = promote.len())
    )]
    pub(crate) async fn c2_promote(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        promote: &[NodeId],
    ) -> Result<(), SchedError> {
        for &id in promote {
            let node = dag
                .nodes
                .get_mut(&id)
                .ok_or_else(|| SchedError::Invariant(format!("unknown node {id} in c2_promote")))?;
            if node.state != NodeState::Pending {
                return Err(SchedError::Invariant(format!(
                    "c2_promote: node {id} is {:?}, not Pending",
                    node.state
                )));
            }
            node.state = NodeState::Ready;
        }
        let generation = dag.generation;
        self.cas(dag).await?;
        for &id in promote {
            self.append_best_effort(Self::node_state_event(
                ctx,
                id,
                NodeState::Pending,
                NodeState::Ready,
                generation,
                serde_json::Map::new(),
            ))
            .await;
        }
        Ok(())
    }

    // ---- C3: dispatch attempt k ----

    /// C3: `Ready → Running` for `node_id` at `attempt` `k`. The
    /// `NodeState{to:running, attempt:k}` event is **load-bearing** (W4a):
    /// its failure propagates so the caller does not dispatch the node
    /// future.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c3", generation = dag.generation, nodes_changed = 1)
    )]
    pub(crate) async fn c3_dispatch(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        attempt: u32,
    ) -> Result<(), SchedError> {
        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c3_dispatch"))
        })?;
        if node.state != NodeState::Ready {
            return Err(SchedError::Invariant(format!(
                "c3_dispatch: node {node_id} is {:?}, not Ready",
                node.state
            )));
        }
        node.state = NodeState::Running;
        let generation = dag.generation;
        self.cas(dag).await?;
        self.metrics.inc_nodes_dispatched();
        let mut extra = serde_json::Map::new();
        extra.insert("attempt".into(), Value::from(attempt));
        let ev = Self::node_state_event(
            ctx,
            node_id,
            NodeState::Ready,
            NodeState::Running,
            generation,
            extra,
        );
        self.append_load_bearing(ev, dag.id).await?;
        Ok(())
    }

    // ---- C4: node success ----

    /// C4: `Running → Succeeded`, `output_ref = Some(id)` in the same CAS
    /// (OU1). Puts the [`crate::dag::NodeOutputEnvelope`] artifact first
    /// (W1) so the CAS never references a dangling id.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c4", generation = dag.generation, nodes_changed = 1)
    )]
    pub(crate) async fn c4_succeed(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        attempt: u32,
        output_envelope_bytes: Vec<u8>,
    ) -> Result<ArtifactId, SchedError> {
        let generation = dag.generation;
        let artifact_id = self
            .put_artifact(
                dag.id,
                Some(node_id),
                ctx,
                generation,
                "node_output",
                ArtifactKind::Blob,
                Some("application/json"),
                output_envelope_bytes,
            )
            .await?;
        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c4_succeed"))
        })?;
        if node.state != NodeState::Running {
            return Err(SchedError::Invariant(format!(
                "c4_succeed: node {node_id} is {:?}, not Running",
                node.state
            )));
        }
        node.state = NodeState::Succeeded;
        node.output_ref = Some(artifact_id);
        self.cas(dag).await?;
        self.metrics.inc_nodes_succeeded();
        let mut extra = serde_json::Map::new();
        extra.insert("attempt".into(), Value::from(attempt));
        self.append_best_effort(Self::node_state_event(
            ctx,
            node_id,
            NodeState::Running,
            NodeState::Succeeded,
            generation,
            extra,
        ))
        .await;
        Ok(artifact_id)
    }

    // ---- C5: input rewrite ----

    /// C5: rewrite `input_ref` to a new artifact; node state is unchanged
    /// (stays `Ready`). No events (Appendix A: "artifact provenance only").
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c5", generation = dag.generation, nodes_changed = 0)
    )]
    pub(crate) async fn c5_rewrite_input(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        envelope_bytes: Vec<u8>,
    ) -> Result<ArtifactId, SchedError> {
        let generation = dag.generation;
        let artifact_id = self
            .put_artifact(
                dag.id,
                Some(node_id),
                ctx,
                generation,
                "node_input",
                ArtifactKind::Blob,
                Some("application/json"),
                envelope_bytes,
            )
            .await?;
        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c5_rewrite_input"))
        })?;
        node.input_ref = artifact_id;
        self.cas(dag).await?;
        Ok(artifact_id)
    }

    // ---- C6: cancel ----

    /// C6: mark `cancelled` nodes `Cancelled` and `skipped` nodes `Skipped`
    /// in one CAS; `DagState` becomes `Cancelled`. Which nodes fall in
    /// which set is the cancel path's decision (§5.12, P8) — this method
    /// only validates the transitions are legal (Appendix B) and persists
    /// them in order.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c6", generation = dag.generation, nodes_changed = cancelled.len() + skipped.len())
    )]
    pub(crate) async fn c6_cancel(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        cancelled: &[NodeId],
        skipped: &[NodeId],
    ) -> Result<(), SchedError> {
        let generation = dag.generation;
        let mut froms = Vec::with_capacity(cancelled.len() + skipped.len());
        for &id in cancelled {
            let node = dag
                .nodes
                .get_mut(&id)
                .ok_or_else(|| SchedError::Invariant(format!("unknown node {id} in c6_cancel")))?;
            non_terminal_or_err(node.state, id, "c6_cancel")?;
            froms.push((id, node.state, NodeState::Cancelled));
            node.state = NodeState::Cancelled;
        }
        for &id in skipped {
            let node = dag
                .nodes
                .get_mut(&id)
                .ok_or_else(|| SchedError::Invariant(format!("unknown node {id} in c6_cancel")))?;
            non_terminal_or_err(node.state, id, "c6_cancel")?;
            froms.push((id, node.state, NodeState::Skipped));
            node.state = NodeState::Skipped;
        }
        dag.state = crate::scheduler::DagState::Cancelled;
        self.cas(dag).await?;
        self.metrics.inc_nodes_skipped_by(skipped.len());
        for (id, from, to) in froms {
            self.append_best_effort(Self::node_state_event(
                ctx,
                id,
                from,
                to,
                generation,
                serde_json::Map::new(),
            ))
            .await;
        }
        Ok(())
    }

    // ---- C7: terminal ----

    /// C7 (failing path): `failed` node → `Failed`, `skipped` nodes →
    /// `Skipped`, `DagState → Failed`. Puts the `failure_ir` artifact first
    /// (F3) so `NodeState.failure_ref` never dangles.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c7", generation = dag.generation, nodes_changed = skipped.len() + 1)
    )]
    pub(crate) async fn c7_terminal_failed(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        failed: NodeId,
        attempt: Option<u32>,
        failure: &FailureIr,
        skipped: &[NodeId],
    ) -> Result<ArtifactId, SchedError> {
        let generation = dag.generation;
        let failure_bytes = serde_json::to_vec(failure)
            .map_err(|e| SchedError::Internal(format!("encode failure_ir: {e}")))?;
        let failure_ref = self
            .put_artifact(
                dag.id,
                Some(failed),
                ctx,
                generation,
                "failure_ir",
                ArtifactKind::Blob,
                Some("application/json"),
                failure_bytes,
            )
            .await?;

        let node = dag.nodes.get_mut(&failed).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {failed} in c7_terminal_failed"))
        })?;
        non_terminal_or_err(node.state, failed, "c7_terminal_failed")?;
        let failed_from = node.state;
        node.state = NodeState::Failed;

        let mut froms = vec![(failed, failed_from, NodeState::Failed)];
        for &id in skipped {
            let node = dag.nodes.get_mut(&id).ok_or_else(|| {
                SchedError::Invariant(format!("unknown node {id} in c7_terminal_failed"))
            })?;
            non_terminal_or_err(node.state, id, "c7_terminal_failed")?;
            froms.push((id, node.state, NodeState::Skipped));
            node.state = NodeState::Skipped;
        }
        dag.state = crate::scheduler::DagState::Failed;
        self.cas(dag).await?;
        self.metrics.inc_nodes_failed();
        self.metrics.inc_nodes_skipped_by(skipped.len());

        for (id, from, to) in froms {
            let mut extra = serde_json::Map::new();
            if id == failed {
                if let Some(a) = attempt {
                    extra.insert("attempt".into(), Value::from(a));
                }
                extra.insert("failure_ref".into(), Value::String(failure_ref.to_string()));
                extra.insert(
                    "error_class".into(),
                    Value::String(error_class_str(failure.error_class).to_string()),
                );
                extra.insert(
                    "retry".into(),
                    Value::String(retry_disposition_str(failure.retry).to_string()),
                );
            }
            self.append_best_effort(Self::node_state_event(ctx, id, from, to, generation, extra))
                .await;
        }
        Ok(failure_ref)
    }

    /// C7 (success path): every node already reached `Succeeded`/`CachedHit`
    /// via its own C4; this checkpoint only commits `DagState → Succeeded`
    /// (Appendix A). No node transitions, no events.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c7", generation = dag.generation, nodes_changed = 0)
    )]
    pub(crate) async fn c7_terminal_succeeded(&self, dag: &mut TaskDag) -> Result<(), SchedError> {
        dag.state = crate::scheduler::DagState::Succeeded;
        self.cas(dag).await
    }

    // ---- C8: retry admitted ----

    /// C8 (§5.8.3, RT1-RT8): single CAS `Running → Ready`; `DagState` stays
    /// `Running`; two events after the CAS: (c) best-effort `to:failed`
    /// (logical waypoint, never durable — RT1), then (d) **load-bearing**
    /// `to:ready` with `next_attempt` (W4a). Puts the `failure_ir` artifact
    /// first (RT2) so the event pair can reference `failure_ref`.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c8", generation = dag.generation, nodes_changed = 1)
    )]
    pub(crate) async fn c8_retry(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        attempt: u32,
        failure: &FailureIr,
        next_attempt: u32,
        backoff_ms: u64,
    ) -> Result<ArtifactId, SchedError> {
        let generation = dag.generation;
        let failure_bytes = serde_json::to_vec(failure)
            .map_err(|e| SchedError::Internal(format!("encode failure_ir: {e}")))?;
        let failure_ref = self
            .put_artifact(
                dag.id,
                Some(node_id),
                ctx,
                generation,
                "failure_ir",
                ArtifactKind::Blob,
                Some("application/json"),
                failure_bytes,
            )
            .await?;

        let node = dag
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| SchedError::Invariant(format!("unknown node {node_id} in c8_retry")))?;
        if node.state != NodeState::Running {
            return Err(SchedError::Invariant(format!(
                "c8_retry: node {node_id} is {:?}, not Running",
                node.state
            )));
        }
        node.state = NodeState::Ready; // RT1: the node never becomes durably Failed.
        self.cas(dag).await?;
        self.metrics.inc_retries_admitted();

        // (c) logical waypoint, best-effort (RT1; never RF3-repairable while Ready — RF5).
        let mut c_extra = serde_json::Map::new();
        c_extra.insert("attempt".into(), Value::from(attempt));
        c_extra.insert("failure_ref".into(), Value::String(failure_ref.to_string()));
        c_extra.insert(
            "error_class".into(),
            Value::String(error_class_str(failure.error_class).to_string()),
        );
        c_extra.insert(
            "retry".into(),
            Value::String(retry_disposition_str(failure.retry).to_string()),
        );
        self.append_best_effort(Self::node_state_event(
            ctx,
            node_id,
            NodeState::Running,
            NodeState::Failed,
            generation,
            c_extra,
        ))
        .await;

        // (d) load-bearing anchor (W4a).
        let mut d_extra = serde_json::Map::new();
        d_extra.insert("next_attempt".into(), Value::from(next_attempt));
        d_extra.insert("backoff_ms".into(), Value::from(backoff_ms));
        let d_ev = Self::node_state_event(
            ctx,
            node_id,
            NodeState::Failed,
            NodeState::Ready,
            generation,
            d_extra,
        );
        self.append_load_bearing(d_ev, dag.id).await?;
        Ok(failure_ref)
    }

    // ---- C9: gate ----

    /// C9a: `Ready → WaitingApproval`; events `NodeState` then
    /// `ApprovalRequested` (H.2).
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c9a", generation = dag.generation, nodes_changed = 1)
    )]
    pub(crate) async fn c9a_gate_schedule(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        gate_id: GateId,
        reason: &str,
        timeout_ms: u64,
    ) -> Result<(), SchedError> {
        let generation = dag.generation;
        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c9a_gate_schedule"))
        })?;
        if node.state != NodeState::Ready {
            return Err(SchedError::Invariant(format!(
                "c9a_gate_schedule: node {node_id} is {:?}, not Ready",
                node.state
            )));
        }
        node.state = NodeState::WaitingApproval;
        dag.state = crate::scheduler::DagState::WaitingApproval;
        self.cas(dag).await?;
        self.metrics.inc_gates_opened();

        self.append_best_effort(Self::node_state_event(
            ctx,
            node_id,
            NodeState::Ready,
            NodeState::WaitingApproval,
            generation,
            serde_json::Map::new(),
        ))
        .await;

        let mut payload = serde_json::Map::new();
        payload.insert("gate_id".into(), Value::String(gate_id.to_string()));
        payload.insert("node_id".into(), Value::String(node_id.to_string()));
        payload.insert("reason".into(), Value::String(reason.to_string()));
        payload.insert("timeout_ms".into(), Value::from(timeout_ms));
        payload.insert("generation".into(), Value::from(generation));
        self.append_best_effort(NewSessionEvent {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            type_: SessionEventType::ApprovalRequested,
            payload: Value::Object(payload),
        })
        .await;
        Ok(())
    }

    /// C9b: `WaitingApproval → Ready` (allow / allow_once); `DagState`
    /// becomes `Running`.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c9b", generation = dag.generation, nodes_changed = 1)
    )]
    pub(crate) async fn c9b_gate_allow(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        decision: GateDecision,
    ) -> Result<(), SchedError> {
        let generation = dag.generation;
        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c9b_gate_allow"))
        })?;
        if node.state != NodeState::WaitingApproval {
            return Err(SchedError::Invariant(format!(
                "c9b_gate_allow: node {node_id} is {:?}, not WaitingApproval",
                node.state
            )));
        }
        node.state = NodeState::Ready;
        dag.state = crate::scheduler::DagState::Running;
        self.cas(dag).await?;
        self.metrics.inc_gates_allowed();

        let mut extra = serde_json::Map::new();
        extra.insert(
            "decision".into(),
            Value::String(decision.as_str().to_string()),
        );
        self.append_best_effort(Self::node_state_event(
            ctx,
            node_id,
            NodeState::WaitingApproval,
            NodeState::Ready,
            generation,
            extra,
        ))
        .await;
        Ok(())
    }

    /// C9c: gate node → `Cancelled`, remaining non-terminal nodes →
    /// `Skipped`; `DagState → Failed` (deny/expiry are both attributed as a
    /// gate-origin failure via the `Approval` error class — reconciliation
    /// note under Appendix B). Puts an `Approval`-class `failure_ir` first.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c9c", generation = dag.generation, nodes_changed = skipped.len() + 1)
    )]
    pub(crate) async fn c9c_gate_deny(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        node_id: NodeId,
        decision: GateDecision,
        failure: &FailureIr,
        skipped: &[NodeId],
    ) -> Result<ArtifactId, SchedError> {
        let generation = dag.generation;
        let failure_bytes = serde_json::to_vec(failure)
            .map_err(|e| SchedError::Internal(format!("encode failure_ir: {e}")))?;
        let failure_ref = self
            .put_artifact(
                dag.id,
                Some(node_id),
                ctx,
                generation,
                "failure_ir",
                ArtifactKind::Blob,
                Some("application/json"),
                failure_bytes,
            )
            .await?;

        let node = dag.nodes.get_mut(&node_id).ok_or_else(|| {
            SchedError::Invariant(format!("unknown node {node_id} in c9c_gate_deny"))
        })?;
        if node.state != NodeState::WaitingApproval {
            return Err(SchedError::Invariant(format!(
                "c9c_gate_deny: node {node_id} is {:?}, not WaitingApproval",
                node.state
            )));
        }
        node.state = NodeState::Cancelled;

        let mut froms = vec![(node_id, NodeState::WaitingApproval, NodeState::Cancelled)];
        for &id in skipped {
            let n = dag.nodes.get_mut(&id).ok_or_else(|| {
                SchedError::Invariant(format!("unknown node {id} in c9c_gate_deny"))
            })?;
            non_terminal_or_err(n.state, id, "c9c_gate_deny")?;
            froms.push((id, n.state, NodeState::Skipped));
            n.state = NodeState::Skipped;
        }
        dag.state = crate::scheduler::DagState::Failed;
        self.cas(dag).await?;
        match decision {
            GateDecision::Deny => self.metrics.inc_gates_denied(),
            GateDecision::Expired => self.metrics.inc_gates_expired(),
            // `c9c_gate_deny` is deny/expiry only (WaitingApproval precondition,
            // matching the caller's contract); Allow/AllowOnce go through
            // `c9b_gate_allow` and never reach this method.
            GateDecision::Allow | GateDecision::AllowOnce => {}
        }
        self.metrics.inc_nodes_skipped_by(skipped.len());

        for (id, from, to) in froms {
            let mut extra = serde_json::Map::new();
            if id == node_id {
                extra.insert(
                    "decision".into(),
                    Value::String(decision.as_str().to_string()),
                );
                extra.insert("failure_ref".into(), Value::String(failure_ref.to_string()));
                extra.insert(
                    "error_class".into(),
                    Value::String(error_class_str(failure.error_class).to_string()),
                );
                extra.insert(
                    "retry".into(),
                    Value::String(retry_disposition_str(failure.retry).to_string()),
                );
            }
            self.append_best_effort(Self::node_state_event(ctx, id, from, to, generation, extra))
                .await;
        }
        Ok(failure_ref)
    }

    // ---- C10: replan observed ----

    /// C10: `DagState → ReplanRequired`. Node states are untouched (RP2: no
    /// in-flight future at an attempt boundary); no events.
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "c10", generation = dag.generation, nodes_changed = 0)
    )]
    pub(crate) async fn c10_replan(&self, dag: &mut TaskDag) -> Result<(), SchedError> {
        dag.state = crate::scheduler::DagState::ReplanRequired;
        self.cas(dag).await
    }

    // ---- reconcile_terminal_run: bare CAS, no node rewrite ----

    /// RC4 (no non-terminal node remains to attribute) / RC6 (`Succeeded`
    /// requested with non-terminal nodes still present): a single CAS to
    /// `to` with **no** `TaskNode.state` mutation, plus a best-effort
    /// `Error`-typed synthetic event carrying `notes` (there is no node to
    /// attach a `NodeState` event to).
    #[tracing::instrument(
        name = "sched.checkpoint",
        skip_all,
        fields(checkpoint = "reconcile_bare", generation = dag.generation, nodes_changed = 0)
    )]
    pub(crate) async fn c_reconcile_bare(
        &self,
        dag: &mut TaskDag,
        ctx: CheckpointCtx,
        to: crate::scheduler::DagState,
        notes: &str,
    ) -> Result<(), SchedError> {
        dag.state = to;
        self.cas(dag).await?;
        self.append_best_effort(NewSessionEvent {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            type_: SessionEventType::Error,
            payload: serde_json::json!({
                "reconciled_to": to,
                "notes": notes,
            }),
        })
        .await;
        Ok(())
    }

    // -------------------------------------------------------------
    // §5.3.1 attempt-counter rebuild
    // -------------------------------------------------------------

    /// §5.3.1: rebuild `attempts_started` for `node_id` at `generation` from
    /// the session event log (`TaskNode` carries no attempt field).
    /// `durably_running` selects the `Running`-only floor rule.
    ///
    /// `ctx` supplies the full §5.3.1 scan key `(session, run, node)` — the
    /// `run` half matters because Appendix F permits several run rows per
    /// DAG, so a prior run's attempts must not inflate this run's counter.
    pub(crate) async fn rebuild_attempts_started(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        generation: u64,
        durably_running: bool,
    ) -> Result<u32, SchedError> {
        let events = self
            .list_node_state_events(dag_id, ctx, node_id, generation)
            .await?;
        let mut max_attempt: u32 = 0;
        let mut max_next: u32 = 0;
        for ev in &events {
            if let Some(a) = ev.payload.get("attempt").and_then(Value::as_u64) {
                max_attempt = max_attempt.max(a as u32);
            }
            if let Some(n) = ev.payload.get("next_attempt").and_then(Value::as_u64) {
                max_next = max_next.max(n as u32);
            }
        }
        let base = max_attempt.max(max_next.saturating_sub(1));
        if durably_running {
            Ok(base.max(max_next).max(1))
        } else {
            Ok(base)
        }
    }

    /// Page every `NodeState` event for the §5.3.1 scan key
    /// `(session, run, node)` filtered to `payload.generation == generation`.
    ///
    /// **Run matching (§5.3.1).** An event belongs to this scan when its
    /// `run_id` is either this run's or `None`. `None` means the writer was
    /// not run-attributed at all — `reconcile_terminal_run` (A2: not a
    /// scheduler-aware caller) writes its RC4 node states that way, and R9
    /// still has to see them for FN1/FN2/AC 89/AC 92. An unattributed event
    /// cannot belong to a *different* run, so admitting it does not reopen
    /// the cross-run bleed this key exists to close.
    ///
    /// `pub(super)`: also used by [`Self::recover_failure_ir`]'s FO1/FO2 scan
    /// from `loop_.rs`'s R9 fast path.
    pub(super) async fn list_node_state_events(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        generation: u64,
    ) -> Result<Vec<crate::events::SessionEvent>, SchedError> {
        let node_id_str = node_id.to_string();
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .events
                .list_session_events(ctx.session_id, after, MAX_EVENTS_PAGE)
                .await
                .map_err(|e| map_store_error(e, dag_id))?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for ev in page {
                after = Some(ev.seq);
                if ev.type_ != SessionEventType::NodeState {
                    continue;
                }
                if !run_matches(ev.run_id, ctx.run_id) {
                    continue;
                }
                let matches_node =
                    ev.payload.get("node_id").and_then(Value::as_str) == Some(node_id_str.as_str());
                let matches_gen =
                    ev.payload.get("generation").and_then(Value::as_u64) == Some(generation);
                if matches_node && matches_gen {
                    out.push(ev);
                }
            }
            if page_len < MAX_EVENTS_PAGE {
                break;
            }
        }
        Ok(out)
    }

    /// FO1/FO2/FO3 (§5.18): recover the `FailureIr` for a node the caller
    /// has already confirmed reached the durable terminal state `terminal`,
    /// for the R9 already-terminal fast path
    /// (`assemble_already_terminal_outcome`) — the in-memory `FailureIr` a
    /// live `terminal_failed`/`c9c_gate_deny` call would have doesn't exist
    /// across a process restart, so this reconstructs it from what's
    /// durable. Read-only: no checkpoint/CAS.
    ///
    /// `terminal` selects which transition to look for, because the two
    /// attributable shapes differ: FN1 nodes land in `NodeState::Failed`
    /// (C7), while FN2 gate deny/expiry lands in `NodeState::Cancelled`
    /// (C9c / RC4 gate-origin) carrying the same `failure_ref` /
    /// `error_class` / `retry` fields. Matching only `to: "failed"` would
    /// silently drop every FN2 attribution to FO3's synthetic `Internal`.
    ///
    /// Ladder, each rung only reached if the one above fails:
    /// 1. FO1 — the `failure_ref` artifact named on the terminal `NodeState`
    ///    event, parsed back into a `FailureIr`.
    /// 2. FO2 — no artifact (missing/corrupt/unparseable): degrade to a
    ///    `FailureIr` built from that same event's `error_class`/`retry`
    ///    fields, `notes: "failure detail unavailable"`.
    /// 3. FO3 — no matching event at all: synthetic `Internal` /
    ///    `NonRetryable`, `notes: "failure detail unavailable; event missing"`.
    pub(super) async fn recover_failure_ir(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        generation: u64,
        terminal: NodeState,
    ) -> Result<FailureIr, SchedError> {
        let events = self
            .list_node_state_events(dag_id, ctx, node_id, generation)
            .await?;
        // Most-recent match wins: a node that failed, retried (C8's own
        // best-effort `to:failed` waypoint), and failed again durably has
        // more than one `to:failed` event for the same generation, and only
        // the last one is the terminal one.
        let Some(ev) = events
            .iter()
            .rev()
            .find(|ev| ev.payload.get("to").and_then(Value::as_str) == Some(state_str(terminal)))
        else {
            // FO3 floor: no matching event survived at all.
            return Ok(FailureIr {
                node: node_id,
                error_class: ErrorClass::Internal,
                retry: RetryDisposition::NonRetryable,
                diagnostics: vec![],
                notes: "failure detail unavailable; event missing".into(),
            });
        };

        // FO1: the durable failure_ref artifact, when present and parseable.
        if let Some(failure_ref) = ev.payload.get("failure_ref").and_then(Value::as_str) {
            if let Ok(id) = ArtifactId::parse(failure_ref) {
                if let Ok(blob) = self.artifacts.get(id).await {
                    if let Ok(failure) = serde_json::from_slice::<FailureIr>(&blob.bytes) {
                        return Ok(failure);
                    }
                }
            }
        }

        // FO2: degrade to the event's own fields.
        let error_class = ev
            .payload
            .get("error_class")
            .and_then(|v| serde_json::from_value::<ErrorClass>(v.clone()).ok())
            .unwrap_or(ErrorClass::Internal);
        let retry = ev
            .payload
            .get("retry")
            .and_then(|v| serde_json::from_value::<RetryDisposition>(v.clone()).ok())
            .unwrap_or(RetryDisposition::NonRetryable);
        Ok(FailureIr {
            node: node_id,
            error_class,
            retry,
            diagnostics: vec![],
            notes: "failure detail unavailable".into(),
        })
    }

    // -------------------------------------------------------------
    // RF1-RF8: CAS-before-events crash repair
    // -------------------------------------------------------------

    /// RF1-RF3/RF5: repair a missing `NodeState` event for a transition the
    /// blob already committed. RF1 (authoritative state is the blob) is the
    /// caller's job — this function is handed `to` by the caller, which
    /// read it off `dag.nodes[node_id].state`. Idempotent: if an event
    /// already matches the RF5 dedup key, no new event is appended and
    /// `Ok(false)` is returned; otherwise a `repaired: true` event is
    /// appended and `Ok(true)` is returned (and `event_repairs` bumped).
    ///
    /// Wired into R9's already-terminal path (`loop_.rs`) via
    /// [`Self::repair_gate_terminal`], and exercised directly by its own
    /// unit tests (crash-window simulation).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn repair_node_state(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        from: NodeState,
        to: NodeState,
        generation: u64,
        attempt: Option<u32>,
        next_attempt: Option<u32>,
        failure: Option<RepairedFailure>,
    ) -> Result<bool, SchedError> {
        let existing = self
            .list_node_state_events(dag_id, ctx, node_id, generation)
            .await?;
        // RF5 dedup key: (node, generation, to, attempt) when attempt is
        // present, (node, generation, to, next_attempt) when next_attempt is
        // present, otherwise (node, generation, to).
        let already = existing.iter().any(|ev| {
            let ev_to = ev.payload.get("to").and_then(Value::as_str);
            if ev_to != Some(state_str(to)) {
                return false;
            }
            match (attempt, next_attempt) {
                (Some(a), _) => {
                    ev.payload.get("attempt").and_then(Value::as_u64) == Some(u64::from(a))
                }
                (None, Some(n)) => {
                    ev.payload.get("next_attempt").and_then(Value::as_u64) == Some(u64::from(n))
                }
                (None, None) => true,
            }
        });
        if already {
            return Ok(false);
        }
        let mut extra = serde_json::Map::new();
        if let Some(a) = attempt {
            extra.insert("attempt".into(), Value::from(a));
        }
        if let Some(n) = next_attempt {
            extra.insert("next_attempt".into(), Value::from(n));
        }
        // RF7: a repaired terminal event MUST carry the same failure fields
        // the lost original did, or FO1/FO2 recover nothing from it and FN2
        // silently degrades to FO3's synthetic `Internal`.
        if let Some(f) = failure {
            extra.insert(
                "failure_ref".into(),
                Value::String(f.failure_ref.to_string()),
            );
            extra.insert(
                "error_class".into(),
                Value::String(error_class_str(f.error_class).to_string()),
            );
            extra.insert(
                "retry".into(),
                Value::String(retry_disposition_str(f.retry).to_string()),
            );
        }
        extra.insert("repaired".into(), Value::Bool(true));
        let ev = Self::node_state_event(ctx, node_id, from, to, generation, extra);
        self.append_load_bearing(ev, dag_id).await?;
        self.metrics.inc_event_repairs();
        Ok(true)
    }

    /// RF6: repair a missing `ApprovalRequested` for a committed C9a. Same
    /// idempotence contract as [`Self::repair_node_state`], keyed on
    /// `(gate_id, generation)`.
    ///
    /// Wired into `gate.rs`'s `gate_remaining_deadline` (§5.7.3 GR3 — the only
    /// place the scheduler may re-emit it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn repair_approval_requested(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        gate_id: GateId,
        reason: &str,
        timeout_ms: u64,
        generation: u64,
    ) -> Result<bool, SchedError> {
        let gate_id_str = gate_id.to_string();
        let mut after = None;
        loop {
            let page = self
                .events
                .list_session_events(ctx.session_id, after, MAX_EVENTS_PAGE)
                .await
                .map_err(|e| map_store_error(e, dag_id))?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for ev in &page {
                after = Some(ev.seq);
                if ev.type_ != SessionEventType::ApprovalRequested {
                    continue;
                }
                let matches_gate =
                    ev.payload.get("gate_id").and_then(Value::as_str) == Some(gate_id_str.as_str());
                let matches_gen =
                    ev.payload.get("generation").and_then(Value::as_u64) == Some(generation);
                if matches_gate && matches_gen {
                    return Ok(false);
                }
            }
            if page_len < MAX_EVENTS_PAGE {
                break;
            }
        }
        let mut payload = serde_json::Map::new();
        payload.insert("gate_id".into(), Value::String(gate_id_str));
        payload.insert("node_id".into(), Value::String(node_id.to_string()));
        payload.insert("reason".into(), Value::String(reason.to_string()));
        payload.insert("timeout_ms".into(), Value::from(timeout_ms));
        payload.insert("generation".into(), Value::from(generation));
        payload.insert("repaired".into(), Value::Bool(true));
        self.events
            .append_session(NewSessionEvent {
                session_id: ctx.session_id,
                run_id: ctx.run_id,
                type_: SessionEventType::ApprovalRequested,
                payload: Value::Object(payload),
            })
            .await
            .map_err(|e| SchedError::Store(format!("RF6 repair append failed: {e}")))?;
        self.metrics.inc_event_repairs();
        Ok(true)
    }

    /// RF7: gate deny/expiry CAS committed with a missing `NodeState` and/or
    /// missing `failure_ir` artifact. Repairs the `NodeState` (RF3) and, if
    /// `failure_ref` cannot be resolved, puts a synthetic `Approval`
    /// `failure_ir` (`notes: "repaired after crash"`) so FN2/FO2/FO6 hold.
    /// Returns the `failure_ir` artifact id in use (existing or synthesized).
    ///
    /// Idempotent, and cheap when nothing is broken: it resolves the durable
    /// `failure_ref` itself (rather than trusting a caller-supplied one) and
    /// returns without writing when the `Cancelled` event and its artifact
    /// are both already intact. Called from R9 before FN2 selection
    /// (`loop_.rs::assemble_already_terminal_outcome`, AC 92) and from the
    /// gate-origin reconcile path.
    pub(crate) async fn repair_gate_terminal(
        &self,
        dag_id: DagId,
        ctx: CheckpointCtx,
        node_id: NodeId,
        generation: u64,
    ) -> Result<ArtifactId, SchedError> {
        // What survived the crash? The `Cancelled` event may be missing
        // entirely, or present but with an unresolvable `failure_ref`.
        let events = self
            .list_node_state_events(dag_id, ctx, node_id, generation)
            .await?;
        let cancelled_ev = events.iter().rev().find(|ev| {
            ev.payload.get("to").and_then(Value::as_str) == Some(state_str(NodeState::Cancelled))
        });
        let durable_ref = match cancelled_ev {
            Some(ev) => ev
                .payload
                .get("failure_ref")
                .and_then(Value::as_str)
                .and_then(|s| ArtifactId::parse(s).ok()),
            None => None,
        };
        if let Some(id) = durable_ref {
            // Only intact if the artifact itself still resolves (FO1).
            if self.artifacts.get(id).await.is_ok() {
                return Ok(id);
            }
        }

        let synthetic = FailureIr {
            node: node_id,
            error_class: ErrorClass::Approval,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "repaired after crash".into(),
        };
        let bytes = serde_json::to_vec(&synthetic)
            .map_err(|e| SchedError::Internal(format!("encode failure_ir: {e}")))?;
        let failure_ref = self
            .put_artifact(
                dag_id,
                Some(node_id),
                ctx,
                generation,
                "failure_ir",
                ArtifactKind::Blob,
                Some("application/json"),
                bytes,
            )
            .await?;
        // When the event is missing this appends it carrying the synthetic
        // failure fields (so FO1 resolves); when it already exists the RF5
        // dedup key short-circuits and FO2 reads the surviving event's own
        // `error_class`, which is what FN2 needs either way.
        self.repair_node_state(
            dag_id,
            ctx,
            node_id,
            NodeState::WaitingApproval,
            NodeState::Cancelled,
            generation,
            None,
            None,
            Some(RepairedFailure {
                failure_ref,
                error_class: ErrorClass::Approval,
                retry: RetryDisposition::NonRetryable,
            }),
        )
        .await?;
        Ok(failure_ref)
    }
}

/// Durable failure fields an RF7-repaired `NodeState` event must carry so
/// FO1/FO2 can recover a structured failure from it (§5.18).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RepairedFailure {
    /// Artifact holding the serialized [`FailureIr`].
    pub failure_ref: ArtifactId,
    /// Class the repaired event reports.
    pub error_class: ErrorClass,
    /// Retry disposition the repaired event reports.
    pub retry: RetryDisposition,
}

/// §5.3.1 / §5.7.2 run half of the scan key: an event belongs to `want` when
/// it is attributed to that run, or to no run at all (see
/// [`Checkpoint::list_node_state_events`] for why unattributed events count).
pub(super) fn run_matches(event_run: Option<RunId>, want: Option<RunId>) -> bool {
    match (event_run, want) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

fn non_terminal_or_err(state: NodeState, id: NodeId, ctx: &str) -> Result<(), SchedError> {
    if matches!(
        state,
        NodeState::Pending | NodeState::Ready | NodeState::Running | NodeState::WaitingApproval
    ) {
        Ok(())
    } else {
        Err(SchedError::Invariant(format!(
            "{ctx}: node {id} is {state:?}, not non-terminal"
        )))
    }
}

fn state_json(s: NodeState) -> Value {
    Value::String(state_str(s).to_string())
}

fn state_str(s: NodeState) -> &'static str {
    match s {
        NodeState::Pending => "pending",
        NodeState::Ready => "ready",
        NodeState::Running => "running",
        NodeState::Succeeded => "succeeded",
        NodeState::Failed => "failed",
        NodeState::Skipped => "skipped",
        NodeState::Cancelled => "cancelled",
        NodeState::WaitingApproval => "waiting_approval",
        NodeState::CachedHit => "cached_hit",
    }
}

fn error_class_str(c: ErrorClass) -> &'static str {
    match c {
        ErrorClass::Compile => "compile",
        ErrorClass::Test => "test",
        ErrorClass::Tool => "tool",
        ErrorClass::Model => "model",
        ErrorClass::Budget => "budget",
        ErrorClass::Approval => "approval",
        ErrorClass::Internal => "internal",
        ErrorClass::Timeout => "timeout",
        ErrorClass::Cancelled => "cancelled",
    }
}

fn retry_disposition_str(r: RetryDisposition) -> &'static str {
    match r {
        RetryDisposition::Retryable => "retryable",
        RetryDisposition::NonRetryable => "non_retryable",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::dag::{ApprovalSpec, Backoff, NodeKind, RetryPolicy, TaskNode};
    use crate::events::EventSink;
    use crate::scheduler::DagState;
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::diagnostic::DiagnosticEvent;
    use crate::types::ids::ArtifactId;

    struct Fixture {
        _dir: tempfile::TempDir,
        storage: AlloyStorage,
        checkpoint: Checkpoint,
        metrics: Arc<SchedulerCounters>,
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
                Arc::clone(&metrics),
            );
            Self {
                _dir: dir,
                storage,
                checkpoint,
                metrics,
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
    }

    fn plain_node(id: NodeId, state: NodeState) -> TaskNode {
        TaskNode {
            id,
            kind: NodeKind::Analyze,
            capability: None,
            input_ref: ArtifactId::new(),
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

    fn gate_node(id: NodeId, gate: GateId) -> TaskNode {
        let mut n = plain_node(id, NodeState::Pending);
        n.kind = NodeKind::GateHuman;
        n.approval = Some(ApprovalSpec {
            gate,
            reason: "please review".into(),
        });
        n
    }

    async fn seeded_dag(fx: &Fixture, session_id: SessionId, nodes: Vec<TaskNode>) -> TaskDag {
        let mut map = BTreeMap::new();
        for n in nodes {
            map.insert(n.id, n);
        }
        let dag = TaskDag {
            id: DagId::new(),
            session_id,
            generation: 1,
            nodes: map,
            edges: vec![],
            state: DagState::Pending,
        };
        fx.storage.dags().put(&dag).await.unwrap();
        dag
    }

    fn sample_failure(node: NodeId) -> FailureIr {
        FailureIr {
            node,
            error_class: ErrorClass::Compile,
            retry: RetryDisposition::Retryable,
            diagnostics: Vec::<DiagnosticEvent>::new(),
            notes: "cargo check failed".into(),
        }
    }

    // ---- StoreError mapping ----

    #[test]
    fn map_store_error_on_load_maps_not_found_to_dag_not_found() {
        let id = DagId::new();
        let err = map_store_error_on_load(StoreError::NotFound("x".into()), id);
        assert!(matches!(err, SchedError::DagNotFound(got) if got == id));
    }

    #[test]
    fn map_store_error_covers_every_variant() {
        let id = DagId::new();
        assert!(matches!(
            map_store_error(StoreError::Conflict("x".into()), id),
            SchedError::Conflict { dag_id } if dag_id == id
        ));
        assert!(matches!(
            map_store_error(StoreError::NotFound("x".into()), id),
            SchedError::Store(_)
        ));
        assert!(matches!(
            map_store_error(StoreError::Corrupt("x".into()), id),
            SchedError::Invariant(m) if m.contains("corrupt dag blob")
        ));
        assert!(matches!(
            map_store_error(StoreError::Busy, id),
            SchedError::Store(m) if m == "busy"
        ));
        assert!(matches!(
            map_store_error(StoreError::Internal("x".into()), id),
            SchedError::Store(_)
        ));
        assert!(matches!(
            map_store_error(StoreError::Io("x".into()), id),
            SchedError::Store(_)
        ));
        assert!(matches!(
            map_store_error(StoreError::Migration("x".into()), id),
            SchedError::Invariant(m) if m.contains("store migration")
        ));
        assert!(matches!(
            map_store_error(StoreError::DigestMismatch, id),
            SchedError::Invariant(m) if m.contains("digest mismatch")
        ));
        assert!(matches!(
            map_store_error(StoreError::Closed, id),
            SchedError::Store(m) if m.contains("closed")
        ));
    }

    // ---- C1 ----

    #[tokio::test]
    async fn c1_start_moves_pending_to_running() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![plain_node(NodeId::new(), NodeState::Pending)],
        )
        .await;

        fx.checkpoint.c1_start(&mut dag).await.unwrap();
        assert_eq!(dag.state, DagState::Running);
        let got = fx.storage.dags().get(dag.id).await.unwrap().unwrap();
        assert_eq!(got.state, DagState::Running);
        fx.close().await;
    }

    #[tokio::test]
    async fn c1_start_conflict_on_stale_generation() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![plain_node(NodeId::new(), NodeState::Pending)],
        )
        .await;
        // Someone else bumps the generation out from under us.
        let mut winner = dag.clone();
        winner.generation = 2;
        fx.storage.dags().put(&winner).await.unwrap();

        let err = fx.checkpoint.c1_start(&mut dag).await.unwrap_err();
        assert!(matches!(err, SchedError::Conflict { dag_id } if dag_id == dag.id));
        assert_eq!(fx.metrics.snapshot().cas_conflicts, 1);
        fx.close().await;
    }

    // ---- C2 ----

    #[tokio::test]
    async fn c2_promote_moves_pending_to_ready_and_appends_events() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![
                plain_node(a, NodeState::Pending),
                plain_node(b, NodeState::Pending),
            ],
        )
        .await;
        dag.state = DagState::Running;
        fx.storage.dags().put(&dag).await.unwrap();

        let ctx = fx.ctx(session);
        fx.checkpoint
            .c2_promote(&mut dag, ctx, &[a, b])
            .await
            .unwrap();
        assert_eq!(dag.nodes[&a].state, NodeState::Ready);
        assert_eq!(dag.nodes[&b].state, NodeState::Ready);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let node_state_count = events
            .iter()
            .filter(|e| e.type_ == SessionEventType::NodeState)
            .count();
        assert_eq!(node_state_count, 2);
        fx.close().await;
    }

    #[tokio::test]
    async fn c2_promote_rejects_non_pending_node() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        let ctx = fx.ctx(session);
        let err = fx
            .checkpoint
            .c2_promote(&mut dag, ctx, &[a])
            .await
            .unwrap_err();
        assert!(matches!(err, SchedError::Invariant(_)));
        fx.close().await;
    }

    // ---- C3 ----

    #[tokio::test]
    async fn c3_dispatch_moves_ready_to_running_with_load_bearing_event() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Ready)]).await;
        let ctx = fx.ctx(session);

        fx.checkpoint
            .c3_dispatch(&mut dag, ctx, a, 1)
            .await
            .unwrap();
        assert_eq!(dag.nodes[&a].state, NodeState::Running);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let ev = events
            .iter()
            .find(|e| e.type_ == SessionEventType::NodeState)
            .unwrap();
        assert_eq!(ev.payload["to"], "running");
        assert_eq!(ev.payload["attempt"], 1);
        fx.close().await;
    }

    // ---- C4 ----

    #[tokio::test]
    async fn c4_succeed_sets_output_ref_and_state() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        let ctx = fx.ctx(session);

        let payload_bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1})).unwrap();
        let artifact_id = fx
            .checkpoint
            .c4_succeed(&mut dag, ctx, a, 1, payload_bytes)
            .await
            .unwrap();
        assert_eq!(dag.nodes[&a].state, NodeState::Succeeded);
        assert_eq!(dag.nodes[&a].output_ref, Some(artifact_id));

        let blob = fx.storage.artifacts().get(artifact_id).await.unwrap();
        assert_eq!(
            blob.meta
                .labels
                .get("alloy.envelope")
                .and_then(|v| v.as_str()),
            Some("node_output")
        );
        fx.close().await;
    }

    // ---- C5 ----

    #[tokio::test]
    async fn c5_rewrite_input_updates_input_ref_and_keeps_state() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Ready)]).await;
        let ctx = fx.ctx(session);
        let old_ref = dag.nodes[&a].input_ref;

        let new_ref = fx
            .checkpoint
            .c5_rewrite_input(&mut dag, ctx, a, b"{}".to_vec())
            .await
            .unwrap();
        assert_ne!(new_ref, old_ref);
        assert_eq!(dag.nodes[&a].input_ref, new_ref);
        assert_eq!(dag.nodes[&a].state, NodeState::Ready);
        fx.close().await;
    }

    // ---- C6 ----

    #[tokio::test]
    async fn c6_cancel_marks_in_flight_cancelled_and_others_skipped() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let running = NodeId::new();
        let pending = NodeId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![
                plain_node(running, NodeState::Running),
                plain_node(pending, NodeState::Pending),
            ],
        )
        .await;
        let ctx = fx.ctx(session);

        fx.checkpoint
            .c6_cancel(&mut dag, ctx, &[running], &[pending])
            .await
            .unwrap();
        assert_eq!(dag.nodes[&running].state, NodeState::Cancelled);
        assert_eq!(dag.nodes[&pending].state, NodeState::Skipped);
        assert_eq!(dag.state, DagState::Cancelled);
        fx.close().await;
    }

    // ---- C7 ----

    #[tokio::test]
    async fn c7_terminal_failed_marks_failed_and_skips_rest() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let failed = NodeId::new();
        let other = NodeId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![
                plain_node(failed, NodeState::Running),
                plain_node(other, NodeState::Pending),
            ],
        )
        .await;
        let ctx = fx.ctx(session);
        let failure = sample_failure(failed);

        let failure_ref = fx
            .checkpoint
            .c7_terminal_failed(&mut dag, ctx, failed, Some(2), &failure, &[other])
            .await
            .unwrap();
        assert_eq!(dag.nodes[&failed].state, NodeState::Failed);
        assert_eq!(dag.nodes[&other].state, NodeState::Skipped);
        assert_eq!(dag.state, DagState::Failed);

        let blob = fx.storage.artifacts().get(failure_ref).await.unwrap();
        let decoded: FailureIr = serde_json::from_slice(&blob.bytes).unwrap();
        assert_eq!(decoded.error_class, ErrorClass::Compile);
        fx.close().await;
    }

    #[tokio::test]
    async fn c7_terminal_succeeded_sets_dag_state_only() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Succeeded)]).await;

        fx.checkpoint.c7_terminal_succeeded(&mut dag).await.unwrap();
        assert_eq!(dag.state, DagState::Succeeded);
        assert_eq!(dag.nodes[&a].state, NodeState::Succeeded);
        fx.close().await;
    }

    // ---- C8 ----

    #[tokio::test]
    async fn c8_retry_leaves_node_ready_and_dag_running() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        dag.state = DagState::Running;
        fx.storage.dags().put(&dag).await.unwrap();
        let ctx = fx.ctx(session);
        let failure = sample_failure(a);

        fx.checkpoint
            .c8_retry(&mut dag, ctx, a, 1, &failure, 2, 500)
            .await
            .unwrap();
        assert_eq!(dag.nodes[&a].state, NodeState::Ready);
        assert_eq!(dag.state, DagState::Running);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let node_state: Vec<_> = events
            .iter()
            .filter(|e| e.type_ == SessionEventType::NodeState)
            .collect();
        assert_eq!(node_state.len(), 2);
        assert_eq!(node_state[0].payload["to"], "failed");
        assert_eq!(node_state[1].payload["to"], "ready");
        assert_eq!(node_state[1].payload["next_attempt"], 2);
        fx.close().await;
    }

    // ---- C9 ----

    #[tokio::test]
    async fn c9a_then_c9b_gate_allow_round_trip() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![gate_node(node_id, gate)]).await;
        {
            let n = dag.nodes.get_mut(&node_id).unwrap();
            n.state = NodeState::Ready;
        }
        dag.state = DagState::Running;
        fx.storage.dags().put(&dag).await.unwrap();
        let ctx = fx.ctx(session);

        fx.checkpoint
            .c9a_gate_schedule(&mut dag, ctx, node_id, gate, "please review", 60_000)
            .await
            .unwrap();
        assert_eq!(dag.nodes[&node_id].state, NodeState::WaitingApproval);
        assert_eq!(dag.state, DagState::WaitingApproval);

        fx.checkpoint
            .c9b_gate_allow(&mut dag, ctx, node_id, GateDecision::Allow)
            .await
            .unwrap();
        assert_eq!(dag.nodes[&node_id].state, NodeState::Ready);
        assert_eq!(dag.state, DagState::Running);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|e| e.type_ == SessionEventType::ApprovalRequested));
        fx.close().await;
    }

    #[tokio::test]
    async fn c9c_gate_deny_cancels_gate_and_skips_rest() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let other = NodeId::new();
        let mut dag = seeded_dag(
            &fx,
            session,
            vec![
                gate_node(node_id, gate),
                plain_node(other, NodeState::Pending),
            ],
        )
        .await;
        {
            let n = dag.nodes.get_mut(&node_id).unwrap();
            n.state = NodeState::WaitingApproval;
        }
        dag.state = DagState::WaitingApproval;
        fx.storage.dags().put(&dag).await.unwrap();
        let ctx = fx.ctx(session);
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Approval,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "approval denied".into(),
        };

        fx.checkpoint
            .c9c_gate_deny(
                &mut dag,
                ctx,
                node_id,
                GateDecision::Deny,
                &failure,
                &[other],
            )
            .await
            .unwrap();
        assert_eq!(dag.nodes[&node_id].state, NodeState::Cancelled);
        assert_eq!(dag.nodes[&other].state, NodeState::Skipped);
        assert_eq!(dag.state, DagState::Failed);
        fx.close().await;
    }

    // ---- C10 ----

    #[tokio::test]
    async fn c10_replan_sets_dag_state_only() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let mut dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Ready)]).await;

        fx.checkpoint.c10_replan(&mut dag).await.unwrap();
        assert_eq!(dag.state, DagState::ReplanRequired);
        assert_eq!(dag.nodes[&a].state, NodeState::Ready);
        fx.close().await;
    }

    // ---- §5.3.1 attempt rebuild ----

    #[tokio::test]
    async fn rebuild_attempts_started_first_c3_lost_event_floors_at_one() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;

        // No events at all for this node/generation: durably Running must
        // still floor at 1 (first C3 lost its event).
        let attempts = fx
            .checkpoint
            .rebuild_attempts_started(dag.id, fx.ctx(session), a, dag.generation, true)
            .await
            .unwrap();
        assert_eq!(attempts, 1);
        fx.close().await;
    }

    #[tokio::test]
    async fn rebuild_attempts_started_c8_next_attempt_then_lost_c3() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        let ctx = fx.ctx(session);

        // C8 recorded next_attempt = 2 (attempt 1 failed and was retried);
        // the C3 event for attempt 2 was then lost. attempts_started must be 2.
        let mut extra = serde_json::Map::new();
        extra.insert("next_attempt".into(), Value::from(2));
        let ev = Checkpoint::node_state_event(
            ctx,
            a,
            NodeState::Failed,
            NodeState::Ready,
            dag.generation,
            extra,
        );
        fx.storage.events().append_session(ev).await.unwrap();

        let attempts = fx
            .checkpoint
            .rebuild_attempts_started(dag.id, ctx, a, dag.generation, true)
            .await
            .unwrap();
        assert_eq!(attempts, 2);
        fx.close().await;
    }

    #[tokio::test]
    async fn rebuild_attempts_started_ignores_other_generation() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Ready)]).await;
        let ctx = fx.ctx(session);

        let mut extra = serde_json::Map::new();
        extra.insert("attempt".into(), Value::from(5));
        let ev = Checkpoint::node_state_event(
            ctx,
            a,
            NodeState::Ready,
            NodeState::Running,
            dag.generation + 1, // different generation — a prior plan's replay
            extra,
        );
        fx.storage.events().append_session(ev).await.unwrap();

        let attempts = fx
            .checkpoint
            .rebuild_attempts_started(dag.id, ctx, a, dag.generation, false)
            .await
            .unwrap();
        assert_eq!(attempts, 0);
        fx.close().await;
    }

    // ---- RF1-RF5 crash repair ----

    #[tokio::test]
    async fn repair_node_state_appends_when_missing_and_is_idempotent() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        let ctx = fx.ctx(session);

        // Simulate: C3's CAS committed (node is Running in the blob) but its
        // event append was lost to a crash — no NodeState event exists yet.
        let repaired = fx
            .checkpoint
            .repair_node_state(
                dag.id,
                ctx,
                a,
                NodeState::Ready,
                NodeState::Running,
                dag.generation,
                Some(1),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(repaired, "first call must append the repair event");
        assert_eq!(fx.metrics.snapshot().event_repairs, 1);

        let again = fx
            .checkpoint
            .repair_node_state(
                dag.id,
                ctx,
                a,
                NodeState::Ready,
                NodeState::Running,
                dag.generation,
                Some(1),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(!again, "second call must be a no-op (RF5 idempotence)");
        assert_eq!(fx.metrics.snapshot().event_repairs, 1);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let node_state_count = events
            .iter()
            .filter(|e| e.type_ == SessionEventType::NodeState)
            .count();
        assert_eq!(node_state_count, 1);
        fx.close().await;
    }

    #[tokio::test]
    async fn repair_node_state_next_retry_cycle_repairable_after_prior_attempt_repaired() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Running)]).await;
        let ctx = fx.ctx(session);

        // Attempt 1's running event is missing and gets repaired.
        fx.checkpoint
            .repair_node_state(
                dag.id,
                ctx,
                a,
                NodeState::Ready,
                NodeState::Running,
                dag.generation,
                Some(1),
                None,
                None,
            )
            .await
            .unwrap();

        // A later retry cycle's attempt 2 running event is also missing and
        // must still be independently repairable (RF5: dedup keys on
        // attempt, not just `to`).
        let repaired = fx
            .checkpoint
            .repair_node_state(
                dag.id,
                ctx,
                a,
                NodeState::Ready,
                NodeState::Running,
                dag.generation,
                Some(2),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(repaired);
        assert_eq!(fx.metrics.snapshot().event_repairs, 2);
        fx.close().await;
    }

    // ---- RF6/RF7 gate repair ----

    #[tokio::test]
    async fn repair_approval_requested_appends_when_missing_and_is_idempotent() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![gate_node(node_id, gate)]).await;
        let ctx = fx.ctx(session);

        let repaired = fx
            .checkpoint
            .repair_approval_requested(
                dag.id,
                ctx,
                node_id,
                gate,
                "please review",
                60_000,
                dag.generation,
            )
            .await
            .unwrap();
        assert!(repaired);

        let again = fx
            .checkpoint
            .repair_approval_requested(
                dag.id,
                ctx,
                node_id,
                gate,
                "please review",
                60_000,
                dag.generation,
            )
            .await
            .unwrap();
        assert!(!again);

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let count = events
            .iter()
            .filter(|e| e.type_ == SessionEventType::ApprovalRequested)
            .count();
        assert_eq!(count, 1);
        fx.close().await;
    }

    #[tokio::test]
    async fn repair_gate_terminal_synthesizes_failure_ir_when_missing() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![gate_node(node_id, gate)]).await;
        let ctx = fx.ctx(session);

        let failure_ref = fx
            .checkpoint
            .repair_gate_terminal(dag.id, ctx, node_id, dag.generation)
            .await
            .unwrap();

        let blob = fx.storage.artifacts().get(failure_ref).await.unwrap();
        let decoded: FailureIr = serde_json::from_slice(&blob.bytes).unwrap();
        assert_eq!(decoded.error_class, ErrorClass::Approval);
        assert_eq!(decoded.notes, "repaired after crash");

        let events = fx
            .storage
            .events()
            .list_session_events(session, None, 10)
            .await
            .unwrap();
        let repaired = events
            .iter()
            .find(|e| e.type_ == SessionEventType::NodeState && e.payload["to"] == "cancelled")
            .expect("RF7 must append the lost cancelled event");
        // The repaired event has to carry the failure fields too, or FO1/FO2
        // recover nothing from it and FN2 degrades to FO3's `Internal`.
        assert_eq!(repaired.payload["failure_ref"], failure_ref.to_string());
        assert_eq!(repaired.payload["error_class"], "approval");
        assert_eq!(repaired.payload["retry"], "non_retryable");
        fx.close().await;
    }

    #[tokio::test]
    async fn repair_gate_terminal_is_idempotent_across_repeated_resumes() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![gate_node(node_id, gate)]).await;
        let ctx = fx.ctx(session);

        let first = fx
            .checkpoint
            .repair_gate_terminal(dag.id, ctx, node_id, dag.generation)
            .await
            .unwrap();
        let second = fx
            .checkpoint
            .repair_gate_terminal(dag.id, ctx, node_id, dag.generation)
            .await
            .unwrap();
        // Second resume must resolve the first repair's artifact, not mint a
        // fresh one and re-append: R9 calls this on every already-terminal
        // `run()`, which for a polled CLI is unbounded.
        assert_eq!(first, second);
        assert_eq!(fx.metrics.snapshot().event_repairs, 1);
        fx.close().await;
    }

    #[tokio::test]
    async fn repair_gate_terminal_reuses_existing_failure_ref() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let gate = GateId::new();
        let node_id = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![gate_node(node_id, gate)]).await;
        let ctx = fx.ctx(session);
        let failure = FailureIr {
            node: node_id,
            error_class: ErrorClass::Approval,
            retry: RetryDisposition::NonRetryable,
            diagnostics: vec![],
            notes: "approval denied".into(),
        };
        let bytes = serde_json::to_vec(&failure).unwrap();
        let existing_ref = fx
            .checkpoint
            .put_artifact(
                dag.id,
                Some(node_id),
                ctx,
                dag.generation,
                "failure_ir",
                ArtifactKind::Blob,
                Some("application/json"),
                bytes,
            )
            .await
            .unwrap();
        // Nothing was lost: the C9c event survived and names the artifact.
        fx.storage
            .events()
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: None,
                type_: SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": node_id.to_string(),
                    "from": "waiting_approval",
                    "to": "cancelled",
                    "generation": dag.generation,
                    "failure_ref": existing_ref.to_string(),
                    "error_class": "approval",
                    "retry": "non_retryable",
                }),
            })
            .await
            .unwrap();

        let returned = fx
            .checkpoint
            .repair_gate_terminal(dag.id, ctx, node_id, dag.generation)
            .await
            .unwrap();
        assert_eq!(returned, existing_ref);
        assert_eq!(
            fx.metrics.snapshot().event_repairs,
            0,
            "an intact gate terminal must not be repaired"
        );
        fx.close().await;
    }

    // ---- §5.3.1 run half of the scan key ----

    #[test]
    fn run_matches_admits_same_run_and_unattributed_only() {
        let a = RunId::new();
        let b = RunId::new();
        assert!(run_matches(Some(a), Some(a)));
        assert!(!run_matches(Some(b), Some(a)), "cross-run must not match");
        // Unattributed writers (reconcile, A2) stay visible to every run.
        assert!(run_matches(None, Some(a)));
        assert!(run_matches(Some(a), None));
    }

    #[tokio::test]
    async fn attempt_rebuild_ignores_another_runs_events() {
        let fx = Fixture::new().await;
        let session = SessionId::new();
        let a = NodeId::new();
        let dag = seeded_dag(&fx, session, vec![plain_node(a, NodeState::Ready)]).await;
        let mine = RunId::new();
        let theirs = RunId::new();

        // A different run of the same DAG+generation reached attempt 3.
        fx.storage
            .events()
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: Some(theirs),
                type_: SessionEventType::NodeState,
                payload: serde_json::json!({
                    "node_id": a.to_string(),
                    "from": "ready",
                    "to": "running",
                    "generation": dag.generation,
                    "attempt": 3u64,
                }),
            })
            .await
            .unwrap();

        let ctx = CheckpointCtx {
            session_id: session,
            run_id: Some(mine),
        };
        let attempts = fx
            .checkpoint
            .rebuild_attempts_started(dag.id, ctx, a, dag.generation, false)
            .await
            .unwrap();
        assert_eq!(
            attempts, 0,
            "another run's attempts must not burn this run's retry budget"
        );
        fx.close().await;
    }
}
