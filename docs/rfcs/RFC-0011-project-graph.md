# RFC-0011: ProjectGraph (`alloy-index`)

| Field | Value |
| --- | --- |
| **Status** | Implemented (syn-deep + un-stubbed Callers/Refs/Impls/SimilarFixes; **cargo metadata deferred post-Beta**; RA passthrough still deferred) |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md) (merged), [RFC-0002](./RFC-0002-storage-artifacts-session-events.md) (merged) |
| **Effort** | 6–10 person-days (M7 thin slice: 2–4 pd; Beta deepening: 4–6 pd) |
| **Crate (implementation)** | `alloy-index` — store, ingest, query engine, migrations |
| **Crate (seam only)** | `alloy-runtime::graph` — trait + IR types so `CapabilityContext` and the Context Engine can name them without a dependency cycle (§2.4) |
| **Related RFCs** | [0004](./RFC-0004-observability-cost-metering.md) tracing/metrics conventions · [0010](./RFC-0010-scheduler-runtime-adapters.md) diagnostics producer · [0012](./RFC-0012-context-engine.md) sole MVP query consumer · [0013](./RFC-0013-capability-registry-workers.md) `CapabilityContext.graph` · [0014](./RFC-0014-language-backend-rust.md) `LanguageBackend::index` (Beta) · [0015](./RFC-0015-cli-profiles-config.md) ingest trigger |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | The 126-line outline of this filename (expanded to implementation grade) |

**Mental model (V2 §7 / ADR F-02 / F-04):** the ProjectGraph is a **derived cache**, not a source of truth. Every row is reconstructible from the workspace on disk. It is written by exactly one in-process writer (the graph service), read in-process by workers through a handle with no mutation surface, and exposed to nothing over MCP. When the graph and the filesystem disagree, the filesystem wins and the graph is rebuilt.

**Authority order (highest → lowest):** current `main` source → merged RFCs 0001–0010, 0016 → Architecture V2 → this document → roadmaps. This RFC does not reshape any merged public type; the only changes to merged crates are the additive amendments explicitly authorised in §2.3.

---

## 1. Overview

### 1.1 Purpose

Fill the intentionally empty `alloy-index` crate with the **thin** ProjectGraph that V2 §7 specifies and that roadmap **M7** scopes:

1. A **normative node/edge model** — Workspace / Crate / Module / Item nodes, `Defines` / `Imports` edges, plus Diagnostic and FixEvent ingest records (V2 §7.2).
2. A **`ProjectGraph` trait** matching V2 §7.2 verbatim, with a read-only `GraphViewHandle` for workers and an ingest-only write surface.
3. A **SQLite store** under the already-reserved `<data_dir>/graph/` directory (`StorageLayout::graph_dir`), with its own migration ladder, following RFC-0002's storage conventions.
4. A **deterministic, idempotent, offline ingest pipeline** built from a workspace filesystem walk plus `Cargo.toml` manifest facts — **no `syn`, no `cargo metadata` subprocess, no network**.
5. **Query semantics** for the queries RFC-0012 actually consumes in MVP (`Symbol`, `Diagnostics`, `Subgraph`), with `Callers` / `SimilarFixes` / `Refs` / `Impls` specified as **Stub** returning empty views. *(All four have since been un-stubbed: `SimilarFixes` by A-0011-5, the other three by A-0011-6 — see §2.3c.)*
6. **Version, snapshot, corruption and quarantine** semantics so a bad graph is always recoverable by rebuilding from source (V2 §5.6).

### 1.2 Problem statement

`crates/alloy-index/src/lib.rs` is five lines of doc comment and one lint attribute. `StorageLayout::graph_dir` and the `sessions.graph_version` column were reserved by RFC-0002 and are unused. `GraphNodeId` and `GraphVersion` exist in `alloy-runtime::types::ids` with no consumer. RFC-0012's WorkingSet domain has no graph projection to project, RFC-0013's `CapabilityContext.graph` has no type to hold, and RFC-0014's `LanguageBackend::index(&self, root, graph: &dyn ProjectGraph)` has no trait to reference. Without this RFC every one of those seams stays notional.

The failure mode to avoid is the opposite one: V2 explicitly classifies the "typed multi-layer graph" as **Original proposal overstated**. The graph must stay thin, must never invent edges it cannot justify, and must not become a compiler frontend.

### 1.3 Scope

| In scope | Detail |
| --- | --- |
| Node / edge model | `GraphNodeKind`, `GraphEdgeKind`, stable paths, deterministic ids (§4) |
| Trait seam | `ProjectGraph`, `GraphQuery`, `GraphView`, `GraphError`, `GraphViewHandle`, `NullProjectGraph` in `alloy-runtime::graph` (§3) |
| SQLite store | Own DB file, own migration ladder, PRAGMAs, quarantine (§5, Appendix A) |
| Ingest | Filesystem walk + manifest parse; determinism, idempotency, caps (§6) |
| Incremental | `FileChange` application at crate / module-subtree granularity (§6.6) |
| Queries | `Symbol`, `Diagnostics`, `Subgraph` live; `Refs`/`Impls`/`Callers`/`SimilarFixes` **Stub** at MVP, all four live since A-0011-5/A-0011-6 (§7) |
| Diagnostic / fix ingest | `record_diagnostic` / `record_fix` round-trip (§6.7) |
| Versioning | `GraphVersion` monotonicity, content digest, snapshots (§4.6, §4.7) |
| Concurrency | Single writer, `spawn_blocking` for SQLite, no `unsafe` (§8) |
| Errors | `GraphError` taxonomy mapped onto `StoreError` (§9) |
| Observability | Tracing spans + `GraphMetricsSnapshot`; **no** session events from `alloy-index` (§10) |
| Security | In-process read-only workers, ingest-only writes, no MCP tool, no network, no symlink escape (§11) |
| Tests | Unit, integration, determinism, CI greps (§13) |

### 1.4 Non-goals

Each deferral names the seam that will carry it, so nothing has to be redesigned to enable it.

| Deferred item | Seam that already exists for it | Owner / when |
| --- | --- | --- |
| `syn`-deep AST index (real `Item` nodes, `Imports` edges) | `GraphNodeKind::Item`, `GraphEdgeKind::Imports`, `GraphFidelity::SynDeep` | **Beta** (roadmap "0011 deep") |
| `LanguageBackend` integration | `LanguageBackend::index(root, &dyn ProjectGraph)` (V2 §15) — this RFC ships the `&dyn ProjectGraph` half | **RFC-0014**, Beta |
| `cargo metadata` subprocess (resolved deps, features, workspace inheritance) | `IngestReport.source: GraphFidelity` (no separate `IngestSource` type); would need a new fidelity or report field when wired | **Post-Beta** — requires sandbox-mediated `Exec` (SEC5 forbids bare `Command` in `alloy-index`; `SqliteProjectGraph::open` / rebuild has no `SandboxBroker` / Exec grant today). Wiring that without a host-side callback redesign is out of Beta scope. |
| Typed `Calls` / `HasLifetime` edges | `graph_edges.confidence` column reserved; `GraphEdgeKind` is `#[non_exhaustive]` | Deferred (V2 §7.2 "Deferred") |
| `SimilarFixes` auto-retrieve beyond the A-0011-5c advisory note | `GraphQuery::SimilarFixes` reads recorded fixes back since A-0011-5a; `RepairWorker` renders one bounded note (≤ 4 codes, ≤ 8 rows, ≤ 1 KiB, never patch bodies) | Wider injection deferred until precision measured (V2 §7.2 upgrade path) |
| Embedding index / External Memory | none — explicitly absent | Deferred (ADR F-23) |
| Worker-facing `graph_query` MCP tool | **none, permanently** | **Eliminated** (ADR F-04) — see §11 |
| rust-analyzer passthrough for `Refs` / `Impls` (rustc-grade answers; the syntactic subset shipped via A-0011-6) | `GraphFidelity::Analyzer` reserved | Beta / M3, gated on RA being wired by RFC-0006 |
| Merkle multi-layer incremental | `graph_files.digest` + crate-granular invalidation | Deferred (V2 §7.2) |
| Background `alloyd` indexer | none | Deferred (ADR F-27) |
| PromptPack assembly | `GraphView` is the input; assembly is elsewhere | **RFC-0012** |
| Postgres / a sixth crate / any `unsafe` | none | Forbidden |
| Writing or overwriting `.env` | none | Forbidden |

### 1.5 Day-1 MVP (normative)

1. `alloy-index` MUST compile with `#![forbid(unsafe_code)]` **and** `#![deny(missing_docs)]`, and MUST add **no new external workspace dependency** (§12).
2. The trait, IR types, `GraphViewHandle`, and `NullProjectGraph` MUST live in `alloy-runtime::graph`; the store, ingest, and query engine MUST live in `alloy-index`. `alloy-runtime` MUST NOT depend on `alloy-index` (rule **C2**, CI-grepped).
3. `SqliteProjectGraph` MUST persist to `<data_dir>/graph/graph.sqlite`, derived from the existing `StorageLayout::graph_dir`, with its own `graph_schema_migrations` ledger at code version 1. It MUST NOT add a migration to `alloy.sqlite` (rule **S1**).
4. `rebuild` MUST be **deterministic** (same tree → same node ids, same edge set, same content digest) and **idempotent** (a second `rebuild` over an unchanged tree MUST NOT bump `GraphVersion`) — rules **IN5**, **IN6**.
5. Ingest MUST be **offline and exec-free**: filesystem reads and `Cargo.toml` parsing only. No subprocess, no network, no symlink traversal (rules **IN3**, **IN4**, **SEC5**).
6. MVP (manifest) ingest MUST create zero `Item` nodes and zero `Imports` edges (rules **IN8**, **IN9**). Those were **Stub** surfaces reserved for the Beta `syn` pass; that pass has since landed under RFC-0014 and populates both (§2.3c).
7. *(amended by A-0011-6, §2.3b)* `GraphQuery::Callers`, `Refs` and `Impls` answer from the `calls`/`references`/`impls` edges the deep pass records — never an error, never fabricated rows (rules **Q4**, **Q5** as amended). `GraphQuery::SimilarFixes` reads recorded fixes back since amendment A-0011-5a (§2.3a); it too never errors and never fabricates rows.
8. `GraphViewHandle` MUST expose no mutation method and MUST NOT be constructible into a writer (rule **SEC1**).
9. No `graph_query` MCP tool MUST exist for Alloy workers, in any crate (rule **SEC2**, CI-grepped).
10. `alloy-index` MUST NOT append session events or decision records; it emits `tracing` spans and an atomic metrics snapshot only (rule **OB1**).
11. Ingest MUST NOT be triggered by a capability worker. Only the CLI (RFC-0015) and the runtime host may call `rebuild` / `apply_incremental`; only the runtime host's verify path may call `record_diagnostic` / `record_fix` (rules **IN1**, **SEC4**).
12. Corruption MUST be handled by quarantine-and-rebuild, never by partial-row repair (rule **S8**, V2 §5.6).

### 1.6 Rule-ID index

| Prefix | Domain | Section |
| --- | --- | --- |
| **C** | Crate placement and dependency direction | §2.4 |
| **G** | Graph model invariants (ids, kinds, paths, versions) | §4 |
| **S** | Storage, schema, migration, quarantine | §5 |
| **IN** | Ingest pipeline | §6 |
| **Q** | Query semantics | §7 |
| **X** | Lifecycle and concurrency | §8 |
| **E** | Error taxonomy | §9 |
| **OB** | Observability | §10 |
| **SEC** | Security posture | §11 |
| **T** | Testing and CI greps | §13 |

---

## 2. Architecture Integration

### 2.1 Relationship to Architecture V2

| V2 reference | Application here |
| --- | --- |
| §4 Knowledge pillar | ProjectGraph is Knowledge; it owns `GraphStore`, it does **not** own LLM prompts (§11) |
| §5.2 responsibilities | "ProjectGraph — Index + diagnostic/fix ingest — owns GraphStore — does not own LLM prompts" (§3.5, §10) |
| §5.3 process topology | Single binary; `ProjectGraph (alloy-index)` is in-process (§8.1) |
| §5.4 crate layout | `alloy-index/ # ProjectGraph MVP` — implementation crate (§2.4) |
| §5.6 failure handling | "Graph corruption → rebuild from source; quarantine snapshot" (§5.7, §9.4) |
| §7.1 purpose | Persistent, queryable, survives sessions, feeds bounded Context projections (§7) |
| §7.2 architectural interface | Trait shape verbatim (§3.5); single writer (X1); read-only worker handle (SEC1); ingest-only writes (SEC3); **no worker `graph_query` MCP** (SEC2) |
| §7.2 MVP implementation | Workspace/Crate/Module/Item + Diagnostic + FixEvent; structural `Defines`/`Imports` **as available**; file-digest invalidation of module subgraphs (§4.2, §6.6) |
| §7.2 Stub | Shipped as: `Callers` / `SimilarFixes` empty; edge `confidence` reserved. Superseded: both live since A-0011-6 / A-0011-5; `confidence` remains reserved (Q5, Q6, S6) |
| §7.2 Deferred | Typed call/lifetime edges; SimilarFixes auto-retrieve; Merkle incremental; alloyd; embeddings (§1.4) |
| §7.2 Evolution | "Add layers behind the same query enum" — `GraphQuery` is frozen to V2's seven variants (Q1) |
| §7.3 persistence | `.alloy/graph/` (or XDG) — satisfied by `StorageLayout::graph_dir` (S1) |
| §8.1 Context Engine | The graph feeds WorkingSet projections; assembly is RFC-0012's (§1.4) |
| §9 `CapabilityContext.graph` | `GraphViewHandle` — "read-only query handle, not a mutation API" (§3.8) |
| §9 `CapabilityOutput` | `graph_mutations` **removed from workers** — no path re-introduces it (SEC3) |
| §12.2 | "Deleted for Alloy workers: `graph_query` MCP (ADR F-04)" (SEC2) |
| §15 LanguageBackend | `index(&self, root, graph: &dyn ProjectGraph)` — this RFC ships the trait object (§3.5) |
| §19.2 M2 / roadmap Beta | metadata+syn+diagnostics/fix ingest — MVP ships manifest ingest; syn landed at Beta (RFC-0014); `cargo metadata` deferred **post-Beta** (§1.4, §14.2) |
| §20 R6 | "Graph incorrect edges" → thin MVP, confidence reserved, rebuild path (G7, S6, §5.7) |
| §20 R16 | "rust-analyzer skew" → optional RA; syn/cargo degraded mode required (Q4, `GraphFidelity`) |
| §21.1 checklist | "Graph: in-process read; ingest-only writes; no worker graph_query MCP — Pass" (§11) |

### 2.2 Relationship to the roadmap (M7 thin vs Beta deep)

The roadmap is explicit: M7 ships **"RFC-0011 thin (trait + stubs / minimal metadata; Callers/SimilarFixes empty)"** and *"Do not block M7 on graph depth."* This RFC therefore makes the thin behaviour **normative**, not a temporary shortcut:

- Every deferred capability has a **named, typed surface** that exists on day 1 and returns an honest empty/degraded answer.
- No `TODO`, no `unimplemented!()`, no `todo!()` anywhere in scope (DoD gate 9). The word **Stub** in this document marks the only permitted "does nothing yet" behaviours, and each is pinned by a rule ID and an acceptance criterion.
- Deepening to Beta MUST NOT change the `ProjectGraph` trait, the `GraphQuery` enum, or the SQL table shapes — only the *population* of `graph_nodes` (Item rows), `graph_edges` (Imports rows), and `GraphView.fidelity`.

### 2.3 Relationship to RFC-0001 and RFC-0002 (merged) + authorised amendments

Reused unchanged: `GraphNodeId`, `GraphVersion`, `Timestamp`, `Digest`, `DigestHasher`, `DiagnosticEvent`, `DiagnosticLevel`, `SpanRef`, `DiagnosticId`, `SessionId`, `RunId`, `NodeId`, `ArtifactId`, `TransactionId`, `LanguageId`, `IdError`, `StoreError`, `StorageLayout`, `SqliteSynchronous`.

Three **additive** amendments are authorised by this RFC. Each is a derive or a new item; none reshapes an existing field.

