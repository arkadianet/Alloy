//! RFC-0014 §12.4 CI-enforceable source greps (T20–T28), the mechanical
//! form of the §4 reserved-seam list. Same harness shape as the RFC-0011
//! grep suite: ordinary `#[test]`s that run under `cargo test --workspace`.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // crates/alloy-index -> workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn rs_files(rel: &str) -> Vec<PathBuf> {
    let dir = workspace_root().join(rel);
    let mut out = Vec::new();
    walk_rs_files(&dir, &mut out);
    assert!(!out.is_empty(), "grep walk found zero files under {rel}");
    out
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Lines of `text` before its first `#[cfg(test)]` marker.
fn non_test_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let test_start = text
        .lines()
        .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
        .unwrap_or(usize::MAX);
    text.lines()
        .enumerate()
        .take_while(move |(i, _)| *i < test_start)
}

/// `true` when a Cargo.toml line declares the dependency `name` (not a
/// substring hit like `syn` inside `async-trait`).
fn declares_dep(manifest: &str, name: &str) -> bool {
    manifest.lines().any(|l| {
        let t = l.trim_start();
        t.strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

const CRATE_MANIFESTS: &[&str] = &[
    "crates/alloy-runtime/Cargo.toml",
    "crates/alloy-tools/Cargo.toml",
    "crates/alloy-index/Cargo.toml",
    "crates/alloy-cli/Cargo.toml",
    "crates/alloy-eval/Cargo.toml",
];

// T20 / RS8, LC6, LC7: `syn` lives in the workspace table and alloy-index's
// manifest only, with the pinned minimal feature set.
#[test]
fn rs8_syn_present_in_alloy_index_manifest_only() {
    for rel in CRATE_MANIFESTS {
        let manifest = read(&workspace_root().join(rel));
        let expected = rel.contains("alloy-index");
        assert_eq!(
            declares_dep(&manifest, "syn"),
            expected,
            "{rel}: syn declaration presence must be {expected} (RS8/LC6)"
        );
    }
    let ws = read(&workspace_root().join("Cargo.toml"));
    let syn_line = ws
        .lines()
        .find(|l| l.trim_start().starts_with("syn "))
        .expect("workspace pins syn (LC7)");
    for needle in [
        "default-features = false",
        "\"full\"",
        "\"parsing\"",
        "\"clone-impls\"",
    ] {
        assert!(
            syn_line.contains(needle),
            "LC7 pin missing {needle}: {syn_line}"
        );
    }
    for forbidden in ["printing", "fold", "visit-mut", "extra-traits", "derive"] {
        assert!(
            !syn_line.contains(&format!("\"{forbidden}\"")),
            "LC7 forbids the {forbidden} feature: {syn_line}"
        );
    }
    // The index consumes the workspace pin; no proc-macro companions arrive.
    let index = read(&workspace_root().join("crates/alloy-index/Cargo.toml"));
    assert!(index.contains("syn = { workspace = true }"));
    for dep in ["quote", "proc-macro2"] {
        assert!(
            !declares_dep(&index, dep),
            "LC7: {dep} must not be a direct dependency of alloy-index"
        );
    }
}

// T21 / RS9: no lang crates, no cdylib, no dynamic loading; workspace stays
// at five crates.
#[test]
fn rs9_no_lang_crates_no_cdylib_no_dynamic_loading() {
    let crates_dir = workspace_root().join("crates");
    let mut names: Vec<String> = std::fs::read_dir(&crates_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "alloy-cli",
            "alloy-eval",
            "alloy-index",
            "alloy-runtime",
            "alloy-tools"
        ],
        "LC4: exactly five crates, none of them alloy-lang-*"
    );
    for rel in CRATE_MANIFESTS {
        let manifest = read(&workspace_root().join(rel));
        for needle in ["crate-type", "cdylib", "libloading", "dlopen"] {
            assert!(
                !manifest.contains(needle),
                "{rel}: {needle} is forbidden (RS9)"
            );
        }
    }
}

// T22 / RS2: the reserved item/imports kinds survive in the seam and in the
// v1 CHECK lists.
#[test]
fn rs2_item_and_imports_kinds_survive() {
    let seam = read(&workspace_root().join("crates/alloy-runtime/src/graph/mod.rs"));
    assert!(seam.contains("Item,"), "GraphNodeKind::Item must exist");
    assert!(
        seam.contains("Imports,"),
        "GraphEdgeKind::Imports must exist"
    );
    let migrate = read(&workspace_root().join("crates/alloy-index/src/migrate.rs"));
    assert!(
        migrate.contains("CHECK (kind IN ('workspace','crate','module','item'))"),
        "graph_nodes CHECK must still admit 'item' (RS2/SY2)"
    );
    assert!(
        migrate.contains("CHECK (kind IN ('defines','imports'))"),
        "graph_edges CHECK must still admit 'imports' (RS2/SY2)"
    );
}

// T23 / RS3: GraphFidelity still has exactly its three variants.
#[test]
fn rs3_graph_fidelity_still_has_three_variants() {
    let seam = read(&workspace_root().join("crates/alloy-runtime/src/graph/mod.rs"));
    let enum_start = seam
        .find("pub enum GraphFidelity")
        .expect("GraphFidelity declared in the seam");
    let body = &seam[enum_start..];
    let body = &body[..body.find('}').expect("enum body closes")];
    for variant in ["Manifest,", "SynDeep,", "Analyzer,"] {
        assert!(body.contains(variant), "GraphFidelity must keep {variant}");
    }
    let variants = body
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.ends_with(',') && !t.starts_with("//") && !t.starts_with('#')
        })
        .count();
    assert_eq!(variants, 3, "RS3: exactly three fidelity variants");
}

