//! Amendment A-0011-6 suite: `Refs` / `Impls` / `Callers` un-stubbed over
//! the `references` / `calls` / `impls` edges the syn deep pass records
//! (`model_version = 3`).
//!
//! Fixtures are built programmatically in tempdirs, mirroring the RFC-0011
//! suite's shape. The cross-crate fixture exercises use-map resolution,
//! same-module resolution, glob fallback, impl blocks, and the honesty
//! rules (nothing is recorded for out-of-workspace or ambiguous targets).

use std::path::{Path, PathBuf};

use alloy_index::{GraphOpenOptions, IngestLimits, SqliteProjectGraph};
use alloy_runtime::graph::{
    derive_node_id, GraphEdgeKind, GraphNodeKind, GraphQuery, ProjectGraph,
};

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Cross-crate workspace with resolvable references, calls and impls.
///
/// - `xc-core` defines `Config`, `Codec`, `encode`, and a `json` module
///   whose `JsonCodec` implements `Codec`.
/// - `xc-cli` calls `encode` cross-crate, references `Config` in a
///   signature, and implements `Codec` for its own `CliCodec`.
fn build_cross_workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/xc-core/Cargo.toml"),
        "[package]\nname = \"xc-core\"\n",
    );
    write(
        &root.join("crates/xc-core/src/lib.rs"),
        "pub mod json;\n\
         pub struct Config { pub pretty: bool }\n\
         pub trait Codec { fn tag(&self) -> u8; }\n\
         pub fn encode(cfg: &Config) -> u8 { helper(cfg) }\n\
         fn helper(cfg: &Config) -> u8 { u8::from(cfg.pretty) }\n",
    );
    write(
        &root.join("crates/xc-core/src/json.rs"),
        "use crate::{Codec, Config};\n\
         pub struct JsonCodec;\n\
         impl Codec for JsonCodec { fn tag(&self) -> u8 { 1 } }\n\
         impl JsonCodec { pub fn with(cfg: &Config) -> u8 { crate::encode(cfg) } }\n",
    );
    write(
        &root.join("crates/xc-cli/Cargo.toml"),
        "[package]\nname = \"xc-cli\"\n",
    );
    write(
        &root.join("crates/xc-cli/src/main.rs"),
        "use xc_core::{Codec, Config};\n\
         pub struct CliCodec;\n\
         impl Codec for CliCodec { fn tag(&self) -> u8 { 2 } }\n\
         fn main() {\n\
             let cfg = Config { pretty: true };\n\
             let _ = xc_core::encode(&cfg);\n\
             let _ = std::mem::size_of::<CliCodec>();\n\
         }\n",
    );
}

struct Fx {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    data: PathBuf,
}

impl Fx {
    fn cross() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let data = dir.path().join("data");
        build_cross_workspace(&ws);
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

    async fn built(&self) -> SqliteProjectGraph {
        let g = self.open().await;
        g.rebuild(&self.ws).await.unwrap();
        g
    }
}

fn item(package: &str, path: &str) -> alloy_runtime::GraphNodeId {
    derive_node_id(GraphNodeKind::Item, &format!("{package}\0{path}"))
}

