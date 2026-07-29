//! ProjectGraph seam (RFC-0011, Architecture V2 §7).
//!
//! This module is types + trait + [`NullProjectGraph`] + [`GraphViewHandle`]
//! only (rule C4): the SQLite store, ingest and query engine live in
//! `alloy-index`, which depends on this crate — never the other way around
//! (rule C2). Workers see the graph exclusively through the read-only
//! [`GraphViewHandle`] (rule SEC1); writes are ingest-only and reach the
//! trait through the host/CLI composition root (rules IN1, SEC4).

mod handle;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest as Sha2Digest, Sha256};

pub use handle::GraphViewHandle;

use crate::storage::StoreError;
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{
    ArtifactId, CrateId, DiagnosticId, Digest, GraphNodeId, GraphSnapshotId, GraphVersion,
    Timestamp, TransactionId,
};

// ---------------------------------------------------------------------
// Node and edge model (§3.2)
// ---------------------------------------------------------------------

/// Kind of a project-graph node (Architecture V2 §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphNodeKind {
    /// The cargo workspace root.
    Workspace,
    /// A cargo package inside the workspace.
    Crate,
    /// A Rust module inferred from source-file layout.
    Module,
    /// A named module-level item (fn/struct/enum/union/trait/type/const/
    /// static). Ingested by the RFC-0014 `syn` deep pass (SY3); `impl`
    /// blocks stay deferred (SY5).
    Item,
}

impl GraphNodeKind {
    /// Stable wire tag, used both by serde and by [`derive_node_id`]'s
    /// domain separation (G3).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Crate => "crate",
            Self::Module => "module",
            Self::Item => "item",
        }
    }
}

/// Kind of a project-graph edge (Architecture V2 §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphEdgeKind {
    /// Structural containment: workspace→crate, crate→module, module→module,
    /// module→item.
    Defines,
    /// `use` relationship, written only for in-workspace targets. Ingested
    /// by the RFC-0014 `syn` deep pass (SY11–SY13).
    Imports,
}

impl GraphEdgeKind {
    /// Stable wire tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Defines => "defines",
            Self::Imports => "imports",
        }
    }
}

/// How much of the graph is derived from real parsing (V2 §20 R16 degraded
/// mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphFidelity {
    /// Manifest + file-layout facts only. The MVP value.
    Manifest,
    /// `syn` item-level parse (RFC-0014 deep pass, `model_version = 2`).
    SynDeep,
    /// Reserved: rust-analyzer passthrough (Beta/M3).
    Analyzer,
}

/// A node as returned by a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Deterministic id (G3).
    pub id: GraphNodeId,
    /// Node kind.
    pub kind: GraphNodeKind,
    /// Canonical stable path (G4), e.g. `my_crate::io::reader`.
    pub path: String,
    /// Owning package, `None` for `Workspace`.
    pub crate_id: Option<CrateId>,
    /// Workspace-relative primary source file, when the node has one.
    pub file: Option<String>,
    /// SHA-256 of `file`'s bytes at ingest time, when `file` is `Some`.
    pub digest: Option<Digest>,
}

/// An edge as returned by a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source node.
    pub from: GraphNodeId,
    /// Target node.
    pub to: GraphNodeId,
    /// Edge kind.
    pub kind: GraphEdgeKind,
    /// Reserved confidence; MVP always `1.0` (S6).
    pub confidence: f32,
}

// ---------------------------------------------------------------------
// Deterministic node ids (G3/G4)
// ---------------------------------------------------------------------

/// Derive the deterministic [`GraphNodeId`] for `(kind, stable_key)` (G3).
///
/// `sha256("alloyg1\0" ‖ kind_tag ‖ "\0" ‖ stable_key)`, first 16 digest
/// bytes, UUID version nibble forced to `8`, RFC-4122 variant bits forced to
/// `0b10`, formatted `8-4-4-4-12` lowercase hex. Implementations of
/// [`ProjectGraph`] MUST use this and MUST NOT call `GraphNodeId::new()`.
///
/// `stable_key` must already be workspace-relative and platform-independent
/// (G4): path separators normalised to `/` before calling.
#[must_use]
pub fn derive_node_id(kind: GraphNodeKind, stable_key: &str) -> GraphNodeId {
    let mut hasher = Sha256::new();
    hasher.update(b"alloyg1\0");
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(stable_key.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80; // version nibble = 8
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant = 0b10

    let s = format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[0..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..16]),
    );
    GraphNodeId::parse(&s).expect("derived uuid string is always canonical")
}

