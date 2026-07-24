# Alloy RFC — Chief Architect Response to Design Review

| Field | Value |
| --- | --- |
| **Author** | arkadianet (chief architect) |
| **Responds to** | `docs/architecture/rfc-design-review.md` |
| **Subject RFC** | `docs/architecture/ai-coding-harness-architecture-rfc.md` |
| **Date** | 2026-07-25 |
| **Status** | Architecture Decision Record — response only (RFC not rewritten herein) |
| **Interactive deliverable** | [rfc-architect-response.canvas.tsx](/home/rkadias/.cursor/projects/home-rkadias-coding-development-arkadianet-alloy/canvases/rfc-architect-response.canvas.tsx) |

---

## 0. Verdict

The review is largely correct on **scope discipline**, **ownership conflicts**, **roadmap honesty**, **sandbox-before-dogfood**, and **unmeasured cost claims**. Those are real failures of the RFC as an implementation contract.

Where the review overreaches is treating **interface-thin / implementation-thin** abstractions as design theater and recommending their deletion. Alloy’s thesis is not “ship a linear chat loop with cargo check.” It is **compile-gated BYOM control flow with inspectable state, MCP-mediated tools, and a path to semantic project understanding**. Deleting the Task DAG, ProjectGraph, Capability, EditEngine, MCP host, and ModelRouter *traits* because the first implementation is thin would freeze us into the exact agent shape we set out to escape—requiring a rewrite at month 6–12 instead of a fill-in.

**Disposition:** Revise architecture commitments along the lines below. Do **not** implement the RFC as written. Do **not** collapse to “§21.4 only forever.” §21.4 is the correct *day-1 vertical slice*; it is not the product architecture.

**Decision counts (findings F-01…F-28):** **18 Accept · 10 Partially accept · 0 Reject** of the finding’s *core critique*. Several kill-list *remedies* that delete load-bearing abstractions are **Rejected** inside those partials (see §3 and §5).

---

## 1. Guiding principles for this response

1. **Interface-stable / implementation-thin** for load-bearing 12–24 month abstractions (DAG, Graph, Capability, EditEngine, MCP host, ModelRouter).
2. **Delete ceremonial surfaces** (seven MCP processes, 11 worker specs, 8 live context domains, alloyd peer, ACP, External Memory embeddings, OverlayFS product, Hint edges / file leases in MVP).
3. **Falsify the thesis early** with compile-gated repair + holdout eval—without pretending that text-diff success alone is the end state.
4. **Single ownership** for mutable control-plane state (DAG topology, graph writes, checkpoints).
5. **Sandbox before dogfood.** Non-negotiable.

Classification legend used below: **Production proven** · **Emerging best practice** · **Original proposal**.

---

## 2. Finding-by-finding responses

### Critical

#### F-01 — Semantic IR as product identity vs admitted MVP
**Critique (fair restatement):** Principle 3.5 and §13 make `SemanticEditOp` the only structured write path, while Week 8 ships gated text edits and §13.7 admits major ops are research stubs. That contradiction will burn months on AST lowering while `ReplaceBody`/diffs become the real path.

| Decision | **Partially accept** |
| --- | --- |

**Justification:** The reviewer is right that IR-as-*only*-write-path was overclaimed for MVP. They are wrong that the IR should be deleted until text edits are “proven the bottleneck.”

Failure mode of “diffs only forever”: multi-file Rust refactors (rename + import fix + signature change) thrash on span drift; workers cannot state intent; the harness cannot gradually introduce RA-assisted ops without inventing a second write stack. That is how every chat agent stays stuck.

**Long-term implication:** Edit path must be a **transactional apply layer** with a stable op envelope. Text patches are one payload type; semantic ops are another. Lowering quality grows behind the same `EditEngine` trait.

