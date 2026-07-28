//! Pure ready-set derivation, backoff, and `DagState` folding (RFC-0010 §3.13).
//!
//! Every function here is synchronous and touches no store handle (§4.1 rule
//! M2), so [`crate::dag::TaskDag`] fixtures built entirely in memory can
//! table-test the scheduler's readiness and state-derivation rules without a
//! `LinearScheduler` instance (RFC-0010 §11.1).

use std::time::Duration;

use crate::dag::{preds_satisfied, Backoff, EdgeKind, NodeKind, NodeState, TaskDag};
use crate::error::SchedError;
use crate::scheduler::DagState;
use crate::types::ids::NodeId;

/// Nodes already in [`NodeState::Ready`], ascending `NodeId` (RFC-0010 §3.13 /
/// §5.4 RS4).
///
/// `TaskDag::nodes` is a `BTreeMap<NodeId, TaskNode>`, so iteration order is
/// already ascending; this function does not need to sort.
#[must_use]
pub fn ready_nodes(dag: &TaskDag) -> Vec<NodeId> {
    dag.nodes
        .iter()
        .filter(|(_, node)| node.state == NodeState::Ready)
        .map(|(id, _)| *id)
        .collect()
}

/// `Pending` nodes whose Data ∪ Sequence predecessors are satisfied under
/// RFC-0009 §5.3.1, ascending `NodeId` (RFC-0010 §3.13 / §5.4 RS1).
///
/// Reuses `dag::preds_satisfied` (RFC-0009 §5.3.1) rather than
/// reimplementing the satisfaction rule (a Skipped predecessor never
/// satisfies a Data edge, a Data predecessor without `output_ref` is
/// unsatisfied, `Hint` is ignored).
/// This function stays pure and infallible: the RS3 "succeeded-without-
/// output_ref is an invariant violation" check is the scheduler loop's job
/// immediately before promotion (§5.4 RS3), not this helper's.
#[must_use]
pub fn promotable_nodes(dag: &TaskDag) -> Vec<NodeId> {
    dag.nodes
        .iter()
        .filter(|(_, node)| node.state == NodeState::Pending)
        .filter(|(id, _)| preds_satisfied(dag, **id))
        .map(|(id, _)| *id)
        .collect()
}

/// Node kinds ER4 blocks from dispatch while `needs_reverify` holds.
///
/// The complement — `VerifyCompile`, `VerifyTest`, `GateHuman`, `Aggregate` —
/// stays dispatchable, which is what lets the pending verify actually run and
/// clear the flag.
#[must_use]
pub fn er4_blocked_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Plan | NodeKind::Analyze | NodeKind::Edit | NodeKind::Review
    )
}

/// Every node reachable from `from` along Data ∪ Sequence edges, following
/// RFC-0009 §5.3.1's edge semantics (`Hint` is not a dependency and is
/// ignored). `from` itself is not included.
#[must_use]
fn data_or_sequence_reachable(dag: &TaskDag, from: NodeId) -> std::collections::BTreeSet<NodeId> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![from];
    while let Some(current) = stack.pop() {
        for edge in &dag.edges {
            if edge.from != current || edge.kind == EdgeKind::Hint {
                continue;
            }
            if seen.insert(edge.to) {
                stack.push(edge.to);
            }
        }
    }
    seen
}

/// Verify nodes reachable from any `Succeeded` `Edit` in this generation
/// (ER4/ER5's "Data∪Sequence-reachable `VerifyCompile`/`VerifyTest`").
///
/// Empty when no `Edit` succeeded, or when the succeeded edits reach no
/// verify at all — ER5's "verify-less DAG" carve-out (e.g. `Edit → GateHuman`,
/// where the human is the check) depends on telling those two apart from
/// "reachable verifies exist but none succeeded".
#[must_use]
pub fn verifies_reachable_from_succeeded_edits(dag: &TaskDag) -> Vec<NodeId> {
    let mut out = std::collections::BTreeSet::new();
    for (id, node) in &dag.nodes {
        if node.kind != NodeKind::Edit || node.state != NodeState::Succeeded {
            continue;
        }
        for reached in data_or_sequence_reachable(dag, *id) {
            if matches!(
                dag.nodes.get(&reached).map(|n| n.kind),
                Some(NodeKind::VerifyCompile | NodeKind::VerifyTest)
            ) {
                out.insert(reached);
            }
        }
    }
    out.into_iter().collect()
}

