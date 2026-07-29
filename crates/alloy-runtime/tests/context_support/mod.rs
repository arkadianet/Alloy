//! Shared fixtures for the RFC-0012 context tests: deterministic in-memory
//! stores, a scripted graph, and the RFC-0011 Appendix B toy workspace.

#![allow(dead_code)] // each test crate uses a different subset

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use alloy_runtime::events::{
    EventSink, EventSinkError, HandoffSnapshot, NewSessionEvent, RuntimeEvent, SessionEvent,
    SessionEventType,
};
use alloy_runtime::graph::{
    derive_node_id, FileChange, FixEvent, GraphEdge, GraphEdgeKind, GraphError, GraphFidelity,
    GraphNode, GraphNodeKind, GraphQuery, GraphView, GraphViewHandle, ProjectGraph,
};
use alloy_runtime::storage::{
    ArtifactBlob, ArtifactKind, ArtifactMeta, ArtifactPut, ArtifactStore, EventStore, StoreError,
};
use alloy_runtime::types::diagnostic::{DiagnosticEvent, DiagnosticLevel, SpanRef};
use alloy_runtime::types::ids::{
    ArtifactId, CapabilityId, DiagnosticId, Digest, EventSeq, GraphSnapshotId, GraphVersion,
    NodeId, RunId, SessionId, Timestamp,
};
use alloy_runtime::{AssembleRequest, ContextHandle};

// ---------------------------------------------------------------------
// Fixed identities: golden bytes must not depend on random UUIDs.
// ---------------------------------------------------------------------

pub fn fixed_session() -> SessionId {
    SessionId::parse("00000000-0000-4000-8000-000000000001").unwrap()
}

pub fn fixed_node() -> NodeId {
    NodeId::parse("00000000-0000-4000-8000-000000000002").unwrap()
}

pub fn fixed_run() -> RunId {
    RunId::parse("00000000-0000-4000-8000-000000000003").unwrap()
}

pub fn fixed_diagnostic_id() -> DiagnosticId {
    DiagnosticId::parse("00000000-0000-4000-8000-00000000000d").unwrap()
}

pub fn fixed_artifact_id(n: u8) -> ArtifactId {
    ArtifactId::parse(&format!("00000000-0000-4000-8000-0000000000a{n}")).unwrap()
}

pub fn fixed_timestamp() -> Timestamp {
    Timestamp(time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap())
}

pub fn repair_request(budget: usize, must_include: Vec<ContextHandle>) -> AssembleRequest {
    AssembleRequest {
        session: fixed_session(),
        node: fixed_node(),
        capability: CapabilityId::new("repair").unwrap(),
        token_budget: budget,
        must_include,
    }
}

// ---------------------------------------------------------------------
// In-memory EventStore
// ---------------------------------------------------------------------

/// Deterministic in-memory event store. `fail` makes every read error.
#[derive(Default)]
pub struct MemEventStore {
    pub events: Mutex<Vec<SessionEvent>>,
    pub fail: bool,
}

impl MemEventStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    pub fn push(&self, session: SessionId, type_: SessionEventType, payload: serde_json::Value) {
        let mut events = self.events.lock().unwrap();
        let seq = EventSeq(events.len() as u64);
        events.push(SessionEvent {
            seq,
            ts: fixed_timestamp(),
            session_id: session,
            run_id: None,
            type_,
            payload,
        });
    }
}

#[async_trait]
impl EventSink for MemEventStore {
    async fn append_runtime(&self, _ev: RuntimeEvent) -> Result<(), EventSinkError> {
        Ok(())
    }

    async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, EventSinkError> {
        self.push(ev.session_id, ev.type_, ev.payload);
        let events = self.events.lock().unwrap();
        Ok(EventSeq(events.len() as u64 - 1))
    }
}

#[async_trait]
impl EventStore for MemEventStore {
    async fn list_session_events(
        &self,
        session: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if self.fail {
            return Err(StoreError::Io("event store down".into()));
        }
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .filter(|e| e.session_id == session)
            .filter(|e| after.is_none_or(|a| e.seq > a))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn replay_session<F>(
        &self,
        session: SessionId,
        mut on_event: F,
    ) -> Result<Option<EventSeq>, StoreError>
    where
        F: FnMut(&SessionEvent) -> Result<(), StoreError> + Send,
    {
        let events = self.events.lock().unwrap().clone();
        let mut last = None;
        for e in events.iter().filter(|e| e.session_id == session) {
            on_event(e)?;
            last = Some(e.seq);
        }
        Ok(last)
    }

    async fn last_seq(&self, session: SessionId) -> Result<Option<EventSeq>, StoreError> {
        if self.fail {
            return Err(StoreError::Io("event store down".into()));
        }
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .filter(|e| e.session_id == session)
            .map(|e| e.seq)
            .max())
    }

