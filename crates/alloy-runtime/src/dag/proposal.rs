//! Proposal wire schema and clamping compiler (RFC-0017 §3.4 / §3.5 / §5.2).
//!
//! A proposal is **model output derived from untrusted goal text and
//! untrusted repository content** (RFC-0017 §2.5). It crosses into the
//! trusted plane only through this compiler and [`DagValidator`]: the
//! proposal chooses node names, kinds, order, and gate reasons — nothing
//! else. Capabilities, budgets, tiers, retries, timeouts, and cache flags
//! are assigned here from the fixed §5.2.3 table (byte-identical to
//! [`super::templates`]'s catalog values), so a proposal has no syntax with
//! which to escape a capability allowlist or a budget ceiling (SEC1–SEC3).
//!
//! Pure and synchronous: no I/O, no model calls, no config reads.
//!
//! Author: arkadianet

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::planner::PlannerConfig;
use crate::types::budget::{ModelTier, TokenBudget};
use crate::types::ids::{ArtifactId, CapabilityId, DagId, GateId, NodeId, SessionId};

use super::templates::{
    adapter_retry, build_topology, llm_retry, verify_retry, BuildTopology, TemplateApprovalSpec,
    TemplateEdgeSpec, TemplateId, TemplateIdMap, TemplateManifest, TemplateNodeSpec,
};
use super::types::{EdgeKind, NodeKind, TaskDag};
use super::validate::{expected_capability, DagValidationError, DagValidator, ValidateOpts};

/// Wire schema version for planning proposals (MUST be 1).
pub const PROPOSAL_SCHEMA_VERSION: u32 = 1;

/// A model-proposed **linear chain**, shape-only. Serialized inside
/// `PlanningProposalPayload.proposal` and as the `plan_proposal` CAS
/// artifact.
///
/// Deliberately cannot express: capabilities, budgets, tiers, retries,
/// timeouts, cache keys, edges, fan-out, `Plan` or `Aggregate` nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProposedDagManifest {
    /// MUST equal [`PROPOSAL_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Execution order. The compiler emits dual Data+Sequence edges between
    /// consecutive entries (RFC-0009 §5.7.2 convention).
    pub nodes: Vec<ProposedNodeSpec>,
    /// Free-text model rationale; audit only. MUST NOT influence compilation
    /// (SEC6).
    pub rationale: String,
}

/// One proposed node: a name, a kind, and (for gates) an approval reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProposedNodeSpec {
    /// `[a-z0-9_]{1,64}`, unique within the manifest (PC5).
    pub name: String,
    /// Only `Analyze | Edit | Review | VerifyCompile | VerifyTest |
    /// GateHuman` are accepted (PC3).
    pub kind: NodeKind,
    /// Required non-empty (after trim, ≤ 500 chars) iff `kind == GateHuman`;
    /// MUST be `None` otherwise (PC6).
    pub approval_reason: Option<String>,
}

/// Why a proposal was rejected. One variant per clamp rule (RFC-0017
/// §5.2.2); every rejection is a *fallback trigger*, never a run failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ProposalRejection {
    /// PC1 — unsupported schema version.
    #[error("unsupported proposal schema_version {got}")]
    SchemaVersion {
        /// Version the proposal carried.
        got: u32,
    },
    /// PC2 — serialized manifest exceeds the configured byte cap.
    #[error("proposal exceeds {max} bytes")]
    TooLarge {
        /// Configured `proposal_max_bytes`.
        max: u32,
    },
    /// PC4 — node count outside `2..=max_proposed_nodes`.
    #[error("node count {got} outside 2..={max}")]
    NodeCount {
        /// Proposed node count.
        got: usize,
        /// Configured ceiling.
        max: u32,
    },
    /// PC3 — `Plan` / `Aggregate` (or any non-allowlisted kind).
    #[error("node kind {kind:?} not allowed in proposals")]
    KindForbidden {
        /// Offending kind.
        kind: NodeKind,
    },
    /// PC5 — name fails `[a-z0-9_]{1,64}` or duplicates another.
    #[error("node name invalid or duplicate: {name}")]
    BadName {
        /// Offending name.
        name: String,
    },
    /// PC6 — `approval_reason` constraint violated.
    #[error("approval_reason constraint violated on {name}")]
    BadApproval {
        /// Offending node name.
        name: String,
    },
    /// PC7 — the chain does not end in `GateHuman`.
    #[error("terminal node must be GateHuman")]
    NoTerminalGate,
    /// PC8 — no verify node precedes the terminal gate.
    #[error("no verify node precedes the terminal gate")]
    NoVerify,
    /// PC8/PC14 — an `Edit` is not covered by a later verify (the gate
    /// would approve an unverified patch).
    #[error("edit node {name} is not followed by a verify node before the terminal gate")]
    UnverifiedEdit {
        /// Offending (last) edit node name.
        name: String,
    },
    /// PC13 — an `Edit` with no preceding `Analyze` or verify in the chain.
    #[error("edit node {name} has no preceding Analyze or verify node")]
    UngroundedEdit {
        /// Offending edit node name.
        name: String,
    },
    /// PC12 — the compiled DAG failed [`DagValidator`].
    #[error("compiled DAG failed validation: {0}")]
    Validation(#[from] DagValidationError),
}