| # | Amendment | Crate | Justification |
| --- | --- | --- | --- |
| **A1** | Add `Copy, PartialOrd, Ord` to `GraphVersion` | `alloy-runtime::types::ids` | It is `pub struct GraphVersion(pub u64)`; monotonicity assertions (G8) and `max()` over stored versions need ordering. Sound for a `u64` newtype. |
| **A2** | Add `name_id!(CrateId)` and `uuid_id!(GraphSnapshotId)` to `types::ids` | `alloy-runtime::types::ids` | V2 §7.2's `GraphQuery::Diagnostics { crate_id: Option<CrateId>, .. }` names a type that does not exist. Minted with the **existing** `name_id!` / `uuid_id!` macros so validation, serde, `Display` and parsing match every other catalog id. Both macros are private `macro_rules!` in `types::ids`, so both new ids MUST be minted there, not in `graph` (the macro is not visible to sibling modules). |
| **A3** | New module `alloy-runtime::graph` + crate-root re-exports | `alloy-runtime` | The seam (§2.4). Purely additive; no existing module changes. |

RFC-0002 conventions this RFC mirrors rather than re-uses (because `storage::open::{DbHandle, spawn_db}` are crate-private): single `Mutex<Option<Connection>>`, `PRAGMA foreign_keys=ON` → `busy_timeout` → `journal_mode=WAL` → `synchronous`, `OpenFlags::READ_WRITE|CREATE|NO_MUTEX`, integer migration ledger with refuse-newer, `spawn_blocking` wrapper, `close()` with WAL truncate-checkpoint, `Drop` warning when closed implicitly, and error mapping by `rusqlite::ErrorCode` (never by message substring).

RFC-0002's `sessions.graph_version INTEGER NULL` column is the **only** cross-database link. Rule **S2**: `alloy-index` MUST NOT read or write `alloy.sqlite`. The runtime host writes the `GraphVersion` integer it obtained from `rebuild` into that column; there is no foreign key across files.

### 2.3a Amendment A-0011-5 — the learning loop is closed (post-merge)

`SimilarFixes` and `record_fix` shipped as a write-only pair: fixes were stored (IN14) and never read back (Q6), because V2 §7.2 forbids auto prompt injection of past fixes "before precision is measured". That measurement cannot happen while nothing writes fixes and nothing reads them. This amendment turns the pair on, keeping every safeguard that made the stub cheap to reverse.

| # | Amendment | Rule amended | Statement |
| --- | --- | --- | --- |
| **A-0011-5a** | `SimilarFixes` is no longer a Stub | **Q6** | `SimilarFixes` MUST return the recorded `graph_fixes` rows whose `diagnostic_code` matches, most recent first (`recorded_at DESC`, insertion order as tie-break), capped by the query's own `limit` and by the store's query cap, setting `truncated` only when rows were left behind. It remains a read-only query (Q10) and still returns an empty view — never an error — when nothing matches. `Callers`, `Refs` and `Impls` stayed Stub at this amendment (Q4, Q5) and were un-stubbed by A-0011-6 (§2.3b). |
| **A-0011-5b** | The verify path records fixes | **IN1**, **IN14** | The runtime host's verify path MAY call `record_fix` as well as `record_diagnostic`. The permitted implementation is a `Verifier` decorator (`alloy-runtime::adapters::FixRecordingVerifier`) composed at the composition root: it records one `FixEvent` per diagnostic code that a failing verification reported, once a later verification of the same run passes *after a new `EditApplied`*. Ingest is bookkeeping — a graph error is logged and dropped, never returned as a verdict. |
| **A-0011-5c** | Past fixes may reach a repair prompt | **SEC4**, RFC-0013 **RW4** | `RepairWorker` MAY issue `SimilarFixes` for the diagnostic codes it already holds and render the result as one bounded, fenced, User-role advisory note (≤ 4 codes, ≤ 8 rows, ≤ 1 KiB). It reads through `GraphViewHandle` exactly as it reads `Diagnostics`; **SEC4** is unchanged — no capability may write the graph, and no worker is handed an `Arc<dyn ProjectGraph>`. Patch *bodies* are still never injected: the note carries codes, packages, dates and artifact ids only. |

Unchanged by this amendment: the `ProjectGraph` trait, the `GraphQuery` enum, `FixEvent`, and every SQL table (`graph_fixes` already indexes `(diagnostic_code, recorded_at)`), so there is no migration and `GRAPH_MODEL_VERSION` stays at its current value. Reversal is deleting the query arm and the decorator.

### 2.3b Amendment A-0011-6 — `Refs` / `Impls` / `Callers` are un-stubbed (post-merge, 2026-07-29)

Q4 and Q5 shipped as Stubs because the MVP graph had no edges that could answer them, and the deferred answer was pencilled in as rust-analyzer passthrough. The RFC-0014 `syn` deep pass changed the ground truth: the graph now holds real `Item` nodes and a real per-module `use` map, which is enough scope information to resolve a useful, **honest** subset of references, calls and impls without RA. This amendment records that subset and turns the three queries on. RA passthrough (`GraphFidelity::Analyzer`) remains the deferred path to *rustc-grade* answers; nothing here forecloses it.