    async fn list_runtime_events(
        &self,
        _after_rowid: Option<i64>,
        _limit: usize,
    ) -> Result<Vec<(i64, RuntimeEvent)>, StoreError> {
        Ok(Vec::new())
    }

    async fn has_session_event_for_run(
        &self,
        _session: SessionId,
        _run: RunId,
        _type_: SessionEventType,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn has_run_accepted_event(&self, _run: RunId) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn has_run_finished_event(&self, _run: RunId) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn import_handoff_snapshot(&self, _snap: HandoffSnapshot) -> Result<(), StoreError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------
// In-memory ArtifactStore
// ---------------------------------------------------------------------

#[derive(Default)]
pub struct MemArtifactStore {
    pub blobs: Mutex<Vec<ArtifactBlob>>,
    pub fail: bool,
}

impl MemArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_at(
        &self,
        id: ArtifactId,
        kind: ArtifactKind,
        bytes: &[u8],
        created_at: Timestamp,
    ) {
        let meta = ArtifactMeta {
            kind,
            content_type: None,
            byte_len: bytes.len() as u64,
            digest: Digest::sha256(bytes),
            created_at,
            session_id: Some(fixed_session()),
            run_id: None,
            labels: serde_json::Map::new(),
        };
        self.blobs.lock().unwrap().push(ArtifactBlob {
            id,
            meta,
            bytes: bytes.to_vec(),
        });
    }

    pub fn insert(&self, id: ArtifactId, kind: ArtifactKind, bytes: &[u8]) {
        let meta = ArtifactMeta {
            kind,
            content_type: None,
            byte_len: bytes.len() as u64,
            digest: Digest::sha256(bytes),
            created_at: fixed_timestamp(),
            session_id: Some(fixed_session()),
            run_id: None,
            labels: serde_json::Map::new(),
        };
        self.blobs.lock().unwrap().push(ArtifactBlob {
            id,
            meta,
            bytes: bytes.to_vec(),
        });
    }
}

#[async_trait]
impl ArtifactStore for MemArtifactStore {
    async fn put(&self, _req: ArtifactPut) -> Result<ArtifactId, StoreError> {
        Err(StoreError::Internal("read-only fixture".into()))
    }

    async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
        if self.fail {
            return Err(StoreError::Io("artifact store down".into()));
        }
        self.blobs
            .lock()
            .unwrap()
            .iter()
            .find(|b| b.id == id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
        if self.fail {
            return Err(StoreError::Io("artifact store down".into()));
        }
        self.blobs
            .lock()
            .unwrap()
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.meta.clone())
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    async fn get_by_digest(&self, digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
        if self.fail {
            return Err(StoreError::Io("artifact store down".into()));
        }
        Ok(self
            .blobs
            .lock()
            .unwrap()
            .iter()
            .find(|b| &b.meta.digest == digest)
            .map(|b| b.id))
    }

    async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
        Err(StoreError::Internal("read-only fixture".into()))
    }
}

// ---------------------------------------------------------------------
// Scripted graph
// ---------------------------------------------------------------------

/// Per-call behaviour of the scripted graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMode {
    /// Serve the Appendix B toy view.
    Toy,
    /// Every query returns an empty view.
    Empty,
    /// Every query fails `Busy` once, then serves the toy view.
    BusyOnce,
    /// Every query fails `Busy` forever.
    BusyAlways,
    /// Every query fails `Corrupt`.
    Corrupt,
    /// Every query fails `Disabled`.
    Disabled,
    /// `version()` fails; queries serve the toy view.
    VersionFails,
}

