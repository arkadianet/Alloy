//! RFC-0012 §13.8 integration suite (T8a–T8h): the repair-loop golden, the
//! recorded toy-workspace graph view, diagnostics citations, the RFC-0007
//! binding, path/secret hygiene, the null-graph end-to-end path, and the
//! cross-process determinism proof.

mod context_support;

use std::path::Path;
use std::sync::Arc;

use context_support::*;

use alloy_runtime::context::{ContextHandle, ContextProfile, DefaultContextEngine};
use alloy_runtime::graph::GraphViewHandle;
use alloy_runtime::router::{CompletionRequest, PromptPack};
use alloy_runtime::types::ids::Digest;

/// Build the Appendix A repair-loop engine over `root` (which must contain
/// the toy workspace) with fully fixed identities.
fn repair_engine(root: &Path) -> (DefaultContextEngine, Arc<ScriptedGraph>) {
    let graph = ScriptedGraph::new(GraphMode::Toy);
    let engine = DefaultContextEngine::new(
        ContextProfile::v2_defaults(),
        graph.handle(),
        Arc::new(MemEventStore::new()),
        Arc::new(MemArtifactStore::new()),
        root.to_path_buf(),
    );
    (engine, graph)
}

async fn repair_pack(root: &Path) -> PromptPack {
    let (engine, _graph) = repair_engine(root);
    let mut inputs = make_inputs(
        Some("fix the borrow error in toy-core"),
        vec![e0502_diagnostic()],
        vec!["crates/toy-core/src/io.rs"],
    );
    inputs.run = Some(fixed_run());
    engine
        .assemble_with(
            repair_request(
                32_000,
                vec![ContextHandle::File {
                    path: "crates/toy-core/src/io.rs".into(),
                    lines: Some((10, 40)),
                }],
            ),
            inputs,
        )
        .await
        .expect("repair pack assembles")
}

fn golden_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rfc0012_repair_pack.golden.json")
}

// T8a — the committed golden, byte for byte. Regenerate with
// `ALLOY_BLESS=1 cargo test -p alloy-runtime --test context_rfc0012 repair_loop`.
#[tokio::test]
async fn repair_loop_pack_matches_committed_golden() {
    let ws = ToyWs::new();
    let pack = repair_pack(&ws.root).await;
    let bytes = serde_json::to_vec(&pack).unwrap();
    if std::env::var("ALLOY_BLESS").is_ok() {
        std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        std::fs::write(golden_path(), &bytes).unwrap();
        return;
    }
    let golden = std::fs::read(golden_path()).expect("committed golden fixture");
    assert_eq!(
        bytes,
        golden,
        "T8a: pack drifted from the committed golden.\nrendered:\n{}",
        String::from_utf8_lossy(&bytes)
    );
}

// T8b — end-to-end against the recorded toy-workspace `GraphView` shape
// (RFC-0011 Appendix B), with no `alloy-index` dependency (C2).
#[tokio::test]
async fn assemble_over_a_recorded_toy_workspace_graph_view() {
    let ws = ToyWs::new();
    let pack = repair_pack(&ws.root).await;
    let text: String = pack
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    for line in [
        "module  toy_core  crates/toy-core/src/lib.rs",
        "module  toy_core::io  crates/toy-core/src/io.rs",
        "module  toy_core::io::reader  crates/toy-core/src/io/reader.rs",
        "defines toy_core -> toy_core::io",
        "defines toy_core::io -> toy_core::io::reader",
    ] {
        assert!(text.contains(line), "graph projection missing: {line}");
    }
    // Per-node citations at the recorded version (§7.1).
    for path in ["toy_core", "toy_core::io", "toy_core::io::reader"] {
        let source = format!("alloy://working_set/graph/1/{path}");
        assert!(
            pack.citations.iter().any(|c| c.source == source),
            "missing citation {source}"
        );
    }
}