fn edge_pairs(data_dir: &Path, kind: &str) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(data_dir.join("graph/graph.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT nf.path, nt.path FROM graph_edges e
              JOIN graph_nodes nf ON nf.id = e.from_id
              JOIN graph_nodes nt ON nt.id = e.to_id
             WHERE e.kind = ?1 ORDER BY nf.path, nt.path",
        )
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn pairs(rows: &[(String, String)]) -> Vec<(&str, &str)> {
    rows.iter().map(|(f, t)| (f.as_str(), t.as_str())).collect()
}

// ---------------------------------------------------------------------
// Ingest: the three new edge kinds are recorded honestly
// ---------------------------------------------------------------------

// A-0011-6a: calls resolve through module scope and the use map; the
// callee must be a known workspace fn item.
#[tokio::test]
async fn ingest_records_calls_edges_for_resolvable_fn_calls() {
    let fx = Fx::cross();
    let g = fx.built().await;
    g.close().await.unwrap();
    assert_eq!(
        pairs(&edge_pairs(&fx.data, "calls")),
        vec![
            // main → xc_core::encode (multi-segment via workspace ident).
            ("xc_cli::main::main", "xc_core::encode"),
            // encode → helper (same-module single segment).
            ("xc_core::encode", "xc_core::helper"),
            // JsonCodec::with → crate::encode, attributed to the self-type
            // item because impl-block methods are not nodes.
            ("xc_core::json::JsonCodec", "xc_core::encode"),
        ]
    );
}

// A-0011-6a: references from signatures, fields and bodies; a call target
// is a Calls edge, not doubled as a References edge.
#[tokio::test]
async fn ingest_records_references_edges_for_resolvable_type_usages() {
    let fx = Fx::cross();
    let g = fx.built().await;
    g.close().await.unwrap();
    let refs = edge_pairs(&fx.data, "references");
    // Signature/body references to Config from both crates.
    assert!(
        pairs(&refs).contains(&("xc_core::encode", "xc_core::Config")),
        "signature type reference: {refs:?}"
    );
    assert!(
        pairs(&refs).contains(&("xc_core::helper", "xc_core::Config")),
        "private fn signature reference: {refs:?}"
    );
    assert!(
        pairs(&refs).contains(&("xc_cli::main::main", "xc_core::Config")),
        "cross-crate struct-literal reference: {refs:?}"
    );
    // A Calls edge target is not duplicated as a References edge.
    assert!(
        !pairs(&refs).contains(&("xc_core::encode", "xc_core::helper")),
        "call targets must not double as references: {refs:?}"
    );
}

// A-0011-6a: impl blocks produce self-type → trait edges; inherent impls
// produce none.
#[tokio::test]
async fn ingest_records_impls_edges_for_trait_impl_blocks() {
    let fx = Fx::cross();
    let g = fx.built().await;
    g.close().await.unwrap();
    assert_eq!(
        pairs(&edge_pairs(&fx.data, "impls")),
        vec![
            ("xc_cli::main::CliCodec", "xc_core::Codec"),
            ("xc_core::json::JsonCodec", "xc_core::Codec"),
        ]
    );
}

// A-0011-6a honesty: out-of-workspace targets, locals and method calls
// produce nothing — no invented nodes, no invented edges (G7).
#[tokio::test]
async fn unresolvable_targets_produce_no_edges_or_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    write(&ws.join("Cargo.toml"), "[package]\nname = \"solo\"\n");
    write(
        &ws.join("src/lib.rs"),
        "use std::collections::HashMap;\n\
         pub struct S;\n\
         impl std::fmt::Debug for S {\n\
             fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
                 f.write_str(\"S\")\n\
             }\n\
         }\n\
         pub fn f(map: HashMap<u8, u8>) -> usize { let local = map.len(); local }\n",
    );
    let data = dir.path().join("data");
    let g = SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    let report = g.rebuild_reported(&ws).await.unwrap();
    g.close().await.unwrap();
    assert_eq!(report.references, 0, "std targets resolve to nothing");
    assert_eq!(report.calls, 0, "method calls are never resolved");
    assert_eq!(report.impls, 0, "foreign-trait impls record nothing");
    let conn = rusqlite::Connection::open(data.join("graph/graph.sqlite")).unwrap();
    let foreign: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM graph_nodes WHERE path LIKE 'std%' OR path LIKE 'core%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(foreign, 0, "no foreign node is invented (G7)");
}

// IN5/IN6 for the new pass: two independent stores agree on report and
// digest; a re-run does not bump the version.
#[tokio::test]
async fn semantic_edges_are_deterministic_and_idempotent() {
    let fx_a = Fx::cross();
    let fx_b = Fx::cross();
    let a = fx_a.open().await;
    let b = fx_b.open().await;
    let ra = a.rebuild_reported(&fx_a.ws).await.unwrap();
    let rb = b.rebuild_reported(&fx_b.ws).await.unwrap();
    assert_eq!(ra, rb, "identical tree → identical report");
    assert!(ra.references > 0 && ra.calls > 0 && ra.impls > 0);
    let again = a.rebuild_reported(&fx_a.ws).await.unwrap();
    assert!(again.unchanged, "IN6 holds with semantic edges present");
    a.close().await.unwrap();
    b.close().await.unwrap();
    let digest = |data: &Path| -> String {
        let conn = rusqlite::Connection::open(data.join("graph/graph.sqlite")).unwrap();
        conn.query_row("SELECT content_digest FROM graph_meta", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(digest(&fx_a.data), digest(&fx_b.data));
    assert_eq!(
        edge_pairs(&fx_a.data, "references"),
        edge_pairs(&fx_b.data, "references")
    );
    assert_eq!(
        edge_pairs(&fx_a.data, "calls"),
        edge_pairs(&fx_b.data, "calls")
    );
}

// ---------------------------------------------------------------------
// Queries: Refs / Impls / Callers round-trip, ordering, limits
// ---------------------------------------------------------------------

// A-0011-6c: Refs returns the anchor plus incoming `references` and
// `imports` edges, deterministically ordered (Q8).
#[tokio::test]
async fn refs_round_trips_incoming_references_and_imports() {
    let fx = Fx::cross();
    let g = fx.built().await;
    let config = item("xc-core", "xc_core::Config");
    let view = g.query(GraphQuery::Refs { node: config }).await.unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            // Q8: (kind, path, id) — kinds in declaration order, so the
            // importing modules sort before the referencing items.
            "xc_cli::main",
            "xc_core::json",
            "xc_cli::main::main",
            "xc_core::Config",
            "xc_core::encode",
            "xc_core::helper",
            "xc_core::json::JsonCodec",
        ]
    );
    assert!(!view.truncated, "nothing was withheld");
    for e in &view.edges {
        assert_eq!(e.to, config, "only incoming edges are returned");
        assert!(matches!(
            e.kind,
            GraphEdgeKind::References | GraphEdgeKind::Imports
        ));
    }
    // Four references (main, encode, helper, JsonCodec::with's signature)
    // plus the two `use` importers.
    assert_eq!(view.edges.len(), 6);
    // Determinism: byte-identical JSON across runs (Q8).
    let a = serde_json::to_vec(&g.query(GraphQuery::Refs { node: config }).await.unwrap()).unwrap();
    let b = serde_json::to_vec(&g.query(GraphQuery::Refs { node: config }).await.unwrap()).unwrap();
    assert_eq!(a, b);
    g.close().await.unwrap();
}

// A-0011-6c: Callers returns incoming `calls` edges only.
#[tokio::test]
async fn callers_round_trips_incoming_calls_edges() {
    let fx = Fx::cross();
    let g = fx.built().await;
    let encode = item("xc-core", "xc_core::encode");
    let view = g
        .query(GraphQuery::Callers { fn_node: encode })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "xc_cli::main::main",
            "xc_core::encode",
            "xc_core::json::JsonCodec",
        ]
    );
    assert!(view
        .edges
        .iter()
        .all(|e| e.kind == GraphEdgeKind::Calls && e.to == encode));
    assert_eq!(view.edges.len(), 2);
    assert!(!view.truncated);

    // A fn nobody calls: anchor only, still not an error.
    let lonely = item("xc-cli", "xc_cli::main::main");
    let view = g
        .query(GraphQuery::Callers { fn_node: lonely })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1);
    assert!(view.edges.is_empty());
    assert!(!view.truncated);
    g.close().await.unwrap();
}

