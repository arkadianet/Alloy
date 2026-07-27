//! Pure TaskDag validation (RFC-0009 §5.4).
//!
//! Stateless. No I/O. First violation wins under published rule order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use crate::types::ids::{CapabilityId, GateId, NodeId};

use super::types::{
    Backoff, DependencyEdge, EdgeKind, NodeKind, NodeState, RetryPolicy, TaskDag, TaskNode,
};

/// Why a [`TaskDag`] was rejected. One variant per validation rule (RFC-0009 §5.4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DagValidationError {
    /// DAG has no nodes.
    #[error("DAG has no nodes")]
    Empty,

    /// Node map key does not match `node.id`.
    #[error("node map key {key} != node.id {node_id}")]
    NodeIdMismatch {
        /// Map key.
        key: NodeId,
        /// `TaskNode.id`.
        node_id: NodeId,
    },

    /// Edge references a missing node.
    #[error("edge endpoint missing: {node}")]
    MissingEndpoint {
        /// Missing node id.
        node: NodeId,
    },

    /// Self-loop edge.
    #[error("self-loop on node {node}")]
    SelfLoop {
        /// Node with self-loop.
        node: NodeId,
    },

    /// Cycle in Data∪Sequence graph.
    #[error("cycle detected involving node {node}")]
    Cycle {
        /// Node on the first back-edge (`to`).
        node: NodeId,
    },

    /// Not exactly one root.
    #[error("DAG must have exactly one root; found {count}")]
    MultipleRoots {
        /// Number of roots found.
        count: usize,
    },

    /// Node unreachable from the unique root.
    #[error("node {node} is unreachable from the unique root")]
    Unreachable {
        /// Unreachable node (lowest id when multiple).
        node: NodeId,
    },

    /// LLM node missing required capability.
    #[error("node {node} kind {kind:?} requires capability {expected}, got {got:?}")]
    CapabilityRequired {
        /// Node id.
        node: NodeId,
        /// Node kind.
        kind: NodeKind,
        /// Required capability id.
        expected: CapabilityId,
        /// Actual capability.
        got: Option<CapabilityId>,
    },

    /// Adapter/structural node must not carry a capability.
    #[error("node {node} kind {kind:?} MUST NOT carry a capability")]
    CapabilityForbidden {
        /// Node id.
        node: NodeId,
        /// Node kind.
        kind: NodeKind,
    },

    /// GateHuman missing approval.
    #[error("node {node} kind {kind:?} MUST carry approval")]
    ApprovalRequired {
        /// Node id.
        node: NodeId,
        /// Node kind.
        kind: NodeKind,
    },

    /// Non-gate node must not carry approval.
    #[error("node {node} kind {kind:?} MUST NOT carry approval")]
    ApprovalForbidden {
        /// Node id.
        node: NodeId,
        /// Node kind.
        kind: NodeKind,
    },

    /// Duplicate gate id across approval nodes.
    #[error("duplicate GateId {gate} on nodes {a} and {b}")]
    DuplicateGateId {
        /// Colliding gate.
        gate: GateId,
        /// First node (ascending id).
        a: NodeId,
        /// Second node.
        b: NodeId,
    },

    /// Adapter/structural node must not carry cache_key.
    #[error("node {node} kind {kind:?} MUST NOT carry cache_key")]
    CacheKeyForbidden {
        /// Node id.
        node: NodeId,
        /// Node kind.
        kind: NodeKind,
    },

    /// Adapter/structural budget must be zero.
    #[error("adapter/structural node {node} budget must be zero")]
    BudgetNotZero {
        /// Node id.
        node: NodeId,
    },

    /// LLM budget must be non-zero on at least one side.
    #[error("LLM node {node} budget must be non-zero on at least one side")]
    BudgetZero {
        /// Node id.
        node: NodeId,
    },

    /// Retry policy incoherent.
    #[error("retry policy on node {node}: {reason:?}")]
    RetryIncoherent {
        /// Node id.
        node: NodeId,
        /// Incoherence reason.
        reason: RetryIncoherence,
    },

    /// Required GateHuman missing.
    #[error("template/gates: missing required GateHuman node")]
    GatesAbsent,

    /// GateHuman approval reason empty after trim.
    #[error("GateHuman node {node} has empty approval.reason")]
    GateReasonEmpty {
        /// Node id.
        node: NodeId,
    },

    /// Aggregate without Data predecessors.
    #[error("Aggregate node {node} has no Data predecessors")]
    AggregateNoDataPreds {
        /// Node id.
        node: NodeId,
    },

    /// Duplicate `(from, to, kind)` edge.
    #[error("duplicate edge {kind:?} {from} -> {to}")]
    DuplicateEdge {
        /// Predecessor.
        from: NodeId,
        /// Successor.
        to: NodeId,
        /// Edge kind.
        kind: EdgeKind,
    },

    /// MVP linearity violated.
    #[error("MVP template linearity violated involving nodes {a} and {b}")]
    NonLinearTopology {
        /// First violating node (ascending id).
        a: NodeId,
        /// Offending distinct pred/succ.
        b: NodeId,
    },

    /// Generation out of range for SQLite INTEGER.
    #[error("generation must be >= 1 and <= i64::MAX as u64, got {got}")]
    InvalidGeneration {
        /// Observed generation.
        got: u64,
    },

    /// `timeout_ms` must be > 0.
    #[error("timeout_ms must be > 0 for node {node}")]
    TimeoutZero {
        /// Node id.
        node: NodeId,
    },
}