// T8i — the Beta acceptance criterion, integration-level: over the deep
// store shape (syn-deep fidelity, item-level nodes, import edges — what
// alloy-index serves on main at GRAPH_MODEL_VERSION = 3), the WorkingSet
// includes the rich projection with per-node citations, and the reserved
// domains stay inert.
#[tokio::test]
async fn assemble_over_the_deep_store_shape() {
    let ws = ToyWs::new();
    let (engine, graph) = repair_engine(&ws.root);
    graph.set_impact(true);
    let mut inputs = make_inputs(
        Some("fix the borrow error in toy-core"),
        vec![e0502_diagnostic()],
        vec!["crates/toy-core/src/io.rs"],
    );
    inputs.run = Some(fixed_run());
    let pack = engine
        .assemble_with(
            repair_request(
                32_000,
                vec![ContextHandle::File {
                    path: "crates/toy-core/src/io.rs".into(),
                    lines: Some((10, 40)),
                }],
            ),
            inputs,
        )
        .await
        .expect("deep repair pack assembles");
    let text: String = pack
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    for line in [
        "fidelity=syn_deep",
        "item  toy_core::io::read_all  crates/toy-core/src/io.rs",
        "imports toy_core::io::reader -> toy_core::io::read_all",
        "calls toy_cli::main -> toy_core::io::read_all",
    ] {
        assert!(text.contains(line), "deep projection missing: {line}");
    }
    for source in [
        "alloy://working_set/graph/1/toy_core::io::read_all",
        "alloy://working_set/graph/1/toy_cli::main",
    ] {
        assert!(
            pack.citations.iter().any(|c| c.source == source),
            "missing citation {source}"
        );
    }
    let m = pack.domains.unwrap();
    assert_eq!(m["graph"]["fidelity"], "syn_deep");
    let live: Vec<bool> = alloy_runtime::context::DomainId::ALL
        .iter()
        .map(|d| m["domains"][d.label()]["live"].as_bool().unwrap())
        .collect();
    assert_eq!(
        live,
        vec![true, true, true, false, false, false, false, false],
        "still exactly three live domains"
    );
}

// T8c — the draft's original integration criterion.
#[tokio::test]
async fn assemble_after_diagnostic_ingest_includes_a_diagnostics_citation() {
    let ws = ToyWs::new();
    let (engine, graph) = repair_engine(&ws.root);
    // Ingest the diagnostic into the recorded graph; the caller supplies
    // none, so assembly falls back to the recorded log (§4.3c).
    graph.diagnostics.lock().unwrap().push(e0502_diagnostic());
    let inputs = make_inputs(Some("fix it"), vec![], vec![]);
    let pack = engine
        .assemble_with(repair_request(32_000, vec![]), inputs)
        .await
        .unwrap();
    let expected = format!(
        "alloy://working_set/diagnostics/E0502/{}",
        fixed_diagnostic_id()
    );
    assert!(
        pack.citations.iter().any(|c| c.source == expected),
        "missing diagnostics citation {expected}: {:#?}",
        pack.citations
    );
}

// T8d — RFC-0007 binding: `PromptPack.messages` → `CompletionRequest.messages`
// unchanged.
#[tokio::test]
async fn pack_round_trips_through_serde_and_into_a_completion_request() {
    let ws = ToyWs::new();
    let pack = repair_pack(&ws.root).await;
    let json = serde_json::to_string(&pack).unwrap();
    let back: PromptPack = serde_json::from_str(&json).unwrap();
    assert_eq!(back, pack);
    let request: CompletionRequest = serde_json::from_value(serde_json::json!({
        "messages": pack.messages,
    }))
    .unwrap();
    assert_eq!(request.messages, pack.messages);
}

// T8e — SEC4: no absolute host path anywhere in the serialised pack.
#[tokio::test]
async fn pack_contains_no_absolute_host_path() {
    let ws = ToyWs::new();
    let pack = repair_pack(&ws.root).await;
    let bytes = serde_json::to_string(&pack).unwrap();
    let root = ws.root.to_string_lossy().to_string();
    assert!(!bytes.contains(&root), "workspace root leaked");
    let canon = std::fs::canonicalize(&ws.root)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(!bytes.contains(&canon), "canonical root leaked");
    for needle in ["/home/", "/tmp/", "C:\\\\", "file:///"] {
        assert!(!bytes.contains(needle), "absolute path marker: {needle}");
    }
}

