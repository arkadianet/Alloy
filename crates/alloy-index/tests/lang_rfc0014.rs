//! RFC-0014 §12.2/§12.3 suite: the `syn` deep pass (T7–T15) and the
//! model-version / diagnostics integrations (T16–T19).
//!
//! Fixtures are built programmatically in tempdirs, mirroring the RFC-0011
//! suite's shape.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_index::{GraphOpenOptions, IngestLimits, RustBackend, SqliteProjectGraph};
use alloy_runtime::graph::{
    derive_node_id, GraphError, GraphFidelity, GraphNodeKind, GraphQuery, ProjectGraph,
};
use alloy_runtime::lang::{
    LangError, LanguageBackend, LanguageRegistry, RustToolchain, Scope, TestSelector,
    ToolchainRunner,
};
use alloy_runtime::types::ids::LanguageId;
use async_trait::async_trait;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// The RFC-0014 Appendix B toy workspace, bodies included.
fn build_toy_workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/toy-core/Cargo.toml"),
        "[package]\nname = \"toy-core\"\n",
    );
    write(
        &root.join("crates/toy-core/src/lib.rs"),
        "pub mod io;\npub mod util;\npub struct Config { pub verbose: bool }\n",
    );
    write(
        &root.join("crates/toy-core/src/io.rs"),
        "mod reader;\npub use reader::Reader;\nuse crate::Config;\nuse std::io::Read;\n\
         pub fn open(cfg: &Config) -> Reader { let _ = cfg; Reader {} }\n",
    );
    write(
        &root.join("crates/toy-core/src/io/reader.rs"),
        "pub struct Reader {}\n",
    );
    write(
        &root.join("crates/toy-core/src/util/mod.rs"),
        "pub const LIMIT: usize = 8;\n",
    );
    write(
        &root.join("crates/toy-cli/Cargo.toml"),
        "[package]\nname = \"toy-cli\"\n",
    );
    write(
        &root.join("crates/toy-cli/src/main.rs"),
        "use toy_core::io;\nfn main() { let _ = io::open; }\n",
    );
}

/// A single-package fixture whose lib.rs is caller-provided.
fn build_solo(root: &Path, lib_rs: &str) {
    write(&root.join("Cargo.toml"), "[package]\nname = \"solo\"\n");
    write(&root.join("src/lib.rs"), lib_rs);
}

struct Fx {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    data: PathBuf,
}

impl Fx {
    fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&ws).unwrap();
        Self {
            _dir: dir,
            ws,
            data,
        }
    }

    fn toy() -> Self {
        let fx = Self::empty();
        build_toy_workspace(&fx.ws);
        fx
    }

    fn solo(lib_rs: &str) -> Self {
        let fx = Self::empty();
        build_solo(&fx.ws, lib_rs);
        fx
    }

    fn opts(&self) -> GraphOpenOptions {
        GraphOpenOptions::for_data_dir(&self.data)
    }

    async fn open(&self) -> SqliteProjectGraph {
        SqliteProjectGraph::open(self.opts()).await.unwrap()
    }
}

fn count(data_dir: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn node_paths(data_dir: &Path, kind: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT path FROM graph_nodes WHERE kind = ?1 ORDER BY path")
        .unwrap();
    let out = stmt
        .query_map([kind], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out
}

fn import_edges(data_dir: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT nf.path, nt.path FROM graph_edges e
              JOIN graph_nodes nf ON nf.id = e.from_id
              JOIN graph_nodes nt ON nt.id = e.to_id
             WHERE e.kind = 'imports' ORDER BY nf.path, nt.path",
        )
        .unwrap();
    let out = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out
}

// ---------------------------------------------------------------------
// §12.2 — the syn pass over tempfile workspaces
// ---------------------------------------------------------------------

// T7 — SY4: item ids come from derive_node_id over the documented key.
#[tokio::test]
async fn item_nodes_use_derive_node_id_and_never_new() {
    let fx = Fx::toy();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    let expected = derive_node_id(GraphNodeKind::Item, "toy-core\0toy_core::Config");
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT id FROM graph_nodes WHERE kind = 'item' AND path = 'toy_core::Config'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, expected.to_string(), "SY4 stable key shape");
}