| # | Amendment | Rule amended | Statement |
| --- | --- | --- | --- |
| **A-0011-6a** | The deep pass records semantic edges | **G2**, §4.3, RFC-0014 SY5 (partially) | Three edge kinds are added to `GraphEdgeKind` (`#[non_exhaustive]`, additive): `References` (a type usage, struct-literal path, trait bound, or multi-segment value path resolving to a workspace item), `Calls` (a call expression whose callee resolves to a workspace module-level `fn` item; a call resolving to any other item — e.g. a tuple-struct constructor — is recorded as `References`), and `Impls` (one `impl Trait for Type` edge, self-type item → trait item, when **both** sides resolve; inherent and negative impls record nothing). Impl blocks and their methods still get no nodes (SY5 holds); references and calls inside an impl block are attributed to the self-type item. All three kinds carry `confidence = 1.0` (G11 unchanged): edges below the confidence bar are **not written** rather than written down-weighted. |
| **A-0011-6b** | Resolution honesty rules | **G7** (restated, not weakened) | Resolution is syntactic, workspace-scoped, and best-effort. A leading segment resolves through, in order: `crate`/`self`/`super`/`::` prefixes; the module's own `use` bindings (rename-aware); workspace crate idents; the module's declared children; then unambiguous glob imports (two candidates → nothing). Never resolved, by design: method calls (no type inference), single-segment value paths (locals), generic parameters and `Self`, macro-generated code, patterns, `std`/registry paths. A missing edge is acceptable; an invented one is not. Known accepted imprecision: a body-local closure shadowing an in-scope `fn` name can mis-attribute a call, and `cfg`-gated variants are all recorded (SY7's posture). `GraphView.fidelity` stays `SynDeep` — these are parse-derived facts, not analyzer facts. |
| **A-0011-6c** | `Refs`, `Impls`, `Callers` answer | **Q4**, **Q5**, §9.3 | `Refs { node }` returns the anchor plus incoming `References` **and** `Imports` edges (a `use` of an item is a reference to it). `Callers { fn_node }` returns the anchor plus incoming `Calls` edges. `Impls { trait_node }` returns `Impls` edges touching the anchor in **either** direction, so one query answers both "who implements this trait" and "which traits does this type implement". All three: Q8 ordering, Q9 truncation at the node-ordering boundary, Q10 read-only. An unknown anchor returns `GraphView::empty(version)` with `truncated = false` — nothing was withheld, and fabricating rows for an id the graph never minted would violate G7. The former unconditional `truncated = true` marker is gone: `truncated` is now always literal. |
| **A-0011-6d** | Storage and versioning | **S3**, **S4** | `GRAPH_SCHEMA_VERSION` = 2: the v1 `graph_edges` kind `CHECK` admitted only `defines`/`imports`, so v2 recreates the table with the expanded list (rows carried over; refuse-newer unchanged). `GRAPH_MODEL_VERSION` = 3: an existing model-2 database is truncated and re-ingested on open (S4), never merged. `fidelity_for_model_version` maps 3 to `SynDeep` — deeper population of the same fidelity, no new `GraphFidelity` variant. |
| **A-0011-6e** | Reporting | `IngestReport` doc contract | `IngestReport` gains `references`, `calls`, `impls` counters (same authorisation as A-0014-3). The workspace `syn` pin gains the `visit` feature and the pass walks item bodies for reference collection — both sanctioned on the RFC-0014 side by amendments A-0014-5/A-0014-6 (the T20 forbidden-feature list is untouched). |
| **A-0011-6f** | `Subgraph` traversal stays structural | **Q7** | `Subgraph` BFS traverses the **structural** kinds only — `Defines`, and `Imports` since the RFC-0014 deep pass (whose Appendix B one-hop-to-an-imported-node projection this codifies). The semantic kinds (`References`/`Calls`/`Impls`) are **never traversed**: a call graph is asked for explicitly via `Callers`/`Refs`/`Impls`, not pulled into a neighbourhood prompt. Semantic edges whose endpoints both land in the view are still **returned**, per the §5 edge-inclusion rule (edges whose endpoints are both in `nodes`). |

Reversal is deleting the three query arms, the collector/resolver in the pass, and bumping the model version again. Consumers that relied on the stub `truncated = true` marker (none were found in-tree; at the time RFC-0012's D14 grep forbade constructing these queries in the context engine — since amended by A-0012-1a to permit bounded `Callers`/`Refs` impact reads) must treat `truncated` literally.

### 2.3c Status of the "deep" remainder (post-merge audit, 2026-07-30)

Where the Beta deepening stands against this RFC's deferred list (§1.4, §14.2):

- **Deep-done, via #61 (A-0011-5) and #62 (A-0011-6), both merged on `main`:** `SimilarFixes` reads recorded fixes back (Q6, `query.rs::similar_fixes`) and the verify path records them (`FixRecordingVerifier`); `Refs` / `Impls` / `Callers` answer from recorded `References` / `Calls` / `Impls` edges (Q4, Q5, `query.rs::neighbours`); `truncated` is literal; `GRAPH_SCHEMA_VERSION = 2`, `GRAPH_MODEL_VERSION = 3` (S3, S4 as amended); `Subgraph` traverses the structural kinds only (Q7). `RepairWorker` renders the bounded A-0011-5c advisory note, and RFC-0012's engine issues bounded `Callers`/`Refs` impact reads (A-0012-1, #63).
- **Owned by RFC-0014 (the `syn` deep pass, on `main` under `crates/alloy-index/src/lang/`):** `Item` nodes, `Imports` edges, `GraphFidelity::SynDeep`, `GRAPH_MODEL_VERSION = 2` (later 3 per A-0011-6d), the `IngestReport.items`/`imports` counters (A-0014-3), and the `RustBackend` `LanguageBackend` seam. RFC-0011 keeps the *query* semantics and the store invariants; the population pass is theirs.
- **Still deferred:** rust-analyzer passthrough for rustc-grade `Refs`/`Impls` (`GraphFidelity::Analyzer`, Beta/M3 as available), Merkle multi-layer incremental, the background `alloyd` indexer (ADR F-27), embedding recall (ADR F-23), sub-1.0 edge confidence, and wider `SimilarFixes` auto-injection pending precision measurement (V2 §7.2 upgrade path).
- **`cargo metadata` facts — deferred past Beta (explicit):** resolved deps, features, and workspace inheritance stay out of the Beta deep remainder. Reason: V2 / this RFC require sandbox-first Exec (no bare process from the index); **SEC5** forbids `std::process::Command` and network deps in `alloy-index`; `GraphOpenOptions` / `rebuild` take no `SandboxBroker` or Exec grant. Implementing metadata ingest would need a host-injected sandboxed Exec seam (composition-root / CLI assembly), not a moderate in-crate change. `IngestReport.source` remains `GraphFidelity` (Manifest / SynDeep today); there is no `IngestSource::CargoMetadata` type. See §1.4, §6.2, §14.2.

### 2.4 Crate placement decision (normative)

`GraphNodeId`, `GraphVersion`, `DiagnosticEvent` and `Timestamp` are already published by `alloy-runtime` (RFC-0001, merged). `CapabilityContext.graph: GraphViewHandle` (V2 §9) and `LanguageBackend::index(.., &dyn ProjectGraph)` (V2 §15) both live in `alloy-runtime`. Therefore `alloy-runtime` must be able to *name* the trait, and `alloy-index` must be able to *use* the merged ids. Only one direction is acyclic.

| Rule | Statement |
| --- | --- |
| **C1** | The `ProjectGraph` trait and all IR types it mentions MUST live in `alloy-runtime::graph`. |
| **C2** | `alloy-runtime` MUST NOT depend on `alloy-index`. Enforced by a CI grep over `crates/alloy-runtime/Cargo.toml` (T9). |
| **C3** | `alloy-index` MUST depend on `alloy-runtime` with `default-features = false` (no `reqwest`/`rustls` linkage), matching `alloy-eval`'s offline posture. |
| **C4** | `alloy-runtime::graph` MUST contain **no** SQL, **no** filesystem walking, and **no** `rusqlite` import. It is types + trait + `NullProjectGraph` + `GraphViewHandle` only. Enforced by a CI grep (T10). |
| **C5** | `alloy-index` MUST NOT depend on `alloy-tools`, `alloy-cli`, or `alloy-eval`. It performs no tool calls and no process execution. |
| **C6** | The workspace stays at **five** crates. No `alloy-graph`, no `alloy-lang-*`. |

This is exactly the precedent RFC-0010 set with its runtime adapters: `VerifyCompileAdapter`, `VerifyTestAdapter`, `GateHumanAdapter` and `CapabilityExecutor` are traits in `alloy-runtime::adapters`, with real implementations wired by the composition root against `alloy-tools`' MCP host. V2 §5.4's `alloy-index/ # ProjectGraph MVP` refers to the index implementation, which is what lands there.

```text
alloy-cli ──► alloy-index ──► alloy-runtime
     │                            ▲
     └──────► alloy-tools ────────┘
```

Wiring: `alloy-cli` constructs `SqliteProjectGraph`, wraps it in `Arc<dyn ProjectGraph>`, hands the runtime a `GraphViewHandle` for `CapabilityContext`, and retains the `Arc` itself for ingest calls.

### 2.5 Already implemented | Added by RFC-0011 | Deferred

| Already on `main` | Added here | Deferred |
| --- | --- | --- |
| `GraphNodeId`, `GraphVersion` | `CrateId` (A2), `GraphSnapshotId` | — |
| `StorageLayout::graph_dir` (created, unused) | `GraphLayout`, `graph.sqlite`, migrations v1 (v2 since A-0011-6d) | schema v3+ |
| `sessions.graph_version` column (never written) | Value produced by `rebuild` for the host to store | Session-pinned historical queries |
| `DiagnosticEvent` + `VerifyOutcome.diagnostics` seam | `record_diagnostic` persistence + `Diagnostics` query | cargo-JSON→`DiagnosticEvent` parser (RFC-0013/0014); diagnostic clustering |
| `alloy-index` empty crate | Whole crate | `syn` pass, RA passthrough |
| `StoreError`, PRAGMA/migration pattern | `GraphError`, `graph_schema_migrations` | — |

### 2.6 What downstream RFCs may rely on

| RFC | May rely on | MUST NOT rely on |
| --- | --- | --- |
| **0012** Context Engine | `GraphViewHandle::query`, `Symbol` / `Diagnostics` / `Subgraph`, bounded `Callers` / `Refs` impact reads (A-0012-1), `GraphView.fidelity`, deterministic ordering (Q8) | `Impls` / `SimilarFixes` (grep-forbidden in `context/**`); rustc-grade completeness — resolution is syntactic and best-effort (G7) |
| **0013** Workers | `CapabilityContext.graph: GraphViewHandle` | Any write method; any MCP graph tool |
| **0014** LanguageBackend | `&dyn ProjectGraph` with `record_diagnostic` and a Beta-added deep-ingest path | Changing the trait signature |
| **0015** CLI | `SqliteProjectGraph::open`, `rebuild`, `close`, `GraphMetricsSnapshot` | Bypassing `rebuild` to write rows |

---

## 3. Public Rust API

All signatures below are normative. `#[non_exhaustive]` is applied where later layers are expected (V2 §7.2 "add layers behind the same query enum").

### 3.1 New identifiers

```rust
// alloy-runtime::types::ids (amendment A2 — both minted here; the id
// macros are private to this module)
name_id!(
    /// Cargo package name as it appears in `[package] name` (RFC-0011).
    CrateId
);
uuid_id!(
    /// Identifier of a recorded graph snapshot (RFC-0011 §4.7).
    GraphSnapshotId
);
```

`GraphNodeId` keeps its merged `uuid_id!` shape. This RFC does **not** mint node ids randomly; see rule **G3**. The derivation itself is part of the seam (T1a–T1c test it there), so `alloy-runtime::graph` exports it:

```rust
// alloy-runtime::graph — the one blessed way to mint a graph node id (G3/G4).
/// Derive the deterministic `GraphNodeId` for `(kind, stable_key)`.
#[must_use]
pub fn derive_node_id(kind: GraphNodeKind, stable_key: &str) -> GraphNodeId;
```

### 3.2 Node and edge model

```rust
/// Kind of a project-graph node (Architecture V2 §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphNodeKind {
    /// The cargo workspace root.
    Workspace,
    /// A cargo package inside the workspace.
    Crate,
    /// A Rust module inferred from source-file layout.
    Module,
    /// A named module-level item (fn/struct/enum/union/trait/type/const/
    /// static). Ingested by the RFC-0014 `syn` deep pass (SY3); `impl`
    /// blocks stay deferred (SY5). Zero written by the MVP manifest pass (IN9).
    Item,
}

/// Kind of a project-graph edge (Architecture V2 §7.2, extended by A-0011-6a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphEdgeKind {
    /// Structural containment: workspace→crate, crate→module, module→module, module→item.
    Defines,
    /// `use` relationship, written only for in-workspace targets. Ingested
    /// by the RFC-0014 `syn` deep pass (SY11–SY13). Zero written by the MVP
    /// manifest pass (IN8).
    Imports,
    /// Path or type usage resolving to an in-workspace item (A-0011-6a).
    References,
    /// Function call whose callee resolves to an in-workspace `fn` item (A-0011-6a).
    Calls,
    /// Trait implementation: self-type item → trait item, both sides resolved (A-0011-6a).
    Impls,
}

/// How much of the graph is derived from real parsing (V2 §20 R16 degraded mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphFidelity {
    /// Manifest + file-layout facts only. The MVP value.
    Manifest,
    /// `syn` item-level parse (RFC-0014 deep pass, `model_version >= 2`).
    SynDeep,
    /// Reserved: rust-analyzer passthrough (Beta/M3).
    Analyzer,
}

/// A node as returned by a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Deterministic id (G3).
    pub id: GraphNodeId,
    /// Node kind.
    pub kind: GraphNodeKind,
    /// Canonical stable path (G4), e.g. `my_crate::io::reader`.
    pub path: String,
    /// Owning package, `None` for `Workspace`.
    pub crate_id: Option<CrateId>,
    /// Workspace-relative primary source file, when the node has one.
    pub file: Option<String>,
    /// SHA-256 of `file`'s bytes at ingest time, when `file` is `Some`.
    pub digest: Option<Digest>,
}

/// An edge as returned by a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source node.
    pub from: GraphNodeId,
    /// Target node.
    pub to: GraphNodeId,
    /// Edge kind.
    pub kind: GraphEdgeKind,
    /// Reserved confidence; `1.0` for every ingested edge (S6, G11) —
    /// edges below the bar are not written rather than written down-weighted.
    pub confidence: f32,
}
```

### 3.3 `GraphQuery` (frozen to V2 §7.2)

```rust
/// Read queries. **Frozen**: exactly the seven variants of Architecture V2 §7.2 (Q1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphQuery {
    /// Resolve a Rust path (`my_crate::io`) or a workspace-relative file path (Q2).
    Symbol { path: String },
    /// Who references this node: the anchor plus incoming `References` and
    /// `Imports` edges. Live since amendment A-0011-6 (Q4); an unknown
    /// anchor is an empty view, never an error.
    Refs { node: GraphNodeId },
    /// `Impls` edges touching this node in either direction: anchored at a
    /// trait it answers "who implements it"; anchored at a type it answers
    /// "which traits does it implement". Live since amendment A-0011-6
    /// (Q4); an unknown anchor is an empty view, never an error.
    Impls { trait_node: GraphNodeId },
    /// Who calls this fn node: the anchor plus incoming `Calls` edges. Live
    /// since amendment A-0011-6 (Q5); an unknown anchor is an empty view,
    /// never an error.
    Callers { fn_node: GraphNodeId },
    /// Recorded diagnostics, optionally scoped and time-filtered (Q3).
    Diagnostics { crate_id: Option<CrateId>, since: Option<Timestamp> },
    /// Fixes recorded for a diagnostic code, most recent first. Live since
    /// amendment A-0011-5; empty until something has been recorded (Q6).
    SimilarFixes { diagnostic_code: String, limit: usize },
    /// Breadth-first neighbourhood around seeds (Q7).
    Subgraph { seeds: Vec<GraphNodeId>, radius: u8 },
}
```

### 3.4 `GraphView`

```rust
/// Result of a query. Always well-formed, possibly empty, never fabricated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphView {
    /// Graph version this view was read at.
    pub version: GraphVersion,
    /// Matched nodes, sorted by (kind, path, id) — Q8.
    pub nodes: Vec<GraphNode>,
    /// Edges whose endpoints are both in `nodes`, sorted by (from, to, kind) — Q8.
    pub edges: Vec<GraphEdge>,
    /// Diagnostics, populated only by `GraphQuery::Diagnostics`.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Fix records, populated only by `GraphQuery::SimilarFixes`.
    pub fixes: Vec<FixEvent>,
    /// Fidelity of the data backing this view, computed from
    /// `graph_meta.model_version` (`Manifest` at model 1, `SynDeep` at
    /// model ≥ 2 — the one seam function, RS4/A-0014-4).
    pub fidelity: GraphFidelity,
    /// `true` when the result was capped (Q9). Since amendment A-0011-6
    /// this flag is literal: an empty answer that withheld nothing is not
    /// truncated.
    pub truncated: bool,
}

impl GraphView {
    /// An empty view at `version` with `GraphFidelity::Manifest`.
    #[must_use]
    pub fn empty(version: GraphVersion) -> Self;

    /// `true` when no nodes, edges, diagnostics or fixes were returned.
    #[must_use]
    pub fn is_empty(&self) -> bool;
}
```

### 3.5 `ProjectGraph` trait (V2 §7.2 verbatim)

```rust
/// Persistent project model. Exactly one writer per data directory (X1).
#[async_trait]
pub trait ProjectGraph: Send + Sync {
    /// Full ingest of `root`. Deterministic and idempotent (IN5, IN6).
    async fn rebuild(&self, root: &Path) -> Result<GraphVersion, GraphError>;

    /// Apply file-level changes. Empty slice is a no-op returning the current version.
    async fn apply_incremental(&self, changes: &[FileChange]) -> Result<GraphVersion, GraphError>;

    /// Read query. MUST NOT mutate persistent state (Q10).
    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError>;

    /// Ingest one compiler/tool diagnostic (IN13).
    async fn record_diagnostic(&self, d: DiagnosticEvent) -> Result<(), GraphError>;

    /// Ingest one applied-fix record (IN14).
    async fn record_fix(&self, f: FixEvent) -> Result<(), GraphError>;

    /// Record an immutable marker of the current version (§4.7).
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError>;
}
```

Additive defaulted method (not in V2, additive-only so no implementor breaks):

```rust
    /// Current version without running a query. Default: `Ok(GraphVersion(0))`.
    async fn version(&self) -> Result<GraphVersion, GraphError> { Ok(GraphVersion(0)) }
```

### 3.6 Ingest-only write types

```rust
/// A file-level change fed to `apply_incremental`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Workspace-relative path with `/` separators (IN11).
    pub path: String,
    /// What happened to it.
    pub kind: FileChangeKind,
}

/// Change classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// File appeared.
    Created,
    /// Contents changed.
    Modified,
    /// File removed.
    Deleted,
}

/// A successfully applied fix, recorded for `SimilarFixes` retrieval
/// (read back since amendment A-0011-5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixEvent {
    /// Diagnostic this fix addressed, when known.
    pub diagnostic: Option<DiagnosticId>,
    /// Diagnostic code the fix addressed (`E0502`, …) — the `SimilarFixes` key.
    pub diagnostic_code: Option<String>,
    /// Owning package, when known.
    pub crate_id: Option<CrateId>,
    /// EditEngine transaction that applied it (RFC-0008), when known.
    pub transaction: Option<TransactionId>,
    /// Content-addressed patch artifact (RFC-0002 CAS), when stored.
    pub patch_artifact: Option<ArtifactId>,
    /// Whether the post-edit verify succeeded.
    pub verified: bool,
    /// Ingest timestamp.
    pub recorded_at: Timestamp,
}

/// Outcome summary of an ingest pass (returned via `rebuild_reported`,
/// logged by the caller). Constructed by `alloy-index`, so not
/// `#[non_exhaustive]` — adding a field is an API change by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// Version after the pass.
    pub version: GraphVersion,
    /// `true` when nothing changed and the version was not bumped (IN6).
    pub unchanged: bool,
    /// Counts.
    pub crates: u32,
    /// Module nodes written.
    pub modules: u32,
    /// Item nodes written (RFC-0014 amendment A-0014-3).
    pub items: u32,
    /// Imports edges written (RFC-0014 amendment A-0014-3).
    pub imports: u32,
    /// References edges written (RFC-0011 amendment A-0011-6).
    pub references: u32,
    /// Calls edges written (RFC-0011 amendment A-0011-6).
    pub calls: u32,
    /// Impls edges written (RFC-0011 amendment A-0011-6).
    pub impls: u32,
    /// Files tracked for digest invalidation.
    pub files: u32,
    /// Files skipped by a cap or a skip rule (IN3).
    pub skipped: u32,
    /// Manifest-level problems that did not abort the pass (IN12).
    pub warnings: Vec<String>,
    /// Where the facts came from (MVP: `Manifest`; `SynDeep` at model ≥ 2).
    pub source: GraphFidelity,
}
```

### 3.7 `GraphError`

```rust
/// ProjectGraph failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// Node, snapshot, or row not present.
    #[error("not found: {0}")]
    NotFound(String),
    /// Workspace root missing, unreadable, or not a cargo workspace.
    #[error("workspace: {0}")]
    Workspace(String),
    /// Manifest could not be parsed.
    #[error("manifest {path}: {reason}")]
    Manifest { path: String, reason: String },
    /// Ingest exceeded a configured cap.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// Query argument rejected before touching storage.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// Store is busy after the configured timeout.
    #[error("busy")]
    Busy,
    /// Filesystem or SQLite I/O.
    #[error("io: {0}")]
    Io(String),
    /// Graph data is unusable; caller should rebuild (§5.7).
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Schema migration failure or refuse-newer.
    #[error("migration: {0}")]
    Migration(String),
    /// The graph has been closed.
    #[error("closed")]
    Closed,
    /// This implementation does not provide a graph (`NullProjectGraph` writes).
    #[error("graph disabled")]
    Disabled,
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StoreError> for GraphError { /* §9.2 table */ }
```

### 3.8 `GraphViewHandle` (worker-facing, read-only)

```rust
/// Read-only query handle handed to capability workers (V2 §9).
///
/// There is deliberately no mutation method and no accessor that yields the
/// underlying `Arc<dyn ProjectGraph>` (SEC1).
#[derive(Clone)]
pub struct GraphViewHandle {
    inner: Arc<dyn ProjectGraph>,
}

impl GraphViewHandle {
    /// Wrap a graph for read-only use.
    #[must_use]
    pub fn new(graph: Arc<dyn ProjectGraph>) -> Self;

    /// A handle backed by `NullProjectGraph` (pre-wiring, tests, `--no-graph`).
    #[must_use]
    pub fn null() -> Self;

    /// Run a read query.
    pub async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError>;

    /// Current graph version.
    pub async fn version(&self) -> Result<GraphVersion, GraphError>;
}

impl std::fmt::Debug for GraphViewHandle { /* opaque: prints `GraphViewHandle` only */ }
```

### 3.9 `NullProjectGraph` (Stub implementation)

```rust
/// Graph that stores nothing. Mirrors `NullScheduler`'s role from RFC-0001.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProjectGraph;

#[async_trait]
impl ProjectGraph for NullProjectGraph {
    async fn rebuild(&self, _root: &Path) -> Result<GraphVersion, GraphError> { Err(GraphError::Disabled) }
    async fn apply_incremental(&self, _c: &[FileChange]) -> Result<GraphVersion, GraphError> { Err(GraphError::Disabled) }
    async fn query(&self, _q: GraphQuery) -> Result<GraphView, GraphError> { Ok(GraphView::empty(GraphVersion(0))) }
    async fn record_diagnostic(&self, _d: DiagnosticEvent) -> Result<(), GraphError> { Err(GraphError::Disabled) }
    async fn record_fix(&self, _f: FixEvent) -> Result<(), GraphError> { Err(GraphError::Disabled) }
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> { Err(GraphError::Disabled) }
    async fn version(&self) -> Result<GraphVersion, GraphError> { Ok(GraphVersion(0)) }
}
```

Rule **Q10**: reads on `NullProjectGraph` succeed empty (so a context assembler never fails because the graph is off); writes fail loudly with `Disabled` (so a mis-wired ingest is not silently swallowed).

### 3.10 `SqliteProjectGraph` (`alloy-index`)

```rust
/// On-disk layout under `StorageLayout::graph_dir`.
#[derive(Debug, Clone)]
pub struct GraphLayout {
    /// `<data_dir>/graph`.
    pub root: PathBuf,
    /// `<data_dir>/graph/graph.sqlite`.
    pub db_path: PathBuf,
    /// `<data_dir>/graph/quarantine`.
    pub quarantine_dir: PathBuf,
}

impl GraphLayout {
    /// Derive from the RFC-0002 storage layout.
    #[must_use]
    pub fn from_storage_layout(layout: &StorageLayout) -> Self;
    /// Derive from a data directory root.
    #[must_use]
    pub fn from_data_dir(data_dir: impl Into<PathBuf>) -> Self;
    /// Create `root` and `quarantine/` if missing.
    pub fn ensure_dirs(&self) -> Result<(), GraphError>;
}

/// Open options mirroring `StorageOpenOptions` (RFC-0002).
#[derive(Debug, Clone)]
pub struct GraphOpenOptions {
    /// Paths.
    pub layout: GraphLayout,
    /// `PRAGMA journal_mode = WAL` (default `true`).
    pub wal: bool,
    /// SQLite busy timeout (default `5000`).
    pub busy_timeout_ms: u32,
    /// `PRAGMA synchronous` (default `Normal`).
    pub synchronous: SqliteSynchronous,
    /// Refuse a DB whose schema version is newer than this build (default `true`).
    pub refuse_newer_schema: bool,
    /// Quarantine + recreate on `Corrupt` at open instead of failing (default `true`).
    pub quarantine_on_corrupt: bool,
    /// Ingest caps.
    pub limits: IngestLimits,
}

/// Deterministic ingest caps (IN3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestLimits {
    /// Max directory depth below the workspace root (default `32`).
    pub max_depth: u32,
    /// Max source files visited per pass (default `50_000`).
    pub max_files: u32,
    /// Max packages (default `1_000`).
    pub max_crates: u32,
    /// Max bytes hashed per file (default `4 * 1024 * 1024`); larger files are
    /// tracked by size+mtime-free marker digest and counted as skipped.
    pub max_file_bytes: u64,
    /// Max nodes returned by one query (default `2_000`) — Q9.
    pub max_query_nodes: u32,
}

/// SQLite-backed `ProjectGraph`. The single writer for its data directory.
#[derive(Debug)]
pub struct SqliteProjectGraph { /* private */ }

impl SqliteProjectGraph {
    /// Open (creating and migrating as needed).
    pub async fn open(opts: GraphOpenOptions) -> Result<Self, GraphError>;