/// ER4's `needs_reverify`, derived from the blob rather than stored.
///
/// True iff some `Edit` in this generation is `Succeeded` **and** some
/// Data∪Sequence-reachable verify is still non-terminal
/// (`Pending`/`Ready`/`Running`). A verify that reached
/// `Succeeded`/`CachedHit` clears it.
///
/// Deliberately a pure predicate over `TaskDag`: nothing is persisted, so
/// there is no flag to keep in sync across a crash, and R15 recomputes the
/// same answer on every resume. (An earlier reading of ER4 assumed it
/// required a `TaskNode.needs_reverify` field and deferred the whole rule on
/// that basis — it does not.)
#[must_use]
pub fn needs_reverify(dag: &TaskDag) -> bool {
    verifies_reachable_from_succeeded_edits(dag)
        .into_iter()
        .any(|id| {
            matches!(
                dag.nodes[&id].state,
                NodeState::Pending | NodeState::Ready | NodeState::Running
            )
        })
}

/// Backoff sleep before the attempt following failed attempt `attempt`
/// (1-based), capped by `max_backoff` (RFC-0010 §5.11.3).
///
/// `raw(Fixed { delay_ms }) = delay_ms`; `raw(Exponential { base_ms, factor },
/// k) = base_ms * factor^(k - 1)`; `delay = min(max(raw, 0), max_backoff)`.
/// `factor` is treated as `1.0` when non-finite or `< 1.0` (B2), and the
/// product saturates at `max_backoff` rather than overflowing.
#[must_use]
pub fn backoff_delay(backoff: &Backoff, attempt: u32, max_backoff: Duration) -> Duration {
    let raw = match backoff {
        Backoff::Fixed { delay_ms } => Duration::from_millis(*delay_ms),
        Backoff::Exponential { base_ms, factor } => {
            let factor = if factor.is_finite() && *factor >= 1.0 {
                *factor
            } else {
                1.0
            };
            let exponent = attempt.saturating_sub(1);
            let multiplier = factor.powi(i32::try_from(exponent).unwrap_or(i32::MAX));
            let scaled = (*base_ms as f64) * multiplier;
            let cap_ms = max_backoff.as_millis() as f64;
            if !scaled.is_finite() || scaled >= cap_ms {
                return max_backoff;
            }
            Duration::from_millis(scaled as u64)
        }
    };
    raw.min(max_backoff)
}

/// Run-local flags the loop tracks, folded into [`derive_dag_state`]
/// (RFC-0010 §3.13).
#[derive(Debug, Clone, Copy, Default)]
pub struct DeriveFlags {
    /// A cancel was requested for this DAG (user, runtime drain, or control
    /// plane).
    pub cancel_requested: bool,
    /// `RunControlState::ReplanRequested` was observed for the owning run.
    pub replan_requested: bool,
    /// A gate resolution (deny/expired) recorded an `ErrorClass::Approval`
    /// failure.
    pub approval_failure: bool,
}

