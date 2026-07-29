//! `RustBackend` — the Rust [`LanguageBackend`] (RFC-0014 §3.4, LC2).
//!
//! Lives here rather than in `alloy-runtime` because the deep index is the
//! bulk of its work and needs this crate's ingest internals; the seam it
//! implements stays in `alloy-runtime::lang` (LC1). Cargo is reached only
//! through the injected [`ToolchainRunner`] (DN2/SC1); nothing here spawns
//! a process or opens a socket.

pub(crate) mod pass;

use std::path::Path;
use std::sync::Arc;

use alloy_runtime::lang::{
    LangError, LanguageBackend, LanguageManifest, RustToolchain, Scope, TestReport, TestSelector,
    TextEdit, ToolchainRunner,
};
use alloy_runtime::types::diagnostic::DiagnosticEvent;
use alloy_runtime::types::ids::{CapabilityId, LanguageId};
#[cfg(test)]
use alloy_runtime::GraphFidelity;
use alloy_runtime::{ProjectGraph, SemanticEditOp};
use async_trait::async_trait;

/// LB5: failing test names are best-effort and capped.
const MAX_FAILURE_NAMES: usize = 200;

/// Rust backend. Constructed with its collaborators; construction performs
/// no I/O (LB12).
pub struct RustBackend {
    runner: Arc<dyn ToolchainRunner>,
    /// Statically-known pin hints handed in by the composition root (TC4);
    /// see [`read_toolchain_hints`].
    toolchain_hints: Vec<String>,
    /// Probe result, fetched once and cached (TC1, AC 49).
    toolchain: tokio::sync::OnceCell<RustToolchain>,
}

impl RustBackend {
    /// Construct over the injected toolchain seam. No I/O (LB12).
    #[must_use]
    pub fn new(runner: Arc<dyn ToolchainRunner>) -> Self {
        Self {
            runner,
            toolchain_hints: Vec::new(),
            toolchain: tokio::sync::OnceCell::new(),
        }
    }

    /// Attach pin hints the composition root read from the workspace (TC4).
    #[must_use]
    pub fn with_toolchain_hints(mut self, hints: Vec<String>) -> Self {
        self.toolchain_hints = hints;
        self
    }

    /// Toolchain identity via [`ToolchainRunner::probe`], fetched once per
    /// backend instance and cached (TC1); reported alongside the rebuild
    /// decision record by the CLI (TC6).
    pub async fn toolchain(&self) -> Result<RustToolchain, LangError> {
        // OnceCell keeps a success; failures are retried on the next call.
        self.toolchain
            .get_or_try_init(|| self.runner.probe())
            .await
            .cloned()
    }
}

impl std::fmt::Debug for RustBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustBackend")
            .field("toolchain_hints", &self.toolchain_hints)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl LanguageBackend for RustBackend {
    fn id(&self) -> LanguageId {
        LanguageId::new("rust").expect("'rust' is a valid catalog id")
    }

    fn manifest(&self) -> LanguageManifest {
        LanguageManifest::new(
            self.id(),
            vec!["rs".into()],
            vec!["Cargo.toml".into()],
            self.toolchain_hints.clone(),
            // RS4: even the advertised index fidelity comes from the one
            // seam function over the model version this build ingests at.
            crate::migrate::fidelity_for_model_version(crate::migrate::GRAPH_MODEL_VERSION),
            Vec::new(), // LE1: Beta lowers nothing.
        )
    }

    #[tracing::instrument(skip_all, fields(language = "rust"), name = "lang.detect")]
    async fn detect(&self, root: &Path) -> Result<bool, LangError> {
        // TC2: pure filesystem — the same predicate RFC-0011 §6.3 uses.
        // A foreign or malformed root is `false`, never an error.
        let manifest = root.join("Cargo.toml");
        let Ok(text) = tokio::fs::read_to_string(&manifest).await else {
            return Ok(false);
        };
        let Ok(doc) = text.parse::<toml::Value>() else {
            return Ok(false);
        };
        Ok(doc.get("workspace").is_some() || doc.get("package").is_some())
    }

    #[tracing::instrument(skip_all, fields(language = "rust"), name = "lang.index")]
    async fn index(&self, root: &Path, graph: &dyn ProjectGraph) -> Result<(), LangError> {
        if !self.detect(root).await? {
            return Err(LangError::NotDetected {
                language: "rust".into(),
                path: root.display().to_string(),
            });
        }
        // §5.5: the parse rides `rebuild`'s ingest path — same single-writer
        // transaction, no private connection, no SQL from here. The store
        // logs the per-pass counts through its own report (LO2).
        let started = std::time::Instant::now();
        let version = graph.rebuild(root).await?;
        tracing::info!(
            graph_version = version.0,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "deep index complete"
        );
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(language = "rust"), name = "lang.diagnostics")]
    async fn diagnostics(
        &self,
        root: &Path,
        scope: Scope,
    ) -> Result<Vec<DiagnosticEvent>, LangError> {
        let stdout = self.runner.check_json(root, &scope).await?;
        // DN1: the one parser RFC-0010 shipped — never a second one (RS7).
        Ok(alloy_runtime::parse_rustc_diagnostics(&stdout))
    }

    #[tracing::instrument(skip_all, fields(language = "rust"), name = "lang.test")]
    async fn test(&self, root: &Path, sel: TestSelector) -> Result<TestReport, LangError> {
        let (ok, output) = self.runner.test(root, &sel).await?;
        Ok(parse_test_report(ok, &output))
    }

    async fn lower_edit(&self, op: &SemanticEditOp) -> Result<Vec<TextEdit>, LangError> {
        // LE1–LE3: Beta lowers nothing; every op fails closed with its
        // stable tag so no caller has to translate (LE2).
        Err(LangError::UnsupportedOp {
            op: op.op_tag().to_string(),
        })
    }

    fn capabilities_extended(&self) -> Vec<CapabilityId> {
        Vec::new()
    }
}

