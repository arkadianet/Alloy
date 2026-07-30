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
//! 3. On this branch (main's store), `Callers`/`Refs` are stubs: even a
//!    correctly item-anchored query returns an **honest empty** view, not
//!    an error. Populated views arrive with the A-0011-6 deep pass
//!    (`feat/graph-refs-impls-callers`), whose `Calls`/`References` edges
//!    anchor exclusively on item nodes; the cross-branch engine+store
//!    integration test lands as a follow-up once that branch merges.

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
async fn item_anchored_callers_and_refs_are_honest_empty_on_the_m7_store() {
    // Today's store: the Callers/Refs stubs. A correctly item-anchored
    // query returns an empty view — no rows, no error — so the context
    // engine renders no relation lines and records no degradation.
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
    let callers = g
        .query(GraphQuery::Callers { fn_node: item.id })
        .await
        .unwrap();
    assert!(callers.nodes.is_empty() && callers.edges.is_empty());
    // The stub marks the empty view truncated (Q5: knowledge is withheld,
    // not absent). The engine's impact fetch must not propagate a marker
    // for it: it only honours `truncated` on non-empty views.
    assert!(callers.truncated, "the Q5 stub is a truncated empty view");
    let refs = g.query(GraphQuery::Refs { node: item.id }).await.unwrap();
    assert!(refs.nodes.is_empty() && refs.edges.is_empty());
    g.close().await.unwrap();
}
