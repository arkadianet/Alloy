//! Domain identities, requests, handles and the WorkingSet payload
//! (RFC-0012 §3.2, §3.4, §3.5).

use serde::{Deserialize, Serialize};

use crate::graph::{GraphEdge, GraphFidelity, GraphNode};
use crate::types::budget::TokenBudget;
use crate::types::diagnostic::{DiagnosticEvent, FailureIr};
use crate::types::ids::{
    ArtifactId, CapabilityId, CrateId, DiagnosticId, Digest, GraphVersion, NodeId, RunId, SessionId,
};

/// Context domain identity. All eight V2 §8.1 variants exist; exactly three
/// are live in MVP (rule D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DomainId {
    /// Goal, prior turns, approvals. **Live.**
    Conversation,
    /// Files + graph projection + diagnostics. **Live.**
    WorkingSet,
    /// Artifact metadata and selected bodies. **Live.**
    Artifacts,
    /// Reserved — empty (V2 §8.1 Deferred).
    Architecture,
    /// Reserved — empty.
    Scratchpad,
    /// Reserved — empty (no fuzzy-recall index; ADR F-23).
    LongTerm,
    /// Reserved — empty (the DAG is the plan; RFC-0009 owns it).
    Planning,
    /// Reserved serde-compat alias — empty; prefer `WorkingSet`.
    ProjectLegacyAlias,
}

impl DomainId {
    /// The three MVP-live domains, in assembly order (rule A2).
    pub const LIVE: [DomainId; 3] = [Self::Conversation, Self::WorkingSet, Self::Artifacts];

    /// All eight variants, for the manifest (rule CIT8).
    pub const ALL: [DomainId; 8] = [
        Self::Conversation,
        Self::WorkingSet,
        Self::Artifacts,
        Self::Architecture,
        Self::Scratchpad,
        Self::LongTerm,
        Self::Planning,
        Self::ProjectLegacyAlias,
    ];

    /// `true` for the three live domains only (rule D1).
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Conversation | Self::WorkingSet | Self::Artifacts
        )
    }

    /// Stable lowercase label used in citations and the manifest (§7.1).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::WorkingSet => "working_set",
            Self::Artifacts => "artifacts",
            Self::Architecture => "architecture",
            Self::Scratchpad => "scratchpad",
            Self::LongTerm => "long_term",
            Self::Planning => "planning",
            Self::ProjectLegacyAlias => "project_legacy_alias",
        }
    }
}

/// A caller-pinned item that MUST appear in the assembled pack (rule B11).
///
/// V2 §8.1 names `ContextHandle` in [`AssembleRequest`] but does not define
/// it; this shape is the normative fill-in (not an amendment — V2 left it
/// open).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextHandle {
    /// Workspace-relative file path, optionally line-bounded.
    File {
        /// Workspace-relative path with `/` separators (SEC4).
        path: String,
        /// Inclusive 1-based line range; `None` means the whole file.
        lines: Option<(u32, u32)>,
    },
    /// A stored artifact, included by body when textual, else by metadata.
    Artifact(ArtifactId),
    /// A recorded diagnostic.
    Diagnostic(DiagnosticId),
    /// A graph node, resolved via `GraphQuery::Symbol` on its path (D14);
    /// graph-unavailable resolution degrades per rule E11.
    Symbol {
        /// Rust path or workspace-relative file path (RFC-0011 Q2).
        path: String,
    },
}

/// Assembly request (V2 §8.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssembleRequest {
    /// Owning session.
    pub session: SessionId,
    /// DAG node this pack is for.
    pub node: NodeId,
    /// Capability that will consume the pack.
    pub capability: CapabilityId,
    /// Caller's ceiling in estimated input tokens (rule B1).
    pub token_budget: usize,
    /// Items that MUST be present or assembly fails (rule B11).
    pub must_include: Vec<ContextHandle>,
}