// A-0011-6c: Impls answers both directions — implementers of a trait, and
// traits implemented by a type.
#[tokio::test]
async fn impls_answers_for_trait_and_for_type() {
    let fx = Fx::cross();
    let g = fx.built().await;
    let codec = item("xc-core", "xc_core::Codec");
    let view = g
        .query(GraphQuery::Impls { trait_node: codec })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "xc_cli::main::CliCodec",
            "xc_core::Codec",
            "xc_core::json::JsonCodec",
        ]
    );
    assert!(view.edges.iter().all(|e| e.kind == GraphEdgeKind::Impls));
    assert_eq!(view.edges.len(), 2);

    // The same query anchored at a type yields the traits it implements.
    let json = item("xc-core", "xc_core::json::JsonCodec");
    let view = g
        .query(GraphQuery::Impls { trait_node: json })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(paths, vec!["xc_core::Codec", "xc_core::json::JsonCodec"]);
    assert_eq!(view.edges.len(), 1);
    g.close().await.unwrap();
}

// A-0011-6c: an unknown anchor is an honest empty view — never an error,
// never `truncated` (nothing was withheld).
#[tokio::test]
async fn unknown_anchor_returns_empty_untruncated_views() {
    let fx = Fx::cross();
    let g = fx.built().await;
    let unknown = derive_node_id(GraphNodeKind::Item, "nope\0nope::Missing");
    for q in [
        GraphQuery::Refs { node: unknown },
        GraphQuery::Impls {
            trait_node: unknown,
        },
        GraphQuery::Callers { fn_node: unknown },
    ] {
        let view = g.query(q.clone()).await.unwrap();
        assert!(view.is_empty(), "{q:?} must be empty");
        assert!(!view.truncated, "{q:?}: nothing was withheld");
    }
    g.close().await.unwrap();
}