    /// Schema version of the open database.
    #[must_use]
    pub fn schema_version(&self) -> u32;

    /// Model version of the open database (§5.4).
    #[must_use]
    pub fn model_version(&self) -> u32;

    /// Layout in use.
    #[must_use]
    pub fn layout(&self) -> &GraphLayout;

    /// Metrics snapshot (§10.2).
    #[must_use]
    pub fn metrics(&self) -> GraphMetricsSnapshot;

    /// Full ingest returning the detailed report (`rebuild` returns only the version).
    pub async fn rebuild_reported(&self, root: &Path) -> Result<IngestReport, GraphError>;

    /// WAL truncate-checkpoint and close. Idempotent (X5).
    pub async fn close(&self) -> Result<(), GraphError>;
}

#[async_trait]
impl ProjectGraph for SqliteProjectGraph { /* §6, §7 */ }
```

### 3.11 Crate-root re-exports

`alloy-runtime` re-exports from `graph`: `derive_node_id`, `FileChange`, `FileChangeKind`, `FixEvent`, `GraphEdge`, `GraphEdgeKind`, `GraphError`, `GraphFidelity`, `GraphNode`, `GraphNodeKind`, `GraphQuery`, `GraphSnapshotId`, `GraphView`, `GraphViewHandle`, `IngestReport`, `NullProjectGraph`, `ProjectGraph` (plus `CrateId` from `types::ids`).

`alloy-index` re-exports: `GraphLayout`, `GraphMetricsSnapshot`, `GraphOpenOptions`, `IngestLimits`, `SqliteProjectGraph`, and `pub use alloy_runtime::graph::*` is **forbidden** — consumers import the seam from `alloy-runtime` directly (rule **C4b**), so there is exactly one path to each type.

---

## 4. Data model and invariants

### 4.1 Rules

| Rule | Statement |
| --- | --- |
| **G1** | The graph is a **derived cache**. Every persisted row MUST be reconstructible from the workspace tree. No row may be the only copy of anything. |
| **G2** | *(amended by A-0011-6a)* Node kinds are exactly `Workspace`, `Crate`, `Module`, `Item`; edge kinds are exactly `Defines`, `Imports`, `References`, `Calls`, `Impls` (V2 §7.2 plus §2.3b). Adding a kind is a schema-model bump (S4), not an ad-hoc write — A-0011-6 bumped both (schema 2, model 3). |
| **G3** | `GraphNodeId` MUST be **derived**, never random: `id = uuid_from(sha256("alloyg1\0" ‖ kind_tag ‖ "\0" ‖ stable_key))`, taking the first 16 digest bytes, forcing the UUID version nibble to `8` and the variant bits to `0b10`, formatting `8-4-4-4-12` lowercase hex, then `GraphNodeId::parse`. `ProjectGraph` implementations MUST NOT call `GraphNodeId::new()`. |
| **G4** | `stable_key` MUST be workspace-relative and platform-independent: `Workspace` → `"."`; `Crate` → `"<package_name>\0<manifest_rel_path>"`; `Module` → `"<package_name>\0<module_path>"`; `Item` → `"<package_name>\0<module_path>\0<item_kind>\0<item_name>"`. All path separators normalised to `/` before hashing. |
| **G5** | There is **no** `File` node kind. Files are tracked in `graph_files` for digest invalidation and referenced by `GraphNode.file`. |
| **G6** | Diagnostics and fixes are **records, not nodes**. They live in their own tables keyed by crate / module / path. No `GraphEdgeKind` is minted to attach them. |
| **G7** | The graph MUST NOT invent edges. If a relationship is not directly derivable from the ingest source, no row is written (V2 §7.2 "never invent call edges"; §20 R6). |
| **G8** | `GraphVersion` is monotonically non-decreasing for the lifetime of a database file and starts at `0` (empty graph). It increments by exactly `1` on every pass that changes the content digest. |
| **G9** | `GraphNode.path` is unique per `(kind, path)`; `graph_nodes` carries a `UNIQUE(kind, path)` constraint. Two ingest passes over the same tree produce byte-identical paths. |
| **G10** | A snapshot is an immutable `(GraphSnapshotId, GraphVersion, content_digest, node_count, edge_count, created_at)` marker. It does **not** copy rows; historical row-level recall is deferred. |
| **G11** | `GraphEdge.confidence` is reserved and MUST be exactly `1.0` for every MVP-ingested edge (S6). |
| **G12** | Every value crossing the API boundary as a path is workspace-relative with `/` separators. Absolute host paths MUST NOT be persisted in `graph_nodes` or `graph_files` (also a redaction property — SEC6). |

### 4.2 Node kinds in MVP

| Kind | MVP populated? | `path` form | `file` | Source |
| --- | --- | --- | --- | --- |
| `Workspace` | Yes, exactly one | `"."` | root `Cargo.toml` | Manifest |
| `Crate` | Yes, one per workspace member | package name, e.g. `alloy-index` | member `Cargo.toml` | Manifest |
| `Module` | Yes, one per inferred module | `crate_ident::a::b` | the `.rs` file | File layout |
| `Item` | No from the manifest pass (IN9); populated by the RFC-0014 deep pass (SY3) | `crate_ident::a::b::Name` | the `.rs` file | `syn` |

`crate_ident` is the package name with `-` replaced by `_` (the Rust identifier), so `alloy-index`'s root module path is `alloy_index`.

### 4.3 Edge kinds in MVP

| Kind | MVP populated? | Endpoints |
| --- | --- | --- |
| `Defines` | Yes | Workspace→Crate, Crate→root Module, Module→child Module, Module→Item (Beta) |
| `Imports` | Beta (RFC-0014 SY11–SY13) | Module→Module / Module→Item |
| `References` | Beta (A-0011-6a) | Item→Item (from an impl block: self-type Item→Item) |
| `Calls` | Beta (A-0011-6a) | Item(fn or self type)→Item(fn) |
| `Impls` | Beta (A-0011-6a) | Item(self type)→Item(trait) |

### 4.4 Diagnostics records

`record_diagnostic` persists the `DiagnosticEvent` JSON plus extracted, indexed columns: `diagnostic_id`, `code`, `level`, `package`, `fingerprint`, `primary_path` (first span's path, workspace-relativised), `recorded_at`. Children are stored inside the JSON blob and are **not** separately indexed.

### 4.5 Fix records

`record_fix` persists the `FixEvent` fields as columns. `diagnostic_code` is indexed because it is the `SimilarFixes` key. The table shipped write-only at MVP and is read back through `query` since amendment A-0011-5a (Q6).

### 4.6 Versioning

The **content digest** is `sha256` over a canonical rendering of the graph: all `graph_nodes` rows sorted by `(kind, path)` then all `graph_edges` rows sorted by `(from_path, to_path, kind)`, each field `\0`-delimited, rows `\n`-delimited. Rule **G8** then reduces to: *bump the version iff the recomputed digest differs from the stored one.*

Diagnostic and fix ingest do **not** bump `GraphVersion` (rule **IN15**) — they are append-only observation records, not structure. This keeps `sessions.graph_version` stable across a repair loop, which is what RFC-0012 needs for cache discipline.

### 4.7 Snapshots

`snapshot()` inserts one `graph_snapshots` row for the current version and returns its id. Two snapshots at the same version are permitted and produce distinct ids. Snapshots are also written automatically before quarantining a corrupt database when the metadata is still readable (§5.7), which is what V2 §5.6's "quarantine snapshot" means here.

---

## 5. Storage

### 5.1 Rules

| Rule | Statement |
| --- | --- |
| **S1** | The graph MUST live in its **own** SQLite file at `<data_dir>/graph/graph.sqlite`, derived from `StorageLayout::graph_dir`. It MUST NOT add a migration to `alloy.sqlite`. |
| **S2** | `alloy-index` MUST NOT open, read, or write `alloy.sqlite`. The only cross-DB link is the `GraphVersion` integer the host stores in `sessions.graph_version`. |
| **S3** | *(amended by A-0011-6d, §2.3b)* The database carries an integer `graph_schema_version` in `graph_schema_migrations`; code version is `GRAPH_SCHEMA_VERSION = 2` (v1 shipped at `1`; v2 recreates `graph_edges` with the expanded kind `CHECK`). Opening a DB with a higher version MUST fail `GraphError::Migration` when `refuse_newer_schema` is set. |
| **S4** | *(amended by A-0011-6d, §2.3b)* The database also carries a **model version** (`GRAPH_MODEL_VERSION = 3`: `1` at MVP, `2` since the RFC-0014 `syn` deep pass, `3` since A-0011-6 records the semantic edge kinds) in `graph_meta`. A model-version mismatch MUST cause the graph tables to be **truncated and re-ingested**, not migrated — the graph is a derived cache (G1). |
| **S5** | Migrations MUST follow RFC-0002's shape: `const V1_SQL: &str`, sequential `if current < N` blocks inside `conn.unchecked_transaction()`, an `INSERT INTO graph_schema_migrations` row, and `current_version = SELECT MAX(version)`. |
| **S6** | `graph_edges.confidence REAL NOT NULL DEFAULT 1.0` is created in v1 and MUST be `1.0` in every MVP row (G11). |
| **S7** | PRAGMAs MUST be applied in RFC-0002's order: `foreign_keys = ON` → `busy_timeout` → `journal_mode = WAL` → `synchronous`. Open flags are `READ_WRITE \| CREATE \| NO_MUTEX`. |
| **S8** | On `GraphError::Corrupt` at open with `quarantine_on_corrupt`, the file MUST be moved to `<graph_dir>/quarantine/graph.sqlite.<unix_nanos>` (with its `-wal`/`-shm` sidecars) and a fresh empty database created. The event MUST be `tracing::warn!`-logged and counted (`quarantines`). No partial-row repair is ever attempted. |
| **S9** | `rusqlite::Error` MUST be mapped by `ErrorCode`, never by message substring, matching `storage::error::from_rusqlite`. |

### 5.2 Why a separate database (justified against V2)

- V2 §7.3 states persistence is `.alloy/graph/` — a directory distinct from the session DB. RFC-0002 already created it as `StorageLayout::graph_dir` and labelled it *"reserved for RFC-0011"*.
- RFC-0002's migration ladder (`CODE_SCHEMA_VERSION`, `V1_SQL`..`V3_SQL`) is private to `alloy-runtime::storage`. A crate that `alloy-runtime` does not depend on cannot register a migration into it (C2).
- The graph is a **derived cache** (G1) with a wipe-and-rebuild recovery path; the session log is durable history that must never be wiped. Co-locating them would make "quarantine the graph" indistinguishable from "lose the session log."
- Blast radius: a corrupt graph file cannot take down session resume.

Cost accepted: no cross-table foreign key between `sessions` and graph rows. Rule S2 makes that explicit rather than accidental.

### 5.3 Tables

Full DDL in **Appendix A**. Summary:

| Table | Purpose | Key |
| --- | --- | --- |
| `graph_schema_migrations` | Migration ledger (RFC-0002 shape) | `version` |
| `graph_meta` | `model_version`, `graph_version`, `content_digest`, `workspace_root_rule`, `updated_at` | single row, `id = 1` |
| `graph_nodes` | Workspace/Crate/Module/Item (Item via the RFC-0014 deep pass) | `id`; `UNIQUE(kind, path)` |
| `graph_edges` | `Defines`/`Imports` plus `References`/`Calls`/`Impls` (A-0011-6a), with reserved `confidence` | `PRIMARY KEY(from_id, to_id, kind)` |
| `graph_files` | Workspace-relative file → digest, owning crate/module | `path` |
| `graph_diagnostics` | `record_diagnostic` sink | `diagnostic_id` |
| `graph_fixes` | `record_fix` sink | `fix_id` |
| `graph_snapshots` | Version markers (G10) | `snapshot_id` |

### 5.4 Model-version discipline

`graph_meta.model_version` records the semantics of the ingest, independent of table shape. This mechanism has fired twice as designed: the RFC-0014 `syn` pass bumped `GRAPH_MODEL_VERSION` to `2` when it started writing `Item` nodes, and A-0011-6d bumped it to `3` for the semantic edge kinds — each time truncating every existing database for re-ingest on next open, so a half-manifest/half-syn graph can never exist. Rule S4 makes the merge case unreachable by construction, which is the cheapest correct answer for a derived cache.

### 5.5 Connection management

`SqliteProjectGraph` holds `Arc<GraphDb>` where `GraphDb { conn: Mutex<Option<Connection>> }` — the same shape as `storage::open::DbHandle`, re-implemented in `alloy-index` because that type is crate-private in `alloy-runtime`. All `ProjectGraph` methods wrap their synchronous body in `tokio::task::spawn_blocking` (X3). A poisoned mutex maps to `GraphError::Internal("db mutex poisoned")`; a `None` connection maps to `GraphError::Closed`.

### 5.6 Write transactions

Every mutating operation runs in **one** transaction:

| Operation | Transaction contents |
| --- | --- |
| `rebuild` | delete all `graph_nodes`/`graph_edges`/`graph_files` rows for the workspace, insert the new set, recompute digest, conditionally bump `graph_meta` |
| `apply_incremental` | apply the per-change effects of §6.6, recompute digest, conditionally bump |
| `record_diagnostic` | one upsert into `graph_diagnostics` (idempotent on `diagnostic_id`) |
| `record_fix` | one insert into `graph_fixes` |
| `snapshot` | one insert into `graph_snapshots` |

Rule **S10**: a failed transaction MUST leave the previous version fully intact. There is no partially-applied ingest state.

### 5.7 Corruption and quarantine

```text
open() ─► PRAGMAs ─► migrate ─► model_version check
   │                     │              │
   │                     │              └─ mismatch ──► truncate + mark rebuild-required (S4)
   │                     └─ Corrupt ───────────────────► quarantine + fresh DB (S8)
   └─ Corrupt ────────────────────────────────────────► quarantine + fresh DB (S8)
```

After quarantine the graph is empty at `GraphVersion(0)`; the caller's next `rebuild` restores it from source. That is precisely V2 §5.6's "Rebuild from source; quarantine snapshot."

---

## 6. Ingest pipeline

### 6.1 Rules

| Rule | Statement |
| --- | --- |
| **IN1** | Ingest MUST NOT be triggered by a capability worker, by the scheduler's node dispatch, or by any MCP tool. Permitted callers: `alloy-cli` (explicit `alloy index` / session bootstrap) and the runtime host's verify path (`record_diagnostic` and, since A-0011-5b, `record_fix`). |
| **IN2** | Ingest MUST NOT run implicitly inside `query`. A stale graph answers with stale data and a truthful `version`; it never silently re-indexes mid-prompt. |
| **IN3** | Ingest MUST enforce `IngestLimits`. Exceeding `max_files` / `max_crates` / `max_depth` MUST return `GraphError::LimitExceeded` and leave the previous version intact (S10). Oversized individual files are skipped and counted, not fatal. |
| **IN4** | The walk MUST NOT follow symbolic links, and MUST NOT visit any path that escapes the workspace root after normalisation. Skipped entries are counted. |
| **IN5** | Ingest MUST be **deterministic**: directory entries are visited in sorted-by-filename-bytes order; the emitted node set, edge set, and content digest depend only on file paths and contents. |
| **IN6** | Ingest MUST be **idempotent**: `rebuild` over an unchanged tree MUST produce an identical content digest and MUST NOT bump `GraphVersion`. |
| **IN7** | Module inference is **file-layout-derived** (§6.4) and MUST NOT parse Rust source. |
| **IN8** | MVP (manifest) ingest MUST write zero `Imports` edges. Shipped as a **Stub** reserved for the Beta `syn` pass; that pass has since landed under RFC-0014 and populates `Imports` (SY11–SY13). |
| **IN9** | MVP (manifest) ingest MUST write zero `Item` nodes. Shipped as a **Stub** reserved for the Beta `syn` pass; the RFC-0014 deep pass now constructs `Item` nodes (SY3) and is their only producer (amended T14 grep). |
| **IN10** | `apply_incremental` MUST be equivalent to, or a conservative superset of, the effect of a full `rebuild` on the same resulting tree; when the two disagree it is a bug, and a test asserts equality of digests (T5). |
| **IN11** | `FileChange.path` MUST be workspace-relative with `/` separators. An absolute or escaping path MUST be rejected with `GraphError::InvalidQuery`. |
| **IN12** | A malformed member manifest MUST NOT abort the pass. The member is skipped, a warning string is pushed to `IngestReport.warnings`, and the pass completes. A malformed **root** manifest is fatal (`GraphError::Manifest`). |
| **IN13** | `record_diagnostic` MUST be idempotent on `DiagnosticEvent.id` (upsert), so a retried verify node does not duplicate rows. |
| **IN14** | `record_fix` MUST be append-only; duplicates are permitted and distinguishable by `fix_id` and `recorded_at`. |
| **IN15** | `record_diagnostic` and `record_fix` MUST NOT change `GraphVersion` or the content digest (§4.6). |

### 6.2 What populates the thin graph

MVP facts come from exactly two sources, both offline:

1. **`Cargo.toml` manifests**, parsed with the already-vendored `toml` crate:
   - root manifest: `[workspace] members` (glob patterns expanded with `globset`, already a workspace dependency), `exclude`;
   - member manifests: `[package] name`, `[lib] path`, `[[bin]] path`, `[package] autobins`-independent conventional targets.
2. **The source tree**, via a bounded `std::fs` walk.

Explicitly **not** used in MVP: `cargo metadata` (requires sandboxed `Exec` and — for unresolved dependencies — network), `syn`, rustdoc JSON, rust-analyzer. V2 §7.2's "ingest from cargo metadata + syn" split across milestones: the **`syn` half landed at Beta** (RFC-0014); **`cargo metadata` is deferred post-Beta** because SEC5 keeps the index exec-free and graph open/rebuild has no Sandbox/Exec grant path yet (§1.4, §14.2). The M7 thin scope remains a strict subset of the V2 target.

### 6.3 Crate discovery

```text
root/Cargo.toml
  ├─ [workspace].members globs      → candidate member dirs (sorted)
  ├─ [workspace].exclude globs      → removed
  ├─ [package] present at root?     → root is itself a member
  └─ each member/Cargo.toml         → CrateId from [package].name