**Smallest change:**
- Reframe principle 3.5: *prefer semantic ops when lowering exists; text patches are first-class MVP serialization, not an escape hatch.*
- Introduce `EditRequest { TextPatch | SemanticOps }` behind `EditEngine::apply`.
- MVP implements `TextPatch` only (+ optional `RenameType` via ra when ready).
- Keep `SemanticEditOp` enum in the crate as **unstable / incomplete**; stubs fail closed—do not schedule SplitCrate/ExtractTrait in 0.1.0.

**Classification:** IR as harness identity was **Original proposal overclaimed**. RA assists are **Production proven**; harness-level op envelope remains **Original proposal** (kept thin).

---

#### F-02 — Project Intelligence Graph scoped as compiler frontend
**Critique:** Typed multi-layer graph (calls, lifetimes, cfg, SimilarFixes, Merkle incremental) is a program-analysis product, not a harness index. Degraded mode + macro holes will dominate; §18 savings assume accuracy that will not exist.

| Decision | **Partially accept** |

**Justification:** Scope of *layers* was wrong. Deleting `ProjectGraph` and living on ad-hoc scrapes is also wrong.

Failure mode of “no graph trait”: every worker invents its own `cargo metadata` + syn parse; diagnostic lineage is lost; Context Engine cannot request bounded projections; incremental indexing later requires rewriting call sites.

**Long-term implication:** Graph accuracy is a **confidence-scored** product that deepens. Call/lifetime edges arrive after eval proves index hit-rate matters—not before the repair loop works.

**Smallest change:**
- Keep `ProjectGraph` trait + SQLite store.
- MVP nodes/edges: Workspace/Crate/Module/Item (syn+metadata) + Diagnostic + FixEvent ingest. **No** Calls/HasLifetime/SimilarFixes auto-retrieve.
- Live RA queries for refs/impls behind the same `query()` API (may be passthrough initially).
- Defer Merkle typed incremental rebuild past 0.1.0; file digest invalidation of module subgraphs is enough.

**Classification:** Aider-style maps **Production proven**; Alloy typed multi-layer jump was overstated **Original proposal**—retain thin ancestor + ingest path.

---

#### F-03 — Multiple DAG topology writers
**Critique:** Planner replan, worker `follow_up_nodes`, and Scheduler apply/cancel all reshape the DAG; “validate acyclic” is not a conflict protocol.

| Decision | **Accept** |

**Justification:** Correct. Dual writers guarantee non-reproducible runs and oscillating repair loops.

**Long-term implication:** Dynamic plan growth is allowed only through a single Replan authority with generation counters and provenance.

**Smallest change:**
- Delete `CapabilityOutput.follow_up_nodes`.
- Workers return structured `FailureIr` / artifacts only.
- Scheduler may *request* replan; only Planner/ReplanService mutates topology (plus Scheduler cancel/skip of existing nodes).

---

#### F-04 — Dual graph access + unclear mutation ownership
**Critique:** Workers hold `Arc<dyn ProjectGraph>` and also MCP `graph_query`; `GraphMutation` owner missing.

| Decision | **Accept** |

**Justification:** Two truth channels violate principle 3.3 and make R6 inevitable under parallel Analyze.

**Long-term implication:** Graph is an in-process service. MCP may later expose *read* tools for *external* agents, not for Alloy’s own workers.

**Smallest change:**
- Workers get read-only `GraphView` / `ProjectGraph` query handle in-process.
- Writes only via Graph service ingest (diagnostics, fixes, index rebuild)—never worker-supplied `GraphMutation` blobs.
- Delete builtin `graph_query` MCP for Alloy workers. Optional later: external-only MCP mirror.

---

#### F-05 — Component / crate topology before a working loop
**Critique:** ~15 components / ~18 crates before any repair loop works; OpenHands V0 complexity.

| Decision | **Partially accept** |

**Justification:** Crate explosion was theater. Collapsing *module* architecture into an unstructured ball of mud is the opposite mistake.

**Long-term implication:** Crate splits follow compile-time / ownership pressure. Component names remain the mental model and folder boundaries.