/// Why a [`RetryPolicy`] failed coherence checks (V14 / escalate-on-non-LLM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryIncoherence {
    /// `max_attempts` was zero.
    MaxAttemptsZero,
    /// `escalate_after` not strictly less than `max_attempts`.
    EscalateAfterOrder,
    /// `escalate_to_tier` set without `escalate_after`.
    EscalateTierWithoutAfter,
    /// `escalate_after` set without `escalate_to_tier`.
    EscalateAfterWithoutTier,
    /// Exponential factor not finite or `< 1.0`.
    ExponentialFactorInvalid,
    /// Escalate fields set on adapter/structural node.
    EscalateOnNonLlm,
}

/// Options controlling optional validation rules.
#[derive(Debug, Clone, Copy)]
pub struct ValidateOpts {
    /// When true, enforce unique-pred/succ linearity (§5.4 V15).
    pub enforce_linear_mvp: bool,
    /// When true, require ≥1 `GateHuman` (§5.4 / V2 §10.2).
    pub require_gates: bool,
}

impl Default for ValidateOpts {
    fn default() -> Self {
        Self {
            enforce_linear_mvp: true,
            require_gates: true,
        }
    }
}

/// Pure validator. No I/O. Stateless. Not injected into services.
#[derive(Debug, Default, Clone, Copy)]
pub struct DagValidator;