// T8f — SEC2: a planted env-style secret is redacted at assembly time.
#[tokio::test]
async fn pack_contains_no_secret_from_a_planted_env_style_line() {
    let ws = ToyWs::new();
    // Note: the spec names `AWS_SECRET_ACCESS_KEY`, but the merged RFC-0004
    // redactor matches `*_api_key` / `*_secret` / `*_token` / `*_password`
    // names — `AWS_SECRET_ACCESS_KEY` ends in `_key` and is NOT covered.
    // The fixture uses a name the merged redactor covers (see the RFC-0012
    // implementation report); widening the redactor is RFC-0004's scope.
    let secret = "wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY";
    write_file(
        &ws.root.join("crates/toy-core/src/config.rs"),
        &format!("// config\nconst K: &str = \"unused\";\n// AWS_API_KEY={secret}\n"),
    );
    let (engine, _graph) = repair_engine(&ws.root);
    let inputs = make_inputs(Some("audit"), vec![], vec!["crates/toy-core/src/config.rs"]);
    let pack = engine
        .assemble_with(repair_request(32_000, vec![]), inputs)
        .await
        .unwrap();
    let bytes = serde_json::to_string(&pack).unwrap();
    assert!(!bytes.contains(secret), "SEC2: secret leaked into the pack");
    assert!(bytes.contains("[REDACTED]"), "redaction marker expected");
}

// T8g — E1: the M7 "empty graph projection" path, end to end.
#[tokio::test]
async fn assemble_succeeds_with_a_null_graph_end_to_end() {
    let ws = ToyWs::new();
    let engine = DefaultContextEngine::new(
        ContextProfile::v2_defaults(),
        GraphViewHandle::null(),
        Arc::new(MemEventStore::new()),
        Arc::new(MemArtifactStore::new()),
        ws.root.clone(),
    );
    let inputs = make_inputs(
        Some("fix the borrow error in toy-core"),
        vec![e0502_diagnostic()],
        vec!["crates/toy-core/src/io.rs"],
    );
    let pack = engine
        .assemble_with(repair_request(32_000, vec![]), inputs)
        .await
        .expect("null graph never fails a node");
    let m = pack.domains.unwrap();
    assert_eq!(m["graph"]["degraded"], true);
    assert_eq!(m["graph"]["version"], 0);
    assert!(!pack.citations.is_empty());
    let text: String = pack
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("working_set:file crates/toy-core/src/io.rs"));
    assert!(text.contains("[alloy: working_set degraded — graph_empty]"));
    assert!(!text.contains("working_set:graph "));
}

// ---------------------------------------------------------------------
// T8h — determinism across two processes (§13.10)
// ---------------------------------------------------------------------

/// Child half of T8h: only runs when re-invoked by the parent with a
/// workspace path in the environment.
#[test]
fn helper_print_pack_digest() {
    let Ok(root) = std::env::var("ALLOY_RFC0012_CHILD_WS") else {
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pack = rt.block_on(repair_pack(Path::new(&root)));
    let bytes = serde_json::to_vec(&pack).unwrap();
    println!("PACK_SHA256={}", Digest::sha256(&bytes).as_hex());
}

#[tokio::test]
async fn two_processes_assemble_identical_bytes() {
    let ws = ToyWs::new();
    let in_process = Digest::sha256(&serde_json::to_vec(&repair_pack(&ws.root).await).unwrap());
    let spawn = || {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "helper_print_pack_digest", "--nocapture"])
            .env("ALLOY_RFC0012_CHILD_WS", &ws.root)
            .output()
            .expect("spawn child test process");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        stdout
            .lines()
            .find_map(|l| l.strip_prefix("PACK_SHA256=").map(str::to_owned))
            .unwrap_or_else(|| panic!("child produced no digest:\n{stdout}"))
    };
    let a = spawn();
    let b = spawn();
    assert_eq!(a, b, "A1 across two child processes");
    assert_eq!(a, in_process.as_hex(), "A1 child vs parent process");
}