**Smallest change (≤5 crates for ~3 months):**
- `alloy-cli` — binary / TTY
- `alloy-runtime` — session, DAG, scheduler, router, capabilities, context, edit apply
- `alloy-tools` — MCP host + in-process cargo/fs/git facades + sandbox
- `alloy-index` — ProjectGraph MVP
- `alloy-eval` — fixtures, ScriptedProvider, gates

Internal modules mirror future crates. No `alloy-daemon`, `alloy-lang-*` packages, or empty peer libs in week 1.

---

#### F-06 — Roadmap honesty failure
**Critique:** Weekly slices vs dogfood after W8 before sandbox/DAG/semantic; §21.4 contradicts §5.4 skeleton; 26-week 0.1.0 fiction.

| Decision | **Accept** |

**Justification:** Correct. Theater greens destroy trust.

**Smallest change:** Replace weekly fantasy with **three milestones × 6–8 weeks**, one falsifiable thesis each. Ban Alloy-on-Alloy dogfood until sandbox + compile-gated repair pass holdout. Do not scaffold 18 crates in week 1.

---

#### F-07 — Critical sandbox threats scheduled after dogfood
**Critique:** `build.rs`/proc-macro RCE Critical in threat model; MCP + stub sandbox early; container broker late.

| Decision | **Accept** |

**Justification:** Scheduling security after dogfood is indefensible.

**Smallest change:** Milestone-1 exit requires Landlock/Seatbelt (or container) on *all* cargo/tool exec; quarantine profile default for network/deps; document that check still runs build scripts; community MCP deferred until allowlists enforced. Dogfood only after that gate.

---

#### F-08 — Cost model used as architecture proof
**Critique:** 30–60% savings and $ bands sold as differentiators while admitted unmeasured; subscription comparison category error.

| Decision | **Accept** |

**Justification:** Correct. Numbers as architecture proof are marketing debt.

**Smallest change:** Strip numeric cost claims from differentiators until Eval calibrates. Keep budgets, tiers, metering APIs. Publish numbers only from measured holdout runs.

---

### High

#### F-09 — MCP custom server sprawl
**Critique:** Thesis warns about schema tax then specifies ~7 custom servers; lazy selectors cut tokens not process/ops/security surface.

| Decision | **Partially accept** |

**Justification:** Seven processes were wrong. Abandoning MCP-native host is wrong for the product thesis (auditable tool bus, permission tiers, future external tools).

Failure mode of “in-process forever, no MCP host”: every new tool bypasses permission/disclosure policy; community tools cannot plug in; “MCP-native” becomes a slide.

**Smallest change:**
- Keep **one MCP host** with lazy disclosure (**Emerging best practice** reaction to schema tax).
- MVP tools = in-process builtins registered *as if* MCP tools (same schema/permission path): `cargo_check`, `cargo_test`, `fs_read`, `apply_patch`, optional `ra_*`.
- Zero extra OS processes for builtins. Add out-of-process servers only when isolation or reuse demands it (crates.io/git/rustdoc deferred).

---

#### F-10 — VerifyCompile dual-modeled
**Critique:** Same step is NodeKind and Capability.

| Decision | **Accept** |

**Smallest change:** `VerifyCompile` / `VerifyTest` / `GateHuman` are **runtime node kinds** executed by deterministic runtime adapters—not LLM Capabilities and not ModelRouter targets.

---

#### F-11 — Classification label inflation
**Critique:** “Production proven” used to bless typed graphs; orphan “Original proposal” stamps; App CU more honest than body.

| Decision | **Accept** |

**Smallest change:** Inline labels only with named prior-art delta. Ban orphan stamps. Relabel graph/IR/capability claims honestly in next RFC revision.

---

#### F-12 — Context domain proliferation
**Critique:** Eight normative domains; half stubbed; APIs encode all eight.

| Decision | **Partially accept** |

**Justification:** Eight live domains are theater. Deleting domain labels entirely loses budget knobs and stale-summary hygiene.

**Smallest change:** `DomainId` enum may list future domains, but MVP **implements** only Conversation, WorkingSet (files+graph projection+diagnostics), Artifacts. Others return empty / unused until measured need. No embedding index in Context Engine for 0.1.0.