/// Arguments for [`compile_proposal`] (RFC-0017 §3.5).
#[derive(Debug)]
pub struct CompileArgs<'a> {
    /// Pre-minted DAG id.
    pub dag_id: DagId,
    /// Owning session.
    pub session_id: SessionId,
    /// Generation to stamp.
    pub generation: u64,
    /// From [`allocate_proposal_ids`] over the proposal names.
    pub ids: &'a TemplateIdMap,
    /// Plan-time input refs (ephemeral for the pre-CAS validation pass,
    /// real after Phase B — RFC-0009 §5.3).
    pub input_refs: &'a BTreeMap<NodeId, ArtifactId>,
    /// Validated planner knobs.
    pub cfg: &'a PlannerConfig,
    /// Whether the `review` capability is registered. When false, proposals
    /// carrying `NodeKind::Review` are rejected before dispatch (RG7).
    pub enable_review: bool,
}

/// Allocate `NodeId`s / `GateId`s for a proposal (name-keyed, mirrors
/// [`super::templates::allocate_ids`]). Rejects rather than panics:
/// proposals are untrusted, unlike embedded manifests.
pub fn allocate_proposal_ids(
    manifest: &ProposedDagManifest,
) -> Result<TemplateIdMap, ProposalRejection> {
    let mut nodes = BTreeMap::new();
    let mut gates = BTreeMap::new();
    for spec in &manifest.nodes {
        if !name_is_valid(&spec.name) || nodes.contains_key(&spec.name) {
            return Err(ProposalRejection::BadName {
                name: spec.name.clone(),
            });
        }
        nodes.insert(spec.name.clone(), NodeId::new());
        if spec.kind == NodeKind::GateHuman {
            gates.insert(spec.name.clone(), GateId::new());
        }
    }
    Ok(TemplateIdMap { nodes, gates })
}

/// PC5 — `[a-z0-9_]{1,64}`.
fn name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

const fn is_verify(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::VerifyCompile | NodeKind::VerifyTest)
}

