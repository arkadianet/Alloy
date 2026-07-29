//! LanguageBackend seam (RFC-0014, Architecture V2 §16.1).
//!
//! This module is the language-agnostic seam only (rule LC1, mirroring the
//! graph seam's C4): the trait, its value types, the [`ToolchainRunner`]
//! seam, the MCP-backed runner, and the [`LanguageRegistry`]. The Rust
//! implementation (`RustBackend` and the `syn` deep pass) lives in
//! `alloy-index` (rule LC2), which depends on this crate — never the other
//! way around (RFC-0011 rule C2). No `syn`, no SQL, no process execution
//! here (rules LC1, SC1).

mod runner;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use runner::McpToolchainRunner;

use crate::edit::SemanticEditOp;
use crate::graph::{GraphError, GraphFidelity, ProjectGraph};
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{ArtifactId, CapabilityId, CrateId, LanguageId};

// ---------------------------------------------------------------------
// The trait (V2 §16.1 verbatim — rule LB1)
// ---------------------------------------------------------------------

/// Language-specific services behind one seam (V2 §16.1, transcribed
/// unchanged — rule LB1). The control plane never learns what Rust is;
/// a second language earns the right to change this signature.
#[async_trait]
pub trait LanguageBackend: Send + Sync {
    /// Catalog id of this backend (`rust`). Synchronous and I/O-free (LB12).
    fn id(&self) -> LanguageId;
    /// Static capability description. Synchronous and I/O-free (LB12).
    fn manifest(&self) -> LanguageManifest;
    /// Whether `root` is a workspace this backend handles (TC2). Pure
    /// filesystem; `Ok(false)` — never an error — for a foreign root.
    async fn detect(&self, root: &Path) -> Result<bool, LangError>;
    /// Deep-index `root` into `graph` through the ingest path (§5.5).
    async fn index(&self, root: &Path, graph: &dyn ProjectGraph) -> Result<(), LangError>;
    /// Run the toolchain's check for `scope` and normalise its output
    /// (DN1–DN8). Returns events; the runtime host ingests them (DN5).
    async fn diagnostics(
        &self,
        root: &Path,
        scope: Scope,
    ) -> Result<Vec<DiagnosticEvent>, LangError>;
    /// Run the selected tests and summarise the outcome (LB5).
    async fn test(&self, root: &Path, sel: TestSelector) -> Result<TestReport, LangError>;
    /// Lower a semantic edit to text edits. Beta: fails closed for every
    /// op (LE1–LE3).
    async fn lower_edit(&self, op: &SemanticEditOp) -> Result<Vec<TextEdit>, LangError>;
    /// Extra capability ids this backend enables. Beta: empty.
    fn capabilities_extended(&self) -> Vec<CapabilityId>;
}

// ---------------------------------------------------------------------
// Value types (§3.2)
// ---------------------------------------------------------------------

/// Static, I/O-free description of a backend (LB2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LanguageManifest {
    /// Catalog id; `rust` for this backend.
    pub id: LanguageId,
    /// Extensions the backend claims, without the dot: `["rs"]`.
    pub file_extensions: Vec<String>,
    /// Manifest filenames that mark a root: `["Cargo.toml"]`.
    pub root_markers: Vec<String>,
    /// Optional toolchain pin hints found without running anything
    /// (`rust-toolchain.toml` channel, `[package] rust-version`).
    pub toolchain_hints: Vec<String>,
    /// Fidelity this backend's `index` produces when it succeeds.
    pub index_fidelity: GraphFidelity,
    /// `SemanticEditOp` tags this backend can lower. Beta: empty (LE1).
    pub lowerable_ops: Vec<String>,
}

impl LanguageManifest {
    /// Construct with every current field — the struct is
    /// `#[non_exhaustive]` (fields grow additively, OQ5), so backends in
    /// other crates build it through here.
    #[must_use]
    pub fn new(
        id: LanguageId,
        file_extensions: Vec<String>,
        root_markers: Vec<String>,
        toolchain_hints: Vec<String>,
        index_fidelity: GraphFidelity,
        lowerable_ops: Vec<String>,
    ) -> Self {
        Self {
            id,
            file_extensions,
            root_markers,
            toolchain_hints,
            index_fidelity,
            lowerable_ops,
        }
    }
}

/// Scope of a diagnostics request (LB3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Whole workspace.
    Workspace,
    /// One package.
    Crate(CrateId),
    /// The package owning a workspace-relative file; degrades to `Workspace`
    /// when ownership cannot be decided without the graph (DN3).
    File(String),
}

/// Test selection (LB4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestSelector {
    /// Everything the workspace defines.
    All,
    /// One package.
    Package(CrateId),
    /// A libtest name filter, passed through verbatim.
    Filter(String),
}