```

A directory whose `Cargo.toml` fails to parse or lacks `[package].name` is skipped with a warning (IN12). Duplicate package names are a `GraphError::Manifest` (they would break G9's uniqueness).

If the root manifest has neither `[workspace]` nor `[package]`, ingest fails with `GraphError::Workspace("not a cargo workspace: …")`.

### 6.4 Module inference (normative)

For each crate, roots are computed first, then descended.

| Step | Rule |
| --- | --- |
| **IN7a** | Library root: `[lib].path` if present, else `src/lib.rs` if it exists. Its module path is `crate_ident`. |
| **IN7b** | Binary roots: `[[bin]].path` entries, else `src/main.rs`, else `src/bin/<name>.rs`. Module path is `crate_ident::<bin_name>` for named bins; `src/main.rs` uses `crate_ident::main`. |
| **IN7c** | For a module rooted at `dir/name.rs` or `dir/name/mod.rs`, child modules are inferred from siblings inside `dir/name/`: each `child.rs` (except `mod.rs`) → `…::child`; each subdirectory containing `mod.rs` → `…::subdir`. |
| **IN7d** | If both `name.rs` and `name/mod.rs` exist, `name.rs` wins deterministically and a warning is recorded. |
| **IN7e** | A subdirectory with neither a sibling `.rs` file nor an inner `mod.rs` is **not** a module; its subtree is not descended. |
| **IN7f** | Inference is an approximation. `#[path]` attributes, `mod` declarations inside `include!`, and `cfg`-gated modules are **not** honoured. `GraphView.fidelity` is `Manifest` precisely so consumers can label this. Missing modules are acceptable; invented ones are not (G7). |
| **IN7g** | Every file that becomes a module is recorded in `graph_files` with its SHA-256 (`Digest::sha256`). |

### 6.5 Full rebuild algorithm

1. Canonicalise `root`; reject if it is not a directory (`GraphError::Workspace`).
2. Parse the root manifest; discover crates (§6.3). Reject on `> max_crates`.
3. For each crate in sorted order, infer modules (§6.4), hashing each module file. Reject on `> max_files` or `> max_depth`.
4. Build the node set: one `Workspace`, N `Crate`, M `Module`. Ids by G3/G4.
5. Build the edge set: `Defines` per §4.3, `confidence = 1.0`.
6. Compute the content digest (§4.6).
7. Open a transaction: replace `graph_nodes`/`graph_edges`/`graph_files`; if the digest changed, bump `graph_meta.graph_version` and store the new digest; commit.
8. Return `IngestReport`.

Step 7 rewrites rows even when the digest is unchanged (cheap, and keeps `graph_files` honest about mtimes we do not track), but the **version** only moves on a digest change — that is what IN6 asserts.

### 6.6 Incremental application

`apply_incremental(&[FileChange])` classifies each change:

| Change | Path pattern | Effect |
| --- | --- | --- |
| any | `**/Cargo.toml` | Mark the owning crate (or the whole workspace, for the root manifest) **dirty**; re-run §6.4 for it. |
| `Created` | `**/*.rs` | Re-run §6.4 for the owning crate's affected module subtree. |
| `Deleted` | `**/*.rs` | Remove the module node and its `Defines` subtree; remove the `graph_files` row. |
| `Modified` | `**/*.rs` | Re-hash. If the digest is unchanged → **no-op**. If changed → update `graph_files.digest` only; the MVP has no intra-file nodes, so structure is unaffected. |
| any | anything else (`.md`, `target/`, …) | Ignored, counted as skipped. |

After all changes are applied, the content digest is recomputed and the version bumped iff it changed. An empty `changes` slice is a no-op that returns the current version without touching the database.

Rule **IN10** is what makes this safe: an integration test applies a change set and compares the resulting digest against a full `rebuild` of the post-change tree; they must be equal.

This satisfies V2 §7.2's "file digest invalidation of module subgraphs" without claiming Merkle multi-layer incrementality (explicitly deferred).

### 6.7 Diagnostic and fix ingest

The producer chain is: RFC-0010's verify adapter runs `cargo_check` through the MCP host and surfaces `VerifyOutcome.diagnostics: Vec<DiagnosticEvent>` (the seam merged in `alloy-runtime::adapters`; the cargo-JSON→`DiagnosticEvent` parser itself arrives with the RFC-0013/0014 adapters) → the **runtime host** (not the worker) calls `record_diagnostic` for each. Spans are workspace-relativised on the way in (G12); a span path outside the workspace stores `None` for `primary_path` rather than an absolute host path.

`record_fix` is called by the runtime host after an EditEngine transaction commits and its verify result is known, carrying the `TransactionId` and the patch's `ArtifactId`.

Rule **SEC4** restates the boundary: no code path from `Capability::execute` reaches either method.

---

## 7. Query semantics

### 7.1 Rules

| Rule | Statement |
| --- | --- |
| **Q1** | `GraphQuery` has exactly the seven V2 §7.2 variants. New capability is added by populating existing variants, not by adding variants. |
| **Q2** | `Symbol { path }` resolution order: (1) exact match on `graph_nodes.path`; (2) if `path` contains `/` or ends in `.rs`, exact match on `graph_files.path` → its owning module node; (3) otherwise empty. Never a fuzzy or prefix match. |
| **Q3** | `Diagnostics { crate_id, since }` filters on `package = crate_id` when `Some` and `recorded_at >= since` when `Some`, ordered by `(recorded_at, diagnostic_id)` ascending, capped by `max_query_nodes`. |
| **Q4** | *(amended by A-0011-6c, §2.3b)* `Refs { node }` MUST return the anchor plus incoming `References` and `Imports` edges; `Impls { trait_node }` MUST return `Impls` edges touching the anchor in either direction. Q8 ordering, Q9 truncation, Q10 read-only. An unknown anchor is `GraphView::empty(version)` with `truncated = false`; neither query MUST ever error. Originally Stubs pending RA passthrough. |
| **Q5** | *(amended by A-0011-6c, §2.3b)* `Callers { fn_node }` MUST return the anchor plus incoming `Calls` edges, under the same ordering/truncation/read-only rules as Q4. Typed call edges exist since A-0011-6a, which is what V2 §7.2 conditioned this on. An unknown anchor is an empty view with `truncated = false`; never an error. |
| **Q6** | *(amended by A-0011-5a, §2.3a)* `SimilarFixes` MUST return the `graph_fixes` rows matching `diagnostic_code`, most recent first, capped by the query's `limit`; `truncated` is set only when matching rows were left behind. Originally a Stub returning an empty view. |
| **Q7** | *(amended by A-0011-6f, §2.3b)* `Subgraph { seeds, radius }` performs BFS over the structural edges — `Defines`, and `Imports` since the RFC-0014 deep pass — in **both** directions from each seed. The semantic kinds (`References`/`Calls`/`Impls`) MUST NOT be traversed; semantic edges with both endpoints in the view are still returned (§5). `radius` is clamped to `3`; `radius = 0` returns just the seeds. Unknown seed ids are ignored, not errors. |
| **Q8** | Result ordering MUST be total and deterministic: nodes by `(kind, path, id)`, edges by `(from, to, kind)`, diagnostics by `(recorded_at, diagnostic_id)`. Two identical queries against an unchanged graph return byte-identical JSON. |
| **Q9** | When a result would exceed `max_query_nodes`, it is truncated at the ordering boundary and `truncated = true`. Truncation MUST NOT be silent. |
| **Q10** | `query` MUST NOT write to the database. Enforced by opening the query path's connection use in read-only statements and asserted by a test that the version and digest are unchanged after a query sweep. |
| **Q11** | Every `GraphView.version` MUST be the version the rows were read at, read inside the same transaction as the rows. |

### 7.2 Query cost posture

Each live query is one or two indexed statements plus a bounded BFS. There is no scoring, no ranking, and no embedding lookup. This is deliberate: V2 §8.1 keeps the embedding index out of the Context Engine for 0.1.0, and putting one behind the graph instead would be the same deferred feature wearing a different hat.

### 7.3 What RFC-0012 gets in MVP

| Need | Query | MVP answer |
| --- | --- | --- |
| "Which module owns `crates/x/src/io.rs`?" | `Symbol { path: "crates/x/src/io.rs" }` | The module node + its `Defines` parent chain |
| "What is near this module?" | `Subgraph { seeds, radius: 1..3 }` | Parent crate, sibling and child modules |
| "What broke recently in crate X?" | `Diagnostics { crate_id, since }` | Recorded `DiagnosticEvent`s |
| "Who calls this?" | `Callers` | *(A-0011-6)* The recorded `Calls` edges; empty when nothing calls it |

---

## 8. Lifecycle and concurrency

| Rule | Statement |
| --- | --- |
| **X1** | Exactly one `SqliteProjectGraph` per data directory per process. Construction MUST create `<graph_dir>/graph.lock` and hold an advisory OS lock for the instance's lifetime, mirroring RFC-0010's `scheduler.lock`; a second open MUST fail `GraphError::Busy`. |
| **X2** | The MVP is single-process (V2 §5.3). Multi-process access is not supported and not defended against beyond X1's advisory lock. |
| **X3** | Every SQLite interaction MUST run inside `tokio::task::spawn_blocking`, matching RFC-0002's `spawn_db`. No `rusqlite` call may happen on an async executor thread. |
| **X4** | Concurrent `query` calls are permitted and serialise on the connection mutex. Concurrent writes serialise likewise; correctness does not depend on the ordering between two ingest calls, only on each being transactional (S10). |
| **X5** | `close()` MUST run `PRAGMA wal_checkpoint(TRUNCATE)` then close the connection, and MUST be idempotent. `Drop` without `close()` MUST `tracing::warn!`, matching `AlloyStorage`. |
| **X6** | No `unsafe`. `alloy-index` carries `#![forbid(unsafe_code)]`. |
| **X7** | Cancellation: `rebuild` and `apply_incremental` are not cancellable mid-transaction. A dropped future may leave the `spawn_blocking` task to run to completion; because the write is one transaction (S10), the observable state is either the old or the new version. |

Runtime phase interaction: the graph is opened after `AlloyStorage` (it reuses its `StorageLayout`) and closed before it, so a shutdown never leaves the graph WAL uncheckpointed while the session DB is already gone.

---

## 9. Error handling

### 9.1 Boundary matrix

| Boundary | Incoming | Outgoing |
| --- | --- | --- |
| SQLite | `rusqlite::Error` | `GraphError` via `StoreError` mapping (S9, §9.2) |
| Filesystem | `std::io::Error` | `GraphError::Io` (ingest reads) / `GraphError::Workspace` (missing root) |
| `toml` | `toml::de::Error` | `GraphError::Manifest { path, reason }` |
| `spawn_blocking` | `tokio::task::JoinError` | `GraphError::Internal` |
| Caller args | invalid path / query | `GraphError::InvalidQuery` before any I/O |

### 9.2 `StoreError` → `GraphError`

| `StoreError` | `GraphError` | Note |
| --- | --- | --- |
| `NotFound(s)` | `NotFound(s)` | — |
| `Conflict(s)` | `Internal(s)` | A constraint violation in a single-writer derived cache is a bug, not contention |
| `Corrupt(s)` | `Corrupt(s)` | Triggers quarantine at open (S8); at runtime it is returned to the caller |
| `Migration(s)` | `Migration(s)` | — |
| `Busy` | `Busy` | Retryable by the caller |
| `Io(s)` | `Io(s)` | — |
| `DigestMismatch` | `Corrupt("digest mismatch")` | — |
| `Closed` | `Closed` | — |
| `Internal(s)` | `Internal(s)` | — |

### 9.3 What MUST NOT be an error

| Situation | Correct behaviour |
| --- | --- |
| `Callers` / `SimilarFixes` / `Refs` / `Impls` finds nothing (or an unknown anchor) | Empty view, `truncated = false`, `Ok` (Q4–Q6 as amended) |
| `Symbol` finds nothing | Empty view, `Ok` |
| `Subgraph` seed id unknown | Ignored (Q7) |
| A member manifest is malformed | Warning in `IngestReport` (IN12) |
| A file exceeds `max_file_bytes` | Skipped and counted (IN3) |
| `apply_incremental(&[])` | `Ok(current_version)` |
| A `.md`/`target/` path in `FileChange` | Ignored and counted |

### 9.4 Recovery semantics

| Failure | Detection | Recovery | Outcome |
| --- | --- | --- | --- |
| Graph file corrupt | `ErrorCode::DatabaseCorrupt` / `NotADatabase` at open | Quarantine + fresh DB (S8) | Empty graph at v0; caller rebuilds |
| Model version drift | `graph_meta.model_version` mismatch | Truncate + require rebuild (S4) | Empty graph at v0 |
| Schema newer than code | `MAX(version) > GRAPH_SCHEMA_VERSION` | Fail `Migration` (S3) | Operator downgrades data or upgrades binary |
| Ingest cap exceeded | Counter during walk | Abort transaction (S10) | Previous version intact |
| Partial write / crash mid-ingest | SQLite transaction rollback | Automatic | Previous version intact |
| Workspace deleted under a live graph | `Workspace` on next ingest | Caller decides | Queries still answer at the last version |

---

## 10. Observability

| Rule | Statement |
| --- | --- |
| **OB1** | `alloy-index` MUST NOT append session events and MUST NOT construct `DecisionRecord`s. RFC-0004's module rule is that `storage`/`session`/`runtime` do not depend on `obs`; the graph is in the same position, and ingest is not a session-scoped decision. |
| **OB2** | Observability is `tracing` spans plus an atomic counter snapshot, following RFC-0002's `StorageMetrics`/`StorageMetricsSnapshot` shape (private `AtomicU64` struct, public plain-`u64` snapshot). |
| **OB3** | Span names are dotted and stable: `index.open`, `index.migrate`, `index.rebuild`, `index.incremental`, `index.query`, `index.record_diagnostic`, `index.record_fix`, `index.snapshot`, `index.close`, `index.quarantine`. |
| **OB4** | Logs MUST NOT contain absolute host paths in `INFO` or above; ingest logs workspace-relative paths (G12, SEC6). Absolute paths may appear at `DEBUG` for the workspace root only. |
| **OB5** | If a caller wants a graph event in the session log, the **caller** records it (e.g. RFC-0015 emitting a `DecisionKind::Custom("graph_rebuild")` record with the returned `IngestReport` counts). This RFC does not add a `SessionEventType` variant. |