/// Compaction strategy. MVP accepts every variant and performs no work (A12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactStrategy {
    /// Drop the memoized projection so the next assemble rebuilds it.
    #[default]
    DropCache,
    /// Reserved: LLM summarization of the domain (V2 §8.1 Deferred).
    Summarize,
}

/// Eviction policy for memoized projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EvictPolicy {
    /// Evict everything.
    All,
    /// Evict entries whose `GraphVersion` differs from `current` (K1).
    StaleGraphVersion {
        /// The version to keep.
        current: GraphVersion,
    },
    /// Evict everything for one session.
    Session(SessionId),
    /// Evict down to at most `keep` entries, oldest-first (K4).
    Lru {
        /// Entries to retain.
        keep: usize,
    },
}

/// Outcome of an eviction pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EvictReport {
    /// Entries removed.
    pub evicted: u32,
    /// Entries retained.
    pub retained: u32,
    /// Estimated tokens freed (rule B2 estimator).
    pub freed_tokens_est: u64,
}

/// Why a projection was marked stale (V2 §20 R1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StaleReason {
    /// The graph advanced past the memoized `GraphVersion` (K1).
    GraphVersionChanged {
        /// Version the projection was built at.
        was: GraphVersion,
        /// Version observed now.
        now: GraphVersion,
    },
    /// A cited file's digest no longer matches (K2).
    ContentDigestChanged {
        /// Workspace-relative path (SEC4).
        path: String,
    },
    /// An edit transaction landed (RFC-0008).
    EditApplied,
    /// Operator or CLI request.
    Manual,
}

/// The V2 §8.1 WorkingSet domain payload: files + graph projection +
/// diagnostics. Every field independently degrades to empty (E2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct WorkingSet {
    /// Selected file excerpts, ordered by rule D8.
    pub files: Vec<FileExcerpt>,
    /// Graph projection; `None` when the graph was unavailable or empty (E2).
    pub graph: Option<GraphProjection>,
    /// Recorded diagnostics, ordered by rule D11.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Why the domain is absent or partial; empty when complete.
    pub degradations: Vec<Degradation>,
}

/// One bounded file excerpt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileExcerpt {
    /// Workspace-relative path with `/` separators (SEC4).
    pub path: String,
    /// Inclusive 1-based first line of `text`.
    pub start_line: u32,
    /// Redacted, fence-safe UTF-8 content (SEC2, SEC3, SEC8).
    pub text: String,
    /// SHA-256 of `text` exactly as rendered (CIT2).
    pub digest: Digest,
    /// `true` when lines were removed; a marker is present in `text` (B7).
    pub truncated: bool,
    /// Owning package when known.
    pub crate_id: Option<CrateId>,
}

/// The graph-derived slice of the WorkingSet (RFC-0011 consumer contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphProjection {
    /// Version the projection was read at; the memo key (K1).
    pub version: GraphVersion,
    /// Fidelity label, rendered as provenance only (CIT6, RFC-0011 E.1.2).
    pub fidelity: GraphFidelity,
    /// Seed nodes resolved from `must_include` and diagnostic spans (D9).
    pub seeds: Vec<GraphNode>,
    /// Neighbourhood nodes from `GraphQuery::Subgraph` (D10).
    pub neighbourhood: Vec<GraphNode>,
    /// Edges whose endpoints are both present, in RFC-0011 Q8 order.
    pub edges: Vec<GraphEdge>,
    /// `true` when RFC-0011 capped the view (`GraphView.truncated`) (B8).
    pub truncated: bool,
    /// Cross-file impact facts from bounded `Callers`/`Refs` queries
    /// (amendment A-0012-1b). Empty while the store's stubs return empty.
    pub impact: Vec<ImpactEntry>,
    /// Impact entries dropped by the `max_impact_nodes` cap; mirrored by a
    /// marker and a manifest counter (B7/B8).
    pub impact_omitted: usize,
}