---

#### F-13 — Capability / worker over-catalog
**Critique:** Eleven full specs; multi-impl unused; planner hallucination surface.

| Decision | **Partially accept** |

**Justification:** Catalog sprawl was wrong. Deleting the Capability trait because there is one impl each throws away the composition model that replaces persona modes.

Failure mode of “hardcoded four procedures with no registry”: adding Review or a rules-based BorrowAnalysis requires rewriting the scheduler; multi-impl scoring never gets an insertion point.

**Smallest change:**
- Keep `Capability` trait + registry (even if resolve is trivial).
- 0.1.0 LLM capabilities: `Repair` (borrow/type), `Edit` (codegen), `Review` (optional), `Planning` (template-first; LLM planner gated—see Eval).
- Testing/Verify = runtime nodes.
- Delete Benchmarking / Documentation / ArchitectureReview / UnsafeAudit / CargoManagement from 0.1.0 schedule.

---

#### F-14 — Hidden write-path coupling
**Critique:** Edit→Graph anchors→RA→MCP edit→Overlay→Sandbox→Checkpoint with no graceful degraded mode.

| Decision | **Partially accept** |

**Justification:** Coupling was overbuilt. Primary MVP path must be `model → patch → apply → check` with git checkpoint.

**Smallest change:** One `EditEngine` apply path; git checkpoint only; RA optional; delete dual edit server/crate split and OverlayFS product. Semantic path plugs in later without new stack.

---

#### F-15 — Language plugin system too early
**Critique:** Trait freeze W23; PY/TS sketches; cdylib before Rust proven.

| Decision | **Partially accept** |

**Justification:** Sketches and cdylib were premature. Deleting any language boundary forever couples cargo/syn into the scheduler.

**Smallest change:** Internal `LanguageBackend` trait in `alloy-runtime` or `alloy-index`, **Rust-only impl** for ≥6 months. No PY/TS crates, no cdylib. Trait freeze only after Rust dogfood—not a week-23 ceremony with empty impls.

---

#### F-16 — Parallelism claims vs serial cargo/edits
**Critique:** `max_parallel_cargo=1`, `max_parallel_edits=1` make DAG parallelism mostly illusory.

| Decision | **Accept** |

**Smallest change:** Document linear MVP honestly. Defer Hint edges, priority function, file leases until eval shows parallel Analyze uplift. Keep DAG for provenance, gates, retries, caching—not for fake parallelism marketing.

---

#### F-17 — Observability / prompt retention defaults
**Critique:** Always-on decision logs with optional full prompts; 14-day retention of sensitive code.

| Decision | **Accept** |

**Smallest change:** Default = metadata + hashes + redacted decision records. Full prompts / file-body tool results opt-in per session. Retention configurable; no file bodies by default.

---

#### F-18 — Ownership theater
**Critique:** Owners are component labels; CODEOWNERS deferred; R10 accepted.

| Decision | **Accept** |

**Smallest change:** Name humans (arkadianet as architect owner until team exists) or drop Owner column. Add CODEOWNERS before Phase 1 substantive merges.

---

#### F-19 — Eval too late relative to claims
**Critique:** Meaningful eval W20; early thresholds without holdout; cost claims earlier.

| Decision | **Accept** |

**Smallest change:** Fixtures + `ScriptedProvider` from week 1. Holdout gates every milestone exit. No cost marketing until calibrated. W8-style numeric thresholds only against holdout.

---

#### F-20 — Model Router over-engineered early
**Critique:** Multi-factor scoring/residency/health before second provider exists.

| Decision | **Partially accept** |

**Justification:** Scoring matrix premature. Router *trait* + tier map is load-bearing for BYOM.

**Smallest change:** MVP = TOML `capability|node_kind → tier` + one openai-compatible provider. Health failover stubs OK. Multi-factor scoring after ≥2 providers and measured misroutes.

---

### Medium (subsystem-touching)