// ---------------------------------------------------------------------
// Queries and views (§3.3, §3.4)
// ---------------------------------------------------------------------

/// Read queries. **Frozen**: exactly the seven variants of Architecture V2
/// §7.2 (Q1). New capability is added by populating existing variants, not
/// by adding variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphQuery {
    /// Resolve a Rust path (`my_crate::io`) or a workspace-relative file
    /// path (Q2).
    Symbol {
        /// Exact Rust path or workspace-relative file path.
        path: String,
    },
    /// References to a node. **Stub**: empty until RA passthrough (Q4).
    Refs {
        /// Node whose references are requested.
        node: GraphNodeId,
    },
    /// Impls of a trait node. **Stub**: empty until RA passthrough (Q4).
    Impls {
        /// Trait node.
        trait_node: GraphNodeId,
    },
    /// Callers of a fn node. **Stub**: always empty (Q5).
    Callers {
        /// Function node.
        fn_node: GraphNodeId,
    },
    /// Recorded diagnostics, optionally scoped and time-filtered (Q3).
    Diagnostics {
        /// Restrict to one package.
        crate_id: Option<CrateId>,
        /// Restrict to diagnostics recorded at or after this instant.
        since: Option<Timestamp>,
    },
    /// Fixes recorded for a diagnostic code, most recent first. Live since
    /// amendment A-0011-5; empty until something has been recorded (Q6).
    SimilarFixes {
        /// Diagnostic code the fix addressed (`E0502`, …).
        diagnostic_code: String,
        /// Maximum rows requested.
        limit: usize,
    },
    /// Breadth-first neighbourhood around seeds (Q7).
    Subgraph {
        /// Seed nodes; unknown ids are ignored.
        seeds: Vec<GraphNodeId>,
        /// BFS radius, clamped to 3.
        radius: u8,
    },
}

/// Result of a query. Always well-formed, possibly empty, never fabricated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphView {
    /// Graph version this view was read at.
    pub version: GraphVersion,
    /// Matched nodes, sorted by (kind, path, id) — Q8.
    pub nodes: Vec<GraphNode>,
    /// Edges whose endpoints are both in `nodes`, sorted by (from, to, kind)
    /// — Q8.
    pub edges: Vec<GraphEdge>,
    /// Diagnostics, populated only by [`GraphQuery::Diagnostics`].
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Fix records, populated only by [`GraphQuery::SimilarFixes`].
    pub fixes: Vec<FixEvent>,
    /// Fidelity of the data backing this view (MVP: always `Manifest`).
    pub fidelity: GraphFidelity,
    /// `true` when the query kind is a Stub or the result was capped (Q9).
    pub truncated: bool,
}

impl GraphView {
    /// An empty view at `version` with [`GraphFidelity::Manifest`].
    #[must_use]
    pub fn empty(version: GraphVersion) -> Self {
        Self {
            version,
            nodes: Vec::new(),
            edges: Vec::new(),
            diagnostics: Vec::new(),
            fixes: Vec::new(),
            fidelity: GraphFidelity::Manifest,
            truncated: false,
        }
    }

    /// `true` when no nodes, edges, diagnostics or fixes were returned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.edges.is_empty()
            && self.diagnostics.is_empty()
            && self.fixes.is_empty()
    }
}

// ---------------------------------------------------------------------
// Ingest-only write types (§3.6)
// ---------------------------------------------------------------------

/// A file-level change fed to `apply_incremental`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Workspace-relative path with `/` separators (IN11).
    pub path: String,
    /// What happened to it.
    pub kind: FileChangeKind,
}