/// Recording, scripted `ProjectGraph` test double.
pub struct ScriptedGraph {
    pub mode: Mutex<GraphMode>,
    pub version: Mutex<u64>,
    pub queries: Mutex<Vec<GraphQuery>>,
    busy_burned: Mutex<bool>,
    pub diagnostics: Mutex<Vec<DiagnosticEvent>>,
    /// When set, every served view reports `truncated = true` (Q9).
    pub truncated: Mutex<bool>,
    /// When set, the double serves the post-A-0011-6 store shape (the
    /// `feat/graph-refs-impls-callers` branch): the `Subgraph` view carries
    /// the `toy_core::io::read_all` **Item** node via a `Defines` edge, and
    /// `Callers`/`Refs` serve populated views **only when anchored on that
    /// item node's id** — never on a module id, because the real deep pass
    /// anchors `Calls`/`References` edges exclusively on item nodes
    /// (`alloy-index/src/lang/rust/pass.rs`: module-level `fn` items are
    /// "the only admissible `Calls` targets"), and the real read path
    /// answers `Callers`/`Refs` with `to_id = anchor` edge lookups
    /// (`alloy-index/src/query.rs::neighbours`). Unset mirrors the M7
    /// store, whose `Callers`/`Refs` stubs return empty and whose manifest
    /// pass emits no item nodes.
    pub impact: Mutex<bool>,
    /// When set, every `Callers`/`Refs` query fails with `Io`.
    pub fail_impact: Mutex<bool>,
}

impl ScriptedGraph {
    pub fn new(mode: GraphMode) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            version: Mutex::new(1),
            queries: Mutex::new(Vec::new()),
            busy_burned: Mutex::new(false),
            diagnostics: Mutex::new(Vec::new()),
            truncated: Mutex::new(false),
            impact: Mutex::new(false),
            fail_impact: Mutex::new(false),
        })
    }

    /// Enable populated `Callers`/`Refs` views (the post-Beta store shape).
    pub fn set_impact(&self, on: bool) {
        *self.impact.lock().unwrap() = on;
    }

    /// Make every `Callers`/`Refs` query fail with `GraphError::Io`.
    pub fn set_fail_impact(&self, on: bool) {
        *self.fail_impact.lock().unwrap() = on;
    }

    /// The deterministic id of the `toy_core::io` module node.
    pub fn io_node_id() -> alloy_runtime::types::ids::GraphNodeId {
        derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io")
    }

    /// The `toy_core::io::read_all` **Item** node: the only node kind the
    /// real store's `Calls`/`References` edges ever anchor on.
    pub fn read_all_node() -> GraphNode {
        GraphNode {
            id: derive_node_id(GraphNodeKind::Item, "toy-core\0toy_core::io::read_all"),
            kind: GraphNodeKind::Item,
            path: "toy_core::io::read_all".to_owned(),
            crate_id: Some(alloy_runtime::CrateId::new("toy-core").unwrap()),
            file: Some("crates/toy-core/src/io.rs".to_owned()),
            digest: None,
        }
    }

    /// The out-of-crate caller node served for `Callers(toy_core::io)`.
    pub fn caller_node() -> GraphNode {
        GraphNode {
            id: derive_node_id(GraphNodeKind::Item, "toy-cli\0toy_cli::main"),
            kind: GraphNodeKind::Item,
            path: "toy_cli::main".to_owned(),
            crate_id: Some(alloy_runtime::CrateId::new("toy-cli").unwrap()),
            file: Some("crates/toy-cli/src/main.rs".to_owned()),
            digest: None,
        }
    }

    pub fn handle(self: &Arc<Self>) -> GraphViewHandle {
        GraphViewHandle::new(self.clone() as Arc<dyn ProjectGraph>)
    }

    pub fn set_mode(&self, mode: GraphMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn set_version(&self, v: u64) {
        *self.version.lock().unwrap() = v;
    }

    pub fn recorded(&self) -> Vec<GraphQuery> {
        self.queries.lock().unwrap().clone()
    }

    fn toy_nodes() -> Vec<GraphNode> {
        let node = |path: &str, file: &str| GraphNode {
            id: derive_node_id(GraphNodeKind::Module, &format!("toy-core\0{path}")),
            kind: GraphNodeKind::Module,
            path: path.to_owned(),
            crate_id: Some(alloy_runtime::CrateId::new("toy-core").unwrap()),
            file: Some(file.to_owned()),
            digest: None,
        };
        vec![
            node("toy_core", "crates/toy-core/src/lib.rs"),
            node("toy_core::io", "crates/toy-core/src/io.rs"),
            node("toy_core::io::reader", "crates/toy-core/src/io/reader.rs"),
        ]
    }

    fn toy_view(&self, q: &GraphQuery) -> GraphView {
        let version = GraphVersion(*self.version.lock().unwrap());
        let nodes = Self::toy_nodes();
        let mut view = GraphView::empty(version);
        view.fidelity = GraphFidelity::Manifest;
        view.truncated = *self.truncated.lock().unwrap();
        let impact = *self.impact.lock().unwrap();
        match q {
            GraphQuery::Symbol { path } => {
                // Faithful to `alloy-index/src/query.rs::symbol` (identical
                // on main and on `feat/graph-refs-impls-callers`): an exact
                // rust-path match over `graph_nodes.path`, else a file-path
                // fallback that resolves through `graph_files.module_id` —
                // i.e. a file path yields the file's **Module** node only,
                // never an item.
                let mut all = nodes;
                if impact {
                    all.push(Self::read_all_node());
                }
                view.nodes = all.iter().filter(|n| n.path == *path).cloned().collect();
                if view.nodes.is_empty() && (path.contains('/') || path.ends_with(".rs")) {
                    view.nodes = all
                        .into_iter()
                        .filter(|n| {
                            n.kind == GraphNodeKind::Module
                                && n.file.as_deref() == Some(path.as_str())
                        })
                        .collect();
                }
            }
            GraphQuery::Subgraph { .. } => {
                let mut edges = vec![
                    GraphEdge {
                        from: nodes[0].id,
                        to: nodes[1].id,
                        kind: GraphEdgeKind::Defines,
                        confidence: 1.0,
                    },
                    GraphEdge {
                        from: nodes[1].id,
                        to: nodes[2].id,
                        kind: GraphEdgeKind::Defines,
                        confidence: 1.0,
                    },
                ];
                view.nodes = nodes;
                if impact {
                    // The deep-pass store shape: the module Defines its
                    // item child, which is how a module seed is expanded
                    // to `Calls`/`References`-anchorable item nodes.
                    let item = Self::read_all_node();
                    edges.push(GraphEdge {
                        from: Self::io_node_id(),
                        to: item.id,
                        kind: GraphEdgeKind::Defines,
                        confidence: 1.0,
                    });
                    view.nodes.push(item);
                }
                view.edges = edges;
            }
            GraphQuery::Diagnostics { .. } => {
                view.diagnostics = self.diagnostics.lock().unwrap().clone();
            }
            // `Callers`/`Refs` answer only for the **item** anchor: the
            // real `neighbours()` read is `to_id = anchor` over edges the
            // deep pass anchors on item nodes exclusively, so a module
            // anchor honestly returns empty even on the semantic store.
            GraphQuery::Callers { fn_node } if impact && *fn_node == Self::read_all_node().id => {
                view.nodes = vec![Self::caller_node()];
            }
            GraphQuery::Refs { node } if impact && *node == Self::read_all_node().id => {
                // A node already present in the subgraph: exercises the
                // "relation line only, no duplicate node line" path. A
                // module *source* is real: `Refs` includes incoming
                // `Imports` edges, whose `from` is the importing module.
                view.nodes = vec![nodes[2].clone()];
            }
            _ => {}
        }
        view
    }
}