#### F-21 — Document sprawl past “End of RFC”
| Decision | **Accept** |
Normative RFC = §§1–21 + appendices A–E (or equivalent short set). Move BL–DB to non-normative handbook or delete.

#### F-22 — Missing RunController interface
| Decision | **Accept** |
`RunController { start, cancel, approve, replan }` owned with Scheduler/runtime. Session owns lifecycle, events, budgets only.

#### F-23 — SimilarFixes / External Memory premature
| Decision | **Accept** |
Successful patches → eval fixtures / curated notes. No auto-retrieved fix memory in prompts until precision measured. External Memory embeddings out of 0.1.0.

#### F-24 — Triple checkpoint story
| Decision | **Accept** |
MVP checkpoint backend = **git only**. Snapshot bundles / OverlayFS deferred.

#### F-25 — Design poorly hermetically testable
| Decision | **Accept** |
ScriptedProvider + recorded cargo JSON fixtures day 1; thesis tests must run offline.

#### F-26 — P0 lifetime goals vs stubbed ops
| Decision | **Partially accept** |
Narrow P0 success criteria to **locally editable diagnostics** (E0502-class, import/type errors fixable by text patch). Lifetime repair is stretch goal after RA-assisted ops exist—not a Week 8 claim.

### Low

#### F-27 — alloyd as optional peer
| Decision | **Accept** |
Remove from architecture body until single-binary p95 fails on real repos. Research backlog only.

#### F-28 — Survey length as normative ballast
| Decision | **Accept** |
Survey → separate doc; one-page gap table remains in RFC.

---

## 3. Rejected kill-list remedies (even where critique accepted)

These remedies from the review/kill list are **Rejected** as stated:

| Remedy rejected | Why original (thinned) design remains preferable |
| --- | --- |
| Delete Task DAG; ship only an opaque retry loop | Loses inspectable plans, gate nodes, per-node budgets, resume provenance—the control-plane thesis. MVP uses **hardcoded DAG templates**, not “no DAG.” |
| Delete `ProjectGraph` trait; scrape ad hoc | Prevents bounded projections, diagnostic lineage, and evolution to typed edges without rewriting workers. |
| Delete Capability registry; hardcode procedures only | Blocks multi-impl and composition without scheduler rewrites. Thin registry + ≤4 caps is enough. |
| Delete MCP host; “just functions” | Abandons permissioned tool bus and MCP-native thesis. In-process builtins behind host is the right thin form. |
| Delete EditEngine; raw FS writes from workers | No transactional checkpoint boundary; semantic ops cannot land later cleanly. |
| Delete ModelRouter; call provider directly | Hardcodes provider into workers; breaks BYOM tier policy. |
| Delete LanguageBackend entirely | Couples rustc/cargo into control plane forever. |
| “If text-diff+check cannot beat naive agent, **stop** (graph/IR will not save thesis)” | Wrong causal story. Failure of text-diff on Rust *supports* investing in graph/IR under compile gates. Correct gate: if **compile-gated DAG + BYOM** cannot beat naive agent on holdout, stop—*control plane* failed. Graph/IR are then prioritized research, not proof the thesis is false. |
| Treat §21.4 as the entire architecture | Day-1 slice ≠ product architecture. |

---

## 4. Ten subsystems — commitment / MVP / deferred / evolution

### 4.1 Task DAG

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | Explicit `TaskDag` with node state machine, Data/Sequence edges, generation, checkpoints, gate nodes. Single topology mutator (Planner/Replan). **Emerging best practice** (workflow DAGs) applied to agents (**Original proposal** synthesis). |
| **MVP implementation** | Hardcoded repair templates (3–5 nodes: analyze → edit → verify → gate). Persist DAG in SQLite. No LLM planner required to mutate shape. |
| **Deferred** | LLM planner as default; Hint edges; fancy priority; dynamic worker-proposed nodes. |
| **Evolution** | Swap template source for Planner behind same DAG schema; add parallel Analyze when eval shows uplift; never reintroduce multi-writer topology. |