impl DagValidator {
    /// Validate structural + contract rules (§5.4) in order V1…Vn.
    ///
    /// Returns the **first** violation.
    pub fn validate(dag: &TaskDag, opts: ValidateOpts) -> Result<(), DagValidationError> {
        let _span = tracing::info_span!(
            "dag.validate",
            dag_id = %dag.id,
            node_count = dag.nodes.len(),
            edge_count = dag.edges.len(),
        )
        .entered();

        // V1
        if dag.nodes.is_empty() {
            return Err(DagValidationError::Empty);
        }

        // V2
        for (key, node) in &dag.nodes {
            if *key != node.id {
                return Err(DagValidationError::NodeIdMismatch {
                    key: *key,
                    node_id: node.id,
                });
            }
        }

        // V3
        if dag.generation < 1 || dag.generation > i64::MAX as u64 {
            return Err(DagValidationError::InvalidGeneration {
                got: dag.generation,
            });
        }

        // V4 — every edge endpoint exists (full pass before V5)
        for edge in &dag.edges {
            if !dag.nodes.contains_key(&edge.from) {
                return Err(DagValidationError::MissingEndpoint { node: edge.from });
            }
            if !dag.nodes.contains_key(&edge.to) {
                return Err(DagValidationError::MissingEndpoint { node: edge.to });
            }
        }

        // V5 — no self-loops
        for edge in &dag.edges {
            if edge.from == edge.to {
                return Err(DagValidationError::SelfLoop { node: edge.from });
            }
        }

        let sched_edges: Vec<&DependencyEdge> = dag
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::Data | EdgeKind::Sequence))
            .collect();

        // Adjacency once: distinct sets for V7/V15; ordered vec for V6 DFS.
        let (preds, succs_set) = build_adj(&sched_edges);
        let succs_ordered = build_succs_ordered(&sched_edges);

        // V6 — cycle (iterative DFS from lowest NodeId; first back-edge `to`)
        if let Some(node) = detect_cycle(&dag.nodes, &succs_ordered) {
            return Err(DagValidationError::Cycle { node });
        }

        // V7 — unique root + reachability
        let roots: Vec<NodeId> = dag
            .nodes
            .keys()
            .copied()
            .filter(|n| preds.get(n).map(|s| s.is_empty()).unwrap_or(true))
            .collect();
        if roots.len() != 1 {
            return Err(DagValidationError::MultipleRoots { count: roots.len() });
        }
        let root = roots[0];
        let reachable = bfs_reachable(root, &succs_set);
        for id in dag.nodes.keys() {
            if !reachable.contains(id) {
                return Err(DagValidationError::Unreachable { node: *id });
            }
        }

        // V8 — unique (from, to, kind) among all edges including Hint
        {
            let mut seen = HashSet::with_capacity(dag.edges.len());
            for edge in &dag.edges {
                let key = (edge.from, edge.to, edge.kind);
                if !seen.insert(key) {
                    return Err(DagValidationError::DuplicateEdge {
                        from: edge.from,
                        to: edge.to,
                        kind: edge.kind,
                    });
                }
            }
        }

        // V9 — per-node contract (ascending NodeId)
        for (id, node) in &dag.nodes {
            check_node_contract(*id, node)?;
        }

        // V10 — GateHuman reason non-empty after trim
        for (id, node) in &dag.nodes {
            if node.kind == NodeKind::GateHuman {
                if let Some(approval) = &node.approval {
                    if approval.reason.trim().is_empty() {
                        return Err(DagValidationError::GateReasonEmpty { node: *id });
                    }
                }
            }
        }

        // V11 — require gates
        if opts.require_gates {
            let has_gate = dag.nodes.values().any(|n| n.kind == NodeKind::GateHuman);
            if !has_gate {
                return Err(DagValidationError::GatesAbsent);
            }
        }

        // V12 — Aggregate Data preds
        for (id, node) in &dag.nodes {
            if node.kind == NodeKind::Aggregate {
                let has_data = dag
                    .edges
                    .iter()
                    .any(|e| e.to == *id && e.kind == EdgeKind::Data);
                if !has_data {
                    return Err(DagValidationError::AggregateNoDataPreds { node: *id });
                }
            }
        }

        // V13 — timeout_ms > 0
        for (id, node) in &dag.nodes {
            if node.timeout_ms == 0 {
                return Err(DagValidationError::TimeoutZero { node: *id });
            }
        }

        // V14 — retry coherence (excluding EscalateOnNonLlm already checked in V9)
        for (id, node) in &dag.nodes {
            if let Some(reason) = retry_coherence(&node.retry) {
                return Err(DagValidationError::RetryIncoherent { node: *id, reason });
            }
        }

        // V15 — linear MVP
        if opts.enforce_linear_mvp {
            if let Some((a, b)) = check_linear(&dag.nodes, &preds, &succs_set) {
                return Err(DagValidationError::NonLinearTopology { a, b });
            }
        }

        // V16 — unique GateId among approval nodes
        {
            let mut by_gate: BTreeMap<GateId, NodeId> = BTreeMap::new();
            for (id, node) in &dag.nodes {
                if let Some(approval) = &node.approval {
                    if let Some(prev) = by_gate.insert(approval.gate, *id) {
                        let (a, b) = if prev < *id { (prev, *id) } else { (*id, prev) };
                        return Err(DagValidationError::DuplicateGateId {
                            gate: approval.gate,
                            a,
                            b,
                        });
                    }
                }
            }
        }

        // V17 — Hint exclusion from V6/V7/V15 already applied; no dedicated variant.

        Ok(())
    }
}

fn expected_capability(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Plan => Some("planning"),
        NodeKind::Analyze => Some("repair"),
        NodeKind::Edit => Some("edit"),
        NodeKind::Review => Some("review"),
        _ => None,
    }
}

fn check_node_contract(id: NodeId, node: &TaskNode) -> Result<(), DagValidationError> {
    let kind = node.kind;

    // capability
    if let Some(expected_str) = expected_capability(kind) {
        let matches = node
            .capability
            .as_ref()
            .is_some_and(|c| c.as_str() == expected_str);
        if !matches {
            let expected = CapabilityId::new(expected_str).expect("static capability id");
            return Err(DagValidationError::CapabilityRequired {
                node: id,
                kind,
                expected,
                got: node.capability.clone(),
            });
        }
    } else if node.capability.is_some() {
        return Err(DagValidationError::CapabilityForbidden { node: id, kind });
    }

    // approval
    match kind {
        NodeKind::GateHuman => {
            if node.approval.is_none() {
                return Err(DagValidationError::ApprovalRequired { node: id, kind });
            }
        }
        _ => {
            if node.approval.is_some() {
                return Err(DagValidationError::ApprovalForbidden { node: id, kind });
            }
        }
    }

    // cache_key
    if expected_capability(kind).is_none() && node.cache_key.is_some() {
        return Err(DagValidationError::CacheKeyForbidden { node: id, kind });
    }

    // budget
    if expected_capability(kind).is_some() {
        if node.budget.max_input == 0 && node.budget.max_output == 0 {
            return Err(DagValidationError::BudgetZero { node: id });
        }
    } else if node.budget.max_input != 0 || node.budget.max_output != 0 {
        return Err(DagValidationError::BudgetNotZero { node: id });
    }

    // escalate-on-non-LLM (part of V9 before V14)
    if expected_capability(kind).is_none()
        && (node.retry.escalate_after.is_some() || node.retry.escalate_to_tier.is_some())
    {
        return Err(DagValidationError::RetryIncoherent {
            node: id,
            reason: RetryIncoherence::EscalateOnNonLlm,
        });
    }

    Ok(())
}