/// Change classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// File appeared.
    Created,
    /// Contents changed.
    Modified,
    /// File removed.
    Deleted,
}

/// A successfully applied fix, recorded for later (deferred) retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixEvent {
    /// Diagnostic this fix addressed, when known.
    pub diagnostic: Option<DiagnosticId>,
    /// Diagnostic code the fix addressed (`E0502`, …) — the `SimilarFixes`
    /// lookup key.
    pub diagnostic_code: Option<String>,
    /// Owning package, when known.
    pub crate_id: Option<CrateId>,
    /// EditEngine transaction that applied it (RFC-0008), when known.
    pub transaction: Option<TransactionId>,
    /// Content-addressed patch artifact (RFC-0002 CAS), when stored.
    pub patch_artifact: Option<ArtifactId>,
    /// Whether the post-edit verify succeeded.
    pub verified: bool,
    /// Ingest timestamp.
    pub recorded_at: Timestamp,
}

/// Outcome summary of an ingest pass (returned via `rebuild_reported`,
/// logged by the caller — OB5). Constructed by `alloy-index`, so not
/// `#[non_exhaustive]` — adding a field is an API change by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// Version after the pass.
    pub version: GraphVersion,
    /// `true` when nothing changed and the version was not bumped (IN6).
    pub unchanged: bool,
    /// Crate nodes written.
    pub crates: u32,
    /// Module nodes written.
    pub modules: u32,
    /// Item nodes written (RFC-0014 amendment A-0014-3).
    pub items: u32,
    /// Imports edges written (RFC-0014 amendment A-0014-3).
    pub imports: u32,
    /// Files tracked for digest invalidation.
    pub files: u32,
    /// Files skipped by a cap or a skip rule (IN3).
    pub skipped: u32,
    /// Manifest-level problems that did not abort the pass (IN12).
    pub warnings: Vec<String>,
    /// Where the facts came from (MVP: `Manifest`).
    pub source: GraphFidelity,
}

// ---------------------------------------------------------------------
// Errors (§3.7, §9)
// ---------------------------------------------------------------------

/// ProjectGraph failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// Node, snapshot, or row not present.
    #[error("not found: {0}")]
    NotFound(String),
    /// Workspace root missing, unreadable, or not a cargo workspace.
    #[error("workspace: {0}")]
    Workspace(String),
    /// Manifest could not be parsed.
    #[error("manifest {path}: {reason}")]
    Manifest {
        /// Workspace-relative manifest path.
        path: String,
        /// Parse failure reason.
        reason: String,
    },
    /// Ingest exceeded a configured cap.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// Query argument rejected before touching storage.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// Store is busy after the configured timeout.
    #[error("busy")]
    Busy,
    /// Filesystem or SQLite I/O.
    #[error("io: {0}")]
    Io(String),
    /// Graph data is unusable; caller should rebuild (§5.7).
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Schema migration failure or refuse-newer.
    #[error("migration: {0}")]
    Migration(String),
    /// The graph has been closed.
    #[error("closed")]
    Closed,
    /// This implementation does not provide a graph (`NullProjectGraph`
    /// writes).
    #[error("graph disabled")]
    Disabled,
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StoreError> for GraphError {
    fn from(e: StoreError) -> Self {
        // §9.2 table. Conflict maps to Internal: a constraint violation in a
        // single-writer derived cache is a bug, not contention.
        match e {
            StoreError::NotFound(s) => GraphError::NotFound(s),
            StoreError::Conflict(s) => GraphError::Internal(s),
            StoreError::Corrupt(s) => GraphError::Corrupt(s),
            StoreError::Migration(s) => GraphError::Migration(s),
            StoreError::Busy => GraphError::Busy,
            StoreError::Io(s) => GraphError::Io(s),
            StoreError::DigestMismatch => GraphError::Corrupt("digest mismatch".into()),
            StoreError::Closed => GraphError::Closed,
            StoreError::Internal(s) => GraphError::Internal(s),
        }
    }
}

// ---------------------------------------------------------------------
// The trait (§3.5, V2 §7.2 verbatim)
// ---------------------------------------------------------------------