### 4.2 Scheduler

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | Ready-queue executor over DAG; retries; budgets; cancel; `RunController`; replan *requests*. |
| **MVP implementation** | Linear / `max_parallel=1` for cargo+edits; in-process; SQLite-backed. |
| **Deferred** | File leases; priority function; distributed workers; Temporal-like durability. |
| **Evolution** | Raise parallelism knobs when measured; keep algorithm interface stable. |

### 4.3 Project Intelligence Graph

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | `ProjectGraph` trait: rebuild/incremental invalidate/query/record_diagnostic/record_fix/snapshot. Single writer service. |
| **MVP implementation** | cargo metadata + syn symbols + diagnostic/fix event store; RA passthrough for refs/impls. |
| **Deferred** | Typed call/lifetime edges; SimilarFixes auto-retrieve; Merkle multi-layer incremental; background alloyd indexer. |
| **Evolution** | Raise edge confidence; add layers behind same query enum; never dual MCP+direct mutation. |

### 4.4 Context Engine

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | `assemble(budget)` → `PromptPack` with citations + domain labels + stale detection hooks. |
| **MVP implementation** | Three live domains: Conversation, WorkingSet, Artifacts. Fixed weights. |
| **Deferred** | Architecture/Scratchpad/Long-Term as live; embedding fuzzy recall; aggressive economy summarization. |
| **Evolution** | Enable domains when metrics show need; keep PromptPack shape stable for cache discipline. |

### 4.5 Capability System

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | `Capability` trait + registry resolve; side-effect class; tool selectors; no topology mutation in output. |
| **MVP implementation** | ≤4 LLM capabilities; one impl each; trivial resolve. |
| **Deferred** | Multi-impl scoring; Benchmarking/UnsafeAudit/CargoManagement/Documentation/ArchitectureReview workers. |
| **Evolution** | Register alternate impls (rules-based BorrowAnalysis) without scheduler changes. |

### 4.6 Semantic Editing

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | `EditEngine` transactional apply + rollback via checkpoint; op envelope `TextPatch | SemanticOps`. |
| **MVP implementation** | Unified diff / text apply + git checkpoint + sandbox check. |
| **Deferred** | Full SemanticEditOp lowering; OverlayFS; SplitCrate/ExtractTrait/MoveModule. |
| **Evolution** | Add RA-backed ops one at a time; workers migrate to ops without new write stack. |

### 4.7 MCP Server Count

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | MCP host = sole tool bus; lazy disclosure; permission tiers; fail-closed. |
| **MVP implementation** | In-process builtins only (check/test/fs/patch[/ra]); **0–1** out-of-process servers. |
| **Deferred** | Custom crates/git/rustdoc/codeintel processes; community MCP until broker allowlists. |
| **Evolution** | Promote builtins to out-of-process when isolation needed; schemas unchanged. |

### 4.8 Worker Count

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | Workers are Capability impls; verify/test are runtime nodes—not workers. |
| **MVP implementation** | Repair, Edit, optional Review; template Planning. |
| **Deferred** | Remaining §10 catalog. |
| **Evolution** | Grow catalog only after holdout plateau on P0 repair. |

### 4.9 Evaluation Framework

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | Eval gates milestones; holdout sets; ScriptedProvider; cost+compile metrics. **Emerging best practice** if enforced. |
| **MVP implementation** | Fixtures from week 1; offline thesis tests; holdout borrow-repair compile success as Milestone-1 bar. |
| **Deferred** | Large multi-crate feature suites; public leaderboard marketing. |
| **Evolution** | Expand fixture corpus; calibrate cost claims; gate each phase exit. |

### 4.10 Plugin Architecture

| Layer | Decision |
| --- | --- |
| **Architectural commitment** | `LanguageBackend` trait for index/diagnostics/test/edit-lower. |
| **MVP implementation** | Rust-only internal module; no dynamic loading. |
| **Deferred** | Python/TS backends; cdylib; trait freeze ceremony. |
| **Evolution** | Second language after ≥6 months Rust dogfood; freeze trait when second impl forces it. |

---

## 5. Decision tables