#[async_trait]
impl ProjectGraph for ScriptedGraph {
    async fn rebuild(&self, _root: &Path) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }

    async fn apply_incremental(&self, _changes: &[FileChange]) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }

    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError> {
        self.queries.lock().unwrap().push(q.clone());
        if *self.fail_impact.lock().unwrap()
            && matches!(q, GraphQuery::Callers { .. } | GraphQuery::Refs { .. })
        {
            return Err(GraphError::Io("impact backend down".into()));
        }
        let mode = *self.mode.lock().unwrap();
        match mode {
            GraphMode::Toy | GraphMode::VersionFails => Ok(self.toy_view(&q)),
            GraphMode::Empty => Ok(GraphView::empty(GraphVersion(
                *self.version.lock().unwrap(),
            ))),
            GraphMode::BusyOnce => {
                let mut burned = self.busy_burned.lock().unwrap();
                if *burned {
                    Ok(self.toy_view(&q))
                } else {
                    *burned = true;
                    Err(GraphError::Busy)
                }
            }
            GraphMode::BusyAlways => Err(GraphError::Busy),
            GraphMode::Corrupt => Err(GraphError::Corrupt("bad page".into())),
            GraphMode::Disabled => Err(GraphError::Disabled),
        }
    }

    async fn record_diagnostic(&self, d: DiagnosticEvent) -> Result<(), GraphError> {
        self.diagnostics.lock().unwrap().push(d);
        Ok(())
    }

    async fn record_fix(&self, _f: FixEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }

    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
        Err(GraphError::Disabled)
    }

    async fn version(&self) -> Result<GraphVersion, GraphError> {
        if *self.mode.lock().unwrap() == GraphMode::VersionFails {
            return Err(GraphError::Io("meta unreadable".into()));
        }
        Ok(GraphVersion(*self.version.lock().unwrap()))
    }
}

