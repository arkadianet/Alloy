//! RFC-0011 §13 integration suite: store (T2), ingest (T3), incremental
//! (T4/T5), queries (T6), records (T7), cross-subsystem (T8).
//!
//! Fixtures are built programmatically in tempdirs (the Appendix B toy
//! workspace), so symlink and mutation cases stay hermetic.

use std::path::{Path, PathBuf};

use alloy_index::{GraphOpenOptions, IngestLimits, SqliteProjectGraph};
use alloy_runtime::graph::{
    derive_node_id, FileChange, FileChangeKind, FixEvent, GraphError, GraphNodeKind, GraphQuery,
    GraphViewHandle, ProjectGraph,
};
use alloy_runtime::types::diagnostic::{DiagnosticEvent, DiagnosticLevel, SpanRef};
use alloy_runtime::types::ids::{DiagnosticId, Digest, GraphVersion, Timestamp};

// ---------------------------------------------------------------------
// Fixture: the Appendix B toy workspace
// ---------------------------------------------------------------------

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Build the toy workspace under `root` — RFC-0011 Appendix B.1's tree with
/// the bodies RFC-0014 Appendix B adds for the Beta deep pass (module
/// declarations, five items, three in-workspace imports).
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
    write(&root.join("target/debug/junk.rs"), "// build output\n");
    write(&root.join("README.md"), "# toy\n");
}

struct Fx {
    _dir: tempfile::TempDir,
    /// Workspace tree.
    ws: PathBuf,
    /// Graph data dir (distinct from the workspace).
    data: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let data = dir.path().join("data");
        build_toy_workspace(&ws);
        Self {
            _dir: dir,
            ws,
            data,
        }
    }

    fn opts(&self) -> GraphOpenOptions {
        GraphOpenOptions::for_data_dir(&self.data)
    }

    async fn open(&self) -> SqliteProjectGraph {
        SqliteProjectGraph::open(self.opts()).await.unwrap()
    }
}

/// Read `(graph_version, content_digest)` straight from the closed DB file.
fn read_meta(data_dir: &Path) -> (u64, String) {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    conn.query_row(
        "SELECT graph_version, content_digest FROM graph_meta WHERE id = 1",
        [],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
    )
    .unwrap()
}

fn count(data_dir: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn sample_diagnostic(package: &str, code: &str, path: &str) -> DiagnosticEvent {
    DiagnosticEvent {
        id: DiagnosticId::new(),
        code: Some(code.to_string()),
        level: DiagnosticLevel::Error,
        message: format!("cannot borrow `x` as mutable ({code})"),
        spans: vec![SpanRef {
            path: path.to_string(),
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 2,
        }],
        children: vec![],
        package: Some(package.to_string()),
        fingerprint: Digest::sha256(format!("{package}{code}{path}").as_bytes()),
        raw_json: None,
    }
}

fn sample_fix(code: &str) -> FixEvent {
    FixEvent {
        diagnostic: None,
        diagnostic_code: Some(code.to_string()),
        crate_id: None,
        transaction: None,
        patch_artifact: None,
        verified: true,
        recorded_at: Timestamp::now(),
    }
}

// ---------------------------------------------------------------------
// T2 — store
// ---------------------------------------------------------------------

// T2a: fresh migrate; reopening is a no-op at the same version.
#[tokio::test]
async fn migrate_fresh_and_idempotent() {
    let fx = Fx::new();
    let g = fx.open().await;
    assert_eq!(g.schema_version(), 1);
    g.close().await.unwrap();
    drop(g); // release the X1 instance lock before reopening
    let g = fx.open().await;
    assert_eq!(g.schema_version(), 1);
    assert_eq!(
        count(&fx.data, "SELECT COUNT(*) FROM graph_schema_migrations"),
        1
    );
    g.close().await.unwrap();
}

// T2b: refuse a schema newer than the build (S3).
#[tokio::test]
async fn refuse_newer_graph_schema() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.close().await.unwrap();
    drop(g);
    {
        let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO graph_schema_migrations (version, applied_at) VALUES (99, 'now')",
            [],
        )
        .unwrap();
    }
    let err = SqliteProjectGraph::open(fx.opts()).await.unwrap_err();
    assert!(matches!(err, GraphError::Migration(_)), "got {err:?}");
}

// T2c: model-version mismatch truncates instead of migrating (S4).
#[tokio::test]
async fn model_version_mismatch_truncates_instead_of_migrating() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    drop(g);
    assert!(count(&fx.data, "SELECT COUNT(*) FROM graph_nodes") > 0);
    {
        let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        conn.execute("UPDATE graph_meta SET model_version = 99", [])
            .unwrap();
    }
    let g = fx.open().await;
    assert_eq!(g.version().await.unwrap(), GraphVersion(0));
    g.close().await.unwrap();
    assert_eq!(count(&fx.data, "SELECT COUNT(*) FROM graph_nodes"), 0);
    assert_eq!(count(&fx.data, "SELECT COUNT(*) FROM graph_edges"), 0);
}

// T2d: corrupt DB is quarantined and recreated (S8).
#[tokio::test]
async fn corrupt_db_is_quarantined_and_recreated() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.data.join("graph")).unwrap();
    std::fs::write(
        fx.data.join("graph/graph.sqlite"),
        b"this is not sqlite at all",
    )
    .unwrap();
    let g = fx.open().await;
    assert_eq!(g.metrics().quarantines, 1);
    assert_eq!(g.version().await.unwrap(), GraphVersion(0));
    g.close().await.unwrap();
    let quarantined: Vec<_> = std::fs::read_dir(fx.data.join("graph/quarantine"))
        .unwrap()
        .collect();
    assert!(
        !quarantined.is_empty(),
        "quarantine dir must hold the old file"
    );
}

