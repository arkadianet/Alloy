//! Hardcoded DAG templates and sync topology builders (RFC-0009).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Serialize;

use crate::dag::types::{
    ApprovalSpec, Backoff, DependencyEdge, EdgeKind, NodeKind, NodeState, RetryPolicy, TaskDag,
    TaskNode,
};
use crate::scheduler::DagState;
use crate::types::budget::{ModelTier, TokenBudget};
use crate::types::diagnostic::ErrorClass;
use crate::types::ids::{ArtifactId, CapabilityId, DagId, GateId, NodeId, SessionId};

/// Closed MVP template identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateId {
    /// Analyze → Edit → VerifyCompile → GateHuman repair chain.
    RepairLocalDiagnostic,
}

impl TemplateId {
    /// Wire / catalog name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepairLocalDiagnostic => "repair_local_diagnostic",
        }
    }

    /// Parse a catalog name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "repair_local_diagnostic" => Some(Self::RepairLocalDiagnostic),
            _ => None,
        }
    }
}

/// Embedded template manifest (not the runtime [`TaskDag`]).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(any(test, feature = "template-json"), derive(serde::Deserialize))]
pub struct TemplateManifest {
    /// Template id.
    pub id: TemplateId,
    /// Human-readable description.
    pub description: String,
    /// Template nodes (local names).
    pub nodes: Vec<TemplateNodeSpec>,
    /// Template edges (by local name).
    pub edges: Vec<TemplateEdgeSpec>,
}

/// Template node specification.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(any(test, feature = "template-json"), derive(serde::Deserialize))]
pub struct TemplateNodeSpec {
    /// Local name within the template.
    pub name: String,
    /// Node kind.
    pub kind: NodeKind,
    /// Optional capability.
    pub capability: Option<CapabilityId>,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Token budget.
    pub budget: TokenBudget,
    /// Model tier hint.
    pub model_tier: ModelTier,
    /// Optional approval (gates).
    pub approval: Option<TemplateApprovalSpec>,
    /// Timeout milliseconds.
    pub timeout_ms: u64,
    /// When false, instantiate with `cache_key = None` (day-1: all false).
    pub enable_cache: bool,
}

/// Template approval without a minted [`GateId`].
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(any(test, feature = "template-json"), derive(serde::Deserialize))]
pub struct TemplateApprovalSpec {
    /// Human-readable reason.
    pub reason: String,
}

/// Template edge by local node name.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(any(test, feature = "template-json"), derive(serde::Deserialize))]
pub struct TemplateEdgeSpec {
    /// Predecessor local name.
    pub from: String,
    /// Successor local name.
    pub to: String,
    /// Edge kind.
    pub kind: EdgeKind,
}

/// Local name → runtime id map produced by [`allocate_ids`].
#[derive(Debug, Clone)]
pub struct TemplateIdMap {
    /// Node local name → [`NodeId`].
    pub nodes: BTreeMap<String, NodeId>,
    /// Gate local name → [`GateId`] (keyed by template node name).
    pub gates: BTreeMap<String, GateId>,
}

/// Arguments for sync topology build (Phase C).
#[derive(Debug)]
pub struct BuildTopology<'a> {
    /// Template manifest.
    pub manifest: &'a TemplateManifest,
    /// Pre-minted DAG id.
    pub dag_id: DagId,
    /// Owning session.
    pub session_id: SessionId,
    /// Generation to stamp.
    pub generation: u64,
    /// Allocated ids.
    pub ids: &'a TemplateIdMap,
    /// Plan-time input artifact refs (every node must be present).
    pub input_refs: &'a BTreeMap<NodeId, ArtifactId>,
}

/// Closed template catalog backed by `OnceLock`.
pub struct TemplateCatalog;

impl TemplateCatalog {
    /// All shipped templates.
    ///
    /// Panics on first use if embedded data cannot build `CapabilityId` (crate bug).
    #[must_use]
    pub fn all() -> &'static [TemplateManifest] {
        static CATALOG: OnceLock<Vec<TemplateManifest>> = OnceLock::new();
        CATALOG.get_or_init(build_catalog).as_slice()
    }

    /// Infallible lookup — [`TemplateId`] is closed.
    #[must_use]
    pub fn get(id: TemplateId) -> &'static TemplateManifest {
        Self::all()
            .iter()
            .find(|m| m.id == id)
            .expect("closed TemplateId must exist in catalog")
    }

    /// Lookup by wire name.
    #[must_use]
    pub fn get_by_name(name: &str) -> Option<&'static TemplateManifest> {
        TemplateId::parse(name).map(Self::get)
    }
}

fn cap(s: &str) -> CapabilityId {
    CapabilityId::new(s).unwrap_or_else(|_| panic!("invalid catalog capability id: {s}"))
}

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

