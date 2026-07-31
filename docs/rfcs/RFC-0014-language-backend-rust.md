# RFC-0014: LanguageBackend (Rust Module)

| Field | Value |
| --- | --- |
| **Status** | Implemented (RustBackend + syn deep pass; RA / SemanticEditOp lowering deferred) |
| **Author** | arkadianet |
| **Architecture** | [Alloy Architecture V2](../architecture/alloy-architecture-v2.md) (**frozen**) |
| **Milestone** | **Beta** — explicitly **not required for MVP** ([roadmap](../roadmap/IMPLEMENTATION-ROADMAP.md): "0014 not required for MVP") |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md), [RFC-0011](./RFC-0011-project-graph.md) |
| **Related RFCs** | [0008](./RFC-0008-edit-engine.md) `SemanticEditOp` envelope · [0010](./RFC-0010-scheduler-runtime-adapters.md) owns `parse_rustc_diagnostics` · [0012](./RFC-0012-context-engine.md) fidelity consumer · [0013](./RFC-0013-capability-registry-workers.md) stays language-agnostic · [0015](./RFC-0015-cli-profiles-config.md) composition root · [0016](./RFC-0016-eval-harness-holdout-gates.md) `ToolchainRecord` shape |
| **Effort** | 3–5 person-days |
| **Crates touched** | `alloy-runtime` (seam), `alloy-index` (implementation), `alloy-cli` (wiring) |

---

## 1. Overview

### 1.1 Purpose

Keep every language-specific fact behind one trait, `LanguageBackend`, so that the control plane — Scheduler, MCP host, Capability workers, Context Engine — never learns what Rust is. This RFC ships the **first and only** implementation: an internal Rust module. No dynamic loading, no `alloy-lang-*` packages, no `cdylib` (V2 §16, ADR F-15).

At **Beta** the backend does three concrete jobs:

1. **Deep index** — a `syn` item/import pass that fills the `Item` nodes and `Imports` edges RFC-0011 reserved as Stubs, flipping the graph from `GraphFidelity::Manifest` to `GraphFidelity::SynDeep`.
2. **Diagnostic normalisation** — a language-owned entry point that turns a cargo/rustc run into `Vec<DiagnosticEvent>`, *reusing* the parser RFC-0010 already shipped rather than adding a second one.
3. **Toolchain awareness** — a typed record of the channel/rustc/cargo/host triple the facts were derived under, so a stale graph or a mismatched recording is detectable.

### 1.2 This RFC is post-MVP. Read §4 first.

The [implementation roadmap](../roadmap/IMPLEMENTATION-ROADMAP.md) places RFC-0014 in **Beta**, alongside "0011 deep" and "0012 deep", and states plainly that MVP (M7) ships without it. Nothing in this document is an M7 work item.

Its **near-term value is therefore not code — it is §4, the reserved-seam list.** Six merged RFCs already carry seams that exist only because this backend is coming: `GraphNodeKind::Item`, `GraphEdgeKind::Imports`, the `'item'`/`'imports'` SQL `CHECK` values, `GraphFidelity::SynDeep`, `Session.language_backends: Vec<LanguageId>`, `LanguageId`'s catalog-id shape, and the `SemanticEditOp` envelope. Every one of them looks like dead code to a reviewer sweeping for MVP simplification. §4 names them, states what M7 must not do to them, and pins the statements to CI greps so the answer to "can we delete this?" is a failing test rather than an argument.

Writing that list was worth doing before the backend existed. The RustBackend + syn deep pass have since landed (see Status); the historical point of §1.2 remains: M7 must not delete the reserved seams while waiting on Beta.

### 1.3 Problem statement

Today `GraphNodeKind::Item` is documented as "**Stub** in MVP: never ingested (IN9)" and `GraphEdgeKind::Imports` as "**Stub** in MVP: never ingested (IN8)". `GraphFidelity::SynDeep` is marked "Reserved: `syn` item-level parse (Beta)" with no producer. `alloy-index`'s crate docs promise a Beta deepening that has no owner. `Session.language_backends` is a validated, persisted `Vec<LanguageId>` that nothing reads. `LanguageBackend::index(&self, root, graph: &dyn ProjectGraph)` is referenced by RFC-0011 §2.6 and Appendix E.3 as an obligation on an RFC that, until now, is 110 lines of sketch.

Either those seams get an owner with a normative spec, or M7 deletes them and Beta pays to reintroduce them — the exact "redesign" the frozen architecture forbids.

### 1.4 Scope

#### In scope (Beta)

- The `LanguageBackend` trait, verbatim from V2 §16.1, plus the value types it names that do not yet exist in the tree (`LanguageManifest`, `Scope`, `TestSelector`, `TestReport`, `TextEdit`, `LangError`).
- `RustBackend`: `id`, `manifest`, `detect`, `index`, `diagnostics`, `test`, `lower_edit` (fail-closed), `capabilities_extended`.
- The `syn` item/import pass, including the `GRAPH_MODEL_VERSION` 1 → 2 bump and truncate-and-re-ingest discipline.
- `RustToolchain` capture and the `ToolchainRunner` seam through which cargo is reached (never spawned directly).
- Crate placement, dependency direction, and the CI greps that keep both legal.
- The reserved-seam list (§4) and the RFC-0011 Beta contract (Appendix C).

#### Out of scope

| Item | Where it goes |
| --- | --- |
| Python / TypeScript backends, `cdylib`, dynamic loading, trait-freeze ceremony | Deferred ≥ 6 months of Rust dogfood (V2 §16.1 Evolution) |
| `SemanticEditOp` lowering beyond fail-closed | RFC-0008 future extension / M3 (one RA-backed op, e.g. `RenameType`) |
| rust-analyzer passthrough, `GraphFidelity::Analyzer`, rustc-grade `Refs`/`Impls`/`Callers` | Deferred; syn-grade answers live since A-0011-6 (RFC-0011 Q4–Q6 as amended) |
| Any Scheduler, MCP-host, or `CapabilityContext` change | **None required** — that is the point of the trait |
| Replacing RFC-0010's `VerifyCompileAdapter` | Never; see RS7 |
| New `GraphQuery` variants, new SQL tables | Forbidden by RFC-0011 E.3.3 / Q1 |

### 1.5 Beta day-1 scope (normative)

On the day this RFC merges: a `rebuild` of a Rust workspace produces Workspace/Crate/Module/**Item** nodes and **Defines**/**Imports** edges; `GraphView.fidelity` reads `SynDeep`; `RustBackend::diagnostics` returns the same `DiagnosticEvent`s the verify adapter already produces, obtained through the same sandboxed cargo path; `lower_edit` returns `LangError::UnsupportedOp { op }` for all nine `SemanticEditOp` tags; and `cargo test --workspace` is green with `syn` present in exactly one manifest.

### 1.6 Rule-ID index

| Family | Range | Topic | Section |
| --- | --- | --- | --- |
| **LB** | LB1–LB12 (incl. LB2a) | Trait, value types, backend behaviour | §3 |
| **RS** | RS1–RS13 | Reserved seams — what M7 must not do | §4 |
| **SY** | SY1–SY15 | `syn` deep pass, determinism, caps | §5 |
| **DN** | DN1–DN8 | Diagnostic normalisation | §6 |
| **TC** | TC1–TC6 (incl. TC1a) | Toolchain awareness | §7 |
| **LE** | LE1–LE4 | `lower_edit` fail-closed | §8 |
| **LC** | LC1–LC7 | Crate placement, dependencies, `syn` gating | §9 |
| **SC** | SC1–SC7 | Security posture | §10 |
| **LO** | LO1–LO5 | Observability | §11 |
| **T** | T1–T28 | Tests and CI greps | §12 |

---

## 2. Architecture integration

### 2.1 Relationship to Architecture V2

V2 numbers the language section **§16 "Language Plugin Architecture"**, with the trait under **§16.1**. RFC-0011 cites the same trait as "V2 §15" in two places (§1.3 obligations table, Appendix D); the section text is identical and §15 is Observability. **This RFC cites §16.1.** The mis-citation in RFC-0011 is cosmetic and is not corrected here (that file is out of scope).

The V2 §16.1 public interface, reproduced verbatim:

```rust
#[async_trait]
pub trait LanguageBackend: Send + Sync {
    fn id(&self) -> LanguageId;
    fn manifest(&self) -> LanguageManifest;
    async fn detect(&self, root: &Path) -> Result<bool, LangError>;
    async fn index(&self, root: &Path, graph: &dyn ProjectGraph) -> Result<(), LangError>;
    async fn diagnostics(&self, root: &Path, scope: Scope) -> Result<Vec<DiagnosticEvent>, LangError>;
    async fn test(&self, root: &Path, sel: TestSelector) -> Result<TestReport, LangError>;
    async fn lower_edit(&self, op: &SemanticEditOp) -> Result<Vec<TextEdit>, LangError>;
    fn capabilities_extended(&self) -> Vec<CapabilityId>;
}
```