/// Shape clamps PC1–PC8 + PC13, in PC order (first violation wins), then
/// §5.2.3 resource assignment and PC9/PC10 edge/resource emission.
///
/// Returned specs/edges are what [`crate::planner`]'s persistence consumes;
/// a template instantiation and a compiled proposal are indistinguishable
/// there by design (AM-0009-6).
pub(crate) fn resolve_proposal(
    manifest: &ProposedDagManifest,
    cfg: &PlannerConfig,
    enable_review: bool,
) -> Result<(Vec<TemplateNodeSpec>, Vec<TemplateEdgeSpec>), ProposalRejection> {
    // PC1.
    if manifest.schema_version != PROPOSAL_SCHEMA_VERSION {
        return Err(ProposalRejection::SchemaVersion {
            got: manifest.schema_version,
        });
    }
    // PC2 — on the manifest's canonical serialization. The proposer applies
    // the same cap to the payload-borne bytes before decode; this re-check
    // makes the compiler safe against callers that skipped it.
    let bytes = serde_json::to_vec(manifest)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if bytes > cfg.proposal_max_bytes as usize {
        return Err(ProposalRejection::TooLarge {
            max: cfg.proposal_max_bytes,
        });
    }
    // PC3 — closed kind allowlist; `Plan` (no recursive planning, SEC4) and
    // `Aggregate` (no structural nodes) are forbidden. Review is only
    // admitted when the review capability is registered (RG7).
    for spec in &manifest.nodes {
        let allowed = match spec.kind {
            NodeKind::Analyze
            | NodeKind::Edit
            | NodeKind::VerifyCompile
            | NodeKind::VerifyTest
            | NodeKind::GateHuman => true,
            NodeKind::Review => enable_review,
            _ => false,
        };
        if !allowed {
            return Err(ProposalRejection::KindForbidden { kind: spec.kind });
        }
    }
    // PC4.
    if manifest.nodes.len() < 2 || manifest.nodes.len() > cfg.max_proposed_nodes as usize {
        return Err(ProposalRejection::NodeCount {
            got: manifest.nodes.len(),
            max: cfg.max_proposed_nodes,
        });
    }
    // PC5.
    let mut seen = std::collections::BTreeSet::new();
    for spec in &manifest.nodes {
        if !name_is_valid(&spec.name) || !seen.insert(spec.name.as_str()) {
            return Err(ProposalRejection::BadName {
                name: spec.name.clone(),
            });
        }
    }
    // PC6.
    for spec in &manifest.nodes {
        let ok = match (&spec.kind, &spec.approval_reason) {
            (NodeKind::GateHuman, Some(reason)) => {
                let trimmed = reason.trim();
                !trimmed.is_empty() && trimmed.chars().count() <= 500
            }
            (NodeKind::GateHuman, None) => false,
            (_, None) => true,
            (_, Some(_)) => false,
        };
        if !ok {
            return Err(ProposalRejection::BadApproval {
                name: spec.name.clone(),
            });
        }
    }
    // PC7.
    let terminal = manifest.nodes.len() - 1;
    if manifest.nodes[terminal].kind != NodeKind::GateHuman {
        return Err(ProposalRejection::NoTerminalGate);
    }
    check_verify_after_final_edit(
        manifest.nodes.iter().map(|n| (n.name.as_str(), n.kind)),
        terminal,
    )?;
    // PC13 — grounding clamp: an edit whose only input would be the bare
    // goal is the blind-edit shape this RFC exists to eliminate.
    for (idx, spec) in manifest.nodes.iter().enumerate() {
        if spec.kind == NodeKind::Edit
            && !manifest.nodes[..idx]
                .iter()
                .any(|n| n.kind == NodeKind::Analyze || is_verify(n.kind))
        {
            return Err(ProposalRejection::UngroundedEdit {
                name: spec.name.clone(),
            });
        }
    }

    // §5.2.3 resource assignment (PC10): every security-relevant field is
    // compiler-owned; capability ids are derived from the RFC-0009 map,
    // never accepted (SEC1).
    let specs = manifest
        .nodes
        .iter()
        .map(|spec| {
            let capability = expected_capability(spec.kind).map(|s| {
                // Static catalog strings; failure is a crate bug, exactly as
                // in `templates::cap`.
                CapabilityId::new(s).unwrap_or_else(|_| panic!("invalid capability id: {s}"))
            });
            let (retry, budget, model_tier, timeout_ms) = match spec.kind {
                NodeKind::Analyze | NodeKind::Edit | NodeKind::Review => (
                    llm_retry(),
                    TokenBudget {
                        max_input: 32_768,
                        max_output: 8_192,
                    },
                    ModelTier::Standard,
                    300_000,
                ),
                NodeKind::VerifyCompile | NodeKind::VerifyTest => (
                    verify_retry(),
                    TokenBudget {
                        max_input: 0,
                        max_output: 0,
                    },
                    ModelTier::Economy,
                    600_000,
                ),
                // PC3 leaves only GateHuman.
                _ => (
                    adapter_retry(),
                    TokenBudget {
                        max_input: 0,
                        max_output: 0,
                    },
                    ModelTier::Economy,
                    3_600_000,
                ),
            };
            TemplateNodeSpec {
                name: spec.name.clone(),
                kind: spec.kind,
                capability,
                retry,
                budget,
                model_tier,
                approval: spec.approval_reason.as_ref().map(|r| TemplateApprovalSpec {
                    reason: r.trim().to_owned(),
                }),
                timeout_ms,
                enable_cache: false, // PC10 — forced, no proposal syntax exists.
            }
        })
        .collect();

    // PC9 — dual Data+Sequence edges between consecutive nodes, nothing else.
    let mut edges = Vec::with_capacity((manifest.nodes.len() - 1) * 2);
    for pair in manifest.nodes.windows(2) {
        for kind in [EdgeKind::Data, EdgeKind::Sequence] {
            edges.push(TemplateEdgeSpec {
                from: pair[0].name.clone(),
                to: pair[1].name.clone(),
                kind,
            });
        }
    }
    Ok((specs, edges))
}