// T8 — SY7: `#[path]` honoured; `cfg` not evaluated (emitted + warned);
// a declared module with no file is skipped, never invented (IN7f).
#[tokio::test]
async fn declaration_driven_modules_honour_path_attribute_and_ignore_cfg_evaluation() {
    let fx = Fx::solo(
        "#[path = \"renamed.rs\"]\nmod elsewhere;\n#[cfg(feature = \"never\")]\nmod gated;\nmod missing;\n",
    );
    write(&fx.ws.join("src/renamed.rs"), "pub fn hidden() {}\n");
    write(&fx.ws.join("src/gated.rs"), "pub fn gated_fn() {}\n");
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();

    let modules = node_paths(&fx.data, "module");
    assert!(
        modules.contains(&"solo::elsewhere".to_string()),
        "#[path] resolved: {modules:?}"
    );
    assert!(
        modules.contains(&"solo::gated".to_string()),
        "cfg-gated module with an existing file is emitted (SY7): {modules:?}"
    );
    assert!(
        !modules.iter().any(|m| m.contains("missing")),
        "missing-ok, invented-never (IN7f): {modules:?}"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("cfg-gated")),
        "SY7 warning: {:?}",
        report.warnings
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("IN7f")),
        "IN7f warning: {:?}",
        report.warnings
    );
    let items = node_paths(&fx.data, "item");
    assert!(items.contains(&"solo::elsewhere::hidden".to_string()));
}

// T9 — SY8: colliding item paths keep the first in traversal order + warn.
#[tokio::test]
async fn colliding_item_paths_keep_first_and_warn() {
    let fx = Fx::solo(
        "#[cfg(unix)]\npub fn platform() -> u8 { 1 }\n#[cfg(windows)]\npub fn platform() -> u8 { 2 }\n",
    );
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    assert_eq!(report.items, 1, "one survivor, no disambiguating suffixes");
    assert_eq!(node_paths(&fx.data, "item"), vec!["solo::platform"]);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("solo::platform") && w.contains("SY8")),
        "warnings: {:?}",
        report.warnings
    );
}

// T10 — SY9: an unparseable file is skipped, counted and warned — never
// fatal; its module node survives from the declaration facts.
#[tokio::test]
async fn unparseable_file_is_skipped_counted_and_warned_not_fatal() {
    let fx = Fx::solo("mod broken;\npub fn fine() {}\n");
    write(&fx.ws.join("src/broken.rs"), "this is not rust ((((\n");
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    assert!(report.skipped >= 1, "counted: {report:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("src/broken.rs") && w.contains("SY9")),
        "warnings: {:?}",
        report.warnings
    );
    // The path and reason are recorded; the source text never is.
    assert!(report.warnings.iter().all(|w| !w.contains("((((")));
    let modules = node_paths(&fx.data, "module");
    assert!(modules.contains(&"solo::broken".to_string()));
    assert_eq!(node_paths(&fx.data, "item"), vec!["solo::fine"]);
}

// T11 — SY11: out-of-workspace imports produce zero edges and zero nodes.
#[tokio::test]
async fn imports_resolve_only_inside_the_workspace() {
    let fx = Fx::solo("use std::fmt;\nuse serde::Serialize;\npub struct S;\n");
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    assert_eq!(report.imports, 0);
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_edges WHERE kind = 'imports'"
        ),
        0
    );
    // No `std` or `serde` node was invented (G7).
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_nodes WHERE path LIKE 'std%' OR path LIKE 'serde%'"
        ),
        0
    );
}

// T12 — SY12: groups, globs, renames and `pub use` expand as specified,
// and duplicate rows collapse.
#[tokio::test]
async fn import_groups_globs_and_renames_expand_as_specified() {
    let fx = Fx::solo(
        "pub mod a;\npub mod b;\npub mod c;\n\
         use crate::a::{one, two};\nuse crate::b::*;\npub use crate::c::three as renamed;\n\
         use crate::c::three;\n",
    );
    write(
        &fx.ws.join("src/a.rs"),
        "pub fn one() {}\npub fn two() {}\n",
    );
    write(&fx.ws.join("src/b.rs"), "pub fn glob_target() {}\n");
    write(&fx.ws.join("src/c.rs"), "pub fn three() {}\n");
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    let edges = import_edges(&fx.data);
    assert_eq!(
        edges,
        vec![
            // group → one edge per leaf; glob → the module node; rename →
            // the original target; the duplicate `three` row collapsed.
            ("solo".to_string(), "solo::a::one".to_string()),
            ("solo".to_string(), "solo::a::two".to_string()),
            ("solo".to_string(), "solo::b".to_string()),
            ("solo".to_string(), "solo::c::three".to_string()),
        ]
    );
    assert_eq!(report.imports, 4);
}