```rust
/// Counter snapshot (RFC-0004 conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct GraphMetricsSnapshot {
    /// Successful full rebuilds.
    pub rebuilds: u64,
    /// Rebuilds that produced no version bump (IN6).
    pub rebuilds_unchanged: u64,
    /// `apply_incremental` calls.
    pub incrementals: u64,
    /// Queries served, all kinds.
    pub queries: u64,
    /// Queries answered by an empty-but-truncated view (Q9 over an
    /// empty result; formerly the Q4–Q6 Stub marker).
    pub queries_stub: u64,
    /// Views truncated by `max_query_nodes` (Q9).
    pub queries_truncated: u64,
    /// Diagnostics ingested.
    pub diagnostics_recorded: u64,
    /// Fixes ingested.
    pub fixes_recorded: u64,
    /// Snapshots taken.
    pub snapshots: u64,
    /// SQLite busy-timeout errors.
    pub busy_errors: u64,
    /// Corrupt databases quarantined (S8).
    pub quarantines: u64,
    /// Files skipped by a cap or skip rule (IN3, IN4).
    pub files_skipped: u64,
}
```

---

## 11. Security posture

| Rule | Statement | Mechanised by |
| --- | --- | --- |
| **SEC1** | `GraphViewHandle` MUST expose only `new`, `null`, `query`, `version`, `Clone`, `Debug`. It MUST NOT expose the inner `Arc<dyn ProjectGraph>`, and MUST NOT have a method whose name contains `rebuild`, `record`, `apply`, `snapshot`, `insert`, or `write`. | T11 grep + API review |
| **SEC2** | **No `graph_query` MCP tool for Alloy workers, in any crate** (ADR F-04, V2 §12.2). The string `graph_query` MUST NOT appear in `crates/alloy-tools/src/**` except in the existing negative assertions and rule doc comments. | T7 grep (extends the existing `no_forbidden_registrations` / `no_bash_registered` tests) |
| **SEC3** | Writes are **ingest-only**. There is no `GraphMutation` type, no worker-supplied mutation payload, and `CapabilityOutput` gains no graph field (V2 §9: `graph_mutations` removed). The identifier `GraphMutation` MUST NOT exist in the workspace. | T8 grep |
| **SEC4** | No code path from `Capability::execute` may reach `rebuild`, `apply_incremental`, `record_diagnostic`, `record_fix`, or `snapshot`. Workers receive a `GraphViewHandle`, never an `Arc<dyn ProjectGraph>`. | T11 + type shape |
| **SEC5** | `alloy-index` performs **no network I/O and no process execution**. It MUST NOT depend on `reqwest`, `rustls`, `url`, `rustix`, `libc`, or `landlock`, and MUST NOT contain `std::process::Command`. | T12 grep + `cargo tree` |
| **SEC6** | Absolute host paths MUST NOT be persisted in graph rows or logged at `INFO`+ (G12, OB4). Query results are model-visible via PromptPacks; leaking `/home/<user>/…` into a prompt is a redaction failure. | Unit test on ingest rows |
| **SEC7** | The walk MUST NOT follow symlinks or escape the workspace root (IN4), so a symlinked `/etc` inside a repo cannot be hashed into the graph and surfaced in a prompt. | Unit test with a symlink fixture |
| **SEC8** | Ingest reads files but never writes to the workspace. `alloy-index` MUST NOT create, modify, or delete any path outside `<data_dir>/graph/`, and MUST NEVER write `.env`. | T13 grep |

Prompt-injection posture (V2 §20 R12): the graph stores *paths and package names* from the repository, which are attacker-influenceable in a hostile repo. It stores no file **contents** except SHA-256 digests, so the injection surface is limited to identifiers. RFC-0012 remains responsible for treating graph-derived strings as untrusted content when assembling PromptPacks.

---

## 12. Crate dependencies and `unsafe`

`alloy-index/Cargo.toml` after this RFC:

```toml
[dependencies]
alloy-runtime = { workspace = true, default-features = false }
async-trait   = { workspace = true }
globset       = { workspace = true }
rusqlite      = { workspace = true }
serde         = { workspace = true }
serde_json    = { workspace = true }
thiserror     = { workspace = true }
time          = { workspace = true }
tokio         = { workspace = true }
toml          = { workspace = true }
tracing       = { workspace = true }
uuid          = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio    = { workspace = true }
```

**No new external dependency.** Every entry is already in `[workspace.dependencies]` and already compiled by another crate in the workspace. In particular:

- `walkdir` / `ignore` are **not** added — the walk is ~80 lines of `std::fs` with an explicit skip list, and writing it by hand is what makes IN4 (no symlink following) and IN5 (sorted traversal) auditable rather than configuration-dependent.
- `cargo_metadata` is **not** added — see §6.2.
- `syn` is **not** added — it is the Beta deepening's dependency, and adding it now would be dead weight under DoD gate 9.
- `sha2` is **not** listed — all hashing goes through `alloy-runtime`'s `Digest::sha256` / `DigestHasher`, so the graph and the seam can never disagree on digest encoding.

Lint attributes added to `crates/alloy-index/src/lib.rs`:

```rust
#![deny(missing_docs)]
#![forbid(unsafe_code)]
```

`forbid` (not `deny`) matches `alloy-runtime` and `alloy-eval`; `alloy-tools` uses `deny` only because it links `libc`/`rustix`, which `alloy-index` does not (SEC5).

---

## 13. Testing strategy

### 13.1 Unit tests (`alloy-runtime::graph`, pure)

| # | Test name | Asserts |
| --- | --- | --- |
| T1a | `node_id_is_deterministic_for_the_same_key` | G3: same `(kind, key)` → same `GraphNodeId` across processes |
| T1b | `node_id_differs_across_kinds_with_the_same_key` | G3 domain separation |
| T1c | `node_id_has_uuid_version_eight_and_rfc4122_variant` | G3 formatting |
| T1d | `graph_view_empty_is_empty_and_manifest_fidelity` | §3.4 |
| T1e | `null_graph_reads_empty_and_writes_disabled` | Q10 |
| T1f | `graph_query_serde_round_trip_covers_all_seven_variants` | Q1 |
| T1g | `graph_error_maps_every_store_error_variant` | §9.2, exhaustive `match` |

### 13.2 Unit tests (`alloy-index`, in-memory / `tempfile`)

| # | Test name | Asserts |
| --- | --- | --- |
| T2a | `migrate_fresh_and_idempotent` | S5; second open is a no-op |
| T2b | `refuse_newer_graph_schema` | S3 |
| T2c | `model_version_mismatch_truncates_instead_of_migrating` | S4 |
| T2d | `corrupt_db_is_quarantined_and_recreated` | S8; file appears under `quarantine/`, metric bumps |
| T2e | `second_open_of_the_same_graph_dir_is_busy` | X1 |
| T2f | `close_is_idempotent_and_checkpoints` | X5 |
| T2g | `edges_always_have_confidence_one` | S6/G11 |

### 13.3 Ingest tests (fixture workspaces under `tests/fixtures/`)

| # | Test name | Asserts |
| --- | --- | --- |
| T3a | `rebuild_toy_workspace_golden_nodes_and_edges` | Appendix B's exact node/edge set |
| T3b | `rebuild_twice_does_not_bump_version` | IN6 |
| T3c | `rebuild_digest_is_stable_across_two_processes` | IN5 |
| T3d | `walk_visits_entries_in_sorted_order` | IN5 |
| T3e | `walk_does_not_follow_symlinks` | IN4/SEC7 |
| T3f | `walk_skips_target_and_dot_git` | IN3 |
| T3g | `mod_rs_and_sibling_rs_prefers_sibling_and_warns` | IN7d |
| T3h | `directory_without_mod_rs_is_not_a_module` | IN7e |
| T3i | `malformed_member_manifest_warns_and_continues` | IN12 |
| T3j | `malformed_root_manifest_is_fatal` | IN12 |
| T3k | `duplicate_package_names_are_rejected` | §6.3 |
| T3l | `exceeding_max_files_leaves_previous_version_intact` | IN3 + S10 |
| T3m | *(superseded by RFC-0014 SY3/SY11)* `deep_pass_writes_item_nodes_and_imports_edges` — the deep pass fills the reserved `item`/`imports` seams; the manifest pass still writes none | IN8, IN9 |
| T3n | `stored_paths_are_workspace_relative_only` | G12/SEC6 |
| T3o | `non_workspace_root_is_workspace_error` | §6.3 |

### 13.4 Incremental tests

| # | Test name | Asserts |
| --- | --- | --- |
| T4a | `modified_file_with_same_digest_is_a_noop` | §6.6 |
| T4b | `modified_file_with_new_digest_updates_digest_and_bumps_version` | §6.6, G8 |
| T4c | `created_module_file_adds_node_and_defines_edge` | §6.6 |
| T4d | `deleted_module_file_removes_subtree` | §6.6 |
| T4e | `manifest_change_reingests_the_owning_crate` | §6.6 |
| T4f | `absolute_or_escaping_file_change_path_is_invalid_query` | IN11 |
| T4g | `empty_change_set_is_a_noop` | §6.6 |
| T5 | `incremental_and_full_rebuild_agree_on_digest` | **IN10** — the key correctness test |

### 13.5 Query tests

| # | Test name | Asserts |
| --- | --- | --- |
| T6a | `symbol_resolves_rust_path_exactly` | Q2(1) |
| T6b | `symbol_resolves_workspace_relative_file_path` | Q2(2) |
| T6c | `symbol_does_not_prefix_match` | Q2 |
| T6d | *(superseded by A-0011-6)* `former_stub_queries_answer_from_semantic_edges` + `callers_round_trips_incoming_calls_edges` | Q5 |
| T6e | *(superseded by A-0011-5a)* `similar_fixes_returns_recent_rows_first_and_honours_limit` | Q6 |
| T6f | *(superseded by A-0011-6)* `refs_round_trips_incoming_references_and_imports` + `impls_answers_for_trait_and_for_type` | Q4 |
| T6g | `diagnostics_filters_by_crate_and_since` | Q3 |
| T6h | `subgraph_radius_zero_returns_seeds_only` | Q7 |
| T6i | `subgraph_traverses_defines_both_directions_and_clamps_radius` | Q7 |
| T6j | `subgraph_ignores_unknown_seeds` | Q7 |
| T6k | `query_results_are_deterministically_ordered` | Q8 |
| T6l | `oversized_result_sets_truncated_flag` | Q9 |
| T6m | `subgraph_traverses_structural_edges_only` | Q7 as amended (A-0011-6f) |
| T6n | `high_degree_anchor_stays_under_the_sqlite_variable_limit` | Q4/Q5 robustness |
| T6o | `generic_parameter_heads_never_resolve_to_workspace_items` | A-0011-6b, G7 |
| T6p | `query_sweep_does_not_change_version_or_digest` + `unstubbed_query_sweep_changes_neither_version_nor_digest` | Q10 |

### 13.6 Ingest-record tests

| # | Test name | Asserts |
| --- | --- | --- |
| T7a | `record_diagnostic_round_trips_through_diagnostics_query` | IN13 |
| T7b | `record_diagnostic_is_idempotent_on_diagnostic_id` | IN13 |
| T7c | `record_fix_appends_and_is_surfaced_by_similar_fixes` (renamed by A-0011-5a) | IN14, Q6 |
| T7d | `record_diagnostic_and_record_fix_do_not_bump_version` | IN15 |
| T7e | `snapshot_records_version_and_counts` | G10 |

### 13.7 Cross-subsystem integration (`crates/alloy-index/tests/graph_rfc0011.rs`)

| # | Test name | Asserts |
| --- | --- | --- |
| T8a | `graph_opens_beside_alloy_sqlite_without_touching_it` | S1, S2 — `alloy.sqlite` mtime and `schema_migrations` are unchanged |
| T8b | `rebuild_then_reopen_preserves_version_and_digest` | Persistence across process restarts |
| T8c | `worker_style_handle_answers_after_host_ingest` | End-to-end: host `rebuild` → `GraphViewHandle::query` |
| T8d | `verify_outcome_shaped_diagnostics_ingest_and_query_back` | Ingests `DiagnosticEvent`s shaped exactly like `VerifyOutcome.diagnostics` (spans, children, fingerprint) and queries them back |

### 13.8 CI grep rules (`crates/alloy-index/tests/rfc0011_ci_greps.rs`)

Implemented as ordinary `#[test]`s using the `rfc0010_ci_greps.rs` harness shape (recursive `walk_rs_files` from `CARGO_MANIFEST_DIR`, per-line `assert!(!line.contains(..))`, plus a "the walk found zero files" guard).

| # | Test name | Rule |
| --- | --- | --- |
| **T7** | `sec2_no_graph_query_tool_outside_negative_assertions` | SEC2 — `graph_query` absent from `alloy-tools/src` except the existing forbidden-registration lists and rule doc comments |
| **T8** | `sec3_no_graph_mutation_type_anywhere` | SEC3 — the identifier `GraphMutation` does not exist in the workspace |
| **T9** | `c2_alloy_runtime_does_not_depend_on_alloy_index` | C2 — `crates/alloy-runtime/Cargo.toml` contains no `alloy-index` |
| **T10** | `c4_graph_seam_has_no_sql_or_rusqlite` | C4 — `crates/alloy-runtime/src/graph/**` contains no `rusqlite`, `CREATE TABLE`, or `SELECT ` |
| **T11** | `sec1_graph_view_handle_exposes_no_write_method` | SEC1 — `graph/handle.rs` contains no `fn rebuild`/`fn record_`/`fn apply_`/`fn snapshot`/`fn inner` |
| **T12** | `sec5_alloy_index_has_no_network_or_exec` | SEC5 — no `reqwest`/`rustls`/`landlock`/`rustix`/`libc` in the manifest; no `std::process::Command` in sources |
| **T13** | `sec8_alloy_index_never_writes_dot_env` | SEC8 — no `".env"` literal in `alloy-index` sources |
| **T14** | *(amended by RFC-0014 SY3)* `in9_item_node_construction_only_in_the_lang_pass` | IN9 — outside `src/lang/` (the one legal producer), `GraphNodeKind::Item` appears in `alloy-index/src` only in seam-mapping `match`/rank code |

CI wiring: none needed. `cargo test --workspace` already runs in `.github/workflows/ci.yml`'s "Tests" step, and `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS: -D warnings` already enforces the `#![deny(missing_docs)]` documentation gate.

### 13.9 Determinism harness

`T3c` runs the ingest twice in separate processes over a `tempfile` copy of the fixture and compares `IngestReport` and the content digest. `T6k` serialises two identical query results to JSON and compares bytes. Together they are the mechanical proof of IN5 and Q8.

---

## 14. MVP vs deferred

### 14.1 MVP (this RFC, M7)

Trait seam · SQLite store with own migration ladder · manifest+layout ingest · Workspace/Crate/Module nodes · `Defines` edges · file digest tracking · crate/module-subtree incremental · `Symbol`/`Diagnostics`/`Subgraph` queries · `Callers`/`SimilarFixes`/`Refs`/`Impls` Stubs (all four since un-stubbed — A-0011-5/A-0011-6, §2.3c) · diagnostic/fix ingest · versions, snapshots, quarantine · read-only worker handle · metrics + spans.

### 14.2 Deferred (with the seam that carries it)