// T2e: second instance on the same graph dir is Busy (X1).
#[tokio::test]
async fn second_open_of_the_same_graph_dir_is_busy() {
    let fx = Fx::new();
    let g = fx.open().await;
    assert!(fx.data.join("graph/graph.lock").exists());
    let err = SqliteProjectGraph::open(fx.opts()).await.unwrap_err();
    assert!(matches!(err, GraphError::Busy), "got {err:?}");
    g.close().await.unwrap();
}

// T2f: close checkpoints and is idempotent (X5).
#[tokio::test]
async fn close_is_idempotent_and_checkpoints() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    g.close().await.unwrap(); // idempotent
                              // After a TRUNCATE checkpoint the -wal file is empty or gone.
    let wal = fx.data.join("graph/graph.sqlite-wal");
    if wal.exists() {
        assert_eq!(
            std::fs::metadata(&wal).unwrap().len(),
            0,
            "WAL not truncated"
        );
    }
    // Operations after close fail Closed, not panic.
    let err = g.version().await.unwrap_err();
    assert!(matches!(err, GraphError::Closed), "got {err:?}");
}

// T2g: every persisted edge has confidence exactly 1.0 (S6/G11).
#[tokio::test]
async fn edges_always_have_confidence_one() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    let off: i64 = count(
        &fx.data,
        "SELECT COUNT(*) FROM graph_edges WHERE confidence != 1.0",
    );
    assert_eq!(off, 0);
    assert!(count(&fx.data, "SELECT COUNT(*) FROM graph_edges") > 0);
}

// ---------------------------------------------------------------------
// T3 — ingest
// ---------------------------------------------------------------------

// T3a: Appendix B golden node/edge set — the RFC-0014 Beta totals
// (13 nodes, 12 Defines, 3 Imports).
#[tokio::test]
async fn rebuild_toy_workspace_golden_nodes_and_edges() {
    let fx = Fx::new();
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.version, GraphVersion(1));
    assert_eq!(report.crates, 2);
    assert_eq!(report.modules, 5);
    assert_eq!(report.items, 5);
    assert_eq!(report.imports, 3);
    g.close().await.unwrap();

    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT kind, path, crate_id, file FROM graph_nodes ORDER BY kind, path")
        .unwrap();
    let nodes: Vec<(String, String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let expect = vec![
        (
            "crate",
            "toy-cli",
            Some("toy-cli"),
            Some("crates/toy-cli/Cargo.toml"),
        ),
        (
            "crate",
            "toy-core",
            Some("toy-core"),
            Some("crates/toy-core/Cargo.toml"),
        ),
        (
            "item",
            "toy_cli::main::main",
            Some("toy-cli"),
            Some("crates/toy-cli/src/main.rs"),
        ),
        (
            "item",
            "toy_core::Config",
            Some("toy-core"),
            Some("crates/toy-core/src/lib.rs"),
        ),
        (
            "item",
            "toy_core::io::open",
            Some("toy-core"),
            Some("crates/toy-core/src/io.rs"),
        ),
        (
            "item",
            "toy_core::io::reader::Reader",
            Some("toy-core"),
            Some("crates/toy-core/src/io/reader.rs"),
        ),
        (
            "item",
            "toy_core::util::LIMIT",
            Some("toy-core"),
            Some("crates/toy-core/src/util/mod.rs"),
        ),
        (
            "module",
            "toy_cli::main",
            Some("toy-cli"),
            Some("crates/toy-cli/src/main.rs"),
        ),
        (
            "module",
            "toy_core",
            Some("toy-core"),
            Some("crates/toy-core/src/lib.rs"),
        ),
        (
            "module",
            "toy_core::io",
            Some("toy-core"),
            Some("crates/toy-core/src/io.rs"),
        ),
        (
            "module",
            "toy_core::io::reader",
            Some("toy-core"),
            Some("crates/toy-core/src/io/reader.rs"),
        ),
        (
            "module",
            "toy_core::util",
            Some("toy-core"),
            Some("crates/toy-core/src/util/mod.rs"),
        ),
        ("workspace", ".", None, Some("Cargo.toml")),
    ];
    let got: Vec<(&str, &str, Option<&str>, Option<&str>)> = nodes
        .iter()
        .map(|(k, p, c, f)| (k.as_str(), p.as_str(), c.as_deref(), f.as_deref()))
        .collect();
    assert_eq!(got, expect, "B.2 node table at Beta");

    let mut stmt = conn
        .prepare(
            "SELECT nf.path, nt.path FROM graph_edges e
              JOIN graph_nodes nf ON nf.id = e.from_id
              JOIN graph_nodes nt ON nt.id = e.to_id
             WHERE e.kind = 'defines' ORDER BY nf.path, nt.path",
        )
        .unwrap();
    let edges: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let expect_edges = vec![
        (".", "toy-cli"),
        (".", "toy-core"),
        ("toy-cli", "toy_cli::main"),
        ("toy-core", "toy_core"),
        ("toy_cli::main", "toy_cli::main::main"),
        ("toy_core", "toy_core::Config"),
        ("toy_core", "toy_core::io"),
        ("toy_core", "toy_core::util"),
        ("toy_core::io", "toy_core::io::open"),
        ("toy_core::io", "toy_core::io::reader"),
        ("toy_core::io::reader", "toy_core::io::reader::Reader"),
        ("toy_core::util", "toy_core::util::LIMIT"),
    ];
    let got: Vec<(&str, &str)> = edges
        .iter()
        .map(|(f, t)| (f.as_str(), t.as_str()))
        .collect();
    assert_eq!(got, expect_edges, "B.3 edge table at Beta");
}