// Q9 for the un-stubbed queries: over-cap results truncate at the ordering
// boundary and say so.
#[tokio::test]
async fn refs_over_cap_sets_truncated() {
    let fx = Fx::cross();
    let mut opts = fx.opts();
    opts.limits = IngestLimits {
        max_query_nodes: 2,
        ..IngestLimits::default()
    };
    let g = SqliteProjectGraph::open(opts).await.unwrap();
    g.rebuild(&fx.ws).await.unwrap();
    let config = item("xc-core", "xc_core::Config");
    let view = g.query(GraphQuery::Refs { node: config }).await.unwrap();
    assert!(view.truncated, "seven candidate nodes against a cap of two");
    assert_eq!(view.nodes.len(), 2);
    // Q9/Q8: truncation happens at the ordering boundary — the kept rows
    // are the prefix of the full (kind, path, id) ordering.
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(paths, vec!["xc_cli::main", "xc_core::json"]);
    g.close().await.unwrap();
}

// Q10 endures: the un-stubbed queries write nothing.
#[tokio::test]
async fn unstubbed_query_sweep_changes_neither_version_nor_digest() {
    let fx = Fx::cross();
    let g = fx.built().await;
    g.close().await.unwrap();
    drop(g);
    let meta = |data: &Path| -> (i64, String) {
        let conn = rusqlite::Connection::open(data.join("graph/graph.sqlite")).unwrap();
        conn.query_row(
            "SELECT graph_version, content_digest FROM graph_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let before = meta(&fx.data);
    let g = fx.open().await;
    let codec = item("xc-core", "xc_core::Codec");
    for q in [
        GraphQuery::Refs { node: codec },
        GraphQuery::Impls { trait_node: codec },
        GraphQuery::Callers { fn_node: codec },
    ] {
        g.query(q).await.unwrap();
    }
    g.close().await.unwrap();
    assert_eq!(meta(&fx.data), before);
}

// ---------------------------------------------------------------------
// Migration: schema v2, model v3
// ---------------------------------------------------------------------

// S3/S4 as amended: fresh stores open at schema 2 / model 3; a model-2
// database is truncated for re-ingest, and its next rebuild records the
// semantic edges.
#[tokio::test]
async fn model_v2_database_truncates_and_reingests_with_semantic_edges() {
    let fx = Fx::cross();
    let g = fx.built().await;
    assert_eq!(g.schema_version(), 2);
    assert_eq!(g.model_version(), 3);
    g.close().await.unwrap();
    drop(g);
    {
        let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        conn.execute_batch(
            "DELETE FROM graph_edges WHERE kind IN ('references','calls','impls');
             UPDATE graph_meta SET model_version = 2, graph_version = 7;",
        )
        .unwrap();
    }
    let g = fx.open().await;
    assert_eq!(g.version().await.unwrap().0, 0, "S4: truncated, not merged");
    let report = g.rebuild_reported(&fx.ws).await.unwrap();
    assert!(report.references > 0 && report.calls > 0 && report.impls > 0);
    g.close().await.unwrap();
    let conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
    let model: i64 = conn
        .query_row("SELECT model_version FROM graph_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(model, 3);
}

// ---------------------------------------------------------------------
// Q7 as amended (A-0011-6f): Subgraph traversal stays structural
// ---------------------------------------------------------------------

// Subgraph BFS walks `Defines`/`Imports` only; the semantic kinds are never
// traversed — a call graph is asked for via Callers/Refs/Impls, not pulled
// into a neighbourhood. Semantic edges whose endpoints both land in the
// view are still returned (the §5 edge-inclusion rule).
#[tokio::test]
async fn subgraph_traverses_structural_edges_only() {
    let fx = Fx::cross();
    let g = fx.built().await;
    let encode = item("xc-core", "xc_core::encode");

    // Radius 1 from `encode`: only the defining module — not the Calls
    // targets (`helper`), Calls sources (`main`, `JsonCodec`) or the
    // References target (`Config`).
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![encode],
            radius: 1,
        })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["xc_core", "xc_core::encode"],
        "semantic edges must not be traversed (Q7 as amended by A-0011-6f)"
    );

    // Radius 2 reaches `helper` and `Config` structurally (via the module),
    // and the semantic edges between in-view nodes are returned.
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![encode],
            radius: 2,
        })
        .await
        .unwrap();
    let paths: Vec<&str> = view.nodes.iter().map(|n| n.path.as_str()).collect();
    assert!(paths.contains(&"xc_core::helper") && paths.contains(&"xc_core::Config"));
    let helper = item("xc-core", "xc_core::helper");
    let config = item("xc-core", "xc_core::Config");
    assert!(
        view.edges
            .iter()
            .any(|e| e.kind == GraphEdgeKind::Calls && e.from == encode && e.to == helper),
        "in-view semantic edges are still returned"
    );
    assert!(view
        .edges
        .iter()
        .any(|e| e.kind == GraphEdgeKind::References && e.from == encode && e.to == config));

    // Even at the radius cap, the caller of `encode` (one Calls hop away,
    // four structural hops away) stays out of view.
    let view = g
        .query(GraphQuery::Subgraph {
            seeds: vec![encode],
            radius: 3,
        })
        .await
        .unwrap();
    assert!(
        !view.nodes.iter().any(|n| n.path == "xc_cli::main::main"),
        "Calls edges must not shortcut the structural BFS"
    );
    g.close().await.unwrap();
}