| Item | Seam | Milestone |
| --- | --- | --- |
| ~~`Item` nodes, `Imports` edges~~ (landed via the RFC-0014 `syn` deep pass; `GRAPH_MODEL_VERSION = 2` at landing, `3` since A-0011-6d) | `GraphNodeKind::Item`, `GraphEdgeKind::Imports` | Beta |
| `cargo metadata` facts (deps, features) | `IngestReport.source: GraphFidelity` (no `IngestSource` enum); needs host sandboxed Exec — see §1.4 reason | **Post-Beta** |
| RA passthrough for `Refs`/`Impls` (rustc-grade answers; syn-grade shipped by A-0011-6) | `GraphFidelity::Analyzer` | Beta / M3 (as available) |
| ~~Typed `Calls` edges~~ (landed via A-0011-6a at `confidence = 1.0`; sub-1.0 confidence weighting stays deferred) | `graph_edges.confidence` | Post-Beta |
| ~~`SimilarFixes` retrieval~~ (landed via A-0011-5a; the A-0011-5c prompt note is bounded to codes/packages/dates/artifact ids — wider auto-injection stays deferred until precision is measured) | `graph_fixes` table already populated | After precision measured |
| Merkle multi-layer incremental | `graph_files.digest` | Deferred |
| Background indexer | — | Deferred (ADR F-27) |
| Embedding recall | — | Deferred (ADR F-23) |
| External-only MCP graph mirror | — | Deferred; never for Alloy workers (ADR F-04) |

---

## 15. Acceptance criteria

Each criterion is verifiable by a named test from §13, by a CI grep, or by a mechanical diff/compile check. All start unchecked.

- [ ] 1. `alloy-index/src/lib.rs` carries `#![forbid(unsafe_code)]` **and** `#![deny(missing_docs)]`.
- [ ] 2. `alloy-index/Cargo.toml` introduces **no new `[workspace.dependencies]` entry**; every dependency is already used by another crate.
- [ ] 3. `crates/alloy-runtime/Cargo.toml` contains no `alloy-index` dependency (**C2**, T9).
- [ ] 4. `crates/alloy-runtime/src/graph/**` contains no `rusqlite`, `CREATE TABLE`, or `SELECT ` (**C4**, T10).
- [ ] 5. `alloy-index` depends on `alloy-runtime` with `default-features = false` (**C3**).
- [ ] 6. The workspace still has exactly five members (**C6**).
- [ ] 7. `ProjectGraph`'s six methods match Architecture V2 §7.2 signature-for-signature; `version()` is the only addition and is defaulted.
- [ ] 8. `GraphQuery` has exactly the seven V2 variants with the V2 field names and types (**Q1**, T1f).
- [ ] 9. `GraphVersion` gains `Copy, PartialOrd, Ord` and no other change (amendment **A1**, compile check).
- [ ] 10. `CrateId` is minted via the existing `name_id!` macro and validates 1..=128 bytes (amendment **A2**).
- [ ] 11. `GraphNodeId` values are derived, not random: `GraphNodeId::new()` appears nowhere in `alloy-index` (**G3**, T1a–T1c, grep).
- [ ] 12. Node ids are stable across processes for an unchanged tree (**G3**, T1a).
- [ ] 13. Node `path` values are unique per `(kind, path)`; the DB enforces it (**G9**).
- [ ] 14. No `File` node kind exists; files live in `graph_files` (**G5**, schema diff).
- [ ] 15. Diagnostics and fixes are stored as records, not nodes or edges (**G6**, schema diff).
- [ ] 16. Every persisted edge has `confidence = 1.0` (**G11**, T2g).
- [ ] 17. The graph database is `<data_dir>/graph/graph.sqlite`, derived from `StorageLayout::graph_dir` (**S1**, T8a).
- [ ] 18. `alloy.sqlite` is untouched by any `alloy-index` code path (**S2**, T8a).
- [ ] 19. `graph_schema_migrations` follows RFC-0002's ledger shape and migrates idempotently (**S5**, T2a).
- [ ] 20. A newer-than-code schema version is refused with `GraphError::Migration` (**S3**, T2b).
- [ ] 21. A model-version mismatch truncates and requires rebuild rather than migrating (**S4**, T2c).
- [ ] 22. PRAGMAs are applied in the order `foreign_keys` → `busy_timeout` → `journal_mode` → `synchronous` (**S7**).
- [ ] 23. A corrupt database is quarantined under `graph/quarantine/` and recreated empty; `quarantines` increments (**S8**, T2d).
- [ ] 24. `rusqlite` errors are mapped by `ErrorCode`, never by message substring (**S9**, code review + T1g).
- [ ] 25. Every mutating operation is a single transaction; a failed ingest leaves the previous version intact (**S10**, T3l).
- [ ] 26. `rebuild` over the toy workspace produces exactly Appendix B's nodes and edges (**T3a**).
- [ ] 27. A second `rebuild` over an unchanged tree does not bump `GraphVersion` (**IN6**, T3b).
- [ ] 28. The content digest is identical across two separate processes over the same tree (**IN5**, T3c).
- [ ] 29. Directory entries are visited in sorted-by-name order (**IN5**, T3d).
- [ ] 30. The walk does not follow symlinks and cannot escape the workspace root (**IN4/SEC7**, T3e).
- [ ] 31. `target/` and `.git/` subtrees are skipped and counted (**IN3**, T3f).
- [ ] 32. Ingest performs no subprocess execution and no network I/O (**IN3/SEC5**, T12).
- [ ] 33. Module inference follows IN7a–IN7g exactly, including the `foo.rs` vs `foo/mod.rs` tie-break with a warning (**T3g**) and the no-`mod.rs` directory rule (**T3h**).
- [ ] 34. A malformed member manifest warns and continues; a malformed root manifest is fatal (**IN12**, T3i, T3j).
- [ ] 35. Duplicate package names are rejected with `GraphError::Manifest` (T3k).
- [x] 36. *(superseded by RFC-0014 SY3/SY11)* MVP **manifest** ingest writes zero `Item` nodes and zero `Imports` edges; the deep pass populates both (**IN8/IN9** as amended, T3m superseded by `deep_pass_writes_item_nodes_and_imports_edges`, T14 as amended).
- [ ] 37. No absolute host path is persisted in any graph row (**G12/SEC6**, T3n).
- [ ] 38. `apply_incremental` on an unchanged-digest `Modified` file is a no-op (**T4a**).
- [ ] 39. `Created` / `Deleted` `.rs` changes add / remove the module node and its `Defines` subtree (**T4c**, T4d).
- [ ] 40. A `Cargo.toml` change re-ingests the owning crate (**T4e**).
- [ ] 41. An absolute or escaping `FileChange.path` is rejected with `InvalidQuery` (**IN11**, T4f).
- [ ] 42. `apply_incremental(&[])` returns the current version without a write (**T4g**).
- [ ] 43. Incremental application and a full rebuild of the same post-change tree agree on the content digest (**IN10**, T5).
- [ ] 44. `Symbol` resolves an exact Rust path and an exact workspace-relative file path, and never prefix-matches (**Q2**, T6a–T6c).
- [x] 45. *(superseded by A-0011-6c)* `Callers` returns the anchor plus incoming `Calls` edges and never errors (**Q5**, T6d).
- [x] 46. *(superseded by A-0011-5a)* `SimilarFixes` returns the matching `graph_fixes` rows, most recent first, honouring `limit` (**Q6**, T7c/T7c2).
- [x] 47. *(superseded by A-0011-6c)* `Refs` and `Impls` answer from the `references`/`imports`/`impls` edges without ever erroring (**Q4**, T6f).
- [ ] 48. `Diagnostics` filters by `crate_id` and `since` and orders by `(recorded_at, diagnostic_id)` (**Q3**, T6g).
- [ ] 49. `Subgraph` honours `radius = 0`, clamps to 3, traverses only the structural edges (`Defines`/`Imports`) in both directions — never the semantic kinds — and ignores unknown seeds (**Q7** as amended by A-0011-6f, T6h–T6j, T6m).
- [ ] 50. Two identical queries over an unchanged graph produce byte-identical JSON (**Q8**, T6k).
- [ ] 51. Over-cap results set `truncated = true` (**Q9**, T6l).
- [ ] 52. A full query sweep leaves `GraphVersion` and the content digest unchanged (**Q10**, T6p).
- [ ] 53. `record_diagnostic` round-trips through the `Diagnostics` query and is idempotent on `DiagnosticEvent.id` (**IN13**, T7a, T7b).
- [ ] 54. `record_diagnostic` / `record_fix` do not bump `GraphVersion` (**IN15**, T7d).
- [ ] 55. `snapshot()` records version, counts and digest, and repeated snapshots at one version are distinct ids (**G10**, T7e).
- [ ] 56. A second `SqliteProjectGraph` on the same graph directory fails `Busy`; the lock file exists (**X1**, T2e).
- [ ] 57. Every SQLite call runs inside `spawn_blocking` (**X3**, code review; no `rusqlite` call outside the blocking wrapper module).
- [ ] 58. `close()` checkpoints and is idempotent; `Drop` without `close()` warns (**X5**, T2f).
- [ ] 59. `alloy-index` emits no session events and constructs no `DecisionRecord` (**OB1**, grep for `EventSink`/`DecisionLog` in `alloy-index`).
- [ ] 60. Span names match the OB3 list exactly.
- [ ] 61. `GraphMetricsSnapshot` exposes the §10 counters and follows RFC-0004's snapshot shape (**OB2**).
- [ ] 62. `GraphViewHandle` exposes no write method and no inner-`Arc` accessor (**SEC1**, T11).
- [ ] 63. No `graph_query` MCP tool exists in any crate (**SEC2**, T7 + the existing `no_forbidden_registrations` / `no_bash_registered` tests).
- [ ] 64. The identifier `GraphMutation` does not exist in the workspace (**SEC3**, T8).
- [ ] 65. `alloy-index` contains no `std::process::Command` and no network dependency (**SEC5**, T12).
- [ ] 66. `alloy-index` writes nothing outside `<data_dir>/graph/` and never writes `.env` (**SEC8**, T13).
- [ ] 67. `NullProjectGraph` reads empty and fails writes with `Disabled` (**Q10**, T1e).
- [ ] 68. Reopening after a rebuild preserves version and digest (**T8b**).
- [ ] 69. `VerifyOutcome.diagnostics`-shaped `DiagnosticEvent`s ingest and query back unchanged (**T8d**).
- [ ] 70. `cargo doc --workspace --no-deps` is warning-free with `-D warnings`; every public item in §3 has a doc comment.

---

## 16. Definition of Done

| # | Requirement |
| --- | --- |
| 1 | Every AC in §15 is implemented as a passing test, a CI grep, or a mechanical compile/diff check. |
| 2 | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` are green. |
| 3 | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS: -D warnings` is clean; every public item in §3 is documented. |
| 4 | Architecture compliance: **PASS** — trait matches V2 §7.2; thin MVP nodes only; deferred items stay deferred. |
| 5 | `#![forbid(unsafe_code)]` and `#![deny(missing_docs)]` hold in `alloy-index`; **no new external dependency**. |
| 6 | Amendments A1–A3 have landed additively with their own tests; no merged field shape changed. |
| 7 | The only "not implemented yet" behaviours are the ones this RFC marks **Stub**. Since A-0011-5/A-0011-6 no Stub *queries* remain (Q4–Q6 un-stubbed); IN8/IN9's zero-population holds of the manifest pass only, the RFC-0014 deep pass having landed; `NullProjectGraph` writes still fail `Disabled` (Q10). No `TODO`, `todo!()`, `unimplemented!()`, or placeholder in scope. |
| 8 | Public APIs reviewed and stable: §3 signatures match the implementation with no silent drift. |
| 9 | Security rules SEC1–SEC8 each have a passing grep or unit test. |
| 10 | The V2 obligation mapping in Appendix D is complete — every V2 §7 clause traces to a section here. |
| 11 | RFC text, module docs, and `example.env` comments (for `ALLOY_GRAPH_*` knobs, if any land) are up to date; `.env` untouched. |
| 12 | Code review: **approved**. |

---

## 17. Open Questions

| # | Question | Current answer | Owner |
| --- | --- | --- | --- |
| Q1 | Should the graph live in `alloy.sqlite` via a migration instead of its own file? | No — §5.2. The migration ladder is crate-private to `alloy-runtime`, and a derived cache must be wipeable without touching the durable session log. Revisit only if cross-table joins become load-bearing. | Closed |
| Q2 | Should `GraphQuery` gain a `ModulesForPaths` variant for RFC-0012? | No — `Symbol` with a file path (Q2) covers it without widening a V2-frozen enum. Revisit if 0012 needs batch resolution and N round-trips measure badly. | RFC-0012 |
| Q3 | Should `apply_incremental` be driven by a filesystem watcher? | No for MVP — no watcher, no `alloyd`. Changes are supplied by the caller (CLI / EditEngine post-commit). | Deferred (ADR F-27) |
| Q4 | Should `record_diagnostic` bump `GraphVersion`? | No (IN15). Structure and observations version independently so `sessions.graph_version` is stable across a repair loop. | Closed |
| Q5 | Should the module walk parse `mod` declarations to avoid IN7f's blind spots? | No for the manifest walk (IN7) — that is `syn`, and the `syn` pass has since landed under RFC-0014 (item-level facts on top of the layout-derived modules). Missing modules are acceptable; invented ones are not. | Closed (RFC-0014 deep pass on `main`) |
| Q6 | Should `Symbol` support fuzzy/prefix matching for LLM-supplied paths? | No (Q2). Fuzzy resolution invents relationships the graph cannot justify; RFC-0012 should retry with a corrected path instead. | RFC-0012 |
| Q7 | Should snapshots copy rows so historical versions are queryable? | No (G10) — markers only. Row-level history needs a retention policy nobody has specified. | Later |
| Q8 | Is `globset` the right expander for `[workspace] members`? | Yes — already a workspace dependency (RFC-0005/0006 use it), and cargo's member globs are shallow. Revisit if a fixture exposes a cargo-glob semantic we do not match. | Open |
| Q9 | Should the graph expose an external-only (non-worker) MCP mirror? | Not in MVP. V2 §7.2 permits it "optional later"; ADR F-04 forbids it for Alloy workers, permanently. | Post-Beta |

---

## 18. Estimated implementation effort

| Slice | Work | Effort |
| --- | --- | --- |
| A | `alloy-runtime::graph` seam: ids, node/edge model, `GraphQuery`/`GraphView`, `GraphError`, `GraphViewHandle`, `NullProjectGraph`, re-exports, amendments A1–A3 | 1.0–1.25 pd |
| B | `alloy-index` store: layout, open/PRAGMA/migrate, model-version check, quarantine, lock, metrics, `close` | 1.25–1.75 pd |
| C | Deterministic id derivation + content digest + version bump discipline | 0.5 pd |
| D | Walk + manifest parse + module inference + caps + `IngestReport` | 1.5–2.0 pd |
| E | Incremental application + IN10 equivalence test | 0.75–1.25 pd |
| F | Query engine: `Symbol`, `Diagnostics`, `Subgraph`, Stubs, ordering, truncation | 1.0–1.25 pd |
| G | Diagnostic/fix ingest + snapshots | 0.5 pd |
| H | Fixtures, integration tests, CI greps, docs | 1.0–1.5 pd |
| **Total** | | **~7.5–10.5 pd raw → 6–10 pd with overlap** |

**M7 thin slice (2–4 pd, roadmap-scoped):** A + B + C + F's Stub paths + G, i.e. everything RFC-0012 and RFC-0013 need to compile and answer honestly. D and E may land in the same milestone if the schedule allows, but M7's exit gate does not depend on them — the roadmap's "Do not block M7 on graph depth" is satisfied because an empty graph answers every query without erroring.

Critical path: A → B → D → E. F depends only on A and B and can proceed in parallel with D.

---

## Appendix A — SQL schema (normative, `graph_schema_version = 2`)

The DDL below is **v1** as shipped. **v2** (amendment A-0011-6d) recreates `graph_edges` with the kind `CHECK` expanded to `('defines','imports','references','calls','impls')`, carrying rows over (`CREATE TABLE graph_edges_v2 … INSERT … SELECT … DROP … RENAME`), because SQLite cannot alter a `CHECK`. No other table changed shape; refuse-newer is unchanged.