// T3b: IN6 — a second rebuild over an unchanged tree does not bump.
#[tokio::test]
async fn rebuild_twice_does_not_bump_version() {
    let fx = Fx::new();
    let g = fx.open().await;
    assert_eq!(g.rebuild(&fx.ws).await.unwrap(), GraphVersion(1));
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.version, GraphVersion(1));
    assert!(report.unchanged);
    assert_eq!(g.metrics().rebuilds_unchanged, 1);
    g.close().await.unwrap();
}

// T3c: IN5 — digest identical across two independent stores over the same
// tree (id derivation and digesting are pure functions of the tree).
#[tokio::test]
async fn rebuild_digest_is_stable_across_two_stores() {
    let fx_a = Fx::new();
    let fx_b = Fx::new();
    let a = fx_a.open().await;
    let b = fx_b.open().await;
    a.rebuild(&fx_a.ws).await.unwrap();
    b.rebuild(&fx_b.ws).await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
    assert_eq!(read_meta(&fx_a.data).1, read_meta(&fx_b.data).1);
}

// T3d/T3f: sorted traversal; target/ and .git/ skipped.
#[tokio::test]
async fn walk_skips_target_and_dot_git() {
    let fx = Fx::new();
    write(&fx.ws.join(".git/config"), "[core]\n");
    write(
        &fx.ws.join("target/generated/Cargo.toml"),
        "[package]\nname = \"evil\"\n",
    );
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(
        report.crates, 2,
        "target/-embedded manifest must not become a crate"
    );
    g.close().await.unwrap();
    let evil = count(
        &fx.data,
        "SELECT COUNT(*) FROM graph_nodes WHERE path = 'evil'",
    );
    assert_eq!(evil, 0);
}

// T3e: IN4/SEC7 — symlinks are not followed.
#[cfg(unix)]
#[tokio::test]
async fn walk_does_not_follow_symlinks() {
    let fx = Fx::new();
    let outside = fx._dir.path().join("outside");
    write(
        &outside.join("Cargo.toml"),
        "[package]\nname = \"outside\"\n",
    );
    write(&outside.join("src/lib.rs"), "// outside\n");
    std::os::unix::fs::symlink(&outside, fx.ws.join("crates/linked")).unwrap();
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.crates, 2, "symlinked crate must not be ingested");
    assert!(report.skipped > 0, "the symlink must be counted as skipped");
    g.close().await.unwrap();
    let outside_rows = count(
        &fx.data,
        "SELECT COUNT(*) FROM graph_nodes WHERE path = 'outside'",
    );
    assert_eq!(outside_rows, 0);
}

// T3g: IN7d — foo.rs beats foo/mod.rs, with a warning.
#[tokio::test]
async fn mod_rs_and_sibling_rs_prefers_sibling_and_warns() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/toy-core/src/io/mod.rs"),
        "// shadowed\n",
    );
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("io.rs") && w.contains("IN7d")),
        "warnings: {:?}",
        report.warnings
    );
    g.close().await.unwrap();
    // The module file is still io.rs, not io/mod.rs.
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let file: String = conn
        .query_row(
            "SELECT file FROM graph_nodes WHERE path = 'toy_core::io'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file, "crates/toy-core/src/io.rs");
}

// T3h: IN7e — a directory without mod.rs (and no sibling .rs) is no module.
#[tokio::test]
async fn directory_without_mod_rs_is_not_a_module() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/toy-core/src/loose/free.rs"),
        "// loose\n",
    );
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    let loose = count(
        &fx.data,
        "SELECT COUNT(*) FROM graph_nodes WHERE path LIKE '%loose%'",
    );
    assert_eq!(loose, 0);
}

// IN7b: a bin root with an explicit path inside src/ is claimed — it must
// not reappear as a lib child module (same module path would violate G9).
#[tokio::test]
async fn explicit_bin_path_inside_src_is_not_double_ingested() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/toy-core/Cargo.toml"),
        "[package]\nname = \"toy-core\"\n[[bin]]\nname = \"runner\"\npath = \"src/runner.rs\"\n",
    );
    write(
        &fx.ws.join("crates/toy-core/src/runner.rs"),
        "fn main() {}\n",
    );
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::runner".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1, "exactly one node for the bin root");
    g.close().await.unwrap();
}

// IN7b: name-only [[bin]] entries resolve to conventional src/bin/<name>.rs
// even alongside an explicit-path sibling.
#[tokio::test]
async fn name_only_bin_resolves_conventionally_beside_explicit_bins() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/toy-cli/Cargo.toml"),
        "[package]\nname = \"toy-cli\"\n[[bin]]\nname = \"cli\"\npath = \"src/main.rs\"\n[[bin]]\nname = \"helper\"\n",
    );
    write(
        &fx.ws.join("crates/toy-cli/src/bin/helper.rs"),
        "fn main() {}\n",
    );
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    for path in ["toy_cli::cli", "toy_cli::helper"] {
        let view = g
            .query(GraphQuery::Symbol { path: path.into() })
            .await
            .unwrap();
        assert_eq!(view.nodes.len(), 1, "{path} must exist");
    }
    g.close().await.unwrap();
}