/// Read statically-known toolchain pin hints for [`LanguageManifest`] (TC4):
/// `rust-toolchain.toml`'s channel and the root `[package] rust-version`.
/// A free function so `RustBackend::new` and `manifest()` stay I/O-free
/// (LB12); the composition root calls it once and passes the result to
/// [`RustBackend::with_toolchain_hints`].
#[must_use]
pub fn read_toolchain_hints(root: &Path) -> Vec<String> {
    let mut hints = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("rust-toolchain.toml")) {
        if let Ok(doc) = text.parse::<toml::Value>() {
            if let Some(channel) = doc
                .get("toolchain")
                .and_then(|t| t.get("channel"))
                .and_then(|c| c.as_str())
            {
                hints.push(format!("channel={channel}"));
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) {
        if let Ok(doc) = text.parse::<toml::Value>() {
            for table in [
                doc.get("package"),
                doc.get("workspace").and_then(|w| w.get("package")),
            ] {
                if let Some(rv) = table
                    .and_then(|p| p.get("rust-version"))
                    .and_then(|v| v.as_str())
                {
                    hints.push(format!("rust-version={rv}"));
                }
            }
        }
    }
    hints
}

/// LB5: summarise a libtest run from its **stable human summary lines**,
/// never the unstable JSON. Counts sum across test binaries; unrecognised
/// output leaves them `None`. `ok` always comes from the exit status.
fn parse_test_report(ok: bool, output: &str) -> TestReport {
    let mut passed: Option<u32> = None;
    let mut failed: Option<u32> = None;
    let mut ignored: Option<u32> = None;
    let mut failures: Vec<String> = Vec::new();
    let mut in_failure_list = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "failures:" {
            in_failure_list = true;
            continue;
        }
        if in_failure_list {
            if trimmed.is_empty() || trimmed.starts_with("test result:") {
                in_failure_list = false;
            } else if line.starts_with("    ") && failures.len() < MAX_FAILURE_NAMES {
                failures.push(trimmed.to_string());
            }
            // fall through: a summary line also ends the list.
        }
        if let Some(rest) = trimmed.strip_prefix("test result:") {
            // `ok. 3 passed; 1 failed; 0 ignored; …` — scan `<count> <what>`
            // word pairs; the leading status word is skipped naturally.
            let words: Vec<&str> = rest
                .split_whitespace()
                .map(|w| w.trim_matches([';', ',', '.']))
                .collect();
            for pair in words.windows(2) {
                let Ok(count) = pair[0].parse::<u32>() else {
                    continue;
                };
                let acc = match pair[1] {
                    "passed" => &mut passed,
                    "failed" => &mut failed,
                    "ignored" => &mut ignored,
                    _ => continue,
                };
                *acc = Some(acc.unwrap_or(0) + count);
            }
        }
    }
    failures.sort();
    failures.dedup();
    // raw_artifact: stored by the caller, when it stores one.
    TestReport::new(ok, passed, failed, ignored, failures, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::types::ids::CrateId;

    /// A runner that panics on any call — proves `id()`/`manifest()` and
    /// construction perform no I/O and touch no collaborator (LB12).
    struct PanickingRunner;

    #[async_trait]
    impl ToolchainRunner for PanickingRunner {
        async fn check_json(&self, _root: &Path, _scope: &Scope) -> Result<String, LangError> {
            panic!("manifest()/id() must not reach the toolchain (LB12)")
        }
        async fn test(
            &self,
            _root: &Path,
            _sel: &TestSelector,
        ) -> Result<(bool, String), LangError> {
            panic!("manifest()/id() must not reach the toolchain (LB12)")
        }
        async fn probe(&self) -> Result<RustToolchain, LangError> {
            panic!("manifest()/id() must not reach the toolchain (LB12)")
        }
    }

    fn backend() -> RustBackend {
        RustBackend::new(Arc::new(PanickingRunner))
    }

    // T1 — LB12: static, I/O-free manifest.
    #[test]
    fn manifest_is_static_and_io_free() {
        let b = backend().with_toolchain_hints(vec!["channel=1.97.1".into()]);
        assert_eq!(b.id().as_str(), "rust");
        let m = b.manifest();
        assert_eq!(m.id.as_str(), "rust");
        assert_eq!(m.file_extensions, vec!["rs".to_string()]);
        assert_eq!(m.root_markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(m.toolchain_hints, vec!["channel=1.97.1".to_string()]);
        assert_eq!(m.index_fidelity, GraphFidelity::SynDeep);
        assert!(m.lowerable_ops.is_empty(), "LE1: Beta lowers nothing");
    }

    // T2 — TC2: detect is a pure filesystem predicate.
    #[tokio::test]
    async fn detect_true_for_workspace_and_package_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        assert!(backend().detect(&ws).await.unwrap());
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"solo\"\n").unwrap();
        assert!(backend().detect(&ws).await.unwrap());
    }

    #[tokio::test]
    async fn detect_false_for_a_node_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        assert!(!backend().detect(dir.path()).await.unwrap());
        // A Cargo.toml with neither [workspace] nor [package] is not a root.
        std::fs::write(dir.path().join("Cargo.toml"), "[dependencies]\n").unwrap();
        assert!(!backend().detect(dir.path()).await.unwrap());
    }

    // T3 — LE1/LE2: all nine ops fail closed with the matching op_tag.
    #[tokio::test]
    async fn lower_edit_rejects_all_nine_ops_with_matching_op_tag() {
        let ops = vec![
            SemanticEditOp::RenameType {
                from_path: "a::B".into(),
                to_name: "C".into(),
                update_references: true,
            },
            SemanticEditOp::UpdateImports {
                file: "src/lib.rs".into(),
                add: vec![],
                remove: vec![],
            },
            SemanticEditOp::ReplaceBody {
                item_path: "a::f".into(),
                new_body: String::new(),
            },
            SemanticEditOp::InsertImpl {
                file: "src/lib.rs".into(),
                type_path: "a::B".into(),
                body: String::new(),
            },
            SemanticEditOp::AddMethod {
                item_path: "a::B".into(),
                method_source: String::new(),
            },
            SemanticEditOp::MoveModule {
                from_path: "a::b".into(),
                to_path: "a::c".into(),
            },
            SemanticEditOp::ExtractTrait {
                type_path: "a::B".into(),
                trait_name: "T".into(),
                method_names: vec![],
            },
            SemanticEditOp::SplitCrate {
                source_crate: "a".into(),
                new_crate: "b".into(),
                move_paths: vec![],
            },
            SemanticEditOp::AddField {
                type_path: "a::B".into(),
                field_source: String::new(),
            },
        ];
        assert_eq!(ops.len(), 9, "LE2: exactly the nine SemanticEditOp tags");
        let b = backend();
        for op in ops {
            let err = b.lower_edit(&op).await.unwrap_err();
            match err {
                LangError::UnsupportedOp { op: tag } => assert_eq!(tag, op.op_tag()),
                other => panic!("expected UnsupportedOp, got {other:?}"),
            }
        }
        assert!(b.capabilities_extended().is_empty());
    }

    // AC 48 — LB5: summary-line parsing.
    #[test]
    fn test_report_parses_summary_lines_and_failure_names() {
        let output = "\nrunning 3 tests\ntest io::reads ... FAILED\n\nfailures:\n\n---- io::reads stdout ----\nboom\n\nfailures:\n    io::reads\n\ntest result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\ntest result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let report = parse_test_report(false, output);
        assert!(!report.ok);
        assert_eq!(report.passed, Some(6), "counts sum across binaries");
        assert_eq!(report.failed, Some(1));
        assert_eq!(report.ignored, Some(1));
        assert_eq!(report.failures, vec!["io::reads".to_string()]);
        assert_eq!(report.raw_artifact, None);
    }

    #[test]
    fn test_report_counts_are_none_when_summary_is_unrecognised() {
        let report = parse_test_report(true, "no libtest ran here\n");
        assert!(report.ok, "ok comes from the exit status alone");
        assert_eq!(report.passed, None);
        assert_eq!(report.failed, None);
        assert_eq!(report.ignored, None);
        assert!(report.failures.is_empty());
    }

    // TC4: pin hints are read by an explicit free function, never by
    // construction or `manifest()`.
    #[test]
    fn read_toolchain_hints_surfaces_channel_and_rust_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.97.1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"solo\"\nrust-version = \"1.97\"\n",
        )
        .unwrap();
        assert_eq!(
            read_toolchain_hints(dir.path()),
            vec![
                "channel=1.97.1".to_string(),
                "rust-version=1.97".to_string()
            ]
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(read_toolchain_hints(empty.path()).is_empty());
    }

    /// Scripted runner double for the probe-cache and diagnostics tests.
    struct ScriptedRunner {
        probes: std::sync::Mutex<u32>,
        check_stdout: String,
    }

    #[async_trait]
    impl ToolchainRunner for ScriptedRunner {
        async fn check_json(&self, _root: &Path, _scope: &Scope) -> Result<String, LangError> {
            Ok(self.check_stdout.clone())
        }
        async fn test(
            &self,
            _root: &Path,
            _sel: &TestSelector,
        ) -> Result<(bool, String), LangError> {
            Ok((
                true,
                "test result: ok. 1 passed; 0 failed; 0 ignored;".into(),
            ))
        }
        async fn probe(&self) -> Result<RustToolchain, LangError> {
            *self.probes.lock().unwrap() += 1;
            Ok(RustToolchain {
                channel: "1.97.1".into(),
                rustc_version: "rustc 1.97.1".into(),
                cargo_version: "cargo 1.97.1".into(),
                host_triple: None,
            })
        }
    }

    // AC 49 — TC1: probe once, cache per instance.
    #[tokio::test]
    async fn toolchain_probe_is_cached_per_backend_instance() {
        let runner = Arc::new(ScriptedRunner {
            probes: std::sync::Mutex::new(0),
            check_stdout: String::new(),
        });
        let b = RustBackend::new(Arc::clone(&runner) as Arc<dyn ToolchainRunner>);
        let a = b.toolchain().await.unwrap();
        let c = b.toolchain().await.unwrap();
        assert_eq!(a, c);
        assert_eq!(*runner.probes.lock().unwrap(), 1, "TC1: fetched once");
    }

    // AC 42/45 — DN1: diagnostics delegates to the one existing parser.
    #[tokio::test]
    async fn diagnostics_delegates_to_parse_rustc_diagnostics() {
        let line = serde_json::json!({
            "reason": "compiler-message",
            "target": {"name": "toy-core"},
            "message": {
                "code": {"code": "E0502"},
                "level": "error",
                "message": "cannot borrow",
                "spans": [],
                "children": [],
            }
        })
        .to_string();
        let runner = Arc::new(ScriptedRunner {
            probes: std::sync::Mutex::new(0),
            check_stdout: line.clone(),
        });
        let b = RustBackend::new(runner as Arc<dyn ToolchainRunner>);
        let got = b
            .diagnostics(
                Path::new("/ws"),
                Scope::Crate(CrateId::new("toy-core").unwrap()),
            )
            .await
            .unwrap();
        let want = alloy_runtime::parse_rustc_diagnostics(&line);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fingerprint, want[0].fingerprint);
        assert_eq!(got[0].code, want[0].code);
        assert_eq!(got[0].package.as_deref(), Some("toy-core"));
    }

    #[tokio::test]
    async fn test_wraps_the_runner_summary() {
        let runner = Arc::new(ScriptedRunner {
            probes: std::sync::Mutex::new(0),
            check_stdout: String::new(),
        });
        let b = RustBackend::new(runner as Arc<dyn ToolchainRunner>);
        let report = b.test(Path::new("/ws"), TestSelector::All).await.unwrap();
        assert!(report.ok);
        assert_eq!(report.passed, Some(1));
    }
}