```sql
-- Ledger, bootstrapped before v1 (RFC-0002 shape).
CREATE TABLE graph_schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

-- Single-row metadata. `model_version` drives truncate-and-rebuild (S4).
CREATE TABLE graph_meta (
  id                  INTEGER PRIMARY KEY CHECK (id = 1),
  model_version       INTEGER NOT NULL,
  graph_version       INTEGER NOT NULL,
  content_digest      TEXT NOT NULL,
  workspace_root_rule TEXT NOT NULL,
  updated_at          TEXT NOT NULL
);

-- Workspace / Crate / Module / Item (Item populated by the RFC-0014 deep pass).
CREATE TABLE graph_nodes (
  id       TEXT PRIMARY KEY,
  kind     TEXT NOT NULL CHECK (kind IN ('workspace','crate','module','item')),
  path     TEXT NOT NULL,
  crate_id TEXT NULL,
  file     TEXT NULL,
  digest   TEXT NULL,
  UNIQUE (kind, path)
);

CREATE INDEX idx_graph_nodes_crate ON graph_nodes(crate_id);
CREATE INDEX idx_graph_nodes_file  ON graph_nodes(file);

-- Defines / Imports (+ References/Calls/Impls at schema v2, A-0011-6d).
-- `confidence` reserved, 1.0 in every row (S6, G11).
CREATE TABLE graph_edges (
  from_id    TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  to_id      TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('defines','imports')),  -- v2: +'references','calls','impls'
  confidence REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (from_id, to_id, kind)
);

CREATE INDEX idx_graph_edges_to ON graph_edges(to_id, kind);

-- File digest tracking for module-subgraph invalidation (V2 §7.2).
CREATE TABLE graph_files (
  path      TEXT PRIMARY KEY,
  crate_id  TEXT NULL,
  module_id TEXT NULL REFERENCES graph_nodes(id) ON DELETE SET NULL,
  digest    TEXT NOT NULL,
  byte_len  INTEGER NOT NULL
);

CREATE INDEX idx_graph_files_crate ON graph_files(crate_id);

-- record_diagnostic sink. Upsert on diagnostic_id (IN13).
CREATE TABLE graph_diagnostics (
  diagnostic_id TEXT PRIMARY KEY,
  code          TEXT NULL,
  level         TEXT NOT NULL,
  package       TEXT NULL,
  fingerprint   TEXT NOT NULL,
  primary_path  TEXT NULL,
  message       TEXT NOT NULL,
  event_json    TEXT NOT NULL,
  recorded_at   TEXT NOT NULL
);

CREATE INDEX idx_graph_diagnostics_pkg_time ON graph_diagnostics(package, recorded_at);
CREATE INDEX idx_graph_diagnostics_code     ON graph_diagnostics(code);
CREATE INDEX idx_graph_diagnostics_fp       ON graph_diagnostics(fingerprint);

-- record_fix sink. Append-only (IN14). Write-only at MVP; read back by
-- `SimilarFixes` since amendment A-0011-5a (Q6).
CREATE TABLE graph_fixes (
  fix_id          TEXT PRIMARY KEY,
  diagnostic_id   TEXT NULL,
  diagnostic_code TEXT NULL,
  crate_id        TEXT NULL,
  transaction_id  TEXT NULL,
  patch_artifact  TEXT NULL,
  verified        INTEGER NOT NULL CHECK (verified IN (0,1)),
  recorded_at     TEXT NOT NULL
);

CREATE INDEX idx_graph_fixes_code ON graph_fixes(diagnostic_code, recorded_at);

-- Immutable version markers (G10).
CREATE TABLE graph_snapshots (
  snapshot_id    TEXT PRIMARY KEY,
  graph_version  INTEGER NOT NULL,
  content_digest TEXT NOT NULL,
  node_count     INTEGER NOT NULL,
  edge_count     INTEGER NOT NULL,
  created_at     TEXT NOT NULL
);

CREATE INDEX idx_graph_snapshots_version ON graph_snapshots(graph_version);
```

Timestamps are RFC-3339 `TEXT`, matching RFC-0002's `Timestamp` serde. Ids are canonical lowercase UUID `TEXT`.

---

## Appendix B — Worked example

### B.1 Input tree

```text
/tmp/toy/
  Cargo.toml                 # [workspace] members = ["crates/*"]
  crates/
    toy-core/
      Cargo.toml             # [package] name = "toy-core"
      src/
        lib.rs
        io.rs
        io/
          reader.rs          # not a module: no io/mod.rs, but io.rs exists → IN7c applies
        util/
          mod.rs
    toy-cli/
      Cargo.toml             # [package] name = "toy-cli"
      src/
        main.rs
  target/                    # skipped (IN3)
  README.md                  # ignored
```

Applying IN7a–IN7e to `toy-core`: root module `toy_core` at `src/lib.rs`; `src/io.rs` → `toy_core::io`; because `io.rs` exists, `src/io/` is descended and `reader.rs` → `toy_core::io::reader`; `src/util/mod.rs` → `toy_core::util`. `toy-cli` has no `src/lib.rs`, so its only root is `src/main.rs` → `toy_cli::main`.

### B.2 Resulting `graph_nodes`

| kind | path | crate_id | file |
| --- | --- | --- | --- |
| `workspace` | `.` | — | `Cargo.toml` |
| `crate` | `toy-cli` | `toy-cli` | `crates/toy-cli/Cargo.toml` |
| `crate` | `toy-core` | `toy-core` | `crates/toy-core/Cargo.toml` |
| `module` | `toy_cli::main` | `toy-cli` | `crates/toy-cli/src/main.rs` |
| `module` | `toy_core` | `toy-core` | `crates/toy-core/src/lib.rs` |
| `module` | `toy_core::io` | `toy-core` | `crates/toy-core/src/io.rs` |
| `module` | `toy_core::io::reader` | `toy-core` | `crates/toy-core/src/io/reader.rs` |
| `module` | `toy_core::util` | `toy-core` | `crates/toy-core/src/util/mod.rs` |

Eight nodes. Zero `item` nodes from the manifest pass (IN9) — the RFC-0014 deep pass adds one `item` per module-level item in these files on top of this projection.

### B.3 Resulting `graph_edges`

| from | to | kind | confidence |
| --- | --- | --- | --- |
| `.` | `toy-cli` | `defines` | 1.0 |
| `.` | `toy-core` | `defines` | 1.0 |
| `toy-cli` | `toy_cli::main` | `defines` | 1.0 |
| `toy-core` | `toy_core` | `defines` | 1.0 |
| `toy_core` | `toy_core::io` | `defines` | 1.0 |
| `toy_core` | `toy_core::util` | `defines` | 1.0 |
| `toy_core::io` | `toy_core::io::reader` | `defines` | 1.0 |

Seven edges. Zero `imports` edges from the manifest pass (IN8) — again, the deep pass adds the `use`-derived `imports` (and, since A-0011-6, any resolvable `references`/`calls`/`impls`).

### B.4 Example query results

`rebuild` returns `GraphVersion(1)`; a second `rebuild` returns `GraphVersion(1)` with `IngestReport.unchanged = true` (IN6).

*The JSON below shows the MVP thin (model-1) projection: `"fidelity": "manifest"` and no item rows. Current builds ingest at model 3, so the same queries report `"fidelity": "syn_deep"` and their node/edge sets include the deep-pass rows noted in B.2/B.3.*

```jsonc
// query(Symbol { path: "crates/toy-core/src/io.rs" })   — Q2 branch (2)
{
  "version": 1,
  "nodes": [
    { "kind": "module", "path": "toy_core::io", "crate_id": "toy-core",
      "file": "crates/toy-core/src/io.rs", "digest": "9f2c…" }
  ],
  "edges": [],
  "diagnostics": [], "fixes": [],
  "fidelity": "manifest", "truncated": false
}

// query(Subgraph { seeds: [id("toy_core::io")], radius: 1 })   — Q7
{
  "version": 1,
  "nodes": [
    { "kind": "module", "path": "toy_core",              "…": "…" },
    { "kind": "module", "path": "toy_core::io",          "…": "…" },
    { "kind": "module", "path": "toy_core::io::reader",  "…": "…" }
  ],
  "edges": [
    { "from": "…toy_core", "to": "…toy_core::io",         "kind": "defines", "confidence": 1.0 },
    { "from": "…toy_core::io", "to": "…toy_core::io::reader", "kind": "defines", "confidence": 1.0 }
  ],
  "diagnostics": [], "fixes": [],
  "fidelity": "manifest", "truncated": false
}

// query(Callers { fn_node: <unknown id> })   — Q5 as amended by A-0011-6:
// an unknown anchor is empty and honestly untruncated; a known fn returns
// the anchor plus its incoming `calls` edges.
{ "version": 1, "nodes": [], "edges": [], "diagnostics": [], "fixes": [],
  "fidelity": "manifest", "truncated": false }
```

### B.5 Incremental example

Editing `crates/toy-core/src/io.rs` and calling
`apply_incremental(&[FileChange { path: "crates/toy-core/src/io.rs".into(), kind: Modified }])`:

- the file re-hashes to a new digest → `graph_files.digest` updated;
- no node or edge changes (MVP has no intra-file nodes);
- content digest changes → `GraphVersion(2)`.

Adding `crates/toy-core/src/io/writer.rs` with `kind: Created` adds one `module` node `toy_core::io::writer` and one `defines` edge from `toy_core::io`, and bumps to `GraphVersion(3)`. A full `rebuild` at that point produces the same content digest (IN10, T5).

---

## Appendix C — `GraphError` decision table for callers

| Caller | On `Busy` | On `Corrupt` | On `Disabled` | On `Workspace`/`Manifest` |
| --- | --- | --- | --- | --- |
| RFC-0012 Context Engine | Retry once, then omit the graph projection | Omit the projection; log | Omit the projection (graph is off) | N/A (never ingests) |
| RFC-0013 workers | N/A — workers only `query`, and `query` failures degrade the PromptPack, never the run | — | — | — |
| RFC-0015 CLI `alloy index` | Retry with backoff | Report; the store self-quarantines and the retry rebuilds | Report "graph disabled" | Report the path and reason; exit non-zero |
| Runtime host verify path | Log and continue — a lost diagnostic record MUST NOT fail the run | Log and continue | Log once and continue | N/A |

Rule **E1**: a graph failure MUST NEVER fail a DAG node. The graph is an accelerator; the repair loop works without it.

---

## Appendix D — Architecture V2 obligation mapping

| V2 clause | Obligation | Where satisfied |
| --- | --- | --- |
| §5.2 | ProjectGraph owns `GraphStore`, not LLM prompts | §3.10, §11 (no prompt assembly here) |
| §5.3 | In-process, single binary | §8 X2 |
| §5.4 | `alloy-index` = ProjectGraph MVP; ≤5 crates | §2.4 C1, C6 |
| §5.6 | Graph corruption → rebuild from source; quarantine snapshot | §5.7 S8, §9.4 |
| §7.1 | Persistent, queryable, survives sessions, feeds Context | §5, §7, §7.3 |
| §7.2 interface | Trait: rebuild / apply_incremental / query / record_diagnostic / record_fix / snapshot | §3.5 |
| §7.2 interface | **Single writer service** | §8 X1 |
| §7.2 interface | Workers get read-only `GraphView` / query handle in-process | §3.8, §11 SEC1 |
| §7.2 interface | Writes only via Graph service ingest — never worker `GraphMutation` | §11 SEC3, SEC4 |
| §7.2 interface | **No builtin `graph_query` MCP for Alloy workers** (ADR F-04) | §11 SEC2, T7 |
| §7.2 interface | Optional later: external-only MCP mirror | §17 Q9 (deferred) |
| §7.2 MVP | Nodes: Workspace / Crate / Module / Item + Diagnostic + FixEvent | §4.2, §4.4, §4.5 |
| §7.2 MVP | Edges: structural Defines/Imports **as available** | §4.3, IN8 (Imports not available in MVP) |
| §7.2 MVP | **No** Calls / HasLifetime / SimilarFixes auto-retrieve | Shipped so at MVP; syntactic `Calls` edges (confidence 1.0) recorded since A-0011-6a, `SimilarFixes` read-back + bounded advisory note since A-0011-5 — `HasLifetime` and wider auto-injection stay deferred (§1.4, Q6) |
| §7.2 MVP | Live RA queries for refs/impls **may** passthrough behind `query()` | Q4 answers syntactically since A-0011-6c; RA passthrough (`GraphFidelity::Analyzer`) remains the deferred rustc-grade path |
| §7.2 MVP | File digest invalidation of module subgraphs, not Merkle multi-layer | §6.6, `graph_files` |
| §7.2 Deferred | Typed call/lifetime edges; SimilarFixes; Merkle; alloyd; embeddings | §1.4, §14.2 |
| §7.2 Evolution | Raise edge confidence; add layers behind the same query enum; never dual MCP+direct mutation | S6 (confidence column), Q1 (frozen enum), SEC2+SEC3 |
| §7.2 GraphQuery | Seven variants, exact fields | §3.3 |
| §7.2 Internal | `alloy-index` SQLite; ingest from cargo metadata + syn; diagnostics from check JSON | §5 (SQLite), §6.2 (manifest + syn at Beta; **cargo metadata post-Beta**), §6.7 (check JSON) |
| §7.2 Stub | `Callers` / `SimilarFixes` return empty; confidence reserved | Superseded — both live since A-0011-6/A-0011-5 (Q5, Q6); confidence still reserved (S6) |
| §7.2 Upgrade path | Fixes go to eval fixtures / curated notes first, not auto prompt injection | Q6: stored at MVP; read back since A-0011-5a and surfaced only as the bounded A-0011-5c advisory note (codes/packages/dates/artifact ids — never patch bodies); wider injection still gated on precision |
| §7.3 | `.alloy/graph/` (or XDG); sessions reference `GraphVersion` | S1 (`StorageLayout::graph_dir`, which honours the XDG fallback), S2 (`sessions.graph_version`) |
| §9 `CapabilityContext` | `graph: GraphViewHandle` — "read-only query handle, not a mutation API" | §3.8, SEC1 |
| §9 `CapabilityOutput` | `graph_mutations` removed from workers | SEC3 |
| §12.2 | `graph_query` MCP deleted for Alloy workers | SEC2 |
| §15 | `LanguageBackend::index(root, &dyn ProjectGraph)` | §3.5 (trait object shipped); RFC-0014 owns the backend |
| §20 R6 | Graph incorrect edges → thin MVP; confidence later; rebuild | G7, S6, §5.7 |
| §20 R16 | RA skew → optional RA; syn/cargo degraded mode required | Q4, `GraphFidelity`, §6.2 |
| §21.1 | "Graph: in-process read; ingest-only writes; no worker graph_query MCP — Pass" | §11 |

---

## Appendix E — What downstream RFCs must do

### E.1 RFC-0012 (Context Engine)

1. MUST hold a `GraphViewHandle`, never an `Arc<dyn ProjectGraph>`.
2. MUST treat `GraphView.fidelity` as a citation label and MUST NOT present `Manifest`-fidelity data as call-graph knowledge.
3. MUST tolerate an empty `GraphView` from every query kind; the WorkingSet domain degrades, it does not fail (Appendix C, E1).
4. MUST treat graph-derived strings (paths, package names) as untrusted repository content when assembling PromptPacks (§11).
5. MUST NOT cache `GraphView`s across a `GraphVersion` change without revalidating.

### E.2 RFC-0013 (Capability Registry & Workers)

1. `CapabilityContext.graph: GraphViewHandle` — the field type is fixed by §3.8.
2. MUST NOT add any graph field to `CapabilityOutput` (SEC3).
3. MUST NOT register a `graph_query` tool or any tool that proxies `ProjectGraph` (SEC2).

### E.3 RFC-0014 (LanguageBackend, Beta)

1. `index(&self, root, graph: &dyn ProjectGraph)` receives the trait object from §3.5 unchanged.
2. Adding `Item` nodes / `Imports` edges MUST bump `GRAPH_MODEL_VERSION` (S4) so stale manifest-only databases are re-ingested rather than merged.
3. MUST NOT change the `ProjectGraph` trait, the `GraphQuery` enum, or any Appendix A table shape.

### E.4 RFC-0015 (CLI, Profiles & Config)

1. Owns the ingest trigger (IN1): an explicit subcommand plus optional bootstrap at session create.
2. Owns writing the returned `GraphVersion` into `sessions.graph_version` (S2).
3. Owns any `DecisionKind::Custom("graph_rebuild")` record derived from `IngestReport` (OB5).
4. MUST document any `ALLOY_GRAPH_*` knob in `example.env` and MUST NEVER write `.env`.