/// Result of `test` (LB5). Counts are `Option` because Beta parses the
/// stable human summary line, never the unstable libtest JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TestReport {
    /// Whether the run succeeded, from the tool's exit status.
    pub ok: bool,
    /// Parsed counts, `None` when the summary line was not recognised.
    pub passed: Option<u32>,
    /// Parsed failure count.
    pub failed: Option<u32>,
    /// Parsed ignored count.
    pub ignored: Option<u32>,
    /// Failing test names, best-effort and capped at 200.
    pub failures: Vec<String>,
    /// Raw captured output stored by the caller, when it stored one.
    pub raw_artifact: Option<ArtifactId>,
}

impl TestReport {
    /// Construct with every current field — the struct is
    /// `#[non_exhaustive]` (structured libtest fields arrive additively,
    /// OQ5), so backends in other crates build it through here.
    #[must_use]
    pub fn new(
        ok: bool,
        passed: Option<u32>,
        failed: Option<u32>,
        ignored: Option<u32>,
        failures: Vec<String>,
        raw_artifact: Option<ArtifactId>,
    ) -> Self {
        Self {
            ok,
            passed,
            failed,
            ignored,
            failures,
            raw_artifact,
        }
    }
}

/// A byte-range replacement in one file (LB6). V2 names `Vec<TextEdit>` as
/// `lower_edit`'s return type; the type itself is defined here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    /// Workspace-relative path, `/` separators.
    pub file: String,
    /// Byte offset of the replacement start.
    pub start: usize,
    /// Byte offset of the replacement end (exclusive).
    pub end: usize,
    /// Replacement text.
    pub replacement: String,
}

/// Backend failure (LB7).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LangError {
    /// `root` is not a workspace this backend handles.
    #[error("not a {language} workspace: {path}")]
    NotDetected {
        /// Language that failed to detect.
        language: String,
        /// Root that was probed.
        path: String,
    },
    /// A manifest could not be read or parsed.
    #[error("manifest {path}: {reason}")]
    Manifest {
        /// Workspace-relative manifest path.
        path: String,
        /// Parse failure reason.
        reason: String,
    },
    /// Source could not be parsed; carries the file, never the source text.
    #[error("parse {path}: {reason}")]
    Parse {
        /// Workspace-relative source path.
        path: String,
        /// Parse failure reason.
        reason: String,
    },
    /// The toolchain could not be reached or reported failure.
    #[error("toolchain: {0}")]
    Toolchain(String),
    /// A `SemanticEditOp` this backend cannot lower (LE2).
    #[error("unsupported op: {op}")]
    UnsupportedOp {
        /// The op's stable `op_tag()` string.
        op: String,
    },
    /// A cap in `IngestLimits` was exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// The graph rejected an ingest call.
    #[error("graph: {0}")]
    Graph(#[from] GraphError),
    /// Filesystem I/O.
    #[error("io: {0}")]
    Io(String),
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------
// The toolchain seam (§3.3)
// ---------------------------------------------------------------------

/// The single seam through which a language backend reaches a toolchain
/// (LB9). Implementations route to the MCP host under a `PermissionToken`;
/// no implementation spawns a process from `alloy-runtime` or `alloy-index`.
#[async_trait]
pub trait ToolchainRunner: Send + Sync {
    /// `cargo check --message-format=json` for `scope`; returns stdout.
    async fn check_json(&self, root: &Path, scope: &Scope) -> Result<String, LangError>;
    /// `cargo test` for `sel`; returns (exit-ok, captured output).
    async fn test(&self, root: &Path, sel: &TestSelector) -> Result<(bool, String), LangError>;
    /// `rustc -V` / `cargo -V` probe, cached by the caller.
    async fn probe(&self) -> Result<RustToolchain, LangError>;
}

/// Toolchain identity (LB10). Field-compatible with `alloy-eval`'s
/// `ToolchainRecord` by intent, not by reuse (TC5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustToolchain {
    /// Channel, e.g. `1.97.1`.
    pub channel: String,
    /// `rustc -V` output.
    pub rustc_version: String,
    /// `cargo -V` output.
    pub cargo_version: String,
    /// Host target triple when reported, else `None`.
    pub host_triple: Option<String>,
}

/// DN3: map a diagnostics [`Scope`] to the `cargo_check` `package` argument.
///
/// Returns `(package, degraded)`. `File` degrades to `Workspace` — guessing
/// a package from a path prefix is forbidden; degrading is correct.
#[must_use]
pub fn scope_package(scope: &Scope) -> (Option<String>, bool) {
    match scope {
        Scope::Workspace => (None, false),
        Scope::Crate(id) => (Some(id.as_str().to_string()), false),
        Scope::File(_) => (None, true),
    }
}