fn retry_coherence(retry: &RetryPolicy) -> Option<RetryIncoherence> {
    if retry.max_attempts == 0 {
        return Some(RetryIncoherence::MaxAttemptsZero);
    }
    if let Some(n) = retry.escalate_after {
        if n >= retry.max_attempts {
            return Some(RetryIncoherence::EscalateAfterOrder);
        }
    }
    match (retry.escalate_after, retry.escalate_to_tier) {
        (None, Some(_)) => return Some(RetryIncoherence::EscalateTierWithoutAfter),
        (Some(_), None) => return Some(RetryIncoherence::EscalateAfterWithoutTier),
        _ => {}
    }
    if let Backoff::Exponential { factor, .. } = retry.backoff {
        if !factor.is_finite() || factor < 1.0 {
            return Some(RetryIncoherence::ExponentialFactorInvalid);
        }
    }
    None
}

fn build_adj(
    edges: &[&DependencyEdge],
) -> (
    HashMap<NodeId, BTreeSet<NodeId>>,
    HashMap<NodeId, BTreeSet<NodeId>>,
) {
    let mut preds: HashMap<NodeId, BTreeSet<NodeId>> = HashMap::new();
    let mut succs: HashMap<NodeId, BTreeSet<NodeId>> = HashMap::new();
    for e in edges {
        preds.entry(e.to).or_default().insert(e.from);
        succs.entry(e.from).or_default().insert(e.to);
        preds.entry(e.from).or_default();
        succs.entry(e.to).or_default();
    }
    (preds, succs)
}

/// Successor lists in Data∪Sequence **edge-vector order** (first occurrence wins).
fn build_succs_ordered(edges: &[&DependencyEdge]) -> HashMap<NodeId, Vec<NodeId>> {
    let mut succs: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut seen_pair: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in edges {
        if seen_pair.insert((e.from, e.to)) {
            succs.entry(e.from).or_default().push(e.to);
        }
    }
    succs
}

fn bfs_reachable(root: NodeId, succs: &HashMap<NodeId, BTreeSet<NodeId>>) -> HashSet<NodeId> {
    let mut seen = HashSet::new();
    let mut q = VecDeque::new();
    q.push_back(root);
    seen.insert(root);
    while let Some(n) = q.pop_front() {
        if let Some(next) = succs.get(&n) {
            for s in next {
                if seen.insert(*s) {
                    q.push_back(*s);
                }
            }
        }
    }
    seen
}

/// Iterative DFS cycle detection: start from each unvisited node in ascending id order.
/// Successors are visited in edge-vector order. Returns `to` of the first back-edge.
fn detect_cycle(
    nodes: &BTreeMap<NodeId, TaskNode>,
    succs: &HashMap<NodeId, Vec<NodeId>>,
) -> Option<NodeId> {
    let mut color: HashMap<NodeId, u8> = HashMap::new(); // 0 white, 1 gray, 2 black
    for id in nodes.keys() {
        color.insert(*id, 0);
    }

    // Frame: (node, next successor index)
    let mut stack: Vec<(NodeId, usize)> = Vec::new();

    for &start in nodes.keys() {
        if color.get(&start).copied() != Some(0) {
            continue;
        }
        color.insert(start, 1);
        stack.push((start, 0));
        while let Some((u, i)) = stack.pop() {
            let outs = succs.get(&u).map(Vec::as_slice).unwrap_or(&[]);
            if i < outs.len() {
                stack.push((u, i + 1));
                let v = outs[i];
                match color.get(&v).copied().unwrap_or(0) {
                    1 => return Some(v),
                    0 => {
                        color.insert(v, 1);
                        stack.push((v, 0));
                    }
                    _ => {}
                }
            } else {
                color.insert(u, 2);
            }
        }
    }
    None
}