// T13 — RS13: the graph stays optional with a backend registered.
#[tokio::test]
async fn null_project_graph_still_answers_with_a_backend_registered() {
    struct NoRunner;
    #[async_trait]
    impl ToolchainRunner for NoRunner {
        async fn check_json(&self, _root: &Path, _scope: &Scope) -> Result<String, LangError> {
            Err(LangError::Toolchain("unwired".into()))
        }
        async fn test(
            &self,
            _root: &Path,
            _sel: &TestSelector,
        ) -> Result<(bool, String), LangError> {
            Err(LangError::Toolchain("unwired".into()))
        }
        async fn probe(&self) -> Result<RustToolchain, LangError> {
            Err(LangError::Toolchain("unwired".into()))
        }
    }

    let backend: Arc<dyn LanguageBackend> = Arc::new(RustBackend::new(Arc::new(NoRunner)));
    let registry = LanguageRegistry::new([backend]);
    let rust = LanguageId::new("rust").unwrap();
    let backend = registry.get(&rust).expect("registered");

    // Reads stay empty, writes stay Disabled — and an index attempt fails
    // loudly through the seam instead of becoming a hard dependency.
    let graph = alloy_runtime::NullProjectGraph;
    let view = graph
        .query(GraphQuery::Symbol { path: "x".into() })
        .await
        .unwrap();
    assert!(view.is_empty());

    let fx = Fx::toy();
    let err = backend.index(&fx.ws, &graph).await.unwrap_err();
    assert!(
        matches!(err, LangError::Graph(GraphError::Disabled)),
        "got {err:?}"
    );
}

// T14 — SY14: two processes' worth of stores over the same tree produce an
// identical digest and identical IngestReport (extends RFC-0011 T3c).
#[tokio::test]
async fn syn_pass_is_deterministic_across_two_stores() {
    let fx_a = Fx::toy();
    let fx_b = Fx::toy();
    let a = fx_a.open().await;
    let b = fx_b.open().await;
    let ra = a.rebuild_reported(&fx_a.ws).await.unwrap();
    let rb = b.rebuild_reported(&fx_b.ws).await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
    assert_eq!(ra, rb, "byte-identical input → identical report");
    let digest = |data: &Path| -> String {
        let conn = rusqlite::Connection::open(data.join("graph/graph.sqlite")).unwrap();
        conn.query_row("SELECT content_digest FROM graph_meta", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(digest(&fx_a.data), digest(&fx_b.data));
}

// T15 — SY15: max_items enforced with the previous version intact;
// max_items = 0 rejected at open.
#[tokio::test]
async fn max_items_cap_returns_limit_exceeded_and_leaves_version_intact() {
    let fx = Fx::toy();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    drop(g);

    let mut opts = fx.opts();
    opts.limits = IngestLimits {
        max_items: 2,
        ..IngestLimits::default()
    };
    let g = SqliteProjectGraph::open(opts).await.unwrap();
    // Five items against a cap of two: the re-scan trips before commit.
    let err = g.rebuild(&fx.ws).await.unwrap_err();
    assert!(matches!(err, GraphError::LimitExceeded(_)), "got {err:?}");
    assert_eq!(
        g.version().await.unwrap().0,
        1,
        "S10: previous version intact"
    );
    g.close().await.unwrap();
    assert!(count(&fx.data, "SELECT COUNT(*) FROM graph_nodes") > 0);
}

#[tokio::test]
async fn max_items_zero_is_rejected_at_open() {
    let fx = Fx::toy();
    let mut opts = fx.opts();
    opts.limits = IngestLimits {
        max_items: 0,
        ..IngestLimits::default()
    };
    let err = SqliteProjectGraph::open(opts).await.unwrap_err();
    assert!(matches!(err, GraphError::LimitExceeded(_)), "got {err:?}");
}

// ---------------------------------------------------------------------
// §12.3 — model-version transition and diagnostics parity
// ---------------------------------------------------------------------

// T16 — SY1/SY2: opening a model_version=1 database truncates, resets, and
// the next rebuild re-ingests deeply; no SQL migration runs.
#[tokio::test]
async fn model_version_bump_truncates_and_reingests() {
    let fx = Fx::toy();

    // Simulate an MVP (model_version = 1) database: build, then strip the
    // deep rows and stamp the old version straight in SQL.
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    drop(g);
    {
        let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        conn.execute_batch(
            "DELETE FROM graph_nodes WHERE kind = 'item';
             DELETE FROM graph_edges WHERE kind = 'imports';
             UPDATE graph_meta SET model_version = 1, graph_version = 7;",
        )
        .unwrap();
    }

    let g = fx.open().await;
    // The open truncated (S4): nothing left, version reset — no merge.
    assert_eq!(g.version().await.unwrap().0, 0);
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.version.0, 1);
    assert_eq!(report.items, 5, "re-ingest produced Item rows");
    assert_eq!(report.source, GraphFidelity::SynDeep);
    assert_eq!(
        g.schema_version(),
        2,
        "S4: the model transition itself runs no SQL migration"
    );
    g.close().await.unwrap();
    assert_eq!(
        count(&fx.data, "SELECT COUNT(*) FROM graph_schema_migrations"),
        2,
        "the ledger still holds exactly the shipped migrations"
    );
    assert_eq!(
        count(&fx.data, "SELECT model_version FROM graph_meta"),
        3,
        "SY1/A-0011-6: model_version is 3 after the transition"
    );
}

// T17 — A-0014-4/RS4: fidelity is SynDeep from model_version 2 up (3 since
// A-0011-6), over a fresh store and a truncated-and-reingested store.
#[tokio::test]
async fn fidelity_is_syn_deep_from_model_version_two() {
    let fx = Fx::toy();
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.source, GraphFidelity::SynDeep);
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::io".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.fidelity, GraphFidelity::SynDeep);
    g.close().await.unwrap();
    drop(g);

    // Truncate-and-reingest path: stamp v1, reopen, query again.
    {
        let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        conn.execute("UPDATE graph_meta SET model_version = 1", [])
            .unwrap();
    }
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::io".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.fidelity, GraphFidelity::SynDeep);
    g.close().await.unwrap();
}