// T3i: IN12 — malformed member manifest warns and continues.
#[tokio::test]
async fn malformed_member_manifest_warns_and_continues() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/broken/Cargo.toml"),
        "not [valid toml ==\n",
    );
    let g = fx.open().await;
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert_eq!(report.crates, 2);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("crates/broken/Cargo.toml")),
        "warnings: {:?}",
        report.warnings
    );
    g.close().await.unwrap();
}

// T3j: IN12 — malformed root manifest is fatal.
#[tokio::test]
async fn malformed_root_manifest_is_fatal() {
    let fx = Fx::new();
    write(&fx.ws.join("Cargo.toml"), "definitely ]] not toml\n");
    let g = fx.open().await;
    let err = g.rebuild(&fx.ws).await.unwrap_err();
    assert!(matches!(err, GraphError::Manifest { .. }), "got {err:?}");
    g.close().await.unwrap();
}

// T3k: duplicate package names are rejected (§6.3).
#[tokio::test]
async fn duplicate_package_names_are_rejected() {
    let fx = Fx::new();
    write(
        &fx.ws.join("crates/toy-dup/Cargo.toml"),
        "[package]\nname = \"toy-core\"\n",
    );
    write(&fx.ws.join("crates/toy-dup/src/lib.rs"), "// dup\n");
    let g = fx.open().await;
    let err = g.rebuild(&fx.ws).await.unwrap_err();
    assert!(matches!(err, GraphError::Manifest { .. }), "got {err:?}");
    g.close().await.unwrap();
}

// T3l: IN3 + S10 — cap exceeded leaves the previous version intact.
#[tokio::test]
async fn exceeding_max_files_leaves_previous_version_intact() {
    let fx = Fx::new();
    let g = fx.open().await;
    assert_eq!(g.rebuild(&fx.ws).await.unwrap(), GraphVersion(1));
    g.close().await.unwrap();
    drop(g);

    let mut opts = fx.opts();
    opts.limits = IngestLimits {
        max_files: 2,
        ..IngestLimits::default()
    };
    let g = SqliteProjectGraph::open(opts).await.unwrap();
    let err = g.rebuild(&fx.ws).await.unwrap_err();
    assert!(matches!(err, GraphError::LimitExceeded(_)), "got {err:?}");
    assert_eq!(
        g.version().await.unwrap(),
        GraphVersion(1),
        "S10: previous version intact"
    );
    g.close().await.unwrap();
    assert!(count(&fx.data, "SELECT COUNT(*) FROM graph_nodes") > 0);
}

// T3m (superseded by RFC-0014 SY3/SY11): the deep pass fills the reserved
// `item`/`imports` seams — the v1 CHECK lists admit them with no DDL (SY2).
#[tokio::test]
async fn deep_pass_writes_item_nodes_and_imports_edges() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'item'"
        ),
        5
    );
    assert_eq!(
        count(
            &fx.data,
            "SELECT COUNT(*) FROM graph_edges WHERE kind = 'imports'"
        ),
        3
    );
}

// T3n: G12/SEC6 — only workspace-relative paths are persisted.
#[tokio::test]
async fn stored_paths_are_workspace_relative_only() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT file FROM graph_nodes WHERE file IS NOT NULL UNION ALL SELECT path FROM graph_files")
        .unwrap();
    let paths: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!paths.is_empty());
    for p in paths {
        assert!(!p.starts_with('/'), "absolute path persisted: {p}");
        assert!(!p.contains('\\'), "non-normalised separator: {p}");
    }
}

// T3o: a non-workspace root is a Workspace error (§6.3).
#[tokio::test]
async fn non_workspace_root_is_workspace_error() {
    let fx = Fx::new();
    let empty = fx._dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let g = fx.open().await;
    let err = g.rebuild(&empty).await.unwrap_err();
    assert!(matches!(err, GraphError::Workspace(_)), "got {err:?}");
    // A manifest with neither [workspace] nor [package] is likewise rejected.
    write(&empty.join("Cargo.toml"), "[dependencies]\n");
    let err = g.rebuild(&empty).await.unwrap_err();
    assert!(matches!(err, GraphError::Workspace(_)), "got {err:?}");
    g.close().await.unwrap();
}

// ---------------------------------------------------------------------
// T4/T5 — incremental
// ---------------------------------------------------------------------

// T4a: Modified with an unchanged digest is a no-op.
#[tokio::test]
async fn modified_file_with_same_digest_is_a_noop() {
    let fx = Fx::new();
    let g = fx.open().await;
    let v1 = g.rebuild(&fx.ws).await.unwrap();
    let v2 = g
        .apply_incremental(&[FileChange {
            path: "crates/toy-core/src/io.rs".into(),
            kind: FileChangeKind::Modified,
        }])
        .await
        .unwrap();
    assert_eq!(v1, v2);
    g.close().await.unwrap();
}

// T4b: Modified with a new digest updates the digest and bumps (B.5).
#[tokio::test]
async fn modified_file_with_new_digest_updates_digest_and_bumps_version() {
    let fx = Fx::new();
    let g = fx.open().await;
    let v1 = g.rebuild(&fx.ws).await.unwrap();
    write(&fx.ws.join("crates/toy-core/src/io.rs"), "// io v2\n");
    let v2 = g
        .apply_incremental(&[FileChange {
            path: "crates/toy-core/src/io.rs".into(),
            kind: FileChangeKind::Modified,
        }])
        .await
        .unwrap();
    assert_eq!(v2, GraphVersion(v1.0 + 1));
    g.close().await.unwrap();
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let (file_digest, node_digest): (String, String) = conn
        .query_row(
            "SELECT f.digest, n.digest FROM graph_files f
              JOIN graph_nodes n ON n.file = f.path
             WHERE f.path = 'crates/toy-core/src/io.rs'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        file_digest, node_digest,
        "file and node digests stay in sync"
    );
    assert_eq!(file_digest, Digest::sha256(b"// io v2\n").as_hex());
}