fn check_linear(
    nodes: &BTreeMap<NodeId, TaskNode>,
    preds: &HashMap<NodeId, BTreeSet<NodeId>>,
    succs: &HashMap<NodeId, BTreeSet<NodeId>>,
) -> Option<(NodeId, NodeId)> {
    for id in nodes.keys() {
        let pred_set = preds.get(id);
        let succ_set = succs.get(id);
        let p = pred_set.map(|s| s.len()).unwrap_or(0);
        let s = succ_set.map(|s| s.len()).unwrap_or(0);
        if p > 1 {
            let b = *pred_set
                .expect("p>1 implies preds entry")
                .iter()
                .next()
                .expect("non-empty");
            return Some((*id, b));
        }
        if s > 1 {
            let b = *succ_set
                .expect("s>1 implies succs entry")
                .iter()
                .next()
                .expect("non-empty");
            return Some((*id, b));
        }
    }
    None
}

/// Declarative predecessor satisfaction (RFC-0009 §5.3.1).
///
/// Edges with `kind ∈ {Data, Sequence}` participate; `Hint` is ignored.
/// A node MAY become Ready iff every such predecessor is satisfied.
///
/// Runtime Ready transitions remain RFC-0010; this helper ships for unit tests
/// and for the scheduler to reuse without inventing a second rule set.
///
/// Used in production by `scheduler::linear::ready::promotable_nodes`
/// (RFC-0010 §3.13 / §5.4), not only by tests.
#[must_use]
pub(crate) fn preds_satisfied(dag: &TaskDag, node: NodeId) -> bool {
    for edge in &dag.edges {
        if edge.to != node {
            continue;
        }
        match edge.kind {
            EdgeKind::Hint => continue,
            EdgeKind::Sequence => {
                let Some(pred) = dag.nodes.get(&edge.from) else {
                    return false;
                };
                if !matches!(
                    pred.state,
                    NodeState::Succeeded | NodeState::Skipped | NodeState::CachedHit
                ) {
                    return false;
                }
            }
            EdgeKind::Data => {
                let Some(pred) = dag.nodes.get(&edge.from) else {
                    return false;
                };
                if !matches!(pred.state, NodeState::Succeeded | NodeState::CachedHit)
                    || pred.output_ref.is_none()
                {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::DagState;
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::diagnostic::ErrorClass;
    use crate::types::ids::{ArtifactId, CapabilityId, DagId, Digest, GateId, SessionId};

    use super::super::types::{ApprovalSpec, CacheKey, RetryPolicy};

    fn llm_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            backoff: Backoff::Fixed { delay_ms: 1000 },
            retry_on: vec![ErrorClass::Model],
            escalate_after: None,
            escalate_to_tier: None,
        }
    }

    fn adapter_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![],
            escalate_after: None,
            escalate_to_tier: None,
        }
    }

    fn llm_node(id: NodeId, kind: NodeKind, cap: &str) -> TaskNode {
        TaskNode {
            id,
            kind,
            capability: Some(CapabilityId::new(cap).unwrap()),
            input_ref: ArtifactId::new(),
            output_ref: None,
            state: NodeState::Pending,
            retry: llm_retry(),
            cache_key: None,
            budget: TokenBudget {
                max_input: 100,
                max_output: 100,
            },
            model_tier: ModelTier::Standard,
            approval: None,
            timeout_ms: 1000,
        }
    }

    fn adapter_node(id: NodeId, kind: NodeKind) -> TaskNode {
        TaskNode {
            id,
            kind,
            capability: None,
            input_ref: ArtifactId::new(),
            output_ref: None,
            state: NodeState::Pending,
            retry: adapter_retry(),
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
        let mut n = adapter_node(id, NodeKind::GateHuman);
        n.approval = Some(ApprovalSpec {
            gate,
            reason: "approve".into(),
        });
        n
    }

    fn dag_from(nodes: BTreeMap<NodeId, TaskNode>, edges: Vec<DependencyEdge>) -> TaskDag {
        TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes,
            edges,
            state: DagState::Pending,
        }
    }

    /// Minimal valid linear chain: Analyze → Edit → GateHuman with dual edges.
    fn valid_chain() -> (TaskDag, NodeId, NodeId, NodeId) {
        let a = NodeId::new();
        let e = NodeId::new();
        let g = NodeId::new();
        // Ensure deterministic order for some tests by using the actual ids.
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(e, llm_node(e, NodeKind::Edit, "edit"));
        nodes.insert(g, gate_node(g, GateId::new()));
        let edges = vec![
            DependencyEdge {
                from: a,
                to: e,
                kind: EdgeKind::Data,
            },
            DependencyEdge {
                from: a,
                to: e,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: e,
                to: g,
                kind: EdgeKind::Data,
            },
            DependencyEdge {
                from: e,
                to: g,
                kind: EdgeKind::Sequence,
            },
        ];
        (dag_from(nodes, edges), a, e, g)
    }

    #[test]
    fn valid_dual_edge_chain_passes() {
        let (dag, _, _, _) = valid_chain();
        assert!(DagValidator::validate(&dag, ValidateOpts::default()).is_ok());
    }

    #[test]
    fn empty_dag() {
        let dag = dag_from(BTreeMap::new(), vec![]);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::Empty)
        );
    }

    #[test]
    fn node_id_mismatch() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        let mut n = llm_node(b, NodeKind::Analyze, "repair");
        // Force mismatch: key a, id b
        n.id = b;
        nodes.insert(a, n);
        let g = gate_node(NodeId::new(), GateId::new());
        let gid = g.id;
        nodes.insert(gid, g);
        // Need edge so V7 MultipleRoots does not fire before V2 NodeIdMismatch.
        let edges = vec![DependencyEdge {
            from: a,
            to: gid,
            kind: EdgeKind::Sequence,
        }];
        let dag = dag_from(nodes, edges);
        assert!(matches!(
            DagValidator::validate(
                &dag,
                ValidateOpts {
                    require_gates: false,
                    ..Default::default()
                }
            ),
            Err(DagValidationError::NodeIdMismatch { .. })
        ));
    }

    #[test]
    fn invalid_generation_zero() {
        let (mut dag, _, _, _) = valid_chain();
        dag.generation = 0;
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::InvalidGeneration { got: 0 })
        );
    }

    #[test]
    fn invalid_generation_above_i64_max() {
        let (mut dag, _, _, _) = valid_chain();
        dag.generation = (i64::MAX as u64) + 1;
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::InvalidGeneration {
                got: (i64::MAX as u64) + 1
            })
        );
    }

    #[test]
    fn v4_missing_endpoint_before_v5_self_loop() {
        let (mut dag, a, _, _) = valid_chain();
        let missing = NodeId::new();
        // Self-loop first in vector, missing endpoint later — V4 must win.
        dag.edges.insert(
            0,
            DependencyEdge {
                from: a,
                to: a,
                kind: EdgeKind::Hint,
            },
        );
        dag.edges.push(DependencyEdge {
            from: a,
            to: missing,
            kind: EdgeKind::Hint,
        });
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::MissingEndpoint { node: missing })
        );
    }

    #[test]
    fn missing_endpoint_prefers_from() {
        let (mut dag, _, e, _) = valid_chain();
        let missing = NodeId::new();
        dag.edges.push(DependencyEdge {
            from: missing,
            to: e,
            kind: EdgeKind::Hint,
        });
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::MissingEndpoint { node: missing })
        );
    }

    #[test]
    fn missing_endpoint() {
        let (mut dag, a, _, _) = valid_chain();
        let missing = NodeId::new();
        dag.edges.push(DependencyEdge {
            from: a,
            to: missing,
            kind: EdgeKind::Hint,
        });
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::MissingEndpoint { node: missing })
        );
    }

    #[test]
    fn self_loop() {
        let (mut dag, a, _, _) = valid_chain();
        dag.edges.push(DependencyEdge {
            from: a,
            to: a,
            kind: EdgeKind::Hint,
        });
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::SelfLoop { node: a })
        );
    }

    #[test]
    fn cycle() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(b, llm_node(b, NodeKind::Edit, "edit"));
        let edges = vec![
            DependencyEdge {
                from: a,
                to: b,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: b,
                to: a,
                kind: EdgeKind::Sequence,
            },
        ];
        let dag = dag_from(nodes, edges);
        assert!(matches!(
            DagValidator::validate(
                &dag,
                ValidateOpts {
                    require_gates: false,
                    enforce_linear_mvp: false,
                }
            ),
            Err(DagValidationError::Cycle { .. })
        ));
    }

    #[test]
    fn multiple_roots_isolated_node() {
        let (mut dag, _, _, _) = valid_chain();
        let extra = NodeId::new();
        dag.nodes
            .insert(extra, llm_node(extra, NodeKind::Review, "review"));
        assert_eq!(
            DagValidator::validate(
                &dag,
                ValidateOpts {
                    enforce_linear_mvp: false,
                    require_gates: true,
                }
            ),
            Err(DagValidationError::MultipleRoots { count: 2 })
        );
    }

    #[test]
    fn duplicate_edge() {
        let (mut dag, a, e, _) = valid_chain();
        dag.edges.push(DependencyEdge {
            from: a,
            to: e,
            kind: EdgeKind::Data,
        });
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::DuplicateEdge {
                from: a,
                to: e,
                kind: EdgeKind::Data,
            })
        );
    }

    #[test]
    fn capability_required() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().capability = None;
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::CapabilityRequired { .. })
        ));
    }

    #[test]
    fn appendix_a_capability_ids_table_driven() {
        let cases = [
            (NodeKind::Plan, "planning"),
            (NodeKind::Analyze, "repair"),
            (NodeKind::Edit, "edit"),
            (NodeKind::Review, "review"),
        ];
        for (kind, expected) in cases {
            let id = NodeId::new();
            let mut nodes = BTreeMap::new();
            let mut n = llm_node(id, kind, "wrong");
            n.capability = Some(CapabilityId::new("wrong").unwrap());
            nodes.insert(id, n);
            let g = gate_node(NodeId::new(), GateId::new());
            let gid = g.id;
            nodes.insert(gid, g);
            let edges = vec![DependencyEdge {
                from: id,
                to: gid,
                kind: EdgeKind::Sequence,
            }];
            let dag = dag_from(nodes, edges);
            match DagValidator::validate(&dag, ValidateOpts::default()) {
                Err(DagValidationError::CapabilityRequired {
                    kind: k,
                    expected: exp,
                    ..
                }) => {
                    assert_eq!(k, kind);
                    assert_eq!(exp.as_str(), expected);
                }
                other => panic!("kind {kind:?}: expected CapabilityRequired, got {other:?}"),
            }
        }
    }

    #[test]
    fn v6_cycle_before_v9_capability() {
        // Multi-fault: cycle (V6) must win over capability (V9).
        let (mut dag, a, e, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().capability = None;
        dag.edges.push(DependencyEdge {
            from: e,
            to: a,
            kind: EdgeKind::Sequence,
        });
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::Cycle { .. })
        ));
    }

    #[test]
    fn capability_forbidden() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes.get_mut(&g).unwrap().capability = Some(CapabilityId::new("repair").unwrap());
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::CapabilityForbidden { .. })
        ));
    }

    #[test]
    fn approval_required() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes.get_mut(&g).unwrap().approval = None;
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::ApprovalRequired { .. })
        ));
    }

    #[test]
    fn approval_forbidden() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().approval = Some(ApprovalSpec {
            gate: GateId::new(),
            reason: "no".into(),
        });
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::ApprovalForbidden { .. })
        ));
    }

    #[test]
    fn cache_key_forbidden() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes.get_mut(&g).unwrap().cache_key = Some(CacheKey(Digest::sha256(b"x")));
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::CacheKeyForbidden { .. })
        ));
    }

    #[test]
    fn budget_not_zero() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes.get_mut(&g).unwrap().budget.max_input = 1;
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::BudgetNotZero { .. })
        ));
    }

    #[test]
    fn budget_zero() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().budget = TokenBudget {
            max_input: 0,
            max_output: 0,
        };
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::BudgetZero { .. })
        ));
    }

    #[test]
    fn gates_absent() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::GatesAbsent)
        );
    }

    #[test]
    fn gate_reason_empty() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes
            .get_mut(&g)
            .unwrap()
            .approval
            .as_mut()
            .unwrap()
            .reason = "   ".into();
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::GateReasonEmpty { .. })
        ));
    }

    #[test]
    fn aggregate_no_data_preds() {
        let a = NodeId::new();
        let agg = NodeId::new();
        let g = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(agg, adapter_node(agg, NodeKind::Aggregate));
        nodes.insert(g, gate_node(g, GateId::new()));
        let edges = vec![
            DependencyEdge {
                from: a,
                to: agg,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: agg,
                to: g,
                kind: EdgeKind::Sequence,
            },
        ];
        let dag = dag_from(nodes, edges);
        assert!(matches!(
            DagValidator::validate(
                &dag,
                ValidateOpts {
                    enforce_linear_mvp: true,
                    require_gates: true,
                }
            ),
            Err(DagValidationError::AggregateNoDataPreds { .. })
        ));
    }

    #[test]
    fn timeout_zero() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().timeout_ms = 0;
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::TimeoutZero { .. })
        ));
    }

    #[test]
    fn retry_max_attempts_zero() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().retry.max_attempts = 0;
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: a,
                reason: RetryIncoherence::MaxAttemptsZero,
            })
        );
    }

    #[test]
    fn retry_escalate_after_order() {
        let (mut dag, a, _, _) = valid_chain();
        let n = dag.nodes.get_mut(&a).unwrap();
        n.retry.max_attempts = 2;
        n.retry.escalate_after = Some(2);
        n.retry.escalate_to_tier = Some(ModelTier::Premium);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: a,
                reason: RetryIncoherence::EscalateAfterOrder,
            })
        );
    }

    #[test]
    fn retry_escalate_tier_without_after() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().retry.escalate_to_tier = Some(ModelTier::Premium);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: a,
                reason: RetryIncoherence::EscalateTierWithoutAfter,
            })
        );
    }

    #[test]
    fn retry_escalate_after_without_tier() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().retry.escalate_after = Some(0);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: a,
                reason: RetryIncoherence::EscalateAfterWithoutTier,
            })
        );
    }

    #[test]
    fn retry_exponential_factor_invalid() {
        let (mut dag, a, _, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().retry.backoff = Backoff::Exponential {
            base_ms: 10,
            factor: 0.5,
        };
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: a,
                reason: RetryIncoherence::ExponentialFactorInvalid,
            })
        );
    }

    #[test]
    fn retry_escalate_on_non_llm() {
        let (mut dag, _, _, g) = valid_chain();
        dag.nodes.get_mut(&g).unwrap().retry.escalate_after = Some(0);
        dag.nodes.get_mut(&g).unwrap().retry.escalate_to_tier = Some(ModelTier::Premium);
        assert_eq!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::RetryIncoherent {
                node: g,
                reason: RetryIncoherence::EscalateOnNonLlm,
            })
        );
    }

    #[test]
    fn non_linear_diamond() {
        // a → b, a → c, b → d, c → d — diamond
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let g = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(b, llm_node(b, NodeKind::Edit, "edit"));
        nodes.insert(c, llm_node(c, NodeKind::Review, "review"));
        nodes.insert(d, llm_node(d, NodeKind::Plan, "planning"));
        nodes.insert(g, gate_node(g, GateId::new()));
        let edges = vec![
            DependencyEdge {
                from: a,
                to: b,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: a,
                to: c,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: b,
                to: d,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: c,
                to: d,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: d,
                to: g,
                kind: EdgeKind::Sequence,
            },
        ];
        let dag = dag_from(nodes, edges);
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::NonLinearTopology { .. })
        ));
    }

    #[test]
    fn duplicate_gate_id() {
        let a = NodeId::new();
        let g1 = NodeId::new();
        let g2 = NodeId::new();
        let gate = GateId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(g1, gate_node(g1, gate));
        nodes.insert(g2, gate_node(g2, gate));
        let edges = vec![
            DependencyEdge {
                from: a,
                to: g1,
                kind: EdgeKind::Sequence,
            },
            DependencyEdge {
                from: g1,
                to: g2,
                kind: EdgeKind::Sequence,
            },
        ];
        let dag = dag_from(nodes, edges);
        assert!(matches!(
            DagValidator::validate(&dag, ValidateOpts::default()),
            Err(DagValidationError::DuplicateGateId { .. })
        ));
    }

    #[test]
    fn hint_only_extras_on_valid_chain_pass() {
        let (mut dag, a, e, _) = valid_chain();
        dag.edges.push(DependencyEdge {
            from: a,
            to: e,
            kind: EdgeKind::Hint,
        });
        assert!(DagValidator::validate(&dag, ValidateOpts::default()).is_ok());
    }

    #[test]
    fn skipped_does_not_satisfy_data() {
        let (mut dag, a, e, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().state = NodeState::Skipped;
        dag.nodes.get_mut(&a).unwrap().output_ref = Some(ArtifactId::new());
        assert!(!preds_satisfied(&dag, e));
    }

    #[test]
    fn succeeded_with_output_satisfies_data() {
        let (mut dag, a, e, _) = valid_chain();
        dag.nodes.get_mut(&a).unwrap().state = NodeState::Succeeded;
        dag.nodes.get_mut(&a).unwrap().output_ref = Some(ArtifactId::new());
        assert!(preds_satisfied(&dag, e));
    }

    #[test]
    fn skipped_satisfies_sequence_only_edge() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, llm_node(a, NodeKind::Analyze, "repair"));
        nodes.insert(b, llm_node(b, NodeKind::Edit, "edit"));
        nodes.get_mut(&a).unwrap().state = NodeState::Skipped;
        let edges = vec![DependencyEdge {
            from: a,
            to: b,
            kind: EdgeKind::Sequence,
        }];
        let dag = dag_from(nodes, edges);
        assert!(preds_satisfied(&dag, b));
    }
}