fn build_catalog() -> Vec<TemplateManifest> {
    let repair = TemplateManifest {
        id: TemplateId::RepairLocalDiagnostic,
        description: "Local diagnostic repair: analyze, edit, verify compile, human gate".into(),
        nodes: vec![
            TemplateNodeSpec {
                name: "analyze".into(),
                kind: NodeKind::Analyze,
                capability: Some(cap("repair")),
                retry: llm_retry(),
                budget: TokenBudget {
                    max_input: 32768,
                    max_output: 8192,
                },
                model_tier: ModelTier::Standard,
                approval: None,
                timeout_ms: 300_000,
                enable_cache: false,
            },
            TemplateNodeSpec {
                name: "edit".into(),
                kind: NodeKind::Edit,
                capability: Some(cap("edit")),
                retry: llm_retry(),
                budget: TokenBudget {
                    max_input: 32768,
                    max_output: 8192,
                },
                model_tier: ModelTier::Standard,
                approval: None,
                timeout_ms: 300_000,
                enable_cache: false,
            },
            TemplateNodeSpec {
                name: "verify".into(),
                kind: NodeKind::VerifyCompile,
                capability: None,
                retry: adapter_retry(),
                budget: TokenBudget {
                    max_input: 0,
                    max_output: 0,
                },
                model_tier: ModelTier::Economy,
                approval: None,
                timeout_ms: 600_000,
                enable_cache: false,
            },
            TemplateNodeSpec {
                name: "gate".into(),
                kind: NodeKind::GateHuman,
                capability: None,
                retry: adapter_retry(),
                budget: TokenBudget {
                    max_input: 0,
                    max_output: 0,
                },
                model_tier: ModelTier::Economy,
                approval: Some(TemplateApprovalSpec {
                    reason: "Approve repair diff before completion".into(),
                }),
                timeout_ms: 3_600_000,
                enable_cache: false,
            },
        ],
        edges: vec![
            TemplateEdgeSpec {
                from: "analyze".into(),
                to: "edit".into(),
                kind: EdgeKind::Data,
            },
            TemplateEdgeSpec {
                from: "analyze".into(),
                to: "edit".into(),
                kind: EdgeKind::Sequence,
            },
            TemplateEdgeSpec {
                from: "edit".into(),
                to: "verify".into(),
                kind: EdgeKind::Data,
            },
            TemplateEdgeSpec {
                from: "edit".into(),
                to: "verify".into(),
                kind: EdgeKind::Sequence,
            },
            TemplateEdgeSpec {
                from: "verify".into(),
                to: "gate".into(),
                kind: EdgeKind::Data,
            },
            TemplateEdgeSpec {
                from: "verify".into(),
                to: "gate".into(),
                kind: EdgeKind::Sequence,
            },
        ],
    };

    // Validate catalog integrity at init (crate bug → panic).
    for m in std::slice::from_ref(&repair) {
        let mut names = std::collections::HashSet::new();
        for n in &m.nodes {
            if !names.insert(n.name.as_str()) {
                panic!("duplicate template node name {} in {:?}", n.name, m.id);
            }
        }
        for e in &m.edges {
            if !names.contains(e.from.as_str()) || !names.contains(e.to.as_str()) {
                panic!("unresolvable edge in {:?}: {} -> {}", m.id, e.from, e.to);
            }
        }
    }

    vec![repair]
}

/// Phase A — sync. Allocate fresh node/gate ids for a manifest.
///
/// Duplicate names or unknown edge endpoints in hand-built manifests panic.
#[must_use]
pub fn allocate_ids(manifest: &TemplateManifest) -> TemplateIdMap {
    let mut nodes = BTreeMap::new();
    let mut gates = BTreeMap::new();
    for spec in &manifest.nodes {
        if nodes.contains_key(&spec.name) {
            panic!("duplicate template node name: {}", spec.name);
        }
        let nid = NodeId::new();
        nodes.insert(spec.name.clone(), nid);
        if spec.approval.is_some() {
            gates.insert(spec.name.clone(), GateId::new());
        }
    }
    for e in &manifest.edges {
        if !nodes.contains_key(&e.from) || !nodes.contains_key(&e.to) {
            panic!(
                "unknown edge endpoint in template {:?}: {} -> {}",
                manifest.id, e.from, e.to
            );
        }
    }
    TemplateIdMap { nodes, gates }
}