// ---------------------------------------------------------------------
// Toy workspace fixture (RFC-0011 Appendix B.1 shape)
// ---------------------------------------------------------------------

pub fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Build the Appendix B toy tree under `root`, with an `io.rs` big enough
/// for a line-window excerpt.
pub fn build_toy_workspace(root: &Path) {
    write_file(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write_file(
        &root.join("crates/toy-core/Cargo.toml"),
        "[package]\nname = \"toy-core\"\n",
    );
    write_file(&root.join("crates/toy-core/src/lib.rs"), "// lib\n");
    let mut io_rs = String::new();
    for i in 1..=50 {
        match i {
            10 => io_rs.push_str("pub fn read_all(buf: &mut Vec<u8>) -> usize {\n"),
            23 => io_rs.push_str("    let n = buf.len(); // E0502 here\n"),
            40 => io_rs.push_str("}\n"),
            _ => io_rs.push_str(&format!("// io line {i}\n")),
        }
    }
    write_file(&root.join("crates/toy-core/src/io.rs"), &io_rs);
    write_file(
        &root.join("crates/toy-core/src/io/reader.rs"),
        "// reader\n",
    );
    write_file(&root.join("crates/toy-core/src/util/mod.rs"), "// util\n");
    write_file(
        &root.join("crates/toy-cli/Cargo.toml"),
        "[package]\nname = \"toy-cli\"\n",
    );
    write_file(&root.join("crates/toy-cli/src/main.rs"), "fn main() {}\n");
    write_file(&root.join("README.md"), "# toy\n");
}

/// The Appendix A E0502 diagnostic, fully fixed.
pub fn e0502_diagnostic() -> DiagnosticEvent {
    DiagnosticEvent {
        id: fixed_diagnostic_id(),
        code: Some("E0502".into()),
        level: DiagnosticLevel::Error,
        message: "cannot borrow `*buf` as mutable".into(),
        spans: vec![SpanRef {
            path: "crates/toy-core/src/io.rs".into(),
            start_line: 23,
            start_col: 9,
            end_line: 23,
            end_col: 12,
        }],
        children: vec![DiagnosticEvent {
            id: DiagnosticId::parse("00000000-0000-4000-8000-00000000000e").unwrap(),
            code: None,
            level: DiagnosticLevel::Note,
            message: "immutable borrow occurs here".into(),
            spans: vec![SpanRef {
                path: "crates/toy-core/src/io.rs".into(),
                start_line: 21,
                start_col: 17,
                end_line: 21,
                end_col: 20,
            }],
            children: vec![],
            package: Some("toy-core".into()),
            fingerprint: Digest::sha256(b"e0502-child"),
            raw_json: None,
        }],
        package: Some("toy-core".into()),
        fingerprint: Digest::sha256(b"e0502"),
        raw_json: Some(serde_json::json!({"never": "rendered"})),
    }
}

/// A goal-bearing input envelope for the repair node.
pub fn goal_envelope(goal_text: &str) -> alloy_runtime::NodeInputEnvelope {
    alloy_runtime::NodeInputEnvelope::new(
        alloy_runtime::DagId::parse("00000000-0000-4000-8000-00000000000f").unwrap(),
        fixed_node(),
        alloy_runtime::NodeKind::Edit,
        1,
        alloy_runtime::NodeInputPayload::Goal(alloy_runtime::Goal {
            text: goal_text.to_owned(),
            constraints: vec![],
            attachments: vec![],
        }),
    )
}

/// Build `AssembleInputs` by mutation (the struct is `#[non_exhaustive]`).
pub fn make_inputs(
    goal: Option<&str>,
    diagnostics: Vec<DiagnosticEvent>,
    focus_paths: Vec<&str>,
) -> alloy_runtime::AssembleInputs {
    let mut inputs = alloy_runtime::AssembleInputs::default();
    inputs.input = goal.map(goal_envelope);
    inputs.diagnostics = diagnostics;
    inputs.focus_paths = focus_paths.into_iter().map(str::to_owned).collect();
    inputs
}

/// Tempdir with the toy workspace built inside.
pub struct ToyWs {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
}

impl ToyWs {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        build_toy_workspace(&root);
        Self { dir, root }
    }
}