// T4c: Created module file adds the node and its Defines edge. Module
// inference is declaration-driven at Beta (RFC-0014 A-0014-2), so the new
// file arrives together with its `mod writer;` declaration.
#[tokio::test]
async fn created_module_file_adds_node_and_defines_edge() {
    let fx = Fx::new();
    let g = fx.open().await;
    let v1 = g.rebuild(&fx.ws).await.unwrap();
    write(
        &fx.ws.join("crates/toy-core/src/io/writer.rs"),
        "pub struct Writer {}\n",
    );
    write(
        &fx.ws.join("crates/toy-core/src/io.rs"),
        "mod reader;\nmod writer;\npub use reader::Reader;\nuse crate::Config;\n\
         pub fn open(cfg: &Config) -> Reader { let _ = cfg; Reader {} }\n",
    );
    let v2 = g
        .apply_incremental(&[
            FileChange {
                path: "crates/toy-core/src/io/writer.rs".into(),
                kind: FileChangeKind::Created,
            },
            FileChange {
                path: "crates/toy-core/src/io.rs".into(),
                kind: FileChangeKind::Modified,
            },
        ])
        .await
        .unwrap();
    assert!(v2 > v1);
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::io::writer".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    g.close().await.unwrap();
}

// T4d: Deleted module file removes the node and its subtree.
#[tokio::test]
async fn deleted_module_file_removes_subtree() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    std::fs::remove_file(fx.ws.join("crates/toy-core/src/io.rs")).unwrap();
    std::fs::remove_dir_all(fx.ws.join("crates/toy-core/src/io")).unwrap();
    g.apply_incremental(&[FileChange {
        path: "crates/toy-core/src/io.rs".into(),
        kind: FileChangeKind::Deleted,
    }])
    .await
    .unwrap();
    for gone in ["toy_core::io", "toy_core::io::reader"] {
        let view = g
            .query(GraphQuery::Symbol { path: gone.into() })
            .await
            .unwrap();
        assert!(view.nodes.is_empty(), "{gone} should be gone");
    }
    g.close().await.unwrap();
}

// T4e: a manifest change re-ingests the owning crate.
#[tokio::test]
async fn manifest_change_reingests_the_owning_crate() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    // Point toy-cli at an explicit bin path.
    write(
        &fx.ws.join("crates/toy-cli/Cargo.toml"),
        "[package]\nname = \"toy-cli\"\n[[bin]]\nname = \"cli\"\npath = \"src/main.rs\"\n",
    );
    g.apply_incremental(&[FileChange {
        path: "crates/toy-cli/Cargo.toml".into(),
        kind: FileChangeKind::Modified,
    }])
    .await
    .unwrap();
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_cli::cli".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1, "renamed bin module must exist");
    g.close().await.unwrap();
}

// T4f: IN11 — absolute or escaping change paths are InvalidQuery.
#[tokio::test]
async fn absolute_or_escaping_file_change_path_is_invalid_query() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    for bad in ["/etc/passwd", "../outside.rs", "a/../../b.rs"] {
        let err = g
            .apply_incremental(&[FileChange {
                path: bad.into(),
                kind: FileChangeKind::Modified,
            }])
            .await
            .unwrap_err();
        assert!(matches!(err, GraphError::InvalidQuery(_)), "{bad}: {err:?}");
    }
    g.close().await.unwrap();
}

// T4g: empty change set is a no-op.
#[tokio::test]
async fn empty_change_set_is_a_noop() {
    let fx = Fx::new();
    let g = fx.open().await;
    let v1 = g.rebuild(&fx.ws).await.unwrap();
    assert_eq!(g.apply_incremental(&[]).await.unwrap(), v1);
    g.close().await.unwrap();
}

// T5: IN10 — incremental and full rebuild agree on the digest.
#[tokio::test]
async fn incremental_and_full_rebuild_agree_on_digest() {
    let fx_inc = Fx::new();
    let fx_full = Fx::new();

    // Incremental store: rebuild, then apply a mixed change set.
    let g = fx_inc.open().await;
    g.rebuild(&fx_inc.ws).await.unwrap();
    write(&fx_inc.ws.join("crates/toy-core/src/io.rs"), "// io v2\n");
    write(&fx_inc.ws.join("crates/toy-core/src/net.rs"), "// net\n");
    std::fs::remove_file(fx_inc.ws.join("crates/toy-core/src/util/mod.rs")).unwrap();
    std::fs::remove_dir(fx_inc.ws.join("crates/toy-core/src/util")).unwrap();
    g.apply_incremental(&[
        FileChange {
            path: "crates/toy-core/src/io.rs".into(),
            kind: FileChangeKind::Modified,
        },
        FileChange {
            path: "crates/toy-core/src/net.rs".into(),
            kind: FileChangeKind::Created,
        },
        FileChange {
            path: "crates/toy-core/src/util/mod.rs".into(),
            kind: FileChangeKind::Deleted,
        },
    ])
    .await
    .unwrap();
    g.close().await.unwrap();

    // Full store: rebuild the equivalent post-change tree from scratch.
    write(&fx_full.ws.join("crates/toy-core/src/io.rs"), "// io v2\n");
    write(&fx_full.ws.join("crates/toy-core/src/net.rs"), "// net\n");
    std::fs::remove_file(fx_full.ws.join("crates/toy-core/src/util/mod.rs")).unwrap();
    std::fs::remove_dir(fx_full.ws.join("crates/toy-core/src/util")).unwrap();
    let g = fx_full.open().await;
    g.rebuild(&fx_full.ws).await.unwrap();
    g.close().await.unwrap();

    assert_eq!(
        read_meta(&fx_inc.data).1,
        read_meta(&fx_full.data).1,
        "IN10: digests must agree"
    );
}