/// Phase C — sync, pure. Look up every node's `input_ref`; missing key panics.
#[must_use]
pub fn build_topology(args: BuildTopology<'_>) -> TaskDag {
    let mut nodes = BTreeMap::new();
    for spec in &args.manifest.nodes {
        let id = *args
            .ids
            .nodes
            .get(&spec.name)
            .unwrap_or_else(|| panic!("missing id for template node {}", spec.name));
        let input_ref = *args
            .input_refs
            .get(&id)
            .unwrap_or_else(|| panic!("missing input_ref for node {id}"));
        let approval = spec.approval.as_ref().map(|a| ApprovalSpec {
            gate: *args
                .ids
                .gates
                .get(&spec.name)
                .unwrap_or_else(|| panic!("missing gate id for {}", spec.name)),
            reason: a.reason.clone(),
        });
        nodes.insert(
            id,
            TaskNode {
                id,
                kind: spec.kind,
                capability: spec.capability.clone(),
                input_ref,
                output_ref: None,
                state: NodeState::Pending,
                retry: spec.retry.clone(),
                cache_key: None, // day-1 templates set enable_cache=false; hits owned by 0010
                budget: spec.budget.clone(),
                model_tier: spec.model_tier,
                approval,
                timeout_ms: spec.timeout_ms,
            },
        );
    }

    let mut edges = Vec::with_capacity(args.manifest.edges.len());
    for e in &args.manifest.edges {
        edges.push(DependencyEdge {
            from: *args.ids.nodes.get(&e.from).expect("edge from"),
            to: *args.ids.nodes.get(&e.to).expect("edge to"),
            kind: e.kind,
        });
    }

    TaskDag {
        id: args.dag_id,
        session_id: args.session_id,
        generation: args.generation,
        nodes,
        edges,
        state: DagState::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{DagValidator, ValidateOpts};

    #[test]
    fn catalog_parses() {
        let all = TemplateCatalog::all();
        assert_eq!(all.len(), 1);
        let m = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
        assert_eq!(m.id.as_str(), "repair_local_diagnostic");
        let mut names = std::collections::HashSet::new();
        for n in &m.nodes {
            assert!(names.insert(n.name.clone()));
            assert!(!n.enable_cache);
        }
        for e in &m.edges {
            assert!(names.contains(&e.from));
            assert!(names.contains(&e.to));
        }
        assert!(TemplateCatalog::get_by_name("nope").is_none());
        assert!(TemplateId::parse("repair_local_diagnostic").is_some());
        assert!(TemplateId::parse("unknown").is_none());
    }

    #[test]
    fn repair_local_diagnostic_validates() {
        let m = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
        let ids = allocate_ids(m);
        let mut input_refs = BTreeMap::new();
        for nid in ids.nodes.values() {
            input_refs.insert(*nid, ArtifactId::new());
        }
        let dag = build_topology(BuildTopology {
            manifest: m,
            dag_id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            ids: &ids,
            input_refs: &input_refs,
        });
        assert!(DagValidator::validate(&dag, ValidateOpts::default()).is_ok());
        for n in dag.nodes.values() {
            assert!(n.cache_key.is_none());
            assert_eq!(n.state, NodeState::Pending);
        }
    }

    #[test]
    fn repair_local_diagnostic_topology() {
        let m = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
        let ids = allocate_ids(m);
        let mut input_refs = BTreeMap::new();
        for nid in ids.nodes.values() {
            input_refs.insert(*nid, ArtifactId::new());
        }
        let dag = build_topology(BuildTopology {
            manifest: m,
            dag_id: DagId::new(),
            session_id: SessionId::new(),
            generation: 1,
            ids: &ids,
            input_refs: &input_refs,
        });

        // Reverse map node id → template name
        let name_of: BTreeMap<NodeId, &str> =
            ids.nodes.iter().map(|(n, id)| (*id, n.as_str())).collect();

        let kinds: BTreeMap<&str, NodeKind> = dag
            .nodes
            .values()
            .map(|n| (name_of[&n.id], n.kind))
            .collect();
        assert_eq!(kinds["analyze"], NodeKind::Analyze);
        assert_eq!(kinds["edit"], NodeKind::Edit);
        assert_eq!(kinds["verify"], NodeKind::VerifyCompile);
        assert_eq!(kinds["gate"], NodeKind::GateHuman);

        assert_eq!(
            dag.nodes
                .values()
                .find(|n| n.kind == NodeKind::Analyze)
                .unwrap()
                .capability
                .as_ref()
                .unwrap()
                .as_str(),
            "repair"
        );
        assert_eq!(
            dag.nodes
                .values()
                .find(|n| n.kind == NodeKind::Edit)
                .unwrap()
                .capability
                .as_ref()
                .unwrap()
                .as_str(),
            "edit"
        );

        let analyze = dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Analyze)
            .unwrap();
        assert_eq!(analyze.retry.max_attempts, 2);
        assert_eq!(analyze.timeout_ms, 300_000);
        assert_eq!(analyze.budget.max_input, 32768);

        let gate = dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::GateHuman)
            .unwrap();
        assert_eq!(
            gate.approval.as_ref().unwrap().reason,
            "Approve repair diff before completion"
        );
        assert_eq!(gate.timeout_ms, 3_600_000);

        // Edge multiset by template name
        let mut edge_names: Vec<(&str, &str, EdgeKind)> = dag
            .edges
            .iter()
            .map(|e| (name_of[&e.from], name_of[&e.to], e.kind))
            .collect();
        edge_names.sort_by(|a, b| {
            (a.0, a.1, format!("{:?}", a.2)).cmp(&(b.0, b.1, format!("{:?}", b.2)))
        });
        assert_eq!(
            edge_names,
            vec![
                ("analyze", "edit", EdgeKind::Data),
                ("analyze", "edit", EdgeKind::Sequence),
                ("edit", "verify", EdgeKind::Data),
                ("edit", "verify", EdgeKind::Sequence),
                ("verify", "gate", EdgeKind::Data),
                ("verify", "gate", EdgeKind::Sequence),
            ]
        );
    }
}