// T18 — Appendix B: exact Beta totals for the toy workspace, and the std
// import produces nothing.
#[tokio::test]
async fn toy_workspace_gains_items_and_imports() {
    let fx = Fx::toy();
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(
        (report.crates, report.modules, report.items, report.imports),
        (2, 5, 5, 3)
    );
    assert_eq!(report.source, GraphFidelity::SynDeep);
    assert!(!report.unchanged);

    // IN6 via SY15: a second rebuild of the unchanged tree does not bump.
    let again = g.rebuild_reported(&fx.ws).await.unwrap();
    assert!(again.unchanged);
    assert_eq!(again.version, report.version);
    g.close().await.unwrap();

    assert_eq!(count(&fx.data, "SELECT COUNT(*) FROM graph_nodes"), 13);
    // 12 Defines + 3 Imports + 3 References (A-0011-6: open -> Config,
    // open -> Reader, toy_cli main -> open).
    assert_eq!(count(&fx.data, "SELECT COUNT(*) FROM graph_edges"), 18);
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_edges WHERE kind = 'defines'"
        ),
        12
    );
    // The three in-workspace imports; `use std::io::Read` produced nothing.
    // (Leaf targets per SY12: `pub use reader::Reader` targets the `Reader`
    // item — Appendix B's first row names the module, its totals are these.)
    assert_eq!(
        import_edges(&fx.data),
        vec![
            ("toy_cli::main".to_string(), "toy_core::io".to_string()),
            ("toy_core::io".to_string(), "toy_core::Config".to_string()),
            (
                "toy_core::io".to_string(),
                "toy_core::io::reader::Reader".to_string()
            ),
        ]
    );
}