// ---------------------------------------------------------------------
// T6 — queries
// ---------------------------------------------------------------------

async fn built(fx: &Fx) -> SqliteProjectGraph {
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g
}

// T6a/T6b/T6c: Symbol semantics (Q2).
#[tokio::test]
async fn symbol_resolves_exactly_and_never_prefix_matches() {
    let fx = Fx::new();
    let g = built(&fx).await;
    // (1) exact Rust path.
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::io".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    assert_eq!(view.nodes[0].path, "toy_core::io");
    assert_eq!(view.version, GraphVersion(1));
    // (2) workspace-relative file path → owning module.
    let view = g
        .query(GraphQuery::Symbol {
            path: "crates/toy-core/src/io.rs".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    assert_eq!(view.nodes[0].path, "toy_core::io");
    // (3) no prefix matching.
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core::i".into(),
        })
        .await
        .unwrap();
    assert!(view.nodes.is_empty());
    g.close().await.unwrap();
}

// T6d/T6f: the three remaining Stub kinds return empty truncated views
// (Q4, Q5). `SimilarFixes` left the Stub set with amendment A-0011-5.
#[tokio::test]
async fn stub_queries_return_empty_truncated_views() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let node = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core");
    let queries = vec![
        GraphQuery::Callers { fn_node: node },
        GraphQuery::Refs { node },
        GraphQuery::Impls { trait_node: node },
    ];
    for q in queries {
        let view = g.query(q.clone()).await.unwrap();
        assert!(view.is_empty(), "{q:?} must be empty");
        assert!(view.truncated, "{q:?} must set truncated");
    }
    assert!(g.metrics().queries_stub >= 3);
    g.close().await.unwrap();
}

// T6g: Diagnostics filters by crate and since (Q3).
#[tokio::test]
async fn diagnostics_filters_by_crate_and_since() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let d_core = sample_diagnostic("toy-core", "E0502", "crates/toy-core/src/io.rs");
    let d_cli = sample_diagnostic("toy-cli", "E0308", "crates/toy-cli/src/main.rs");
    g.record_diagnostic(d_core.clone()).await.unwrap();
    g.record_diagnostic(d_cli).await.unwrap();

    let view = g
        .query(GraphQuery::Diagnostics {
            crate_id: Some(alloy_runtime::CrateId::new("toy-core").unwrap()),
            since: None,
        })
        .await
        .unwrap();
    assert_eq!(view.diagnostics.len(), 1);
    assert_eq!(view.diagnostics[0].id, d_core.id);

    // A future `since` excludes everything.
    let future = Timestamp(time::OffsetDateTime::now_utc() + time::Duration::hours(1));
    let view = g
        .query(GraphQuery::Diagnostics {
            crate_id: None,
            since: Some(future),
        })
        .await
        .unwrap();
    assert!(view.diagnostics.is_empty());
    g.close().await.unwrap();
}

// T6h/T6i/T6j: Subgraph semantics (Q7).
#[tokio::test]
async fn subgraph_bfs_honours_radius_directions_and_unknown_seeds() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let io = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");

    // radius 0 → seeds only.
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 0,
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);

    // radius 1 → both directions over Defines *and* Imports (RFC-0014
    // Appendix B's projection: items and the imported node are one hop out,
    // and the importing bin module is reachable in reverse).
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 1,
        })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "toy_cli::main",
            "toy_core",
            "toy_core::io",
            "toy_core::io::reader",
            "toy_core::Config",
            "toy_core::io::open",
            "toy_core::io::reader::Reader",
        ]
    );
    assert_eq!(view.edges.len(), 8);

    // radius clamps at 3 — same result as an absurd radius.
    let a = g
        .query(GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 3,
        })
        .await
        .unwrap();
    let b = g
        .query(GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 200,
        })
        .await
        .unwrap();
    assert_eq!(a, b);

    // Unknown seeds are ignored, not errors.
    let unknown = derive_node_id(GraphNodeKind::Module, "nope\0nope");
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![unknown],
            radius: 2,
        })
        .await
        .unwrap();
    assert!(view.nodes.is_empty());
    g.close().await.unwrap();
}

// T6k: Q8 — byte-identical JSON for identical queries.
#[tokio::test]
async fn query_results_are_deterministically_ordered() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let io = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");
    let q = GraphQuery::Subgraph {
        seeds: vec![io],
        radius: 3,
    };
    let a = serde_json::to_vec(&g.query(q.clone()).await.unwrap()).unwrap();
    let b = serde_json::to_vec(&g.query(q).await.unwrap()).unwrap();
    assert_eq!(a, b);
    g.close().await.unwrap();
}

// T6l: Q9 — over-cap results set truncated.
#[tokio::test]
async fn oversized_result_sets_truncated_flag() {
    let fx = Fx::new();
    let mut opts = fx.opts();
    opts.limits = IngestLimits {
        max_query_nodes: 2,
        ..IngestLimits::default()
    };
    let g = SqliteProjectGraph::open(opts).await.unwrap();
    g.rebuild(&fx.ws).await.unwrap();
    let io = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 3,
        })
        .await
        .unwrap();
    assert!(view.truncated);
    assert_eq!(view.nodes.len(), 2);
    assert!(g.metrics().queries_truncated >= 1);
    g.close().await.unwrap();
}