/// Persistent project model. Exactly one writer per data directory (X1).
#[async_trait]
pub trait ProjectGraph: Send + Sync {
    /// Full ingest of `root`. Deterministic and idempotent (IN5, IN6).
    async fn rebuild(&self, root: &Path) -> Result<GraphVersion, GraphError>;

    /// Apply file-level changes. Empty slice is a no-op returning the
    /// current version.
    async fn apply_incremental(&self, changes: &[FileChange]) -> Result<GraphVersion, GraphError>;

    /// Read query. MUST NOT mutate persistent state (Q10).
    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError>;

    /// Ingest one compiler/tool diagnostic (IN13).
    async fn record_diagnostic(&self, d: DiagnosticEvent) -> Result<(), GraphError>;

    /// Drop every recorded diagnostic; returns how many were removed.
    ///
    /// A full check of the workspace supersedes all prior diagnostics —
    /// the pre-plan seed calls this so retries never prompt the model with
    /// already-fixed errors (dogfood, 2026-07-29). Default no-op for
    /// stores without diagnostic persistence.
    async fn clear_diagnostics(&self) -> Result<u64, GraphError> {
        Ok(0)
    }

    /// Ingest one applied-fix record (IN14).
    async fn record_fix(&self, f: FixEvent) -> Result<(), GraphError>;

    /// Record an immutable marker of the current version (§4.7).
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError>;

    /// Current version without running a query. Default: `Ok(GraphVersion(0))`.
    async fn version(&self) -> Result<GraphVersion, GraphError> {
        Ok(GraphVersion(0))
    }
}

// ---------------------------------------------------------------------
// NullProjectGraph (§3.9)
// ---------------------------------------------------------------------

/// Graph that stores nothing. Mirrors `NullScheduler`'s role from RFC-0001.
///
/// Rule Q10: reads succeed empty (a context assembler never fails because
/// the graph is off); writes fail loudly with [`GraphError::Disabled`] (a
/// mis-wired ingest is not silently swallowed).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProjectGraph;

#[async_trait]
impl ProjectGraph for NullProjectGraph {
    async fn rebuild(&self, _root: &Path) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn apply_incremental(&self, _changes: &[FileChange]) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn query(&self, _q: GraphQuery) -> Result<GraphView, GraphError> {
        Ok(GraphView::empty(GraphVersion(0)))
    }
    async fn record_diagnostic(&self, _d: DiagnosticEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }
    async fn record_fix(&self, _f: FixEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn version(&self) -> Result<GraphVersion, GraphError> {
        Ok(GraphVersion(0))
    }
}

/// Shared constructor used by [`GraphViewHandle::null`].
pub(crate) fn null_graph_arc() -> Arc<dyn ProjectGraph> {
    Arc::new(NullProjectGraph)
}

#[cfg(test)]
mod tests {
    use super::*;

    // T1a — G3: same (kind, key) → same id across calls/processes (pure fn).
    #[test]
    fn node_id_is_deterministic_for_the_same_key() {
        let a = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");
        let b = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");
        assert_eq!(a, b);
        let c = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::util");
        assert_ne!(a, c);
    }

    // T1b — G3 domain separation across kinds.
    #[test]
    fn node_id_differs_across_kinds_with_the_same_key() {
        let key = "toy-core\0same_key";
        let module = derive_node_id(GraphNodeKind::Module, key);
        let item = derive_node_id(GraphNodeKind::Item, key);
        let krate = derive_node_id(GraphNodeKind::Crate, key);
        assert_ne!(module, item);
        assert_ne!(module, krate);
        assert_ne!(item, krate);
    }

    // T1c — G3 formatting: version nibble 8, RFC-4122 variant.
    #[test]
    fn node_id_has_uuid_version_eight_and_rfc4122_variant() {
        let id = derive_node_id(GraphNodeKind::Workspace, ".");
        let s = id.to_string();
        let chars: Vec<char> = s.chars().collect();
        // 8-4-4-4-12: version nibble is position 14, variant nibble position 19.
        assert_eq!(chars[14], '8', "version nibble must be 8: {s}");
        assert!(
            matches!(chars[19], '8' | '9' | 'a' | 'b'),
            "variant must be RFC-4122 (10xx): {s}"
        );
        assert_eq!(s, s.to_lowercase());
    }