// T19 — DN1/DN7: identical recorded cargo JSON through the verify adapter
// and the backend produces identical diagnostics, fingerprints included.
#[tokio::test]
async fn diagnostics_entry_point_matches_verify_adapter_output() {
    use alloy_runtime::adapters::{
        NodeExecContext, NodeExecRef, ToolCaller, ToolCallerError, Verifier, VerifyClass,
        VerifyPermissions,
    };
    use alloy_runtime::storage::{AlloyStorage, StorageOpenOptions};
    use alloy_runtime::types::ids::{DagId, NodeId, ProfileId, RunId, SessionId};
    use alloy_runtime::types::permission::PermissionToken;
    use alloy_runtime::types::tools::{ToolCall, ToolName, ToolResult};
    use alloy_runtime::McpVerifyCompileAdapter;

    let recorded = [
        serde_json::json!({
            "reason": "compiler-message",
            "target": {"name": "toy-core"},
            "message": {
                "code": {"code": "E0502"},
                "level": "error",
                "message": "cannot borrow `x` as mutable",
                "spans": [{
                    "file_name": "crates/toy-core/src/io.rs",
                    "line_start": 3, "column_start": 5,
                    "line_end": 3, "column_end": 9,
                    "is_primary": true,
                }],
                "children": [],
            }
        })
        .to_string(),
        serde_json::json!({"reason": "build-finished", "success": false}).to_string(),
    ]
    .join("\n");

    // The backend side: a runner replaying the recorded stdout.
    struct ReplayRunner(String);
    #[async_trait]
    impl ToolchainRunner for ReplayRunner {
        async fn check_json(&self, _root: &Path, _scope: &Scope) -> Result<String, LangError> {
            Ok(self.0.clone())
        }
        async fn test(
            &self,
            _root: &Path,
            _sel: &TestSelector,
        ) -> Result<(bool, String), LangError> {
            Ok((true, String::new()))
        }
        async fn probe(&self) -> Result<RustToolchain, LangError> {
            Err(LangError::Toolchain("replay".into()))
        }
    }
    let backend = RustBackend::new(Arc::new(ReplayRunner(recorded.clone())));
    let from_backend = backend
        .diagnostics(Path::new("/ws"), Scope::Workspace)
        .await
        .unwrap();

    // The verify side: the same stdout through McpVerifyCompileAdapter.
    struct OneShot(String);
    #[async_trait]
    impl ToolCaller for OneShot {
        async fn call(
            &self,
            call: ToolCall,
            _perms: PermissionToken,
        ) -> Result<ToolResult, ToolCallerError> {
            assert_eq!(call.name.as_str(), "cargo_check");
            Ok(ToolResult::ok(
                ToolName::new("cargo_check").unwrap(),
                serde_json::json!({
                    "exit_code": 0, "signal": null,
                    "stdout_utf8": self.0, "stdout_truncated": false,
                }),
                1,
            ))
        }
    }
    struct Grant;
    #[async_trait]
    impl VerifyPermissions for Grant {
        async fn token_for(
            &self,
            ctx: &NodeExecRef,
            _class: VerifyClass,
        ) -> Result<PermissionToken, alloy_runtime::AdapterError> {
            Ok(PermissionToken {
                profile: ProfileId::new("default").unwrap(),
                grants: vec![],
                expires: None,
                run_id: ctx.run_id,
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let adapter = McpVerifyCompileAdapter::new(
        Arc::new(OneShot(recorded)),
        Arc::new(Grant),
        storage.artifacts(),
    );
    let ctx = NodeExecContext {
        meta: NodeExecRef {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            dag_id: DagId::new(),
            node_id: NodeId::new(),
            workspace_root: PathBuf::from("/ws"),
            attempt: 1,
        },
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let outcome = adapter.verify(&ctx).await.unwrap();
    storage.close().await.unwrap();

    // Identical events including fingerprints. `DiagnosticEvent.id` is a
    // fresh uuid per parse by design (DG-series), so identity is compared
    // over every other field.
    let key = |d: &alloy_runtime::DiagnosticEvent| {
        (
            d.code.clone(),
            d.level,
            d.message.clone(),
            d.spans.clone(),
            d.package.clone(),
            d.fingerprint.clone(),
        )
    };
    assert_eq!(from_backend.len(), 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(key(&from_backend[0]), key(&outcome.diagnostics[0]));
}

// SY3/SY5/SY10 (ACs 26/28/33): all eight module-level item kinds are
// emitted; impl blocks, macro invocations and function-body items are not.
#[tokio::test]
async fn item_kinds_cover_sy3_and_exclude_impls_and_bodies() {
    let fx = Fx::solo(
        "pub fn f() { fn inner() {} struct Hidden; }\n\
         pub struct S;\n\
         pub enum E { A }\n\
         pub union U { x: u8 }\n\
         pub trait T {}\n\
         pub type A = u8;\n\
         pub const C: u8 = 0;\n\
         pub static G: u8 = 0;\n\
         impl S { pub fn method(&self) {} }\n\
         impl T for S {}\n\
         macro_rules! m { () => {} }\n\
         m!();\n",
    );
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    assert_eq!(report.items, 8, "SY3: the eight item kinds, nothing more");
    assert_eq!(
        node_paths(&fx.data, "item"),
        vec![
            "solo::A", "solo::C", "solo::E", "solo::G", "solo::S", "solo::T", "solo::U", "solo::f",
        ]
    );
    // SY5/SY10: no impl node, no body-scoped node, no macro expansion.
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_nodes
             WHERE path LIKE '%inner%' OR path LIKE '%Hidden%'
                OR path LIKE '%method%' OR path LIKE '%impl%'"
        ),
        0
    );
}