/// LB4: map a [`TestSelector`] to `(package, test_name_filter)` arguments
/// for the `cargo_test` tool.
#[must_use]
pub fn selector_args(sel: &TestSelector) -> (Option<String>, Option<String>) {
    match sel {
        TestSelector::All => (None, None),
        TestSelector::Package(id) => (Some(id.as_str().to_string()), None),
        TestSelector::Filter(f) => (None, Some(f.clone())),
    }
}

// ---------------------------------------------------------------------
// The registry (§3.4)
// ---------------------------------------------------------------------

/// Resolution of `Session.language_backends` to implementations (LB11).
/// Owned by the composition root (RFC-0015), never by a worker.
pub struct LanguageRegistry {
    // Keyed by the catalog name string: `LanguageId` deliberately stays a
    // `name_id!` without ordering (RS6), and the map must stay sorted.
    backends: BTreeMap<String, Arc<dyn LanguageBackend>>,
}

impl LanguageRegistry {
    /// Build a registry keyed by each backend's `id()`. A later backend with
    /// a duplicate id replaces the earlier one.
    #[must_use]
    pub fn new(backends: impl IntoIterator<Item = Arc<dyn LanguageBackend>>) -> Self {
        Self {
            backends: backends
                .into_iter()
                .map(|b| (b.id().as_str().to_string(), b))
                .collect(),
        }
    }

    /// Backend registered under `id`, when any.
    #[must_use]
    pub fn get(&self, id: &LanguageId) -> Option<Arc<dyn LanguageBackend>> {
        self.backends.get(id.as_str()).cloned()
    }

    /// Registered ids, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<LanguageId> {
        self.backends
            .keys()
            .map(|k| LanguageId::new(k.clone()).expect("registry keys are valid catalog ids"))
            .collect()
    }

    /// Resolve a session's declared backends (LB11). `Err` carries the first
    /// id with no registered backend; the composition root turns it into a
    /// session-create error — the declaration is validated, never guessed.
    pub fn resolve(&self, ids: &[LanguageId]) -> Result<Vec<Arc<dyn LanguageBackend>>, LanguageId> {
        ids.iter()
            .map(|id| self.get(id).ok_or_else(|| id.clone()))
            .collect()
    }
}