/// First-match-wins `DagState` derivation (RFC-0010 §5.17, D1-D9).
///
/// Pure over node states plus `flags`; never reads the store (DS5). Returns
/// `Err` only for D9 (empty node map — rejected by validation, defended
/// here); every other arm returns `Ok`.
pub fn derive_dag_state(dag: &TaskDag, flags: DeriveFlags) -> Result<DagState, SchedError> {
    let any = |pred: fn(NodeState) -> bool| dag.nodes.values().any(|n| pred(n.state));
    let all = |pred: fn(NodeState) -> bool| dag.nodes.values().all(|n| pred(n.state));
    let has_cancelled = any(|s| s == NodeState::Cancelled);

    // D1
    if flags.replan_requested {
        return Ok(DagState::ReplanRequired);
    }
    // D2
    if flags.cancel_requested && has_cancelled {
        return Ok(DagState::Cancelled);
    }
    // D3
    if any(|s| s == NodeState::Failed) {
        return Ok(DagState::Failed);
    }
    // D4
    if has_cancelled && !flags.approval_failure {
        return Ok(DagState::Cancelled);
    }
    // D5
    if has_cancelled && flags.approval_failure {
        return Ok(DagState::Failed);
    }
    // D6 (loop-internal only; `Scheduler::run` must never return this arm's
    // states directly — that's enforced by the loop, not this helper).
    let in_flight = |s: NodeState| {
        matches!(
            s,
            NodeState::Pending | NodeState::Ready | NodeState::Running | NodeState::WaitingApproval
        )
    };
    if any(in_flight) {
        return Ok(if any(|s| s == NodeState::WaitingApproval) {
            DagState::WaitingApproval
        } else {
            DagState::Running
        });
    }
    // D7
    if !dag.nodes.is_empty() && all(|s| matches!(s, NodeState::Succeeded | NodeState::CachedHit)) {
        return Ok(DagState::Succeeded);
    }
    // D8
    if any(|s| s == NodeState::Skipped)
        && all(|s| {
            matches!(
                s,
                NodeState::Succeeded | NodeState::CachedHit | NodeState::Skipped
            )
        })
    {
        return Ok(DagState::Failed);
    }
    // D9
    Err(SchedError::Invariant("empty dag".into()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::dag::{DependencyEdge, EdgeKind, NodeKind, RetryPolicy, TaskNode};
    use crate::scheduler::DagState;
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::diagnostic::ErrorClass;
    use crate::types::ids::{ArtifactId, DagId, SessionId};

    fn retry(retry_on: Vec<ErrorClass>) -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on,
            escalate_after: None,
            escalate_to_tier: None,
        }
    }

    fn node(id: NodeId, state: NodeState) -> TaskNode {
        TaskNode {
            id,
            kind: NodeKind::Analyze,
            capability: None,
            input_ref: ArtifactId::new(),
            output_ref: None,
            state,
            retry: retry(vec![]),
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

    fn node_with_output(id: NodeId, state: NodeState) -> TaskNode {
        let mut n = node(id, state);
        n.output_ref = Some(ArtifactId::new());
        n
    }

    fn dag_from(nodes: BTreeMap<NodeId, TaskNode>, edges: Vec<DependencyEdge>) -> TaskDag {
        TaskDag {
            id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            nodes,
            edges,
            state: DagState::Running,
        }
    }

    // ---- ready_nodes ----

    #[test]
    fn ready_nodes_returns_only_ready_state_ascending() {
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Ready));
        nodes.insert(b, node(b, NodeState::Pending));
        nodes.insert(c, node(c, NodeState::Ready));
        let dag = dag_from(nodes, vec![]);

        let ready = ready_nodes(&dag);
        let mut expected = vec![a, c];
        expected.sort();
        assert_eq!(ready, expected);
    }

    #[test]
    fn ready_nodes_empty_when_none_ready() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Succeeded));
        let dag = dag_from(nodes, vec![]);
        assert!(ready_nodes(&dag).is_empty());
    }

    // ---- promotable_nodes ----

    #[test]
    fn promotable_nodes_root_with_no_edges_is_promotable() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Pending));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(promotable_nodes(&dag), vec![a]);
    }

    #[test]
    fn promotable_nodes_data_edge_satisfied_by_succeeded_with_output() {
        let p = NodeId::new();
        let n = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(p, node_with_output(p, NodeState::Succeeded));
        nodes.insert(n, node(n, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Data,
            }],
        );
        assert_eq!(promotable_nodes(&dag), vec![n]);
    }

    #[test]
    fn promotable_nodes_data_edge_succeeded_without_output_ref_not_promotable() {
        let p = NodeId::new();
        let n = NodeId::new();
        let mut nodes = BTreeMap::new();
        // Succeeded but no output_ref: not satisfied. promotable_nodes stays
        // infallible (RS3 error is the scheduler loop's job, not this helper's).
        nodes.insert(p, node(p, NodeState::Succeeded));
        nodes.insert(n, node(n, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Data,
            }],
        );
        assert!(promotable_nodes(&dag).is_empty());
    }

    #[test]
    fn promotable_nodes_data_edge_skipped_predecessor_not_promotable() {
        let p = NodeId::new();
        let n = NodeId::new();
        let mut nodes = BTreeMap::new();
        // RS2: a Skipped predecessor MUST NOT satisfy a Data edge.
        nodes.insert(p, node(p, NodeState::Skipped));
        nodes.insert(n, node(n, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Data,
            }],
        );
        assert!(promotable_nodes(&dag).is_empty());
    }

    #[test]
    fn promotable_nodes_sequence_edge_satisfied_by_skipped_predecessor() {
        let p = NodeId::new();
        let n = NodeId::new();
        let mut nodes = BTreeMap::new();
        // Sequence satisfaction allows Skipped (unlike Data).
        nodes.insert(p, node(p, NodeState::Skipped));
        nodes.insert(n, node(n, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Sequence,
            }],
        );
        assert_eq!(promotable_nodes(&dag), vec![n]);
    }

    #[test]
    fn promotable_nodes_hint_edge_ignored() {
        let p = NodeId::new();
        let n = NodeId::new();
        let mut nodes = BTreeMap::new();
        // Hint predecessor still Pending: must be ignored entirely.
        nodes.insert(p, node(p, NodeState::Pending));
        nodes.insert(n, node(n, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![DependencyEdge {
                from: p,
                to: n,
                kind: EdgeKind::Hint,
            }],
        );
        let promotable = promotable_nodes(&dag);
        assert!(promotable.contains(&n));
    }

    #[test]
    fn promotable_nodes_excludes_non_pending_nodes() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Running));
        let dag = dag_from(nodes, vec![]);
        assert!(promotable_nodes(&dag).is_empty());
    }

    // ---- ER4 needs_reverify (§6.5) ----

    fn kinded(id: NodeId, kind: NodeKind, state: NodeState) -> TaskNode {
        let mut n = node(id, state);
        n.kind = kind;
        n
    }

    fn edge(from: NodeId, to: NodeId, kind: EdgeKind) -> DependencyEdge {
        DependencyEdge { from, to, kind }
    }

    /// `Edit(Succeeded) → VerifyCompile(state)`, plus the edge kind to use.
    fn edit_then_verify(verify_state: NodeState, edge_kind: EdgeKind) -> (TaskDag, NodeId, NodeId) {
        let e = NodeId::new();
        let v = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(e, kinded(e, NodeKind::Edit, NodeState::Succeeded));
        nodes.insert(v, kinded(v, NodeKind::VerifyCompile, verify_state));
        (dag_from(nodes, vec![edge(e, v, edge_kind)]), e, v)
    }

    #[test]
    fn needs_reverify_true_when_a_succeeded_edit_has_a_nonterminal_verify() {
        for state in [NodeState::Pending, NodeState::Ready, NodeState::Running] {
            let (dag, _, _) = edit_then_verify(state, EdgeKind::Data);
            assert!(needs_reverify(&dag), "{state:?} must still need re-verify");
        }
    }

    #[test]
    fn needs_reverify_false_once_the_verify_succeeds_or_is_cached() {
        for state in [NodeState::Succeeded, NodeState::CachedHit] {
            let (dag, _, _) = edit_then_verify(state, EdgeKind::Data);
            assert!(!needs_reverify(&dag), "{state:?} must clear the flag");
        }
    }

    #[test]
    fn needs_reverify_follows_sequence_edges_too() {
        // ER4 says "Data ∪ Sequence", not Data alone.
        let (dag, _, _) = edit_then_verify(NodeState::Pending, EdgeKind::Sequence);
        assert!(needs_reverify(&dag));
    }

    #[test]
    fn needs_reverify_ignores_hint_edges() {
        // `Hint` is not a dependency (RFC-0009 §5.3.1), so a verify only
        // reachable through one is not "reachable" for ER4's purposes.
        let (dag, _, _) = edit_then_verify(NodeState::Pending, EdgeKind::Hint);
        assert!(!needs_reverify(&dag));
    }

    #[test]
    fn needs_reverify_false_when_no_edit_succeeded() {
        let e = NodeId::new();
        let v = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(e, kinded(e, NodeKind::Edit, NodeState::Ready));
        nodes.insert(v, kinded(v, NodeKind::VerifyCompile, NodeState::Pending));
        let dag = dag_from(nodes, vec![edge(e, v, EdgeKind::Data)]);
        assert!(!needs_reverify(&dag));
    }

    #[test]
    fn needs_reverify_false_for_a_verify_less_dag() {
        // `Edit → GateHuman`: the human is the check, so ER4/ER5 stay out.
        let e = NodeId::new();
        let g = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(e, kinded(e, NodeKind::Edit, NodeState::Succeeded));
        nodes.insert(g, kinded(g, NodeKind::GateHuman, NodeState::Pending));
        let dag = dag_from(nodes, vec![edge(e, g, EdgeKind::Data)]);
        assert!(!needs_reverify(&dag));
        assert!(verifies_reachable_from_succeeded_edits(&dag).is_empty());
    }

    #[test]
    fn needs_reverify_reaches_a_verify_transitively() {
        // Edit → Review → VerifyCompile: reachability, not just adjacency.
        let e = NodeId::new();
        let r = NodeId::new();
        let v = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(e, kinded(e, NodeKind::Edit, NodeState::Succeeded));
        nodes.insert(r, kinded(r, NodeKind::Review, NodeState::Pending));
        nodes.insert(v, kinded(v, NodeKind::VerifyCompile, NodeState::Pending));
        let dag = dag_from(
            nodes,
            vec![edge(e, r, EdgeKind::Data), edge(r, v, EdgeKind::Data)],
        );
        assert!(needs_reverify(&dag));
    }

    #[test]
    fn needs_reverify_ignores_a_verify_not_downstream_of_the_edit() {
        // A verify that the succeeded edit cannot reach says nothing about
        // whether the edit's changes were verified.
        let e = NodeId::new();
        let v = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(e, kinded(e, NodeKind::Edit, NodeState::Succeeded));
        nodes.insert(v, kinded(v, NodeKind::VerifyCompile, NodeState::Pending));
        let dag = dag_from(nodes, vec![]); // no edges at all
        assert!(!needs_reverify(&dag));
    }

    #[test]
    fn er4_blocked_kind_matches_the_rfc_partition() {
        for kind in [
            NodeKind::Plan,
            NodeKind::Analyze,
            NodeKind::Edit,
            NodeKind::Review,
        ] {
            assert!(er4_blocked_kind(kind), "{kind:?} must be blocked");
        }
        for kind in [
            NodeKind::VerifyCompile,
            NodeKind::VerifyTest,
            NodeKind::GateHuman,
            NodeKind::Aggregate,
        ] {
            assert!(!er4_blocked_kind(kind), "{kind:?} must stay dispatchable");
        }
    }

    // ---- backoff_delay ----

    #[test]
    fn backoff_delay_fixed_returns_delay_capped() {
        let b = Backoff::Fixed { delay_ms: 500 };
        assert_eq!(
            backoff_delay(&b, 1, Duration::from_secs(10)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn backoff_delay_fixed_zero_is_zero() {
        let b = Backoff::Fixed { delay_ms: 0 };
        assert_eq!(
            backoff_delay(&b, 1, Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn backoff_delay_fixed_capped_by_max_backoff() {
        let b = Backoff::Fixed { delay_ms: 60_000 };
        assert_eq!(
            backoff_delay(&b, 1, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn backoff_delay_exponential_first_retry_is_base() {
        let b = Backoff::Exponential {
            base_ms: 100,
            factor: 2.0,
        };
        // k = attempt = 1 (first failed attempt) => factor^0 = 1 => base_ms.
        assert_eq!(
            backoff_delay(&b, 1, Duration::from_secs(60)),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn backoff_delay_exponential_grows_with_attempt() {
        let b = Backoff::Exponential {
            base_ms: 100,
            factor: 2.0,
        };
        assert_eq!(
            backoff_delay(&b, 2, Duration::from_secs(60)),
            Duration::from_millis(200)
        );
        assert_eq!(
            backoff_delay(&b, 3, Duration::from_secs(60)),
            Duration::from_millis(400)
        );
    }

    #[test]
    fn backoff_delay_exponential_capped_by_max_backoff() {
        let b = Backoff::Exponential {
            base_ms: 1_000,
            factor: 10.0,
        };
        assert_eq!(
            backoff_delay(&b, 10, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn backoff_delay_factor_below_one_treated_as_one() {
        let b = Backoff::Exponential {
            base_ms: 250,
            factor: 0.1,
        };
        // factor < 1.0 must be clamped to 1.0: constant base_ms regardless of attempt.
        assert_eq!(
            backoff_delay(&b, 5, Duration::from_secs(60)),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn backoff_delay_non_finite_factor_treated_as_one() {
        let b = Backoff::Exponential {
            base_ms: 250,
            factor: f64::NAN,
        };
        assert_eq!(
            backoff_delay(&b, 4, Duration::from_secs(60)),
            Duration::from_millis(250)
        );

        let b = Backoff::Exponential {
            base_ms: 250,
            factor: f64::INFINITY,
        };
        assert_eq!(
            backoff_delay(&b, 4, Duration::from_secs(60)),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn backoff_delay_overflow_saturates_at_max_backoff_without_panicking() {
        let b = Backoff::Exponential {
            base_ms: u64::MAX,
            factor: 1e300,
        };
        assert_eq!(
            backoff_delay(&b, 1_000_000, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    // ---- derive_dag_state ----

    #[test]
    fn derive_dag_state_replan_requested_wins_first() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Failed));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags {
            replan_requested: true,
            ..Default::default()
        };
        assert_eq!(
            derive_dag_state(&dag, flags).unwrap(),
            DagState::ReplanRequired
        );
    }

    #[test]
    fn derive_dag_state_user_cancel_with_cancelled_node_is_cancelled() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Cancelled));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags {
            cancel_requested: true,
            ..Default::default()
        };
        assert_eq!(derive_dag_state(&dag, flags).unwrap(), DagState::Cancelled);
    }

    #[test]
    fn derive_dag_state_user_cancel_wins_over_failed_sibling() {
        // DS2: user-requested cancel (D2) is evaluated before D3, so it wins
        // even when a Failed sibling exists.
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Failed));
        nodes.insert(b, node(b, NodeState::Cancelled));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags {
            cancel_requested: true,
            ..Default::default()
        };
        assert_eq!(derive_dag_state(&dag, flags).unwrap(), DagState::Cancelled);
    }

    #[test]
    fn derive_dag_state_failed_dominates_non_user_cancelled_sibling() {
        // DS1: without cancel_requested, D3 (Failed) fires ahead of D4/D5.
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Failed));
        nodes.insert(b, node(b, NodeState::Cancelled));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags::default();
        assert_eq!(derive_dag_state(&dag, flags).unwrap(), DagState::Failed);
    }

    #[test]
    fn derive_dag_state_cancelled_without_approval_failure_is_cancelled() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Cancelled));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags::default();
        assert_eq!(derive_dag_state(&dag, flags).unwrap(), DagState::Cancelled);
    }

    #[test]
    fn derive_dag_state_cancelled_with_approval_failure_is_failed() {
        let a = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Cancelled));
        let dag = dag_from(nodes, vec![]);
        let flags = DeriveFlags {
            approval_failure: true,
            ..Default::default()
        };
        assert_eq!(derive_dag_state(&dag, flags).unwrap(), DagState::Failed);
    }

    #[test]
    fn derive_dag_state_running_when_in_flight() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Succeeded));
        nodes.insert(b, node(b, NodeState::Running));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(
            derive_dag_state(&dag, DeriveFlags::default()).unwrap(),
            DagState::Running
        );
    }

    #[test]
    fn derive_dag_state_waiting_approval_when_any_node_waiting() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Pending));
        nodes.insert(b, node(b, NodeState::WaitingApproval));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(
            derive_dag_state(&dag, DeriveFlags::default()).unwrap(),
            DagState::WaitingApproval
        );
    }

    #[test]
    fn derive_dag_state_succeeded_when_all_succeeded_or_cached_hit() {
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Succeeded));
        nodes.insert(b, node(b, NodeState::CachedHit));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(
            derive_dag_state(&dag, DeriveFlags::default()).unwrap(),
            DagState::Succeeded
        );
    }

    #[test]
    fn derive_dag_state_all_skipped_mix_is_failed() {
        // D8: a partially skipped DAG never "succeeds" — this is the state a
        // stall (DS4) produces after marking unreachable Pending nodes Skipped.
        let a = NodeId::new();
        let b = NodeId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(a, node(a, NodeState::Succeeded));
        nodes.insert(b, node(b, NodeState::Skipped));
        let dag = dag_from(nodes, vec![]);
        assert_eq!(
            derive_dag_state(&dag, DeriveFlags::default()).unwrap(),
            DagState::Failed
        );
    }

    #[test]
    fn derive_dag_state_empty_dag_errors() {
        let dag = dag_from(BTreeMap::new(), vec![]);
        let err = derive_dag_state(&dag, DeriveFlags::default()).unwrap_err();
        assert!(matches!(err, SchedError::Invariant(m) if m.contains("empty dag")));
    }
}