/// PC8 — a verify node before the terminal gate (`NoVerify`) and, when the
/// chain contains any `Edit`, a verify strictly after the **last** `Edit`
/// and strictly before the terminal gate (`UnverifiedEdit`). A mid-chain
/// gate does not satisfy this (PC11).
fn check_verify_after_final_edit<'a>(
    chain: impl Iterator<Item = (&'a str, NodeKind)>,
    terminal: usize,
) -> Result<(), ProposalRejection> {
    let chain: Vec<(&str, NodeKind)> = chain.collect();
    if !chain[..terminal].iter().any(|(_, k)| is_verify(*k)) {
        return Err(ProposalRejection::NoVerify);
    }
    if let Some(last_edit) = chain
        .iter()
        .enumerate()
        .rev()
        .find(|(_, (_, k))| *k == NodeKind::Edit)
    {
        let (idx, (name, _)) = last_edit;
        let covered = chain[idx + 1..terminal].iter().any(|(_, k)| is_verify(*k));
        if !covered {
            return Err(ProposalRejection::UnverifiedEdit {
                name: (*name).to_owned(),
            });
        }
    }
    Ok(())
}

/// Pure, sync, no I/O. Applies PC1–PC14, assigns resources per §5.2.3,
/// builds the `TaskDag` through the RFC-0009 machinery, and runs
/// [`DagValidator::validate`] with [`ValidateOpts::default`] as the final
/// gate (PC12).
pub fn compile_proposal(
    manifest: &ProposedDagManifest,
    args: CompileArgs<'_>,
) -> Result<TaskDag, ProposalRejection> {
    let (specs, edges) = resolve_proposal(manifest, args.cfg, args.enable_review)?;
    // The manifest carrier exists only for `build_topology`; the id is the
    // day-1 fallback identity (LP5) and the description is never consumed.
    let carrier = TemplateManifest {
        id: TemplateId::RepairLocalDiagnostic,
        description: String::new(),
        nodes: specs,
        edges,
    };
    let dag = build_topology(BuildTopology {
        manifest: &carrier,
        dag_id: args.dag_id,
        session_id: args.session_id,
        generation: args.generation,
        ids: args.ids,
        input_refs: args.input_refs,
    });
    // PC14 — re-check verify-after-final-Edit on the *built* topology (walk
    // the Sequence chain), so a future manifest form cannot smuggle past
    // the manifest-level check.
    check_built_chain(&dag)?;
    // PC12 — the unchanged validator, default opts (linear V15 + gates V11).
    DagValidator::validate(&dag, ValidateOpts::default())?;
    Ok(dag)
}

