//! Impact read-path shape contract against the **real** SQLite store
//! (A-0012-1 consumer side, verified here rather than assumed by doubles).
//!
//! The RFC-0012 context engine and the RFC-0013 repair worker derive their
//! `Callers`/`Refs` impact anchors from file paths (diagnostic spans and
//! focus paths). This suite pins the store facts that derivation relies on:
//!
//! 1. `GraphQuery::Symbol` on a file path resolves through
//!    `graph_files.module_id` to the file's **Module** node — never an item
//!    (`query.rs::symbol`).
//! 2. The module's item children are reachable via `Defines` edges in a
//!    radius-1 `Subgraph` — the expansion route the consumers use to reach
//!    `Calls`/`References`-anchorable **Item** nodes.
//! 3. Since A-0011-6 (PR #62), a correctly item-anchored `Callers`/`Refs`
//!    query answers from the recorded `Calls`/`References` edges —
//!    populated views, never an error. This is the cross-branch
//!    engine+store integration this suite's third test reserved.

use std::path::Path;

use alloy_index::{GraphOpenOptions, SqliteProjectGraph};
use alloy_runtime::graph::{GraphEdgeKind, GraphNodeKind, GraphQuery, ProjectGraph};

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A two-crate workspace with a cross-crate call: `toy-cli` calls
/// `toy_core::io::read_all`.
fn build_ws(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/toy-core/Cargo.toml"),
        "[package]\nname = \"toy-core\"\n",
    );
    write(&root.join("crates/toy-core/src/lib.rs"), "pub mod io;\n");
    write(
        &root.join("crates/toy-core/src/io.rs"),
        "pub fn read_all(buf: &mut Vec<u8>) -> usize { buf.len() }\n",
    );
    write(
        &root.join("crates/toy-cli/Cargo.toml"),
        "[package]\nname = \"toy-cli\"\n",
    );
    write(
        &root.join("crates/toy-cli/src/main.rs"),
        "use toy_core::io;\nfn main() { let mut b = Vec::new(); let _ = io::read_all(&mut b); }\n",
    );
}

async fn ingested() -> (tempfile::TempDir, SqliteProjectGraph) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    build_ws(&ws);
    let g = SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(dir.path().join("data")))
        .await
        .unwrap();
    g.rebuild(&ws).await.unwrap();
    (dir, g)
}

#[tokio::test]
async fn file_path_symbol_resolves_to_the_module_node_never_an_item() {
    let (_dir, g) = ingested().await;
    let view = g
        .query(GraphQuery::Symbol {
            path: "crates/toy-core/src/io.rs".into(),
        })
        .await
        .unwrap();
    assert_eq!(view.nodes.len(), 1, "one node per file path: {view:?}");
    let node = &view.nodes[0];
    assert_eq!(
        node.kind,
        GraphNodeKind::Module,
        "a file path resolves through graph_files.module_id to the MODULE \
         node — impact consumers must not anchor Callers/Refs on it"
    );
    assert_eq!(node.path, "toy_core::io");
}

#[tokio::test]
async fn module_seeds_expand_to_item_anchors_via_defines_in_a_radius_1_subgraph() {
    let (_dir, g) = ingested().await;
    let module = g
        .query(GraphQuery::Symbol {
            path: "crates/toy-core/src/io.rs".into(),
        })
        .await
        .unwrap()
        .nodes
        .remove(0);
    let sub = g
        .query(GraphQuery::Subgraph {
            seeds: vec![module.id],
            radius: 1,
        })
        .await
        .unwrap();
    let item_children: Vec<_> = sub
        .edges
        .iter()
        .filter(|e| e.kind == GraphEdgeKind::Defines && e.from == module.id)
        .filter_map(|e| sub.nodes.iter().find(|n| n.id == e.to))
        .filter(|n| n.kind == GraphNodeKind::Item)
        .collect();
    assert!(
        item_children
            .iter()
            .any(|n| n.path == "toy_core::io::read_all"),
        "the Defines expansion reaches the item anchor: {sub:?}"
    );
}

#[tokio::test]
async fn item_anchored_callers_and_refs_answer_from_recorded_edges() {
    // Since A-0011-6 (PR #62): the item anchor found through the file →
    // module → Defines expansion above is exactly the anchor shape the
    // `calls`/`references` edges record, so the queries return populated
    // views — the engine renders relation lines, never a degradation.
    let (_dir, g) = ingested().await;
    let module = g
        .query(GraphQuery::Symbol {
            path: "crates/toy-core/src/io.rs".into(),
        })
        .await
        .unwrap()
        .nodes
        .remove(0);
    let sub = g
        .query(GraphQuery::Subgraph {
            seeds: vec![module.id],
            radius: 1,
        })
        .await
        .unwrap();
    let item = sub
        .nodes
        .iter()
        .find(|n| n.kind == GraphNodeKind::Item && n.path == "toy_core::io::read_all")
        .expect("item ingested")
        .clone();

    // `toy_cli::main::main` calls `io::read_all` through its
    // `use toy_core::io;` binding — the one incoming Calls edge.
    let callers = g
        .query(GraphQuery::Callers { fn_node: item.id })
        .await
        .unwrap();
    let caller_paths: Vec<&str> = callers.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        caller_paths,
        vec!["toy_cli::main::main", "toy_core::io::read_all"],
        "Q8 order: the caller item plus the anchor"
    );
    let caller_id = callers.nodes[0].id;
    assert_eq!(callers.edges.len(), 1);
    let edge = &callers.edges[0];
    assert!(
        edge.kind == GraphEdgeKind::Calls && edge.from == caller_id && edge.to == item.id,
        "one incoming Calls edge from the cli main: {edge:?}"
    );
    assert!(!callers.truncated, "nothing was withheld");

    // Nothing references `read_all` as a value/type, and `Vec`/`usize` in
    // its signature resolve outside the workspace — the view is the
    // anchor alone, honestly empty of edges, and not marked truncated
    // (Q5's stub marker died with the stub).
    let refs = g.query(GraphQuery::Refs { node: item.id }).await.unwrap();
    assert_eq!(refs.nodes.len(), 1, "the anchor node only: {refs:?}");
    assert_eq!(refs.nodes[0].id, item.id);
    assert!(refs.edges.is_empty());
    assert!(!refs.truncated);
    g.close().await.unwrap();
}