Rule **LB1**: this signature is transcribed **unchanged**. Method names, argument order, `&dyn ProjectGraph` (not `Arc`, not a generic), and return types are fixed. A second language implementation is what earns the right to change it (V2 §16.1 Evolution: "freeze trait when second impl forces it — not a week-23 empty ceremony").

| V2 §16.1 clause | This RFC |
| --- | --- |
| Architectural interface: trait for index / diagnostics / test / edit-lower | §3.1 verbatim |
| MVP implementation: Rust-only internal module in `alloy-runtime` **or** `alloy-index` | §9 — seam in `alloy-runtime`, impl in `alloy-index`, justified in LC2 |
| Deferred: Python/TS, `cdylib`, freeze ceremony | §1.4 out of scope, RS9 |
| Stub: `lower_edit` fails closed; non-Rust backends absent | §8 (LE1–LE4) |
| Upgrade path: add a backend crate for a second language; Scheduler/MCP unchanged | RS10 keeps the control plane clean so this stays true |

### 2.2 Relationship to the roadmap

| Milestone | This RFC |
| --- | --- |
| M1–M6 | Untouched |
| **M7 / MVP** | **Nothing is implemented.** M7's obligation is the §4 negative list: do not delete or narrow the reserved seams |
| **Beta** | Implemented in full, in parallel with "0011 deep" and "0012 deep"; 3–5 pd of the Beta 9–14 pd budget |
| Production / M3 | One RA-backed `SemanticEditOp` (roadmap: "RFC-0008 / 0014: ≥1 RA-backed `SemanticEditOp`") — a future extension of this RFC, not a new RFC |

Beta acceptance criterion "LanguageBackend Rust-only; no PY/TS/cdylib" is discharged by RS9 + T18/T19.

### 2.3 Relationship to merged code (verified against this tree)

| Seam | Where it is today | What this RFC does |
| --- | --- | --- |
| `ProjectGraph` trait | `crates/alloy-runtime/src/graph/mod.rs` | Receives it as `&dyn ProjectGraph`; **does not change it** |
| `GraphNodeKind::Item` | same file, "**Stub** in MVP: never ingested (IN9)" | Becomes ingested (SY3) |
| `GraphEdgeKind::Imports` | same file, "**Stub** in MVP: never ingested (IN8)" | Becomes ingested (SY6) |
| `GraphFidelity::SynDeep` | same file, "Reserved: `syn` item-level parse (Beta)" | Becomes reachable (A-0014-4, RS4) |
| `derive_node_id` | same file (G3/G4) | Reused unchanged for `Item` ids (SY4) |
| `GRAPH_MODEL_VERSION = 1` | `crates/alloy-index/src/migrate.rs:17` | Becomes `2` (SY1); `check_model_version` already truncates and re-ingests |
| `GRAPH_SCHEMA_VERSION = 1` | same file | Becomes **`2`** with ledgered table recreation for semantic-edge `CHECK` kinds (SY2 as landed; §2.4) |
| `IngestReport` | `graph/mod.rs`, doc: "adding a field is an API change by design" | Gains `items` / `imports` counters (amendment A-0014-3) |
| `IngestLimits` | `crates/alloy-index/src/layout.rs` | Gains `max_items` (amendment A-0014-1) |
| `parse_rustc_diagnostics` | `crates/alloy-runtime/src/adapters/diagnostics.rs`, re-exported at the crate root | **Reused**, not reimplemented (DN1) |
| `DiagnosticEvent` | `crates/alloy-runtime/src/types/diagnostic.rs` | Output shape of `diagnostics()`; unchanged |
| `LanguageId` | `crates/alloy-runtime/src/types/ids.rs:147`, `name_id!`, "MVP: `rust`" | Backend id (`LanguageId::new("rust")`) |
| `Session.language_backends: Vec<LanguageId>` | `crates/alloy-runtime/src/session/traits.rs:76`, validated non-empty in `service.rs:169` | Becomes the registry lookup key (LB11) |
| `SemanticEditOp` (9 variants, `op_tag()`) | `crates/alloy-runtime/src/edit/types.rs:114` | `lower_edit` keys off `op_tag()` (LE2) |
| `ToolchainRecord` | `crates/alloy-eval/src/manifest.rs:140` | **Shape reference only** — not reused; direction would be illegal (TC5) |

**Correction of record.** An earlier note claimed the cargo-JSON → `DiagnosticEvent` parser "does not exist yet, and arrives with RFC-0013/0014" (RFC-0011 §6.7 repeats this). It **does** exist: RFC-0010 shipped `parse_rustc_diagnostics` with rules DG1–DG8/FP1–FP5, 200-diagnostic cap and truncation marker, and `McpVerifyCompileAdapter` already calls it. RFC-0014 therefore adds **no parser**; it adds a language-owned *entry point* that delegates to the existing one (DN1, RS7).

### 2.4 Authorised amendments

Small, additive, and recorded here so no reviewer treats them as drift.