    // T1d — §3.4.
    #[test]
    fn graph_view_empty_is_empty_and_manifest_fidelity() {
        let v = GraphView::empty(GraphVersion(7));
        assert!(v.is_empty());
        assert_eq!(v.version, GraphVersion(7));
        assert_eq!(v.fidelity, GraphFidelity::Manifest);
        assert!(!v.truncated);
    }

    // T1e — Q10: reads empty, writes Disabled.
    #[tokio::test]
    async fn null_graph_reads_empty_and_writes_disabled() {
        let g = NullProjectGraph;
        let view = g
            .query(GraphQuery::Symbol { path: "x".into() })
            .await
            .unwrap();
        assert!(view.is_empty());
        assert!(matches!(
            g.rebuild(Path::new("/nowhere")).await,
            Err(GraphError::Disabled)
        ));
        assert!(matches!(
            g.apply_incremental(&[]).await,
            Err(GraphError::Disabled)
        ));
        assert!(matches!(g.snapshot().await, Err(GraphError::Disabled)));
        assert!(matches!(
            g.record_fix(FixEvent {
                diagnostic: None,
                diagnostic_code: None,
                crate_id: None,
                transaction: None,
                patch_artifact: None,
                verified: false,
                recorded_at: Timestamp::now(),
            })
            .await,
            Err(GraphError::Disabled)
        ));
        assert_eq!(g.version().await.unwrap(), GraphVersion(0));
    }

    // T1f — Q1: all seven variants serde round-trip.
    #[test]
    fn graph_query_serde_round_trip_covers_all_seven_variants() {
        let node = derive_node_id(GraphNodeKind::Module, "k");
        let queries = vec![
            GraphQuery::Symbol {
                path: "a::b".into(),
            },
            GraphQuery::Refs { node },
            GraphQuery::Impls { trait_node: node },
            GraphQuery::Callers { fn_node: node },
            GraphQuery::Diagnostics {
                crate_id: Some(CrateId::new("toy-core").unwrap()),
                since: None,
            },
            GraphQuery::SimilarFixes {
                diagnostic_code: "E0502".into(),
                limit: 5,
            },
            GraphQuery::Subgraph {
                seeds: vec![node],
                radius: 2,
            },
        ];
        assert_eq!(queries.len(), 7, "Q1: exactly seven variants");
        for q in queries {
            let json = serde_json::to_string(&q).unwrap();
            let back: GraphQuery = serde_json::from_str(&json).unwrap();
            assert_eq!(q, back);
        }
    }

    // T1g — §9.2: exhaustive StoreError mapping.
    #[test]
    fn graph_error_maps_every_store_error_variant() {
        type Check = fn(&GraphError) -> bool;
        let cases: Vec<(StoreError, Check)> = vec![
            (StoreError::NotFound("x".into()), |e| {
                matches!(e, GraphError::NotFound(_))
            }),
            (StoreError::Conflict("x".into()), |e| {
                matches!(e, GraphError::Internal(_))
            }),
            (StoreError::Corrupt("x".into()), |e| {
                matches!(e, GraphError::Corrupt(_))
            }),
            (StoreError::Migration("x".into()), |e| {
                matches!(e, GraphError::Migration(_))
            }),
            (StoreError::Busy, |e| matches!(e, GraphError::Busy)),
            (StoreError::Io("x".into()), |e| {
                matches!(e, GraphError::Io(_))
            }),
            (StoreError::DigestMismatch, |e| {
                matches!(e, GraphError::Corrupt(_))
            }),
            (StoreError::Closed, |e| matches!(e, GraphError::Closed)),
            (StoreError::Internal("x".into()), |e| {
                matches!(e, GraphError::Internal(_))
            }),
        ];
        for (input, check) in cases {
            let got = GraphError::from(input);
            assert!(check(&got), "unexpected mapping: {got:?}");
        }
    }
}