// T24 / RS4: in alloy-index, fidelity literals appear in exactly one
// function — the model-version seam.
#[test]
fn rs4_fidelity_literal_appears_in_exactly_one_function() {
    let mut files_with_literals = Vec::new();
    for file in rs_files("crates/alloy-index/src") {
        let text = read(&file);
        let hits: Vec<String> = non_test_lines(&text)
            .filter(|(_, l)| {
                let t = l.trim_start();
                (l.contains("GraphFidelity::Manifest")
                    || l.contains("GraphFidelity::SynDeep")
                    || l.contains("GraphFidelity::Analyzer"))
                    && !t.starts_with("//")
                    && !t.starts_with("//!")
            })
            .map(|(i, _)| format!("{}:{}", file.display(), i + 1))
            .collect();
        if !hits.is_empty() {
            files_with_literals.push((file.clone(), hits));
        }
    }
    assert_eq!(
        files_with_literals.len(),
        1,
        "RS4: fidelity literals in exactly one file: {files_with_literals:?}"
    );
    let (file, _) = &files_with_literals[0];
    assert!(
        file.ends_with("migrate.rs"),
        "RS4: the one producer is the migrate seam, got {}",
        file.display()
    );
    assert!(
        read(file).contains("fn fidelity_for_model_version"),
        "RS4/A-0014-4: the single deciding function exists"
    );
}

// T25 / RS5, RS6: LanguageId stays a name_id! catalog id; the session field
// and its non-empty validation survive.
#[test]
fn rs5_rs6_language_id_and_session_field_shape_unchanged() {
    let ids = read(&workspace_root().join("crates/alloy-runtime/src/types/ids.rs"));
    let squashed: String = ids.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        squashed.contains("LanguageId );") || squashed.contains("LanguageId);"),
        "RS6: LanguageId must stay declared through name_id!"
    );
    assert!(
        !ids.contains("enum Language "),
        "RS6: no `enum Language` shape"
    );
    let traits = read(&workspace_root().join("crates/alloy-runtime/src/session/traits.rs"));
    assert!(
        traits.contains("language_backends: Vec<LanguageId>"),
        "RS5: Session.language_backends stays Vec<LanguageId>"
    );
    let service = read(&workspace_root().join("crates/alloy-runtime/src/session/service.rs"));
    assert!(
        service.contains("language_backends must not be empty"),
        "RS5: the non-empty validation survives"
    );
}

// T26 / RS7: exactly one rustc-JSON parser. The `"compiler-message"`
// filtering literal appears in one non-test source file across the crates
// that can host a DiagnosticEvent parser; `alloy-eval`'s offline fixture
// extractor (RFC-0016, produces ExpectedDiagnostic, not DiagnosticEvent) is
// the pinned exemption.
#[test]
fn rs7_single_rustc_json_parser() {
    let mut hits = Vec::new();
    for rel in [
        "crates/alloy-runtime/src",
        "crates/alloy-tools/src",
        "crates/alloy-index/src",
        "crates/alloy-cli/src",
    ] {
        for file in rs_files(rel) {
            let text = read(&file);
            if non_test_lines(&text).any(|(_, l)| {
                l.contains("\"compiler-message\"") && !l.trim_start().starts_with("//")
            }) {
                hits.push(file);
            }
        }
    }
    assert_eq!(
        hits.len(),
        1,
        "RS7: exactly one compiler-message parser, got {hits:?}"
    );
    assert!(hits[0].ends_with("adapters/diagnostics.rs"));
    // ...and the parser stays re-exported at the crate root (RS7).
    let lib = read(&workspace_root().join("crates/alloy-runtime/src/lib.rs"));
    assert!(
        lib.contains("parse_rustc_diagnostics"),
        "RS7: crate-root re-export of parse_rustc_diagnostics survives"
    );
}

// T27 / RS10: the control plane stays language-agnostic.
#[test]
fn rs10_control_plane_has_no_language_field() {
    for rel in [
        "crates/alloy-runtime/src/adapters/capability.rs",
        "crates/alloy-runtime/src/scheduler",
    ] {
        let path = workspace_root().join(rel);
        let files = if path.is_dir() {
            let mut out = Vec::new();
            walk_rs_files(&path, &mut out);
            out
        } else {
            vec![path]
        };
        for file in files {
            let text = read(&file);
            for needle in ["LanguageBackend", "LanguageId", "LanguageRegistry"] {
                assert!(
                    !text.contains(needle),
                    "{}: {needle} must not reach the control plane (RS10)",
                    file.display()
                );
            }
        }
    }
    // No worker-facing language_backend MCP tool.
    for file in rs_files("crates/alloy-tools/src") {
        assert!(
            !read(&file).contains("language_backend"),
            "{}: no language_backend tool registration (RS10)",
            file.display()
        );
    }
}

// T28 / LC5, SC1: no process execution in the lang seam (RFC-0011's T12
// grep already covers alloy-index).
#[test]
fn lc5_no_process_execution_in_lang_seam() {
    for file in rs_files("crates/alloy-runtime/src/lang") {
        let text = read(&file);
        for needle in ["std::process::Command", "tokio::process"] {
            assert!(
                !text.contains(needle),
                "{}: process execution is forbidden in the lang seam (SC1)",
                file.display()
            );
        }
    }
}