### 5.1 Accepted changes

| ID | Change |
| --- | --- |
| F-03 | Delete `follow_up_nodes`; single Replan authority |
| F-04 | Single in-process graph read path; ingest-only writes; no worker `graph_query` MCP |
| F-06 | Three milestones; no dogfood before sandbox+repair; no 18-crate week 1 |
| F-07 | Sandbox before dogfood; quarantine defaults; defer community MCP |
| F-08 | Strip numeric cost marketing until Eval |
| F-10 | Verify/Test/GateHuman = runtime kinds only |
| F-11 | Honest classification labels with prior-art deltas |
| F-16 | Admit linear MVP; defer leases/Hint/priority |
| F-17 | Metadata+hashes default; prompts opt-in |
| F-18 | Real owners / CODEOWNERS before Phase 1 |
| F-19 | Fixtures+holdout from week 1 |
| F-21 | Cap normative RFC; exile BL–DB |
| F-22 | Add `RunController` |
| F-23 | No auto SimilarFixes/External Memory embeddings |
| F-24 | Git-only checkpoints for MVP |
| F-25 | ScriptedProvider + recorded cargo fixtures day 1 |
| F-27 | Remove alloyd from body until measured need |
| F-28 | Survey out of normative RFC |

### 5.2 Partially accepted changes

| ID | Accept | Reject (within same finding) |
| --- | --- | --- |
| F-01 | Text patches first-class MVP; IR not sole write path | Deleting EditEngine/IR envelope until “bottleneck proven” |
| F-02 | Thin index MVP; no typed call/lifetime layers | Deleting `ProjectGraph` trait |
| F-05 | ≤5 crates for 3 months | Erasing component/module boundaries |
| F-09 | Collapse to in-process builtins; ≤1 external server | Deleting MCP host / MCP-native tool bus |
| F-12 | Three live domains | Deleting domain/budget abstraction |
| F-13 | ≤4 LLM capabilities; test as runtime | Deleting Capability registry |
| F-14 | Primary path model→patch→apply→check | Deleting EditEngine / dual-write chaos forever |
| F-15 | Rust-only, no cdylib/PY/TS | Deleting LanguageBackend trait |
| F-20 | TOML tier map only | Deleting ModelRouter trait |
| F-26 | Narrow P0 away from lifetime claims | Dropping lifetime from long-term goals entirely |

### 5.3 Rejected changes

| Change | Rationale |
| --- | --- |
| Delete Task DAG abstraction | Load-bearing for inspectable compile-gated control flow |
| Delete ProjectGraph / Capability / EditEngine / MCP host / ModelRouter traits | Interface-thin forms preserve 12–24 month evolution; deletion forces rewrite |
| “Stop entirely if text-diff loses to naive agent” | Wrong falsification target; falsify control plane, not semantic depth early |
| Replace architecture with §21.4-only forever | Confuses day-1 slice with product architecture |
| Keep numeric §18 bands / 11 workers / 7 MCP servers / 8 live domains / alloyd / ACP in MVP | Ceremonial or non-thesis scope |

---

## 6. Revised MVP architecture (concise)

Single binary (`alloy`), ≤5 crates, interface-thin internals:

| Component | MVP role |
| --- | --- |
| CLI | Goals, approvals, config (`example.env`, router/profile TOML) |
| Session Manager | Lifecycle, SQLite event log, budgets, resume |
| RunController | start / cancel / approve / request_replan |
| Task DAG + Scheduler | Hardcoded repair DAG; linear exec; retries; gates |
| Planner (template) | Load/select DAG template; LLM planner **off** until eval bar |
| Capability Registry | Resolve ≤4 LLM caps |
| Workers | Repair, Edit, optional Review |
| Model Router | One openai-compatible provider; capability→tier TOML |
| Context Engine | Conversation + WorkingSet + Artifacts → PromptPack |
| ProjectGraph | Metadata + symbols + diagnostics/fixes |
| EditEngine | TextPatch apply + git checkpoint |
| MCP Host | In-process builtins with lazy disclosure + permissions |
| Sandbox Broker | Landlock/Seatbelt or container on all exec; quarantine profile |
| Observability | Decision metadata + hashes; costs metered |
| Eval | Fixtures, ScriptedProvider, holdout gates |