| # | Amendment | Target | Justification |
| --- | --- | --- | --- |
| **A-0014-1** | `IngestLimits` gains `max_items: u32` (default `200_000`) | `alloy-index::layout` | RS12 — caps live in one struct; a Beta cap must not become an ad-hoc `const` |
| **A-0014-2** | RFC-0011 IN7 ("module inference MUST NOT parse Rust source") is amended for `model_version = 2`: declaration-driven inference replaces sibling-guessing | RFC-0011 §6.4 | The prohibition exists because MVP has no parser; SY7 states the superseding rule and keeps IN7f's "invent nothing" |
| **A-0014-3** | `IngestReport` gains `items: u32`, `imports: u32` | `alloy-runtime::graph` | Its own doc comment authorises field additions as deliberate API changes |
| **A-0014-4** | `GraphView.fidelity` becomes a computed value from `graph_meta.model_version` instead of the `GraphView::empty` default | `alloy-index::query` | RS4 — one function decides fidelity, so it cannot silently lie |
| **A-0014-5** *(landed with RFC-0011 A-0011-6 / PR #62)* | SY10's no-body-descent rule is scoped to the **node-emitting item walker**, which still never descends. A dedicated *reference collector* (RFC-0011 §2.3b) MAY walk the bodies and signatures of already-emitted module-level items **and of impl blocks**, attributing an impl block's semantic edges to the emitted self-type item, to record `References`/`Calls`/`Impls` edges. It emits **no nodes** — neither impl nor method nodes (SY5 holds) — never enters macro invocations or attributes, and blanks items nested inside bodies. SC3 is not weakened: `syn::parse_file` already recursed over the full token tree to build the AST the walker holds, so visiting that AST introduces no recursion a parse bomb could not already trigger, and the `max_file_bytes` gate runs before either. | §5 SY10, SC3 | A-0011-6a needs body-level facts; the rule's purpose (bounded recursion, linear node emission) survives intact |
| **A-0014-6** *(landed with RFC-0011 A-0011-6 / PR #62)* | LC7's `syn` feature list gains `visit` (read-only AST traversal), becoming `["full", "parsing", "clone-impls", "visit"]`. Still non-macro: `printing`, `fold`, `visit-mut`, `quote` and the proc-macro bridge remain forbidden, and the T20 placement rule (workspace pin, `alloy-index` alone) is untouched. | §9 LC7, T20 | The collector uses `syn::visit::Visit`; `visit` generates no code and links no compiler machinery |

No amendment touches the `ProjectGraph` trait, the `GraphQuery` enum, or an Appendix A table shape — RFC-0011 E.3.3 holds.

**Landed-state reconciliation (PR #62, RFC-0011 A-0011-6).** Where this text predates that merge, the merged code is newer and governs:

1. **Version numerals.** `GRAPH_MODEL_VERSION` is `3` (SY1's `1 → 2` for items/imports, then `2 → 3` for the A-0011-6 semantic edges) and `GRAPH_SCHEMA_VERSION` is `2`: the `references`/`calls`/`impls` edge kinds needed the `graph_edges` `CHECK` list expanded, which SQLite cannot alter, so the table was recreated in a ledgered migration. SY1's mechanics are unchanged — a model mismatch still truncates and re-ingests, never merges — and SY2's "no DDL" held for this RFC's own rows: the v1 `CHECK` lists admitted `'item'`/`'imports'` exactly as designed.
2. **Fidelity threshold.** `fidelity_for_model_version` (A-0014-4) returns `SynDeep` for `model_version >= 2`; model `3` is a deeper population of the same syn parse, not a new fidelity. T17 landed as `fidelity_is_syn_deep_from_model_version_two`.
3. **Appendix B totals.** The toy workspace's 13 nodes and the item/import counts are as written; the edge total is now 18 (12 `Defines` + 3 `Imports` + 3 `References` — see the Appendix B note) and `IngestReport` gained `references`/`calls`/`impls` counters alongside `items`/`imports`. T18 pins the current totals.
4. **T15 coverage.** SY15's byte-cap arm landed with its own test, `oversized_file_is_tracked_skipped_and_never_parsed`, beside the two `max_items` arms the table names.

### 2.5 What downstream may rely on

1. `Session.language_backends` is the declaration; the host resolves it to backends at composition time (LB11).
2. A backend failure never fails a DAG node (SC7); the graph and the language backend are accelerators.
3. `GraphView.fidelity` is truthful: `SynDeep` implies item/import data was produced by this pass over this workspace.
4. No worker, tool, or scheduler type gains a language field (RS10).

---

## 3. Public Rust API

### 3.1 The trait

Verbatim as §2.1 (LB1), placed in `crates/alloy-runtime/src/lang/mod.rs`.

### 3.2 Value types introduced by this RFC

None of these identifiers exist in the tree today; all are introduced here, in `alloy-runtime::lang`, and pinned there by T20.

Rule **LB2a**: every `#[non_exhaustive]` struct below (`LanguageManifest`, `TestReport`) MUST ship a `::new(...)` constructor taking the currently-required fields. `RustBackend` lives in `alloy-index` and so cannot use a struct literal across the crate boundary — without constructors the types are uninhabitable by their only producer. Later fields are added with defaults inside `new`, preserving the point of `#[non_exhaustive]`.

```rust
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
    NotDetected { language: String, path: String },
    /// A manifest could not be read or parsed.
    #[error("manifest {path}: {reason}")]
    Manifest { path: String, reason: String },
    /// Source could not be parsed; carries the file, never the source text.
    #[error("parse {path}: {reason}")]
    Parse { path: String, reason: String },
    /// The toolchain could not be reached or reported failure.
    #[error("toolchain: {0}")]
    Toolchain(String),
    /// A `SemanticEditOp` this backend cannot lower (LE2).
    #[error("unsupported op: {op}")]
    UnsupportedOp { op: String },
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
```

Rule **LB8**: `LangError` wraps `GraphError` rather than re-encoding it, so RFC-0011's Appendix C caller decision table applies unchanged to graph-origin failures surfaced through the backend.

### 3.3 The toolchain seam

The backend must reach cargo without spawning it: neither `alloy-runtime` nor `alloy-index` contains `std::process::Command` today, and RFC-0011's T12 grep enforces that for `alloy-index`. All process execution belongs to the sandbox broker behind the MCP host (RFC-0005/0006).

```rust
/// The single seam through which a language backend reaches a toolchain
/// (LB9). Implementations route to the MCP host under a `PermissionToken`;
/// no implementation spawns a process from `alloy-runtime` or `alloy-index`.
#[async_trait]
pub trait ToolchainRunner: Send + Sync {
    /// `cargo check --message-format=json` for `scope`; returns stdout.
    async fn check_json(&self, root: &Path, scope: &Scope) -> Result<String, LangError>;
    /// `cargo test` for `sel`; returns (exit-ok, captured output).
    async fn test(&self, root: &Path, sel: &TestSelector) -> Result<(bool, String), LangError>;
    /// Toolchain identity. **Not a live probe at Beta** — see TC1a: the
    /// value is injected by the composition root, and this method fails
    /// closed when none was supplied.
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
```

The production implementation, `McpToolchainRunner`, lives beside `McpVerifyCompileAdapter` in `alloy-runtime::lang` and is built from the existing `Arc<dyn ToolCaller>` plus a permission source — the same two collaborators `crates/alloy-runtime/src/adapters/verify.rs` already uses, calling the same `cargo_check` / `cargo_test` tool names with `message_format: "json"`. Rule **LB9** forbids a second cargo argv construction path: if the argv shape must change, it changes in `build_tool_call` and both callers follow.

### 3.4 The backend and its registry

```rust
/// Rust backend. Constructed with its collaborators; construction performs
/// no I/O (LB12).
pub struct RustBackend { /* runner, limits, cached toolchain */ }

/// Resolution of `Session.language_backends` to implementations (LB11).
/// Owned by the composition root (RFC-0015), never by a worker.
pub struct LanguageRegistry { /* LanguageId -> Arc<dyn LanguageBackend> */ }

impl LanguageRegistry {
    pub fn get(&self, id: &LanguageId) -> Option<Arc<dyn LanguageBackend>>;
    pub fn ids(&self) -> Vec<LanguageId>;
}
```

Rule **LB11**: `Session.language_backends` is a **declaration**, not a handle. The registry resolves it at session create; an id with no registered backend is a session-create error, matching the existing non-empty validation in `session/service.rs`. Rule **LB12**: `id()` and `manifest()` are synchronous and I/O-free; anything requiring the toolchain is `async` and goes through `ToolchainRunner`.

---

## 4. Reserved seams — what M7 must not do

This is the section that pays for itself before a line of Beta code is written. Each rule is a *negative* obligation on MVP work, with the enforcing test named.

| # | Rule | Enforced by |
| --- | --- | --- |
| **RS1** | MUST NOT change the `ProjectGraph` trait signature, the `GraphQuery` enum (frozen at seven variants, Q1), or any Appendix A table shape. `index` receives `&dyn ProjectGraph` exactly as declared. | RFC-0011 E.3.1/E.3.3, T21 |
| **RS2** | MUST NOT delete `GraphNodeKind::Item`, `GraphEdgeKind::Imports`, or the `'item'` / `'imports'` values from the `graph_nodes.kind` / `graph_edges.kind` `CHECK` constraints. They are Beta's landing zone; removing them would force a schema migration this RFC is designed to avoid. | T15 |
| **RS3** | MUST NOT delete `GraphFidelity::SynDeep` or `GraphFidelity::Analyzer`, and MUST NOT collapse `GraphFidelity` into a boolean. RFC-0012 E.1.2 already treats it as a citation label with three meanings. | T16 |
| **RS4** | Fidelity MUST be produced by exactly one function reading `graph_meta.model_version`, not by a literal at each construction site. Today the only literal outside the seam is `store.rs`'s `source: GraphFidelity::Manifest`; amendment A-0014-4 turns it into a call. MUST NOT add new literals. | T17 |
| **RS5** | `Session.language_backends` MUST stay `Vec<LanguageId>` with its non-empty validation and its `language_backends_json` column. MUST NOT be narrowed to a bool, a single id, or dropped as unused. | T22 |
| **RS6** | `LanguageId` MUST stay a `name_id!` catalog id. MUST NOT become `enum Language { Rust }` — that shape makes a second backend a breaking change to every persisted session row. | T22 |
| **RS7** | MUST NOT add a second cargo-JSON → `DiagnosticEvent` parser, and MUST NOT move `parse_rustc_diagnostics` out of the crate root re-export. RFC-0014 delegates to it; two parsers means two fingerprint schemes and a broken dedupe. **Named exemption:** `crates/alloy-eval/src/recording.rs` also matches `"compiler-message"` — it is RFC-0016's offline fixture extractor, produces `ExpectedDiagnostic` (not `DiagnosticEvent`), and participates in no dedupe. It is the *only* permitted second matcher; a third is a violation. | T26 |
| **RS8** | `syn` MUST NOT appear in `[workspace.dependencies]` or any crate manifest before this RFC is implemented. Adding it early is dead weight under DoD gate 9 and hides the moment fidelity changes. | T18 |
| **RS9** | MUST NOT create `alloy-lang-*` crates, a `crate-type = ["cdylib"]` target, or any dynamic-loading mechanism (V2 §16, ADR F-15, ≤5-crate rule). | T19 |
| **RS10** | The control plane MUST stay language-agnostic: no `LanguageBackend`/`LanguageId` field on `CapabilityContext` or `CapabilityOutput`, no `language_backend` MCP tool, no language branch in the scheduler. Workers see the graph through `GraphViewHandle` only. | T24 |
| **RS11** | `SemanticEditOp` variant tags and `op_tag()` strings MUST NOT be renamed (RFC-0008 §3.2 already says so). `lower_edit`'s fail-closed error carries those exact strings. | RFC-0008 tests |
| **RS12** | Ingest caps MUST stay in `IngestLimits`. A new cap added as a private `const` cannot be tuned by the CLI and breaks the additive `max_items` amendment. | Review |
| **RS13** | The graph MUST stay optional. `NullProjectGraph` must keep answering reads empty and writes `Disabled` with a backend registered; a Beta index path MUST NOT become a hard dependency of session create. | T13 |

**Cost of ignoring this section:** RS2 alone converts a zero-DDL Beta into a schema migration with a v1→v2 upgrade path over user databases — for a derived cache that is explicitly wipeable. RS5/RS6 convert an additive second-language change into a persisted-data migration.

---

## 5. The `syn` deep pass

### 5.1 Version discipline

| # | Rule |
| --- | --- |
| **SY1** | Emitting `Item` nodes or `Imports` edges accompanied `GRAPH_MODEL_VERSION: 1 → 2`; A-0011-6 then bumped model version to **`3`** for semantic edges. `check_model_version` truncates `graph_edges` / `graph_nodes` / `graph_files` and resets `graph_meta` on mismatch, so older databases are **re-ingested, never merged**. |
| **SY2** | `GRAPH_SCHEMA_VERSION` is **`2`**. Semantic-edge kinds (`references` / `calls` / `impls`) required expanding the `graph_edges` `CHECK` list; SQLite cannot alter that constraint in place, so migration recreates the table (ledgered DDL). The original v1 `CHECK` lists already admitted `'item'` / `'imports'`; schema v2 is the landed A-0011-6 state. |
| **SY3** | Item kinds emitted: `fn`, `struct`, `enum`, `union`, `trait`, `type` alias, `const`, `static` — at module level, any visibility. |
| **SY4** | Item ids use `derive_node_id(GraphNodeKind::Item, stable_key)` with `stable_key = "<crate_id>\0<module_path>::<ident>"`, mirroring the module key shape already asserted in `graph/mod.rs`'s tests (`"toy-core\0toy_core::io"`). `GraphNodeId::new()` MUST NOT be called (G3). `GraphNode.path` is the Rust path (`toy_core::io::Reader`); `file` is the declaring file; `digest` is that file's SHA-256. |
| **SY5** | `impl` blocks and associated items are **deferred** as nodes. They have no unambiguous path under `UNIQUE (kind, path)` without a name-resolution model the graph does not have. V2 §7.2's mention of `impl` is discharged by the `Impls` query: a syn-grade subset answers since A-0011-6; rustc-grade answers stay deferred to RA passthrough. |

### 5.2 Items

| # | Rule |
| --- | --- |
| **SY6** | Each item gets one `Defines` edge from its owning module node, `confidence = 1.0` (S6 keeps confidence reserved). |
| **SY7** | Module inference becomes **declaration-driven** (amendment A-0014-2): roots still come from the manifest (IN7a/IN7b), then `mod foo;` / `mod foo { … }` in the parent decides children, `#[path = "…"]` is honoured, and directory sibling-guessing (IN7c/IN7e) is retired. `cfg` is **not evaluated**: a `#[cfg]`-gated `mod` whose file exists is emitted and noted in warnings. IN7f survives verbatim — *missing* nodes are acceptable, *invented* ones are not (G7). |
| **SY8** | Path collisions (two items resolving to the same `(Item, path)` — typically `cfg`-duplicated definitions) keep the **first in sorted traversal order**, drop the rest, and push one warning. Disambiguating suffixes are forbidden: they make ids depend on sibling contents and break IN6. |
| **SY9** | A file that fails to parse is **skipped**, counted in `IngestReport.skipped`, and warned about — never fatal, mirroring IN12. `LangError::Parse` carries the path and reason, never source text. |
| **SY10** | *(amended by A-0014-5, §2.4)* The **node-emitting item walker** MUST NOT descend into function bodies, expressions, or macro invocations. It walks items and nested `mod` blocks only. This bounds recursion (SC3) and keeps node emission linear in item count, not token count. The A-0014-5 reference collector may walk bodies of already-emitted items and of impl blocks for edge collection, attributing impl-block edges to the emitted self-type item; it emits no nodes — neither impl nor method nodes — and never enters macros. |

### 5.3 Imports

| # | Rule |
| --- | --- |
| **SY11** | A `use` declaration produces an `Imports` edge from the **importing module node** to the target node, and only when the target resolves to a node already in this graph. Cross-workspace targets (`std::`, registry crates) produce **no edge and no node** (G7). |
| **SY12** | Group imports (`use a::{b, c}`) expand to one edge per leaf; `use a::*` produces one edge to `a`'s module node; `use a::b as c` targets `b`; `pub use` is an ordinary `Imports` edge. Duplicate `(from, to, imports)` rows collapse on the existing primary key. |
| **SY13** | Path resolution is **syntactic**, not semantic. Admissible leading segments are: `crate::`, `self::`, `super::`, an in-workspace crate ident, **and an ident naming a module the importing module itself declares** — `mod reader;` in `io.rs` makes `use reader::Reader;` resolve relative to `toy_core::io`. Without this clause the most common in-file re-export shape (`mod x; pub use x::Item;`) resolves to nothing, as Appendix B demonstrates. All are resolved against the module tree just built. Anything else is unresolved and produces nothing. Confidence stays `1.0`; graded confidence for glob imports is deferred (§14, OQ2). |

### 5.4 Determinism and caps

| # | Rule |
| --- | --- |
| **SY14** | The pass inherits IN3/IN4/IN5/IN6 unchanged: sorted-by-filename-bytes traversal, no symlink following, no escaping the root, byte-identical output for byte-identical input, and no `GraphVersion` bump when the content digest is unchanged. Item and edge sets are emitted in sorted order by `(crate_id, path)` / `(from, to, kind)`. |
| **SY15** | The content digest MUST incorporate the item and import sets, so IN6 remains meaningful once items exist. Caps: `max_items` (A-0014-1, default `200_000`) exceeded → `GraphError::LimitExceeded`, previous version intact (S10); files above `max_file_bytes` are **not parsed**, counted as skipped, and their module still exists from the manifest/layout facts. `max_items = 0` is rejected at open — a zero cap would let the store claim `SynDeep` while emitting nothing. |

### 5.5 Where the pass runs

`RustBackend::index(root, graph)` performs the parse and writes through `ProjectGraph::rebuild`'s ingest path — it does **not** open its own connection or issue SQL. Concretely, `alloy-index` composes the existing manifest+layout pass with the syn pass behind the same single-writer transaction, preserving RFC-0011 X1 (exactly one writer per data directory) and IN1 (ingest triggered only by the CLI/host, never by a worker or the scheduler).

---

## 6. Diagnostic normalisation

| # | Rule |
| --- | --- |
| **DN1** | `RustBackend::diagnostics` MUST delegate parsing to `alloy_runtime::parse_rustc_diagnostics`. A second implementation of rustc-JSON parsing MUST NOT exist in the workspace (RS7). All of DG1–DG8 and FP1–FP5 — `reason == "compiler-message"` filtering, four-level mapping, `is_primary` span selection, package attribution, fingerprint dedupe, the 200-diagnostic cap and its `Note` truncation marker — apply unchanged and untested-again here. |
| **DN2** | The backend MUST NOT spawn cargo. It calls `ToolchainRunner::check_json`, whose production implementation goes through the MCP host under a `PermissionToken` (RFC-0005/0006). |
| **DN3** | `Scope` maps to argv: `Workspace` → no `-p`; `Crate(id)` → `-p <id>`; `File(path)` → the owning package if the caller can supply it, else degrade to `Workspace` and note the degradation. Degrading is correct; guessing a package from a path prefix is not. |
| **DN4** | The backend returns spans exactly as the parser produced them. Workspace-relativisation stays where RFC-0011 G12 put it — the host's `record_diagnostic` path. No double normalisation. |
| **DN5** | `diagnostics()` MUST NOT call `graph.record_diagnostic`. It returns events; the **runtime host** ingests them. This is what keeps IN1/SEC4 true with a backend in the picture. |
| **DN6** | A `ToolchainRunner` failure yields `LangError::Toolchain`. A cargo run that *compiles nothing successfully* is not an error — it is a `Vec<DiagnosticEvent>`, possibly empty. |
| **DN7** | The backend is **not** on the verify path. RFC-0010's `McpVerifyCompileAdapter` keeps producing `VerifyOutcome.diagnostics` for the scheduler exactly as it does today; `RustBackend::diagnostics` serves out-of-band callers (CLI, index refresh, future workers). Two callers, one parser, one argv builder. |
| **DN8** | Diagnostics are never persisted by the backend and never logged with message bodies at `info` or above (LO4). |

---

## 7. Toolchain awareness

| # | Rule |
| --- | --- |
| **TC1** | `manifest()` reports only statically-known facts (`file_extensions = ["rs"]`, `root_markers = ["Cargo.toml"]`, `index_fidelity = SynDeep`). Toolchain identity is obtained by `ToolchainRunner::probe`, cached per backend instance, and never fetched during construction (LB12). |
| **TC1a** | **`probe` has no transport at Beta, and MUST NOT invent one.** The MCP host registers exactly `apply_patch`, `cargo_check`, `cargo_test`, `fs_read`; this RFC forbids host changes (RS10, §1.4), and `cargo -V` / `rustc -V` cannot ride `build_tool_call`'s fixed cargo argv. Therefore `McpToolchainRunner::with_probed_toolchain(RustToolchain)` receives the value from the **composition root** (RFC-0015), which is where a `rustc -V` execution can be authorised; `probe` returns it. Constructed without one, `probe` **fails closed** with `LangError::Toolchain("no probed toolchain supplied")` — it never shells out, never guesses, and never returns a placeholder record. A real probe transport (a `toolchain_version` builtin or a CLI-side exec) is **RFC-0015/RFC-0006 scope**, not this RFC's. |
| **TC2** | `detect(root)` is pure filesystem: `root/Cargo.toml` exists and parses with a `[workspace]` or `[package]` table — the same predicate RFC-0011 §6.3 uses. No subprocess, no network. Returns `Ok(false)`, not an error, for a non-Rust root. |
| **TC3** | Edition is read from `[package] edition` and used to select the parse target. An unknown or future edition parses with the newest supported grammar and records a warning; it never fails the pass. |
| **TC4** | `rust-toolchain.toml`'s channel, when present, is surfaced in `LanguageManifest.toolchain_hints`. It is a hint: enforcing a pin belongs to RFC-0016's harness (`EvalHarness.pin_toolchain_channel`), not here. |
| **TC5** | `RustToolchain` is **not** `alloy-eval::ToolchainRecord`. The fields align deliberately (`channel`, `rustc_version`, `cargo_version`) but `alloy-eval` depends on `alloy-runtime`, so reusing it would invert the dependency direction. Any conversion impl belongs in `alloy-eval`. |
> **Note (pending merge):** the corpus P0 branch (PR #41) lifts `ToolchainRecord` into `alloy-runtime::types::toolchain`. Once merged, `RustToolchain` SHOULD reuse that type directly — the dependency-direction objection disappears — and this RFC's shape MUST stay field-compatible with it in the meantime.
| **TC6** | A recorded `RustToolchain` accompanies the `IngestReport` in the CLI's decision record (LO3), so a graph built under a different channel is diagnosable rather than mysterious. |

---

## 8. `lower_edit` fails closed

| # | Rule |
| --- | --- |
| **LE1** | Beta lowers **nothing**. `LanguageManifest.lowerable_ops` is empty, and every one of the nine `SemanticEditOp` variants returns `Err(LangError::UnsupportedOp { op })`. |
| **LE2** | `op` is exactly `SemanticEditOp::op_tag()` — `rename_type`, `update_imports`, `replace_body`, `insert_impl`, `add_method`, `move_module`, `extract_trait`, `split_crate`, `add_field` — so the string matches RFC-0008's `EditError::UnsupportedOp { op }` and no caller has to translate. |
| **LE3** | A partial or best-effort lowering MUST NOT be returned. A wrong `TextEdit` becomes a workspace mutation through the one write stack (RFC-0008); "fail closed" is the whole design of the envelope. |
| **LE4** | The first real lowering (M3, `RenameType` via rust-analyzer) is a **future extension of this RFC**: it adds the tag to `lowerable_ops` and implements one match arm. It does not change the trait, and it does not get its own RFC. |

---

## 9. Crate placement and dependencies

### 9.1 Placement decision (normative)

V2 §5.4's crate map annotates `alloy-runtime` with "LanguageBackend (Rust module)"; V2 §16.1 says "Rust-only internal module in `alloy-runtime` **or** `alloy-index`". Both are authorised. The split:

| # | Rule |
| --- | --- |
| **LC1** | **Seam in `alloy-runtime`**: `crates/alloy-runtime/src/lang/` holds the trait, `LanguageManifest`, `Scope`, `TestSelector`, `TestReport`, `TextEdit`, `LangError`, `RustToolchain`, `ToolchainRunner`, `McpToolchainRunner`, `LanguageRegistry`. No `syn`, no `rusqlite`, no SQL, no `std::process::Command`. This mirrors RFC-0011 rule C4 exactly (the `ProjectGraph` seam lives in the runtime; the store does not). |
| **LC2** | **Implementation in `alloy-index`**: `crates/alloy-index/src/lang/rust/` holds `RustBackend` and the syn pass. Reason: `index` is 90% of the backend's Beta work, and it needs `derive_node_id` usage conventions, the ingest transaction, `IngestLimits`, and `GRAPH_MODEL_VERSION` — all private to `alloy-index`. Putting `RustBackend` in `alloy-runtime` would require `alloy-runtime → alloy-index`, which RFC-0011 rule C2 forbids and test T9 fails on. |
| **LC3** | **Composition in `alloy-cli`** (RFC-0015), where `SqliteProjectGraph` is already built. The CLI constructs `McpToolchainRunner` from the MCP host, constructs `RustBackend`, registers it under `LanguageId::new("rust")`, and hands the registry to session create. |
| **LC4** | **No new crate.** The workspace stays at five (V2 §5.4). |
| **LC5** | `alloy-index` remains free of process execution and network: `McpToolchainRunner` lives in `alloy-runtime` and reaches cargo through `ToolCaller`, so RFC-0011's T12 grep (`no std::process::Command in alloy-index/src`) stays green with no exemption. |

Dependency direction after this RFC — unchanged and still acyclic:

```text
alloy-cli ──► alloy-tools ──► alloy-runtime ◄── alloy-index
     └────────────────────────────────────────────┘
```

### 9.2 The `syn` dependency

| # | Rule |
| --- | --- |
| **LC6** | `syn` is added **when this RFC is implemented, and not before** (RS8). Until then the identifier `syn` MUST NOT appear as a dependency in `Cargo.toml`, `crates/*/Cargo.toml`, or any lockfile entry introduced deliberately. T18 asserts this in both directions: before implementation it must be absent everywhere; after, present in `alloy-index` only. |
| **LC7** | *(amended by A-0014-6, §2.4)* The dependency is minimal and non-macro: `syn = { version = "2", default-features = false, features = ["full", "parsing", "clone-impls", "visit"] }`, added to `[workspace.dependencies]` and consumed as `syn = { workspace = true }` by `alloy-index` alone. `quote`, `proc-macro2`'s `proc-macro` feature, `syn`'s `printing`/`fold`/`visit-mut` features, and `proc_macro` linkage are **not** taken — nothing here generates code, and `derive`/`printing` would pull the compiler's proc-macro bridge into a plain library. |

`alloy-index/Cargo.toml` after this RFC — one line added to the RFC-0011 list:

```toml
[dependencies]
alloy-runtime = { workspace = true, default-features = false }
async-trait   = { workspace = true }
globset       = { workspace = true }
rusqlite      = { workspace = true }
serde         = { workspace = true }
serde_json    = { workspace = true }
syn           = { workspace = true }   # RFC-0014 only
thiserror     = { workspace = true }
time          = { workspace = true }
tokio         = { workspace = true }
toml          = { workspace = true }
tracing       = { workspace = true }
uuid          = { workspace = true }
```

`#![forbid(unsafe_code)]` and `#![deny(missing_docs)]` continue to apply to both crates.

---

## 10. Security posture

| # | Rule |
| --- | --- |
| **SC1** | No process execution in `alloy-runtime::lang` or `alloy-index`. Cargo runs only via the MCP host under the sandbox broker with a `PermissionToken` (LC5, DN2). |
| **SC2** | No network. `syn` parses bytes already on disk inside the workspace; `cargo metadata`-style resolution that could touch a registry is out of scope, as it was for RFC-0011 §6.2. |
| **SC3** | Parse-bomb resistance: files above `max_file_bytes` are never handed to `syn`, and the visitor does not descend into bodies (SY10), bounding recursion to `mod` nesting, itself capped by `max_depth`. |
| **SC4** | Item paths, module paths and package names are **untrusted repository content**. RFC-0012 E.1.4 already requires that treatment in PromptPacks; more item nodes means more of it, not a new rule. |
| **SC5** | Diagnostic messages and source text are never written to logs above `debug`, never to `.env`, and never to an artifact by the backend (RFC-0011 SEC8 holds unchanged). |
| **SC6** | The backend performs no writes to the workspace. Every workspace mutation stays behind RFC-0008's single write stack; `lower_edit` returns edits, it does not apply them (and at Beta returns none). |
| **SC7** | A backend failure MUST NEVER fail a DAG node — RFC-0011 rule E1 extended to the language seam. Callers degrade: no items, `Manifest`-labelled context, an unchanged repair loop. |

---

## 11. Observability

| # | Rule |
| --- | --- |
| **LO1** | Spans: `lang.detect`, `lang.index`, `lang.diagnostics`, `lang.test`, all with `language = "rust"`. `lang.index` records `crates`, `modules`, `items`, `imports`, `skipped`, `warnings`, `elapsed_ms`. |
| **LO2** | `IngestReport` gains `items` and `imports` (A-0014-3) and reports `source: GraphFidelity::SynDeep` when the pass ran. |
| **LO3** | RFC-0015 owns the `DecisionKind::Custom("graph_rebuild")` record; at Beta it additionally carries the item/import counts and the `RustToolchain` channel (TC6). This RFC adds no new decision kind. |
| **LO4** | Counts and paths, never bodies. No item source, no diagnostic message text, no file contents in structured fields. |
| **LO5** | A fidelity change is worth one `info` line: when `check_model_version` truncates on the 1→2 bump, the existing `warn!` in `migrate.rs` already fires — that log line is the user-visible signal that a manifest-only graph is being rebuilt deeply. |

---

## 12. Testing strategy

### 12.1 Unit — `alloy-runtime::lang` (pure)

| # | Test | Rule |
| --- | --- | --- |
| **T1** | `manifest_is_static_and_io_free` — `id()`/`manifest()` on a backend built with a runner that panics on any call | LB12 |
| **T2** | `detect_true_for_workspace_and_package_roots`, `detect_false_for_a_node_project` (no error) | TC2 |
| **T3** | `lower_edit_rejects_all_nine_ops_with_matching_op_tag` — table over every `SemanticEditOp` variant, asserting `LangError::UnsupportedOp { op } == variant.op_tag()` | LE1, LE2 |
| **T4** | `scope_maps_to_expected_cargo_arguments`, including `File` → `Workspace` degradation | DN3 |
| **T5** | `lang_error_wraps_graph_error_without_reencoding` — `GraphError::Busy` survives `From` | LB8 |
| **T6** | `value_types_round_trip_serde` — `LanguageManifest`, `Scope`, `TestSelector`, `TestReport`, `TextEdit` | LB2–LB6 |

### 12.2 Unit / fixture — `alloy-index` syn pass (`tempfile` workspaces)

| # | Test | Rule |
| --- | --- | --- |
| **T7** | `item_nodes_use_derive_node_id_and_never_new` | SY4 |
| **T8** | `declaration_driven_modules_honour_path_attribute_and_ignore_cfg_evaluation` | SY7 |
| **T9** | `colliding_item_paths_keep_first_and_warn` | SY8 |
| **T10** | `unparseable_file_is_skipped_counted_and_warned_not_fatal` | SY9 |
| **T11** | `imports_resolve_only_inside_the_workspace` — `use std::fmt;` and `use serde::Serialize;` produce zero edges and zero nodes | SY11 |
| **T12** | `import_groups_globs_and_renames_expand_as_specified` | SY12 |
| **T13** | `null_project_graph_still_answers_with_a_backend_registered` | RS13 |
| **T14** | `syn_pass_is_deterministic_across_two_processes` — extends RFC-0011's T3c harness; identical digest, identical `IngestReport` | SY14 |
| **T15** | `max_items_cap_returns_limit_exceeded_and_leaves_version_intact`; `max_items_zero_is_rejected_at_open`; `oversized_file_is_tracked_skipped_and_never_parsed` (the byte-cap arm, added with the landed state — §2.4) | SY15 |

### 12.3 Integration — model-version transition

| # | Test | Rule |
| --- | --- | --- |
| **T16** | `model_version_bump_truncates_and_reingests` — build a graph with `GRAPH_MODEL_VERSION = 1` fixtures, open with the Beta build, assert nodes/edges/files were wiped, `graph_version` reset, and the re-ingest produced `Item` rows; assert **no** SQL migration ran for the model transition (`GRAPH_SCHEMA_VERSION` still `1`; landed: `2` with its own ledgered migration — §2.4) | SY1, SY2 |
| **T17** | `fidelity_is_syn_deep_from_model_version_two` — over a fresh store and a truncated-and-reingested store (`SynDeep` for `model_version >= 2`; renamed post-A-0011-6 since model `3` is the same fidelity — §2.4) | A-0014-4, RS4 |
| **T18** | `toy_workspace_gains_items_and_imports` — the Appendix B tree; exact node/edge counts | Appendix B |
| **T19** | `diagnostics_entry_point_matches_verify_adapter_output` — same recorded cargo JSON through `McpVerifyCompileAdapter` and `RustBackend::diagnostics`; assert equal length, order, and **every field except `id`** (code, level, message, spans, children, package, `fingerprint`, `raw_json`). `DiagnosticId` is a fresh UUID per parse by RFC-0010's design (`DiagnosticId::new()` in `build_diagnostic_event`), so whole-struct equality is unsatisfiable; the **fingerprint** is the stable identity and is compared | DN1, DN7 |

### 12.4 CI greps (`crates/alloy-index/tests/rfc0014_ci_greps.rs`, RFC-0011 T7–T14 harness shape)

| # | Test | Rule |
| --- | --- | --- |
| **T20** | `rs8_syn_absent_from_every_manifest_until_implemented` — before implementation: `syn` in no manifest; after: `alloy-index` only, and never in `alloy-runtime`/`alloy-tools`/`alloy-cli`/`alloy-eval` | RS8, LC6, LC7 |
| **T21** | `rs9_no_lang_crates_no_cdylib_no_dynamic_loading` — no `crates/alloy-lang*` directory, no `crate-type`, no `libloading`/`dlopen` | RS9 |
| **T22** | `rs2_item_and_imports_kinds_survive` — the seam still declares `GraphNodeKind::Item` and `GraphEdgeKind::Imports`, and `migrate.rs`'s `CHECK` lists still contain `'item'` and `'imports'` | RS2 |
| **T23** | `rs3_graph_fidelity_still_has_three_variants` | RS3 |
| **T24** | `rs4_fidelity_literal_appears_in_exactly_one_function` | RS4 |
| **T25** | `rs5_rs6_language_id_and_session_field_shape_unchanged` — `name_id!(… LanguageId)` present; `language_backends: Vec<LanguageId>` present; the non-empty validation string present | RS5, RS6 |
| **T26** | `rs7_single_rustc_json_parser` — the literal `"compiler-message"` appears in exactly two source files: `alloy-runtime/src/adapters/diagnostics.rs` (the parser) and `alloy-eval/src/recording.rs` (the RFC-0016 fixture extractor, exempted by RS7). The test asserts the **named set**, not a count, so a new matcher fails it | RS7 |
| **T27** | `rs10_control_plane_has_no_language_field` — `LanguageBackend` absent from `CapabilityContext`/`CapabilityOutput`/scheduler sources; no `language_backend` tool registration in `alloy-tools/src` | RS10 |
| **T28** | `lc5_no_process_execution_in_lang_seam` — `std::process::Command` absent from `alloy-runtime/src/lang/**` (RFC-0011 T12 already covers `alloy-index`) | LC5, SC1 |

**T20–T28 are the tests that should land in M7, not Beta** — they cost an afternoon and are the mechanical form of §4. They are listed here because this RFC owns the rules they enforce; nothing stops RFC-0015's CI work from adding them early, with the "syn absent" arm active until Beta flips it.

---

## 13. Beta vs deferred

### 13.1 Beta (this RFC)

Trait + value types; `RustBackend` with all eight methods; syn item/import pass at `model_version = 2`; `SynDeep` fidelity; diagnostics entry point delegating to the existing parser; `test()` summary wrapper; toolchain capture; fail-closed `lower_edit`; registry wiring; the §12 tests.

### 13.2 Deferred, with the seam that carries it

| Deferred | Seam already present |
| --- | --- |
| Second language (Python/TS) | `LanguageRegistry` keyed by `LanguageId`; `Session.language_backends` is a `Vec` |
| Dynamic loading / `cdylib` | None, deliberately — rejected by ADR F-15 and RS9 |
| `impl` blocks / associated items as nodes | Deferred (SY5); `GraphQuery::Impls` answers the syn-grade subset via A-0011-6 (no impl/method nodes) |
| Call graph, references (RA-grade) | Syn-grade `Callers` / `Refs` / `Impls` live since A-0011-6; RA passthrough remains for rustc-grade answers |
| rust-analyzer passthrough | `GraphFidelity::Analyzer` |
| Graded edge confidence for glob imports | `GraphEdge.confidence` (S6, always 1.0) |
| `RenameType` lowering | `LanguageManifest.lowerable_ops` + `SemanticEditOp::op_tag()` |
| Structured libtest results | `TestReport`'s `Option` counts |

---

## 14. Open questions

| # | Question | Default if unresolved |
| --- | --- | --- |
| **OQ1** | Should `impl` blocks become `Item` nodes with a synthesised path (`toy_core::io::impl#Reader`)? It would make `Impls` answerable without RA, at the cost of a path scheme the `UNIQUE (kind, path)` constraint did not anticipate. | Stay deferred (SY5) |
| **OQ2** | Should glob imports (`use a::*`) carry `confidence < 1.0` once RFC-0012 consumes confidence? The column exists; nothing reads it. | `1.0` until a consumer exists (SY13) |
| **OQ3** | Should `syn` sit behind a cargo feature so a minimal build skips it? A feature makes fidelity conditional on build flags — the exact lie SY15 forbids. | No feature; unconditional dependency |
| **OQ4** | Should `#[cfg]`-gated modules be emitted (current SY7) or skipped? Emitting risks nodes unreachable in every real build; skipping risks losing the platform-specific file a repair needs. | Emit + warn |
| **OQ5** | When libtest's JSON output stabilises, does `TestReport` gain structured fields or a new type? | Additive `Option` fields on the `#[non_exhaustive]` struct |
| **OQ6** | Should item-level invalidation ride `apply_incremental` (re-parse the changed file only) or force a full `rebuild` at Beta? IN10 requires the two to agree. | Full rebuild at Beta; incremental once T14's digest equality is proven for items |

---

## 15. Acceptance criteria

### Trait and API

- [ ] 1. `LanguageBackend` is transcribed verbatim from V2 §16.1: same eight methods, same order, same signatures, `index` taking `&dyn ProjectGraph` (LB1).
- [ ] 2. The trait lives in `crates/alloy-runtime/src/lang/mod.rs` (LC1).
- [ ] 3. `LanguageManifest`, `Scope`, `TestSelector`, `TestReport`, `TextEdit`, `LangError`, `RustToolchain`, `ToolchainRunner` are all defined in `alloy-runtime::lang` and nowhere else; `LanguageManifest` and `TestReport` each expose a `::new(...)` constructor so `alloy-index` can build them across the crate boundary (LB2, LB2a, LB3–LB9, T20).
- [ ] 4. `id()` and `manifest()` are synchronous, I/O-free, and pass with a panicking runner (LB12, T1).
- [ ] 5. `LangError` wraps `GraphError` via `#[from]` (LB8, T5).
- [ ] 6. `LanguageRegistry` resolves `Session.language_backends`; an unregistered id is a session-create error (LB11).
- [ ] 7. All new public items carry rustdoc; `cargo doc --workspace --no-deps` is clean under `-D warnings`.

### Crate placement and dependencies

- [ ] 8. `RustBackend` and the syn pass live in `crates/alloy-index/src/lang/rust/` (LC2).
- [ ] 9. `crates/alloy-runtime/Cargo.toml` still contains no `alloy-index` (RFC-0011 C2, T9 still green).
- [ ] 10. The workspace still has exactly five crates; no `alloy-lang-*`, no `crate-type = ["cdylib"]`, no dynamic loader (RS9, T21).
- [ ] 11. `syn` appears in `[workspace.dependencies]` and in `alloy-index`'s manifest only, with `default-features = false` and features `["full","parsing","clone-impls","visit"]` (LC7 as amended by A-0014-6, T20).
- [ ] 12. Before this RFC is implemented, `syn` appears in no manifest at all (RS8, T20).
- [ ] 13. `std::process::Command` appears in neither `alloy-runtime/src/lang/**` nor `alloy-index/src/**` (LC5, T28, RFC-0011 T12).
- [ ] 14. Both crates keep `#![forbid(unsafe_code)]` and `#![deny(missing_docs)]`.

### Reserved seams (the M7 obligations)

- [ ] 15. `GraphNodeKind::Item` and `GraphEdgeKind::Imports` still exist, and `'item'` / `'imports'` are still in the `graph_nodes.kind` / `graph_edges.kind` `CHECK` lists (RS2, T22).
- [ ] 16. `GraphFidelity` still has exactly three variants (RS3, T23).
- [ ] 17. Fidelity is produced by one function reading `graph_meta.model_version`; no other literal exists (RS4, T24).
- [ ] 18. `Session.language_backends` is still `Vec<LanguageId>` with its non-empty validation and its persisted column (RS5, T25).
- [ ] 19. `LanguageId` is still a `name_id!` catalog id (RS6, T25).
- [ ] 20. The literal `"compiler-message"` appears only in `adapters/diagnostics.rs` and the RS7-exempted `alloy-eval/src/recording.rs` (RS7, T26).
- [ ] 21. No `LanguageBackend`/`LanguageId` field on `CapabilityContext` or `CapabilityOutput`; no `language_backend` MCP tool; no language branch in the scheduler (RS10, T27).
- [ ] 22. `NullProjectGraph` still reads empty / writes `Disabled` with a backend registered (RS13, T13).
- [ ] 23. `SemanticEditOp` tags are unchanged (RS11).

### Deep index

- [x] 24. `GRAPH_MODEL_VERSION` is `3` and `GRAPH_SCHEMA_VERSION` is `2` in `migrate.rs` (SY1's `1→2` for items/imports, then A-0011-6d's `2→3` / schema `1→2` for semantic edges — §2.4); model mismatch still truncates and re-ingests, never merges (T16).
- [x] 25. Opening a `model_version = 1` database truncates `graph_nodes`/`graph_edges`/`graph_files`, resets `graph_meta`, and re-ingests deeply (SY1, T16).
- [ ] 26. `Item` nodes exist for module-level `fn`, `struct`, `enum`, `union`, `trait`, `type`, `const`, `static` (SY3).
- [ ] 27. Item ids come from `derive_node_id(GraphNodeKind::Item, "<crate>\0<module>::<ident>")`; `GraphNodeId::new()` is never called in the ingest path (SY4, T7).
- [ ] 28. `impl` blocks and associated items produce no nodes (SY5).
- [ ] 29. Every item has exactly one `Defines` edge from its module, `confidence = 1.0` (SY6).
- [ ] 30. Module inference is declaration-driven, honours `#[path]`, does not evaluate `cfg`, and invents no node for a file that does not exist (SY7, T8).
- [ ] 31. Path collisions keep the first in traversal order and warn; no disambiguating suffixes (SY8, T9).
- [ ] 32. An unparseable file is skipped, counted and warned — never fatal (SY9, T10).
- [ ] 33. The node-emitting item walker never descends into function bodies or macro invocations; the A-0014-5 reference collector walks bodies — including impl-block bodies, attributed to the emitted self-type item — for edges only, emitting no nodes (neither impl nor method nodes) and never entering macros (SY10 as amended).
- [ ] 34. `Imports` edges are written only for in-workspace targets; `std::`/registry imports produce no edge and no node (SY11, T11).
- [ ] 35. Groups, globs, renames and `pub use` behave as SY12 specifies (T12).
- [ ] 36. Two ingests of an unchanged tree in separate processes produce identical digests and `IngestReport`s (SY14, T14).
- [ ] 37. The content digest incorporates items and imports, so IN6's no-bump-when-unchanged still holds (SY15).
- [x] 38. `max_items` exists on `IngestLimits`, is enforced, leaves the previous version intact when exceeded, and rejects `0` at open; files above `max_file_bytes` are tracked, skipped, and never parsed (SY15, T15 — incl. `oversized_file_is_tracked_skipped_and_never_parsed`).
- [ ] 39. `IngestReport` reports `items`, `imports`, and `source: SynDeep` (LO2).
- [x] 40. `GraphView.fidelity` is `SynDeep` for `model_version >= 2` (T17 — `fidelity_is_syn_deep_from_model_version_two`).
- [ ] 41. Ingest is still triggered only by the CLI or the runtime host — never a worker, tool or scheduler node (IN1).

### Diagnostics, test, toolchain, edits

- [ ] 42. `RustBackend::diagnostics` delegates to `alloy_runtime::parse_rustc_diagnostics`; no second parser exists (DN1, T26).
- [ ] 43. Given identical recorded cargo JSON, the backend and `McpVerifyCompileAdapter` produce `DiagnosticEvent`s equal in every field except the per-parse `id`, fingerprints included (DN1, T19).
- [ ] 44. The backend never spawns cargo; it goes through `ToolchainRunner` → MCP host → sandbox with a `PermissionToken` (DN2, SC1).
- [ ] 45. `Scope::File` degrades to `Workspace` rather than guessing a package (DN3, T4).
- [ ] 46. `diagnostics()` never calls `record_diagnostic`; the host ingests (DN5).
- [ ] 47. RFC-0010's verify path is unchanged; `VerifyCompileAdapter` still owns scheduler-facing diagnostics (DN7).
- [ ] 48. `TestReport` counts are `Option` and are `None` when the summary line is unrecognised; `ok` comes from exit status (LB5).
- [ ] 49. `RustToolchain` is injected via `McpToolchainRunner::with_probed_toolchain` and reported with the rebuild decision record; `probe` fails closed with `LangError::Toolchain` when none was supplied, and no source line shells out for `rustc -V` / `cargo -V` (TC1, TC1a, TC6).
- [ ] 50. `RustToolchain` is not `alloy-eval::ToolchainRecord`; no `alloy-runtime → alloy-eval` dependency exists (TC5).
- [ ] 51. All nine `SemanticEditOp` variants return `LangError::UnsupportedOp { op }` with `op == op_tag()` (LE1, LE2, T3).
- [ ] 52. `lowerable_ops` is empty; no partial lowering is returned (LE1, LE3).

### Cross-cutting

- [ ] 53. A backend failure never fails a DAG node (SC7).
- [ ] 54. No source text, diagnostic body or file content is logged above `debug` (LO4, SC5).
- [ ] 55. Spans `lang.detect` / `lang.index` / `lang.diagnostics` / `lang.test` exist with the LO1 fields.
- [ ] 56. The `ProjectGraph` trait, the seven `GraphQuery` variants and every Appendix A table shape are byte-identical to RFC-0011 (RS1, T21).
- [ ] 57. `cargo test --workspace`, `cargo clippy` and `cargo fmt --check` are clean.

---

## 16. Definition of Done

Merge only when the series [Definition of Done](./README.md#definition-of-done-merge-gate) is fully met:

| # | Requirement | Series gate |
| --- | --- | --- |
| 1 | Every AC in §15 is implemented as a passing test, a CI grep, or a mechanical compile/diff check. | 2 |
| 2 | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` are green. | 3, 7, 8 |
| 3 | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS: -D warnings` is clean; every public item in §3 is documented. | 5 |
| 4 | Architecture compliance: **PASS** — the trait matches V2 §16.1 verbatim; Rust-only internal module; deferred items stay deferred. | 1 |
| 5 | `#![forbid(unsafe_code)]` and `#![deny(missing_docs)]` hold in both touched crates; the only new external dependency is `syn`, in `alloy-index` alone. | 1 |
| 6 | Amendments A-0014-1 … A-0014-4 have landed additively with their own tests; no merged field shape changed. | 6 |
| 7 | Integration tests T16–T19 pass, including the `model_version` 1→2 truncate-and-re-ingest transition over a pre-existing database. | 4 |
| 8 | Reserved-seam greps T20–T28 pass; §4's negative list is mechanically enforced, not merely written down. | 2 |
| 9 | The only "not implemented yet" behaviours are the ones this RFC marks **Stub** / deferred (`lower_edit`, `impl`-block items as nodes, RA-grade answers). Syn-grade `Callers`/`Refs`/`Impls` are live since A-0011-6 — no longer Stub. No `TODO`, `todo!()`, `unimplemented!()`, or placeholder in scope. | 9 |
| 10 | Public APIs reviewed and stable: §3 signatures match the implementation with no silent drift. | 6 |
| 11 | RFC text, module docs, and `alloy-index`'s crate docs are up to date — the "Stub / never ingested" language for items and imports is removed where it is no longer true. | 5 |
| 12 | Code review: **approved**. | 10 |

Additionally: RFC-0011's own test suite (T1–T14) MUST still pass unmodified except for the fixture expectations that item/import rows legitimately change.

---

## 17. Estimated implementation effort

**3–5 person-days**, the smallest remaining RFC.

| Slice | Days |
| --- | --- |
| Seam: trait, value types, `ToolchainRunner`, `McpToolchainRunner`, registry | 0.5–1 |
| syn pass: items, declaration-driven modules, imports, caps, determinism | 1.5–2 |
| Model-version bump + truncate/re-ingest + fidelity plumbing | 0.5 |
| Diagnostics/test/toolchain entry points (thin — the parser exists) | 0.5 |
| Tests + CI greps + docs | 0.5–1 |

The §12.4 greps are ~0.25 pd and are the portion worth spending during **M7**.

---

## 18. Future extensions

- **M3**: one RA-backed `SemanticEditOp` (`RenameType`) — adds a tag to `lowerable_ops` and one match arm; no trait change (LE4).
- **Post-dogfood (≥ 6 months)**: a second language. Adds a backend crate and a registry entry; Scheduler and MCP host unchanged (V2 §16.1 Upgrade path). *That* is when the trait freeze is earned.
- **After Beta measurement**: graded `Imports` confidence, `Impls` answers from item data, incremental item invalidation (OQ6).

---

## Appendix A — V2 §16.1 obligation mapping

| V2 §16.1 obligation | Where discharged | Rules |
| --- | --- | --- |
| `LanguageBackend` trait for index / diagnostics / test / edit-lower | §3.1 verbatim | LB1 |
| Rust-only internal module; no dynamic loading | §9 placement; §1.4 out of scope | LC1–LC4, RS9 |
| No PY/TS crates, no `cdylib` | CI grep | RS9, T21 |
| Module in `alloy-runtime` **or** `alloy-index` | Seam in the former, impl in the latter | LC1, LC2 |
| `lower_edit` unsupported ops fail closed | §8 | LE1–LE3 |
| Non-Rust backends absent | Registry holds one entry | LB11 |
| Second language after ≥ 6 months dogfood; freeze then | §18 | — |
| Scheduler / MCP unchanged on upgrade | Control plane stays language-free | RS10, T27 |
| ADR F-15: do not delete the trait | The whole RFC | RS1, RS9 |
| V2 §7.2 "ingest from cargo metadata + syn" | §5 (the syn half; `cargo metadata` deferred **post-Beta** per RFC-0011 §1.4 / §6.2 / §14.2) | SY1–SY15 |
| V2 §20 R16 degraded mode | Fidelity is truthful; failures degrade, never fail | A-0014-4, SC7 |

---

## Appendix B — Worked example: the toy workspace at Beta

Reusing RFC-0011 Appendix B's tree verbatim, with bodies added:

```rust
// crates/toy-core/src/lib.rs
pub mod io;
pub mod util;
pub struct Config { pub verbose: bool }

// crates/toy-core/src/io.rs
mod reader;
pub use reader::Reader;
use crate::Config;
use std::io::Read;
pub fn open(cfg: &Config) -> Reader { /* … */ }

// crates/toy-core/src/io/reader.rs
pub struct Reader { /* … */ }

// crates/toy-core/src/util/mod.rs
pub const LIMIT: usize = 8;

// crates/toy-cli/src/main.rs
use toy_core::io;
fn main() { /* … */ }
```

**At MVP** (`model_version = 1`): 8 nodes (1 workspace, 2 crates, 5 modules), 7 `Defines` edges, 0 items, 0 imports, `fidelity = manifest`.

**At Beta** (`model_version = 2`), on first open the store truncates and re-ingests. Modules are now declaration-driven (`mod io;`, `mod reader;`, `mod util;` — the same five, because the layout guess happened to be right), and items appear:

| kind | path | crate_id | file |
| --- | --- | --- | --- |
| `item` | `toy_core::Config` | `toy-core` | `crates/toy-core/src/lib.rs` |
| `item` | `toy_core::io::open` | `toy-core` | `crates/toy-core/src/io.rs` |
| `item` | `toy_core::io::reader::Reader` | `toy-core` | `crates/toy-core/src/io/reader.rs` |
| `item` | `toy_core::util::LIMIT` | `toy-core` | `crates/toy-core/src/util/mod.rs` |
| `item` | `toy_cli::main::main` | `toy-cli` | `crates/toy-cli/src/main.rs` |

Five item nodes → **13 nodes**. Five new `Defines` edges (module → item) → **12 `Defines`**.

`Imports` edges:

| from | to | why |
| --- | --- | --- |
| `toy_core::io` | `toy_core::io::reader::Reader` | `pub use reader::Reader` — SY12 targets the **leaf item**, not the module; SY13's child-module clause makes the leading `reader` segment admissible |
| `toy_core::io` | `toy_core::Config` | `use crate::Config` |
| `toy_cli::main` | `toy_core::io` | `use toy_core::io` — `toy-core` is a workspace member |

**3 `Imports` edges**. `use std::io::Read` produces **nothing**: `std` is not in this graph, and SY11 forbids inventing the node.

Totals: 13 nodes, 15 edges, `fidelity = syn_deep`, `IngestReport { items: 5, imports: 3, source: syn_deep, … }`. A second `rebuild` returns `unchanged = true` (IN6 via SY15).

*Landed state (A-0011-6, §2.4):* the same tree additionally records 3 `References` edges (`open → Config`, `open → Reader`, `toy_cli::main::main → open`), bringing the edge total to **18**; the node total, item/import counts and fidelity are unchanged.

`query(Subgraph { seeds: [id("toy_core::io")], radius: 1 })` now returns the module, its parent, its child module, its item `open`, and — across the `Imports` edge — `toy_core::Config`: the projection RFC-0012's WorkingSet has been reserving space for.

---

## Appendix C — Contract with RFC-0011 (Beta)

RFC-0011 Appendix E.3 states three obligations on this RFC. Discharge:

| E.3 | Obligation | Discharged by |
| --- | --- | --- |
| **1** | `index(&self, root, graph: &dyn ProjectGraph)` receives the §3.5 trait object unchanged | LB1, AC 1; the trait is not touched (RS1, AC 56) |
| **2** | Adding `Item` nodes / `Imports` edges MUST bump `GRAPH_MODEL_VERSION` (S4) so stale manifest-only databases are re-ingested rather than merged | SY1, AC 24/25, T16 |
| **3** | MUST NOT change the `ProjectGraph` trait, the `GraphQuery` enum, or any Appendix A table shape | SY2 (zero DDL — the v1 `CHECK` lists already admit `'item'`/`'imports'`), RS1, AC 56 |

Reciprocal expectations on RFC-0011's Beta deepening, so the two halves do not collide:

1. RFC-0011 keeps `derive_node_id` as the only id source; RFC-0014 uses it for items (SY4).
2. RFC-0011 keeps `IngestLimits` as the single cap surface; RFC-0014 adds `max_items` there (A-0014-1, RS12).
3. RFC-0011 owns the ingest transaction and single-writer rule (X1); RFC-0014 contributes node/edge sets, never SQL (§5.5).
4. RFC-0011's IN7 is amended, not deleted (A-0014-2); IN7f's "invent nothing" survives verbatim (SY7).
5. RFC-0011's IN5/IN6/IN10 apply to the syn pass unchanged; the content digest grows to cover items and imports (SY15).
6. RFC-0011's §6.7 note that the cargo-JSON parser "arrives with RFC-0013/0014" is stale — it shipped with RFC-0010. RFC-0014 adds an entry point, not a parser (§2.3, DN1).

Rule **E1** — a graph failure MUST NEVER fail a DAG node — extends unchanged to the language backend (SC7).