impl std::fmt::Debug for LanguageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::GraphNodeId;

    // T4 — DN3: scope → cargo arguments, including File → Workspace
    // degradation.
    #[test]
    fn scope_maps_to_expected_cargo_arguments() {
        assert_eq!(scope_package(&Scope::Workspace), (None, false));
        assert_eq!(
            scope_package(&Scope::Crate(CrateId::new("toy-core").unwrap())),
            (Some("toy-core".to_string()), false)
        );
        // DN3: File degrades to Workspace (no package) and reports it.
        assert_eq!(
            scope_package(&Scope::File("crates/toy-core/src/io.rs".into())),
            (None, true)
        );
    }

    #[test]
    fn selector_maps_to_expected_cargo_arguments() {
        assert_eq!(selector_args(&TestSelector::All), (None, None));
        assert_eq!(
            selector_args(&TestSelector::Package(CrateId::new("toy-core").unwrap())),
            (Some("toy-core".to_string()), None)
        );
        assert_eq!(
            selector_args(&TestSelector::Filter("io::reads".into())),
            (None, Some("io::reads".to_string()))
        );
    }

    // T5 — LB8: GraphError survives `From` without re-encoding.
    #[test]
    fn lang_error_wraps_graph_error_without_reencoding() {
        let e = LangError::from(GraphError::Busy);
        assert!(matches!(e, LangError::Graph(GraphError::Busy)));
        assert_eq!(e.to_string(), "graph: busy");
        let e = LangError::from(GraphError::Disabled);
        assert!(matches!(e, LangError::Graph(GraphError::Disabled)));
    }

    // T6 — LB2–LB6: value types round-trip serde.
    #[test]
    fn value_types_round_trip_serde() {
        let manifest = LanguageManifest {
            id: LanguageId::new("rust").unwrap(),
            file_extensions: vec!["rs".into()],
            root_markers: vec!["Cargo.toml".into()],
            toolchain_hints: vec!["channel=1.97.1".into()],
            index_fidelity: GraphFidelity::SynDeep,
            lowerable_ops: vec![],
        };
        let back: LanguageManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(manifest, back);

        for scope in [
            Scope::Workspace,
            Scope::Crate(CrateId::new("toy-core").unwrap()),
            Scope::File("src/lib.rs".into()),
        ] {
            let back: Scope =
                serde_json::from_str(&serde_json::to_string(&scope).unwrap()).unwrap();
            assert_eq!(scope, back);
        }
        for sel in [
            TestSelector::All,
            TestSelector::Package(CrateId::new("toy-core").unwrap()),
            TestSelector::Filter("io::".into()),
        ] {
            let back: TestSelector =
                serde_json::from_str(&serde_json::to_string(&sel).unwrap()).unwrap();
            assert_eq!(sel, back);
        }
        let report = TestReport {
            ok: false,
            passed: Some(3),
            failed: Some(1),
            ignored: None,
            failures: vec!["io::reads".into()],
            raw_artifact: None,
        };
        let back: TestReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(report, back);
        let edit = TextEdit {
            file: "src/lib.rs".into(),
            start: 0,
            end: 4,
            replacement: "pub ".into(),
        };
        let back: TextEdit = serde_json::from_str(&serde_json::to_string(&edit).unwrap()).unwrap();
        assert_eq!(edit, back);

        let toolchain = RustToolchain {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1".into(),
            cargo_version: "cargo 1.97.1".into(),
            host_triple: Some("x86_64-unknown-linux-gnu".into()),
        };
        let back: RustToolchain =
            serde_json::from_str(&serde_json::to_string(&toolchain).unwrap()).unwrap();
        assert_eq!(toolchain, back);
    }

    /// Minimal in-memory backend proving the V2 §16.1 trait shape is
    /// object-safe and callable as declared (LB1).
    struct NullBackend;

    #[async_trait]
    impl LanguageBackend for NullBackend {
        fn id(&self) -> LanguageId {
            LanguageId::new("rust").unwrap()
        }
        fn manifest(&self) -> LanguageManifest {
            LanguageManifest {
                id: self.id(),
                file_extensions: vec!["rs".into()],
                root_markers: vec!["Cargo.toml".into()],
                toolchain_hints: vec![],
                index_fidelity: GraphFidelity::SynDeep,
                lowerable_ops: vec![],
            }
        }
        async fn detect(&self, _root: &Path) -> Result<bool, LangError> {
            Ok(false)
        }
        async fn index(&self, _root: &Path, graph: &dyn ProjectGraph) -> Result<(), LangError> {
            // Exercise the `&dyn ProjectGraph` argument shape.
            let _ = graph.version().await?;
            Ok(())
        }
        async fn diagnostics(
            &self,
            _root: &Path,
            _scope: Scope,
        ) -> Result<Vec<DiagnosticEvent>, LangError> {
            Ok(vec![])
        }
        async fn test(&self, _root: &Path, _sel: TestSelector) -> Result<TestReport, LangError> {
            Ok(TestReport {
                ok: true,
                passed: None,
                failed: None,
                ignored: None,
                failures: vec![],
                raw_artifact: None,
            })
        }
        async fn lower_edit(&self, op: &SemanticEditOp) -> Result<Vec<TextEdit>, LangError> {
            Err(LangError::UnsupportedOp {
                op: op.op_tag().to_string(),
            })
        }
        fn capabilities_extended(&self) -> Vec<CapabilityId> {
            vec![]
        }
    }

    #[tokio::test]
    async fn trait_object_is_usable_with_a_dyn_project_graph() {
        let backend: Arc<dyn LanguageBackend> = Arc::new(NullBackend);
        let graph = crate::graph::NullProjectGraph;
        backend.index(Path::new("/ws"), &graph).await.unwrap();
        assert_eq!(backend.id().as_str(), "rust");
        let _ = GraphNodeId::parse("00000000-0000-8000-8000-000000000000").unwrap();
    }

    // LB11 / AC6: the registry resolves declarations; an unregistered id is
    // an error the composition root maps to a session-create failure.
    #[test]
    fn registry_resolves_declared_ids_and_rejects_unregistered_ones() {
        let registry = LanguageRegistry::new([Arc::new(NullBackend) as Arc<dyn LanguageBackend>]);
        let rust = LanguageId::new("rust").unwrap();
        let python = LanguageId::new("python").unwrap();

        assert!(registry.get(&rust).is_some());
        assert!(registry.get(&python).is_none());
        assert_eq!(registry.ids(), vec![rust.clone()]);

        assert_eq!(
            registry.resolve(std::slice::from_ref(&rust)).unwrap().len(),
            1
        );
        let missing = match registry.resolve(&[rust, python.clone()]) {
            Err(id) => id,
            Ok(_) => panic!("unregistered id must be a resolve error (LB11)"),
        };
        assert_eq!(missing, python);
        assert_eq!(
            format!("{registry:?}"),
            "LanguageRegistry { ids: [LanguageId(\"rust\")] }"
        );
    }
}