**MVP thesis bar:** holdout borrow/local-diagnostic repair compile success with inspectable DAG + decision log + BYOM, under sandbox—**without** claiming token-savings percentages.

---

## 7. Revised long-term architecture (concise)

Same control plane, filled in:

| Component | Long-term role |
| --- | --- |
| Planner (LLM) | Goal → DAG under single writer; versioned replans |
| Scheduler | Parallel Analyze; optional leases when eval warrants |
| ProjectGraph | Confidence-scored edges (calls/impls); richer queries; incremental |
| Context Engine | Additional domains as measured; cache-stable packs |
| Capabilities | Multi-impl scoring; specialized Rust workers (miri/unsafe/cargo) |
| Semantic Editing | RA-backed ops; IR primary for supported transforms |
| MCP Platform | Selective out-of-process servers; community MCP behind broker |
| Model Router | Multi-provider scoring after measured misroutes |
| LanguageBackend | Second language after Rust dogfood |
| Observability | Optional TUI; richer traces |
| alloyd / ACP | Only if single-binary or IDE partner demand is measured |
| External Memory | Curated, precision-gated—not auto injection |

---

## 8. Components removed from MVP (retained long-term)

- LLM Planner as default topology author  
- Typed call/lifetime graph layers  
- SemanticEditOp lowering (except optional RenameType)  
- Multi-impl capability scoring  
- Extra context domains + embeddings  
- Custom MCP server fleet (crates/git/rustdoc/…)  
- Benchmarking / UnsafeAudit / CargoManagement / Documentation / ArchitectureReview workers  
- File leases, Hint edges, priority function  
- OverlayFS product / alloy snapshot bundles  
- alloyd daemon  
- ACP adapter  
- External Memory auto-retrieve  
- Postgres; OTel as separate crate; Language plugins beyond Rust  
- Numeric cost differentiators in marketing tables  

---

## 9. Components eliminated entirely (from normative design as stated)

- Worker `follow_up_nodes` as DAG mutation channel  
- Dual graph access (Arc mutate + MCP graph_query for builtins)  
- VerifyCompile/Testing as LLM Capabilities  
- Normative appendices BL–DB (or relocate non-normative)  
- Unmeasured §1.3/§18 token-savings percentages as architecture proof  
- Week-1 scaffold of ~18 empty crates  
- Dogfood-before-sandbox schedule  
- Classification stamps without prior-art deltas  
- Principle 3.5 as “semantic-only writes” absolute (reframed, not kept as written)

---

## 10. Milestone sketch (replaces weekly gantt fiction)

| Milestone | ~Duration | Falsifiable thesis |
| --- | --- | --- |
| **M1 — Control plane** | 6–8 weeks | Sandboxed `tool → model → patch → check → log` on hardcoded DAG beats naive baseline on holdout local diagnostics |
| **M2 — Intelligence thin** | 6–8 weeks | Graph projections + Repair/Edit/Review capabilities improve holdout success/cost *or* clearly measure why not |
| **M3 — Semantic path** | 6–8 weeks | ≥1 RA-backed semantic op + optional LLM planner gated by eval; still single DAG writer |

0.1.0 = M1 complete + M2 started with honest eval. Not “everything in §5 diagram.”

---

## 11. Closing

The review correctly killed **ceremonial completeness**. It incorrectly treated **load-bearing thin interfaces** as the same disease. Alloy ships a **realistic MVP** (linear DAG templates, text patches, thin index, in-process MCP builtins, sandbox-first, eval-first) on **stable abstractions** that grow into semantic graph/editing and multi-capability routing **without a second architecture**.

Next artifact (not this document): a ≤30-page replacement RFC incorporating these decisions. The original RFC remains historical; this response is the governing architecture decision until that rewrite lands.

— arkadianet