// T6m: Q10 — a query sweep changes neither version nor digest.
#[tokio::test]
async fn query_sweep_does_not_change_version_or_digest() {
    let fx = Fx::new();
    let g = built(&fx).await;
    g.close().await.unwrap();
    drop(g);
    let before = read_meta(&fx.data);

    let g = fx.open().await;
    let io = derive_node_id(GraphNodeKind::Module, "toy-core\0toy_core::io");
    let sweep = vec![
        GraphQuery::Symbol {
            path: "toy_core".into(),
        },
        GraphQuery::Refs { node: io },
        GraphQuery::Impls { trait_node: io },
        GraphQuery::Callers { fn_node: io },
        GraphQuery::Diagnostics {
            crate_id: None,
            since: None,
        },
        GraphQuery::SimilarFixes {
            diagnostic_code: "E0502".into(),
            limit: 1,
        },
        GraphQuery::Subgraph {
            seeds: vec![io],
            radius: 3,
        },
    ];
    for q in sweep {
        g.query(q).await.unwrap();
    }
    g.close().await.unwrap();
    assert_eq!(read_meta(&fx.data), before);
}

// ---------------------------------------------------------------------
// T7 — records and snapshots
// ---------------------------------------------------------------------

// T7a/T7b: diagnostic round-trip and idempotency (IN13).
#[tokio::test]
async fn record_diagnostic_round_trips_and_is_idempotent() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let d = sample_diagnostic("toy-core", "E0502", "crates/toy-core/src/io.rs");
    g.record_diagnostic(d.clone()).await.unwrap();
    g.record_diagnostic(d.clone()).await.unwrap(); // retried verify node
    let view = g
        .query(GraphQuery::Diagnostics {
            crate_id: None,
            since: None,
        })
        .await
        .unwrap();
    assert_eq!(view.diagnostics.len(), 1, "upsert on diagnostic_id");
    assert_eq!(view.diagnostics[0], d, "round-trips unchanged");
    g.close().await.unwrap();
}

/// A full workspace check supersedes all prior diagnostics: the pre-plan
/// seed clears before re-ingesting so retries never prompt with
/// already-fixed errors (dogfood, 2026-07-29).
#[tokio::test]
async fn clear_diagnostics_removes_all_and_reports_count() {
    let fx = Fx::new();
    let g = built(&fx).await;
    g.record_diagnostic(sample_diagnostic(
        "toy-core",
        "E0308",
        "crates/toy-core/src/io.rs",
    ))
    .await
    .unwrap();
    g.record_diagnostic(sample_diagnostic(
        "toy-cli",
        "E0277",
        "crates/toy-cli/src/main.rs",
    ))
    .await
    .unwrap();
    assert_eq!(g.clear_diagnostics().await.unwrap(), 2);
    let view = g
        .query(GraphQuery::Diagnostics {
            crate_id: None,
            since: None,
        })
        .await
        .unwrap();
    assert!(view.diagnostics.is_empty(), "{:?}", view.diagnostics);
    assert_eq!(g.clear_diagnostics().await.unwrap(), 0);
    g.close().await.unwrap();
}