/// One cross-file impact fact: a node that calls or references a seed
/// (amendment A-0012-1b).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImpactEntry {
    /// Canonical path of the anchor node the impact was queried for: the
    /// seed itself when the seed is an item, else an item node the seed
    /// module `Defines` (A-0012-1a — `Calls`/`References` edges anchor on
    /// item nodes, so module seeds are expanded before querying).
    pub seed_path: String,
    /// How [`ImpactEntry::node`] relates to the seed.
    pub relation: ImpactRelation,
    /// The impacting node, exactly as the graph returned it.
    pub node: GraphNode,
}

/// How an impact node relates to its seed (amendment A-0012-1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImpactRelation {
    /// The node calls the seed (`GraphQuery::Callers`).
    Caller,
    /// The node references the seed (`GraphQuery::Refs`).
    Reference,
}

impl ImpactRelation {
    /// Stable relation-line verb rendered in the graph fence (A-0012-1b).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Caller => "calls",
            Self::Reference => "refs",
        }
    }
}

/// A named, honest degradation of a domain (E3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Degradation {
    /// Affected domain.
    pub domain: DomainId,
    /// Stable machine-readable reason.
    pub reason: DegradationReason,
    /// Redacted human detail, bounded to 200 bytes.
    pub detail: String,
}

/// Why a domain is incomplete (E3). Never an error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DegradationReason {
    /// `GraphError::Disabled`. (A null graph read succeeds empty —
    /// RFC-0011 Q10 — and therefore maps to `GraphEmpty`, not here.)
    GraphDisabled,
    /// `GraphError::Busy` after one retry (E4).
    GraphBusy,
    /// `GraphError::Corrupt` / `Migration` / `Io` / `Internal` / others.
    GraphUnavailable,
    /// The query succeeded and returned nothing.
    GraphEmpty,
    /// The store returned `StoreError`.
    StoreUnavailable,
    /// A file listed for inclusion could not be read.
    FileUnreadable,
    /// The domain's budget allowance was exhausted (B6).
    BudgetExhausted,
    /// The item was not UTF-8 or tripped the binary guard (D7).
    NotTextual,
}

impl DegradationReason {
    /// Stable snake_case wire label (matches the serde rename).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::GraphDisabled => "graph_disabled",
            Self::GraphBusy => "graph_busy",
            Self::GraphUnavailable => "graph_unavailable",
            Self::GraphEmpty => "graph_empty",
            Self::StoreUnavailable => "store_unavailable",
            Self::FileUnreadable => "file_unreadable",
            Self::BudgetExhausted => "budget_exhausted",
            Self::NotTextual => "not_textual",
        }
    }
}

/// Non-V2 per-call inputs the host already holds. Kept off [`AssembleRequest`]
/// so the V2 struct stays verbatim (C4).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct AssembleInputs {
    /// Run attribution, when the call is inside a run.
    pub run: Option<RunId>,
    /// The node's input envelope (RFC-0009); supplies the `Goal` for D3.
    pub input: Option<crate::dag::NodeInputEnvelope>,
    /// Diagnostics already in hand for this attempt (RFC-0010 `FailureIr`).
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Per-node budget from `CapabilityExecContext` (rule B1).
    pub budget: Option<TokenBudget>,
    /// Files the caller knows are in play (edit targets, diagnostic paths).
    pub focus_paths: Vec<String>,
    /// Terminal failure of this node's previous scheduler attempt
    /// (RFC-0010 `FailureIr`; its `notes` are already redacted and bounded
    /// by RFC-0013 FM15 at production). MUST be `None` on first attempts
    /// and whenever the prior outcome was not captured — a process
    /// restart, a killed attempt — so absence always reads "unknown",
    /// never "no problems". Rendered by the engine as one bounded
    /// `conversation:prior_failure` section carrying the class and notes
    /// only; the carried `diagnostics` are NOT rendered (live diagnostics
    /// arrive via [`AssembleInputs::diagnostics`]). The engine composes the
    /// same section from the run's newest GN13 rollback note in the event
    /// log (an `error` event with class `rollback`) even when this field is
    /// `None`, so a fresh generation-N+1 node still learns what the
    /// rolled-back edit broke.
    pub prior_failure: Option<FailureIr>,
}