/// PC14 over the built DAG: reconstruct the chain order from Sequence edges
/// and re-apply the PC8 predicate.
fn check_built_chain(dag: &TaskDag) -> Result<(), ProposalRejection> {
    let mut succ: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    let mut has_pred: std::collections::BTreeSet<NodeId> = std::collections::BTreeSet::new();
    for e in dag.edges.iter().filter(|e| e.kind == EdgeKind::Sequence) {
        succ.insert(e.from, e.to);
        has_pred.insert(e.to);
    }
    let Some(mut cur) = dag.nodes.keys().find(|id| !has_pred.contains(id)).copied() else {
        // No root ⇒ not a chain; PC12's validator rejects it authoritatively.
        return Ok(());
    };
    let mut chain: Vec<(NodeId, NodeKind)> = Vec::with_capacity(dag.nodes.len());
    loop {
        let Some(node) = dag.nodes.get(&cur) else {
            return Ok(()); // dangling edge — validator's jurisdiction.
        };
        chain.push((cur, node.kind));
        // Bound by node count so a cyclic Sequence walk cannot grow forever
        // before PC12's validator sees it.
        if chain.len() > dag.nodes.len() {
            return Ok(());
        }
        match succ.get(&cur) {
            Some(next) => cur = *next,
            None => break,
        }
    }
    if chain.len() != dag.nodes.len() || chain.last().map(|(_, k)| *k) != Some(NodeKind::GateHuman)
    {
        return Ok(()); // shape defects beyond PC14's scope — validator's.
    }
    let terminal = chain.len() - 1;
    let named: Vec<(String, NodeKind)> = chain.iter().map(|(id, k)| (id.to_string(), *k)).collect();
    check_verify_after_final_edit(named.iter().map(|(n, k)| (n.as_str(), *k)), terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::templates::TemplateCatalog;

    fn node(name: &str, kind: NodeKind) -> ProposedNodeSpec {
        let approval_reason = (kind == NodeKind::GateHuman)
            .then(|| "Approve repair diff before completion".to_owned());
        ProposedNodeSpec {
            name: name.into(),
            kind,
            approval_reason,
        }
    }

    fn manifest(nodes: Vec<ProposedNodeSpec>) -> ProposedDagManifest {
        ProposedDagManifest {
            schema_version: PROPOSAL_SCHEMA_VERSION,
            nodes,
            rationale: "test".into(),
        }
    }

    fn chain(kinds: &[(&str, NodeKind)]) -> ProposedDagManifest {
        manifest(kinds.iter().map(|(n, k)| node(n, *k)).collect())
    }

    fn cfg() -> PlannerConfig {
        PlannerConfig::new()
    }

    fn compile(m: &ProposedDagManifest) -> Result<TaskDag, ProposalRejection> {
        let cfg = cfg();
        let ids = allocate_proposal_ids(m)?;
        let mut input_refs = BTreeMap::new();
        for nid in ids.nodes.values() {
            input_refs.insert(*nid, ArtifactId::new());
        }
        compile_proposal(
            m,
            CompileArgs {
                dag_id: DagId::new(),
                session_id: SessionId::new(),
                generation: 1,
                ids: &ids,
                input_refs: &input_refs,
                cfg: &cfg,
                enable_review: true,
            },
        )
    }

    fn repair_shape() -> ProposedDagManifest {
        chain(&[
            ("analyze", NodeKind::Analyze),
            ("edit", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("gate", NodeKind::GateHuman),
        ])
    }

    /// AC 1: serde round-trip, schema pinned to 1, unknown fields rejected.
    #[test]
    fn ac1_manifest_serde_round_trip_and_unknown_fields_rejected() {
        let m = repair_shape();
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["schema_version"], 1);
        let back: ProposedDagManifest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, m);

        let mut extra = json.clone();
        extra["edges"] = serde_json::json!([]);
        assert!(
            serde_json::from_value::<ProposedDagManifest>(extra).is_err(),
            "AC 6: a proposal has no syntax for edges/fan-out"
        );
        let mut node_extra = json;
        node_extra["nodes"][0]["budget"] = serde_json::json!(999);
        assert!(
            serde_json::from_value::<ProposedDagManifest>(node_extra).is_err(),
            "AC 4: a proposal has no field that can alter resources"
        );
    }

    /// AC 2 (+3, 5, 5b, 5c): one rejection per clamp rule, in PC order.
    #[test]
    fn ac2_each_clamp_rule_rejects_in_pc_order() {
        // PC1.
        let mut m = repair_shape();
        m.schema_version = 2;
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::SchemaVersion { got: 2 }
        );

        // PC2 — oversize rationale; also carries a forbidden kind so this
        // doubles as a first-violation-wins probe (PC2 before PC3).
        let mut m = repair_shape();
        m.rationale = "x".repeat(64 * 1024);
        m.nodes[0].kind = NodeKind::Plan;
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::TooLarge { max: 16_384 }
        );

        // PC3 before PC4: a 1-node Plan proposal names the kind, not the count.
        let m = manifest(vec![node("p", NodeKind::Plan)]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::KindForbidden {
                kind: NodeKind::Plan
            }
        );

        // PC4 low and high.
        let m = manifest(vec![node("gate", NodeKind::GateHuman)]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::NodeCount { got: 1, max: 8 }
        );
        let mut many: Vec<ProposedNodeSpec> = (0..9)
            .map(|i| node(&format!("n{i}"), NodeKind::Analyze))
            .collect();
        many.push(node("gate", NodeKind::GateHuman));
        assert!(matches!(
            resolve_proposal(&manifest(many), &cfg(), true).unwrap_err(),
            ProposalRejection::NodeCount { got: 10, max: 8 }
        ));

        // PC5 — bad chars, over-length, duplicate.
        let long = "a".repeat(65);
        for bad in ["Analyze", "a b", "", long.as_str()] {
            let mut m = repair_shape();
            m.nodes[0].name = bad.into();
            assert!(matches!(
                resolve_proposal(&m, &cfg(), true).unwrap_err(),
                ProposalRejection::BadName { .. }
            ));
        }
        let mut m = repair_shape();
        m.nodes[1].name = "analyze".into();
        assert!(matches!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::BadName { .. }
        ));

        // PC6 — missing, empty-after-trim, over-length, and non-gate reason.
        let mut m = repair_shape();
        m.nodes[3].approval_reason = None;
        assert!(matches!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::BadApproval { .. }
        ));
        let mut m = repair_shape();
        m.nodes[3].approval_reason = Some("   ".into());
        assert!(matches!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::BadApproval { .. }
        ));
        let mut m = repair_shape();
        m.nodes[3].approval_reason = Some("x".repeat(501));
        assert!(matches!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::BadApproval { .. }
        ));
        let mut m = repair_shape();
        m.nodes[0].approval_reason = Some("why".into());
        assert!(matches!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::BadApproval { .. }
        ));

        // PC7.
        let m = chain(&[
            ("analyze", NodeKind::Analyze),
            ("verify", NodeKind::VerifyCompile),
        ]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::NoTerminalGate
        );

        // PC8 — no verify at all.
        let m = chain(&[
            ("analyze", NodeKind::Analyze),
            ("gate", NodeKind::GateHuman),
        ]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::NoVerify
        );
    }

    /// AC 3: `Plan` and `Aggregate` in a proposal → `KindForbidden` (SEC4).
    #[test]
    fn ac3_plan_and_aggregate_kinds_forbidden() {
        for kind in [NodeKind::Plan, NodeKind::Aggregate] {
            let mut m = repair_shape();
            m.nodes[0].kind = kind;
            m.nodes[0].approval_reason = None;
            assert_eq!(
                resolve_proposal(&m, &cfg(), true).unwrap_err(),
                ProposalRejection::KindForbidden { kind }
            );
        }
    }

    /// AC 4/4b: a compiled proposal is resource-identical to
    /// `repair_local_diagnostic` field by field — the §5.2.3 table cannot
    /// drift from `templates.rs` undetected.
    #[test]
    fn ac4b_compiled_resources_equal_repair_local_diagnostic_catalog() {
        let dag = compile(&repair_shape()).unwrap();
        let catalog = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
        for spec in &catalog.nodes {
            let compiled = dag
                .nodes
                .values()
                .find(|n| n.kind == spec.kind)
                .unwrap_or_else(|| panic!("no compiled node of kind {:?}", spec.kind));
            assert_eq!(compiled.capability, spec.capability, "{:?}", spec.kind);
            assert_eq!(compiled.retry, spec.retry, "{:?}", spec.kind);
            assert_eq!(compiled.budget, spec.budget, "{:?}", spec.kind);
            assert_eq!(compiled.model_tier, spec.model_tier, "{:?}", spec.kind);
            assert_eq!(compiled.timeout_ms, spec.timeout_ms, "{:?}", spec.kind);
            assert!(compiled.cache_key.is_none(), "{:?}", spec.kind);
            assert_eq!(
                compiled.approval.as_ref().map(|a| a.reason.as_str()),
                spec.approval.as_ref().map(|a| a.reason.as_str()),
                "{:?}",
                spec.kind
            );
        }
        // Pin the load-bearing verify/gate values explicitly (AC 4b).
        let verify = dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::VerifyCompile)
            .unwrap();
        assert_eq!(verify.retry.max_attempts, 2);
        assert!(matches!(
            verify.retry.backoff,
            crate::dag::Backoff::Fixed { delay_ms: 1000 }
        ));
        assert_eq!(
            verify.retry.retry_on,
            vec![crate::types::diagnostic::ErrorClass::Tool]
        );
        let gate = dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::GateHuman)
            .unwrap();
        assert_eq!(gate.retry.max_attempts, 1);
        assert!(gate.retry.retry_on.is_empty());
        // AC 6: the accepted proposal passed the unchanged validator.
        DagValidator::validate(&dag, ValidateOpts::default()).unwrap();
        // PC9: dual edges only.
        assert_eq!(dag.edges.len(), 6);
    }

    /// AC 5b: the PC8/PC14 adversarial table.
    #[test]
    fn ac5b_verify_after_final_edit_adversarial_table() {
        // Rejected: verify precedes the edit.
        let m = chain(&[
            ("analyze", NodeKind::Analyze),
            ("verify", NodeKind::VerifyCompile),
            ("edit", NodeKind::Edit),
            ("gate", NodeKind::GateHuman),
        ]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::UnverifiedEdit {
                name: "edit".into()
            }
        );
        // Rejected: second edit after the verify (PC8 wins over PC13).
        let m = chain(&[
            ("edit1", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("edit2", NodeKind::Edit),
            ("gate", NodeKind::GateHuman),
        ]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::UnverifiedEdit {
                name: "edit2".into()
            }
        );
        // Accepted.
        compile(&repair_shape()).unwrap();
        // Accepted: mid-chain gate (PC11) with a verify after the last edit.
        let m = chain(&[
            ("analyze", NodeKind::Analyze),
            ("edit", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("gate_mid", NodeKind::GateHuman),
            ("verify2", NodeKind::VerifyCompile),
            ("gate", NodeKind::GateHuman),
        ]);
        compile(&m).unwrap();
        // Accepted: no edit at all.
        let m = chain(&[
            ("verify", NodeKind::VerifyTest),
            ("gate", NodeKind::GateHuman),
        ]);
        compile(&m).unwrap();
    }

    /// AC 5c: PC13 — an edit with no preceding Analyze or verify.
    #[test]
    fn ac5c_ungrounded_edit_rejected() {
        let m = chain(&[
            ("edit", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("gate", NodeKind::GateHuman),
        ]);
        assert_eq!(
            resolve_proposal(&m, &cfg(), true).unwrap_err(),
            ProposalRejection::UngroundedEdit {
                name: "edit".into()
            }
        );
        // Verify-first grounding is acceptable (Appendix B shape).
        let m = chain(&[
            ("precheck", NodeKind::VerifyTest),
            ("edit", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("gate", NodeKind::GateHuman),
        ]);
        compile(&m).unwrap();
    }

    /// `allocate_proposal_ids` rejects rather than panics on untrusted input.
    #[test]
    fn allocate_proposal_ids_rejects_duplicates_and_bad_names() {
        let mut m = repair_shape();
        m.nodes[1].name = "analyze".into();
        assert!(matches!(
            allocate_proposal_ids(&m).unwrap_err(),
            ProposalRejection::BadName { .. }
        ));
        let mut m = repair_shape();
        m.nodes[0].name = "Bad Name".into();
        assert!(matches!(
            allocate_proposal_ids(&m).unwrap_err(),
            ProposalRejection::BadName { .. }
        ));
        let ids = allocate_proposal_ids(&repair_shape()).unwrap();
        assert_eq!(ids.nodes.len(), 4);
        assert_eq!(ids.gates.len(), 1);
    }

    /// SEC6 / AC 38 (compiler half): `rationale` never influences
    /// compilation — two manifests differing only in rationale compile to
    /// kind/resource-identical DAGs.
    #[test]
    fn rationale_never_influences_compilation() {
        let a = compile(&repair_shape()).unwrap();
        let mut m = repair_shape();
        m.rationale = "completely different words".into();
        let b = compile(&m).unwrap();
        let shape = |dag: &TaskDag| {
            let mut kinds: Vec<NodeKind> = dag.nodes.values().map(|n| n.kind).collect();
            kinds.sort_by_key(|k| format!("{k:?}"));
            (kinds, dag.edges.len())
        };
        assert_eq!(shape(&a), shape(&b));
    }
}