// T7c: fixes append (duplicates permitted) and are surfaced by
// SimilarFixes for their own code only (IN14; Q6 as amended by A-0011-5).
#[tokio::test]
async fn record_fix_appends_and_is_surfaced_by_similar_fixes() {
    let fx = Fx::new();
    let g = built(&fx).await;
    g.record_fix(sample_fix("E0502")).await.unwrap();
    g.record_fix(sample_fix("E0502")).await.unwrap(); // duplicates permitted
    let view = g
        .query(GraphQuery::SimilarFixes {
            diagnostic_code: "E0502".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(view.fixes.len(), 2);
    assert!(view.fixes.iter().all(|f| f.verified));
    assert!(
        !view.truncated,
        "two rows under the limit is not truncation"
    );
    // A different code matches nothing.
    let other = g
        .query(GraphQuery::SimilarFixes {
            diagnostic_code: "E0308".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert!(other.fixes.is_empty());
    g.close().await.unwrap();
    assert_eq!(count(&fx.data, "SELECT COUNT(*) FROM graph_fixes"), 2);
}

// T7c2 (A-0011-5): SimilarFixes returns whole `FixEvent` rows, most recent
// first, honouring the query's `limit`.
#[tokio::test]
async fn similar_fixes_returns_recent_rows_first_and_honours_limit() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let base = time::OffsetDateTime::now_utc() - time::Duration::hours(4);
    let mut recorded = Vec::new();
    for hour in 0..3_i64 {
        let mut f = sample_fix("E0502");
        f.recorded_at = Timestamp(base + time::Duration::hours(hour));
        f.crate_id = Some(alloy_runtime::CrateId::new("toy-core").unwrap());
        f.diagnostic = Some(DiagnosticId::new());
        f.transaction = Some(alloy_runtime::TransactionId::new());
        f.patch_artifact = Some(alloy_runtime::ArtifactId::new());
        g.record_fix(f.clone()).await.unwrap();
        recorded.push(f);
    }
    // A fix for another code must never leak in.
    g.record_fix(sample_fix("E0308")).await.unwrap();

    let view = g
        .query(GraphQuery::SimilarFixes {
            diagnostic_code: "E0502".into(),
            limit: 2,
        })
        .await
        .unwrap();
    assert_eq!(view.fixes.len(), 2, "limit honoured");
    assert!(view.truncated, "a third row existed behind the limit");
    assert_eq!(
        view.fixes[0], recorded[2],
        "most recent first, round-tripped"
    );
    assert_eq!(view.fixes[1], recorded[1]);
    assert!(view.nodes.is_empty() && view.diagnostics.is_empty());
    g.close().await.unwrap();
}

// T7d: IN15 — records never bump the version.
#[tokio::test]
async fn record_diagnostic_and_record_fix_do_not_bump_version() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let v = g.version().await.unwrap();
    g.record_diagnostic(sample_diagnostic("toy-core", "E0502", "x.rs"))
        .await
        .unwrap();
    g.record_fix(sample_fix("E0502")).await.unwrap();
    assert_eq!(g.version().await.unwrap(), v);
    g.close().await.unwrap();
}

// T7e: G10 — snapshots record version/counts; same-version snapshots are
// distinct ids.
#[tokio::test]
async fn snapshot_records_version_and_counts() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let a = g.snapshot().await.unwrap();
    let b = g.snapshot().await.unwrap();
    assert_ne!(a, b);
    g.close().await.unwrap();
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let (version, nodes, edges): (i64, i64, i64) = conn
        .query_row(
            "SELECT graph_version, node_count, edge_count FROM graph_snapshots
             WHERE snapshot_id = ?1",
            [a.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(nodes, 13, "RFC-0014 Appendix B: 13 nodes at Beta");
    assert_eq!(edges, 15, "RFC-0014 Appendix B: 12 Defines + 3 Imports");
}

// SEC6: a diagnostic span with an absolute path outside the workspace stores
// no host path.
#[tokio::test]
async fn absolute_span_path_outside_workspace_is_not_persisted() {
    let fx = Fx::new();
    let g = built(&fx).await;
    let d = sample_diagnostic("toy-core", "E0502", "/etc/passwd");
    g.record_diagnostic(d).await.unwrap();
    g.close().await.unwrap();
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let primary: Option<String> = conn
        .query_row("SELECT primary_path FROM graph_diagnostics", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(primary, None, "host paths must not be persisted (SEC6)");
}

// ---------------------------------------------------------------------
// T8 — cross-subsystem
// ---------------------------------------------------------------------

// T8a: S1/S2 — the graph opens beside alloy.sqlite without touching it.
#[tokio::test]
async fn graph_opens_beside_alloy_sqlite_without_touching_it() {
    let fx = Fx::new();
    let storage = alloy_runtime::AlloyStorage::open(
        alloy_runtime::StorageOpenOptions::for_data_dir(&fx.data),
    )
    .await
    .unwrap();
    storage.close().await.unwrap();
    let session_db = fx.data.join("alloy.sqlite");
    let before = std::fs::read(&session_db).unwrap();

    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();

    let after = std::fs::read(&session_db).unwrap();
    assert_eq!(before, after, "S2: alloy.sqlite must be byte-identical");
    assert!(
        fx.data.join("graph/graph.sqlite").exists(),
        "S1: own DB file"
    );
}

// T8b: persistence across re-open.
#[tokio::test]
async fn rebuild_then_reopen_preserves_version_and_digest() {
    let fx = Fx::new();
    let g = fx.open().await;
    g.rebuild(&fx.ws).await.unwrap();
    g.close().await.unwrap();
    drop(g);
    let before = read_meta(&fx.data);

    let g = fx.open().await;
    assert_eq!(g.version().await.unwrap(), GraphVersion(before.0));
    let view = g
        .query(GraphQuery::Symbol {
            path: "toy_core".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    g.close().await.unwrap();
    assert_eq!(read_meta(&fx.data), before);
}

// T8c: end-to-end worker-style read after a host ingest.
#[tokio::test]
async fn worker_style_handle_answers_after_host_ingest() {
    let fx = Fx::new();
    let g = std::sync::Arc::new(fx.open().await);
    g.rebuild(&fx.ws).await.unwrap();
    let graph: std::sync::Arc<dyn ProjectGraph> = g.clone();
    let handle = GraphViewHandle::new(graph);
    let view = handle
        .query(GraphQuery::Symbol {
            path: "crates/toy-core/src/io/reader.rs".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    assert_eq!(view.nodes[0].path, "toy_core::io::reader");
    assert_eq!(handle.version().await.unwrap(), GraphVersion(1));
    g.close().await.unwrap();
}

// T8d: VerifyOutcome.diagnostics-shaped events ingest and query back.
#[tokio::test]
async fn verify_outcome_shaped_diagnostics_ingest_and_query_back() {
    let fx = Fx::new();
    let g = built(&fx).await;
    // The exact shape RFC-0010's verify adapters surface: spans, children,
    // fingerprint, package.
    let mut child = sample_diagnostic("toy-core", "E0502", "crates/toy-core/src/io.rs");
    child.level = DiagnosticLevel::Note;
    child.message = "borrow occurs here".into();
    let mut d = sample_diagnostic("toy-core", "E0502", "crates/toy-core/src/io.rs");
    d.children = vec![child];
    let outcome_diagnostics = vec![d];
    for d in &outcome_diagnostics {
        g.record_diagnostic(d.clone()).await.unwrap();
    }
    let view = g
        .query(GraphQuery::Diagnostics {
            crate_id: Some(alloy_runtime::CrateId::new("toy-core").unwrap()),
            since: None,
        })
        .await
        .unwrap();
    assert_eq!(
        view.diagnostics, outcome_diagnostics,
        "children survive the round-trip"
    );
    g.close().await.unwrap();
}