// ---------------------------------------------------------------------
// Q4/Q5 robustness: high-degree anchors and SQLite's variable limit
// ---------------------------------------------------------------------

// A neighbourhood query over an anchor with tens of thousands of incident
// edges must not build one SQL placeholder per node — SQLite's bundled
// variable limit is 32766. Synthetic rows are injected straight into the
// store (the graph is a derived cache; G1 says every row is fair game).
#[tokio::test]
async fn high_degree_anchor_stays_under_the_sqlite_variable_limit() {
    let fx = Fx::cross();
    let g = fx.built().await;
    g.close().await.unwrap();
    drop(g);

    let encode = item("xc-core", "xc_core::encode").to_string();
    {
        let mut conn = rusqlite::Connection::open(fx.data.join("graph/graph.sqlite")).unwrap();
        let tx = conn.transaction().unwrap();
        for i in 0..40_000u32 {
            let path = format!("xc_core::synthetic::caller_{i:05}");
            let id = item("xc-core", &path).to_string();
            tx.execute(
                "INSERT INTO graph_nodes (id, kind, path, crate_id) VALUES (?1, 'item', ?2, 'xc-core')",
                rusqlite::params![id, path],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO graph_edges (from_id, to_id, kind) VALUES (?1, ?2, 'calls')",
                rusqlite::params![id, encode],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    let g = fx.open().await;
    let view = g
        .query(GraphQuery::Callers {
            fn_node: item("xc-core", "xc_core::encode"),
        })
        .await
        .expect("a high-degree anchor must not blow the SQL variable limit");
    let cap = IngestLimits::default().max_query_nodes as usize;
    assert!(
        view.truncated,
        "40k callers against the {cap}-node default cap"
    );
    assert_eq!(view.nodes.len(), cap);
    g.close().await.unwrap();
}

// ---------------------------------------------------------------------
// A-0011-6b: generic parameters never resolve (G7)
// ---------------------------------------------------------------------

// A generic parameter can share its name with a `use` alias, a workspace
// crate ident, or a module child. Rustc resolves the path through the
// generic (which shadows them all); the pass performs no inference there,
// so it must record nothing — an invented edge violates G7. Control fns
// without the shadowing generic guard against over-suppression.
#[tokio::test]
async fn generic_parameter_heads_never_resolve_to_workspace_items() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let data = dir.path().join("data");
    write(
        &ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &ws.join("crates/gen-dep/Cargo.toml"),
        "[package]\nname = \"gen-dep\"\n",
    );
    write(
        &ws.join("crates/gen-dep/src/lib.rs"),
        "pub fn helper() -> u8 { 1 }\npub struct Thing;\n",
    );
    write(
        &ws.join("crates/gen-app/Cargo.toml"),
        "[package]\nname = \"gen-app\"\n",
    );
    write(
        &ws.join("crates/gen-app/src/lib.rs"),
        "use gen_dep as T;\n\
         pub struct App;\n\
         pub trait Maker { type Thing; fn helper() -> u8; }\n\
         // The generic shadows the alias: `T::helper` is the trait fn.\n\
         pub fn alias_shadowed<T: Maker>() -> u8 { T::helper() }\n\
         // The generic shadows the workspace crate ident.\n\
         #[allow(non_camel_case_types)]\n\
         pub fn crate_shadowed<gen_dep: Maker>() -> u8 { gen_dep::helper() }\n\
         // A multi-segment *type* path headed by the generic.\n\
         pub fn type_shadowed<T: Maker>(_x: T::Thing) -> u8 { 0 }\n\
         // A method-level generic inside an impl block.\n\
         impl App { pub fn m<T: Maker>() -> u8 { T::helper() } }\n\
         // Controls: no shadowing generic in scope, so these DO resolve.\n\
         pub fn control_alias() -> u8 { T::helper() }\n\
         pub fn control_crate() -> u8 { gen_dep::helper() }\n",
    );
    let g = SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    g.rebuild(&ws).await.unwrap();
    g.close().await.unwrap();

    let calls = edge_pairs(&data, "calls");
    let refs = edge_pairs(&data, "references");
    for shadowed in [
        "gen_app::alias_shadowed",
        "gen_app::crate_shadowed",
        "gen_app::type_shadowed",
        "gen_app::App",
    ] {
        let invented: Vec<_> = calls
            .iter()
            .chain(refs.iter())
            .filter(|(from, to)| from == shadowed && to.starts_with("gen_dep::"))
            .collect();
        assert!(
            invented.is_empty(),
            "generic-headed path invented an edge (G7): {invented:?}"
        );
    }
    // Over-suppression guard: the unshadowed controls resolve as before.
    for control in ["gen_app::control_alias", "gen_app::control_crate"] {
        assert!(
            pairs(&calls).contains(&(control, "gen_dep::helper")),
            "control call from {control} was wrongly suppressed: {calls:?}"
        );
    }
}

/// A-0011-6b honesty: items declared inside a body live in their own scope —
/// their references must NOT be attributed to the enclosing module-level
/// item (the RefCollector doc's "items nested inside bodies" clause).
#[tokio::test]
async fn nested_body_items_do_not_leak_refs_to_the_enclosing_item() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let data = dir.path().join("data");
    write(
        &ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/nb\"]\nresolver = \"2\"\n",
    );
    write(
        &ws.join("crates/nb/Cargo.toml"),
        "[package]\nname = \"nb\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &ws.join("crates/nb/src/lib.rs"),
        "pub struct Config;\n\
         pub fn target() -> u8 { 1 }\n\
         pub fn outer() -> u8 {\n\
             fn nested() -> u8 { crate::target() }\n\
             struct Inner { _c: crate::Config }\n\
             nested()\n\
         }\n",
    );
    let g = SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    g.rebuild(&ws).await.unwrap();
    g.close().await.unwrap();
    // Sanity: the ingest saw the crate at all (guards a vacuous pass).
    assert!(
        !edge_pairs(&data, "defines").is_empty(),
        "fixture crate was not ingested"
    );
    for kind in ["calls", "references"] {
        let from_outer: Vec<_> = edge_pairs(&data, kind)
            .into_iter()
            .filter(|(src, _)| src == "nb::outer")
            .collect();
        assert!(
            from_outer.is_empty(),
            "nested-body {kind} leaked to the enclosing item: {from_outer:?}"
        );
    }
}
