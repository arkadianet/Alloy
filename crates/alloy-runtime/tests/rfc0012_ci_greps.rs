//! RFC-0012 §13.9 CI-enforceable source greps (T-CI1–T-CI10).
//!
//! Same harness shape as `rfc0010_ci_greps.rs` / `rfc0011_ci_greps.rs`:
//! recursive `walk_rs_files` from `CARGO_MANIFEST_DIR`, per-line asserts,
//! plus a "the walk found zero files" guard so a moved directory cannot
//! silently vacate a rule.

use std::path::{Path, PathBuf};

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read_dir entry: {e}"))
            .path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn context_files() -> Vec<PathBuf> {
    let dir = crate_root().join("src/context");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files under src/context — the walk itself is broken if this fires"
    );
    files
}

fn assert_absent(files: &[PathBuf], needles: &[&str], rule: &str) {
    for file in files {
        let text = std::fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            for needle in needles {
                assert!(
                    !line.contains(needle),
                    "{rule}: `{needle}` found in {}:{}: {line}",
                    file.display(),
                    n + 1
                );
            }
        }
    }
}

// T-CI1 — C2: context reaches no other workspace crate and no SQLite.
#[test]
fn c2_context_does_not_reference_other_crates() {
    assert_absent(
        &context_files(),
        &[
            "alloy_index",
            "alloy_tools",
            "alloy_cli",
            "alloy_eval",
            "rusqlite",
        ],
        "C2",
    );
}

// T-CI2 — SEC6: no MCP tool exposes the Context Engine.
#[test]
fn sec6_no_context_mcp_tool_exists() {
    let tools_src = crate_root().join("../alloy-tools/src");
    let mut files = Vec::new();
    walk_rs_files(&tools_src, &mut files);
    assert!(!files.is_empty(), "alloy-tools/src walk found nothing");
    assert_absent(
        &files,
        &["context_assemble", "\"assemble\"", "ContextEngine"],
        "SEC6",
    );
}

// T-CI3 — SEC1: context holds a `GraphViewHandle`, never the trait object
// or a write method.
#[test]
fn sec1_context_never_names_project_graph_directly() {
    assert_absent(
        &context_files(),
        &[
            "dyn ProjectGraph",
            "SqliteProjectGraph",
            "rebuild(",
            "record_diagnostic",
            "record_fix",
            "apply_incremental",
        ],
        "SEC1",
    );
}

// T-CI4 — D1: reserved-domain identifiers appear only in the enum
// declaration, `ALL` and `label` (all in types.rs); the manifest loop uses
// `DomainId::ALL` and names no variant.
#[test]
fn d1_reserved_domains_appear_only_in_the_enum_and_the_empty_arm() {
    let reserved = [
        "Architecture",
        "Scratchpad",
        "LongTerm",
        "Planning",
        "ProjectLegacyAlias",
    ];
    for file in context_files() {
        let text = std::fs::read_to_string(&file).unwrap();
        let is_types = file.file_name().is_some_and(|f| f == "types.rs");
        for name in reserved {
            let count = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(name))
                .count();
            if is_types {
                assert_eq!(
                    count, 3,
                    "D1: `{name}` must appear exactly 3 times in types.rs \
                     (enum declaration, ALL, label), found {count}"
                );
            } else {
                assert_eq!(
                    count,
                    0,
                    "D1: `{name}` leaked into {} — reserved domains are \
                     structurally inert outside types.rs",
                    file.display()
                );
            }
        }
    }
}

// T-CI5 — SEC7: no fuzzy-recall / vector-index identifiers. (The negative
// assertion below is the one permitted occurrence of these strings.)
#[test]
fn sec7_no_embedding_index_identifiers() {
    assert_absent(
        &context_files(),
        &[
            "embed",
            "cosine",
            "vector_store",
            "ann_index",
            "faiss",
            "hnsw",
        ],
        "SEC7",
    );
}

// T-CI6 — D14 as amended by A-0012-1a: Symbol / Diagnostics / Subgraph plus
// the bounded impact reads (Callers / Refs). Impls and SimilarFixes remain
// forbidden in context/**.
#[test]
fn d14_only_the_amended_graph_query_kinds_are_constructed() {
    assert_absent(
        &context_files(),
        &["GraphQuery::Impls", "GraphQuery::SimilarFixes"],
        "D14 (A-0012-1a)",
    );
}

// T-CI7 — C3: the dependency is one-way, context → router::types.
#[test]
fn c3_router_does_not_depend_on_context() {
    let dir = crate_root().join("src/router");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    assert!(!files.is_empty(), "src/router walk found nothing");
    assert_absent(&files, &["crate::context", "super::context"], "C3");
}

// T-CI8 — E1: a graph or store failure is a Degradation, never an error.
#[test]
fn e1_no_from_graph_error_or_store_error_for_context_error() {
    let dir = crate_root().join("src");
    let mut files = Vec::new();
    walk_rs_files(&dir, &mut files);
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for needle in [
            "From<GraphError> for ContextError",
            "From<StoreError> for ContextError",
        ] {
            assert!(
                !text.contains(needle),
                "E1: `{needle}` found in {}",
                file.display()
            );
        }
    }
}

// T-CI9 — A14/SEC9: the context module never writes anything, anywhere.
#[test]
fn a14_context_never_writes() {
    assert_absent(
        &context_files(),
        &[
            "fs::write",
            "create_dir",
            "File::create",
            "OpenOptions",
            "remove_file",
            ".put(",
        ],
        "A14",
    );
}

// OB1 half of AC 68: no session events, no DecisionRecords from context.
#[test]
fn ob1_context_appends_no_events_and_no_decisions() {
    assert_absent(
        &context_files(),
        &[
            "EventSink",
            "DecisionLog",
            "DecisionRecord",
            "append_session",
        ],
        "OB1",
    );
}

// T-CI10 — C7: `PromptPack` still declares exactly messages, citations,
// domains, in that order.
#[test]
fn c7_prompt_pack_shape_is_unchanged() {
    let types = crate_root().join("src/router/types.rs");
    let text = std::fs::read_to_string(&types).unwrap();
    let start = text
        .find("pub struct PromptPack {")
        .expect("PromptPack declaration present");
    let block = &text[start..];
    let end = block.find("\n}").expect("PromptPack block closes");
    let block = &block[..end];
    let fields: Vec<&str> = block
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            l.strip_prefix("pub ")
                .filter(|_| !l.starts_with("pub struct"))
                .and_then(|rest| rest.split(':').next())
        })
        .collect();
    assert_eq!(
        fields,
        vec!["messages", "citations", "domains"],
        "C7: PromptPack field set/order changed"
    );
}
