# Alloy RFC Design Review

| Field | Value |
| --- | --- |
| **Subject** | `docs/architecture/ai-coding-harness-architecture-rfc.md` (~4822 lines, §§1–21 + appendices) |
| **Reviewer stance** | Principal engineer / design review — find what is wrong; recommend deletions aggressively |
| **Date** | 2026-07-25 |
| **Implementation status** | Not started |
| **Interactive deliverable** | Cursor Canvas: [rfc-design-review.canvas.tsx](/home/rkadias/.cursor/projects/home-rkadias-coding-development-arkadianet-alloy/canvases/rfc-design-review.canvas.tsx) |
| **Finding counts** | **8 Critical · 12 High · 6 Medium · 2 Low** |

---

## Verdict

**Revise heavily. Do not implement this RFC as written.** Overall risk: **Very High**.

The thesis is sound: Rust correctness needs compile-gated, inspectable control flow plus BYOM—not a smarter chat dump. What surrounds that thesis—15 components, ~18 crates, a typed multi-layer Project Intelligence Graph, a Semantic Editing IR as the only write path, an 11-capability worker catalog, seven custom MCP servers, an 8-domain Context Engine, LanguageBackend plugins, alloyd, and ~80 appendices after an “End of RFC”—is design theater that will burn hundreds of hours before the only claim that matters is falsified. The buried §21.4 day-1 slice is the real architecture; the rest of the document argues against itself via self-critique notes that were never enforced on scope. Shipping the full topology is higher risk than shipping nothing.

---

## Kill List (deletions)

Prefer deletion. Replacements are strictly thinner.

1. **Semantic Editing Engine IR (§13) as MVP write path** — Research program; contradicts Week 8 text edits; couples every write to RA/graph. **Replace:** unified diff apply + `cargo check`; optional RA rename later.

2. **Typed multi-layer Project Intelligence Graph (§7)** — Compiler-frontend scope; savings hypothesis unproven; incremental call graphs fragile with macros. **Replace:** `cargo metadata` + syn symbols + diagnostic/fix event store; live RA queries for refs/impls.

3. **Custom MCP server fleet (§12.3 × ~7)** — Ops/security surface; fights the document’s own schema-tax thesis. **Replace:** in-process cargo/test + fs read; one optional ra MCP.

4. **Capability registry + 11 workers (§9–10)** — Over-abstraction; multi-impl scoring unused with one impl each; planner hallucination surface. **Replace:** ≤4 procedures; verify/test as deterministic runtime nodes.

5. **8-domain Context Engine (§8)** — Second brain; stale-summary risk (R1); APIs encode stubbed domains. **Replace:** Conversation + WorkingSet + Artifacts.

6. **LanguageBackend / plugins / Python·TS sketches (§16)** — Too early; freezes the wrong trait; cdylib security without users. **Replace:** internal rust module only for 6+ months.

7. **alloyd + ACP + External Memory embeddings** — Non-thesis scope; doubles ops; memory is an injection channel. **Replace:** nothing until single-binary limits hurt on real repos.

8. **LLM Planner before linear repair works (§19 W10)** — DAG sketch quality untestable; interacts badly with `follow_up_nodes`. **Replace:** hardcoded repair pipeline template; planner only after ≥60% holdout eval.

9. **Appendices BL–DB after Appendix BK “End of RFC”** — Normative fog; maintenance theater; contradiction multiplier. **Replace:** separate non-normative handbook or delete.

10. **Numeric cost differentiators (§1.3 / §18)** — Unmeasured opinion sold as architecture. **Replace:** budgets + metering only; publish numbers after Eval calibrates.

11. **File leases, Hint edges, priority function (App R / AH)** — Parallelism mostly illusory while `max_parallel_cargo=1` and `max_parallel_edits=1`. **Replace:** linear scheduler; optional parallel analyze later if eval uplift appears.

12. **Benchmarking / Documentation / ArchitectureReview / UnsafeAudit / CargoManagement capabilities** — Dilute P0; schedule miri/containers before repair quality exists. **Replace:** human + cargo CLI until eval plateaus.

---

## Severity-ranked findings

### Critical

#### F-01 — Semantic IR as product identity vs admitted MVP
- **Location:** §3.5, §13, §19.2 Week 8, §13.7
- **Problem:** Principle 3.5 makes the Semantic Editing Engine the only structured write path, while Week 8 ships gated text edits and §13.7 admits `SplitCrate` / `ExtractTrait` / `MoveModule` are research stubs.
- **Why it will hurt:** Months on AST lowering that rust-analyzer already partially owns; op coverage gaps stall borrow-repair; `ReplaceBody` becomes the real path and the IR is dead weight within 12 months.
- **Fix:** Delete `SemanticEditOp` as MVP requirement. Ship diffs + check. Revisit IR only after eval proves text edits are the bottleneck.
- **Classification:** Original was overclaimed (IR as harness identity vs production-proven RA assists).

#### F-02 — Project Intelligence Graph scoped as a compiler frontend
- **Location:** §7, §7.3–7.4, App AV / CO
- **Problem:** Typed multi-layer graph (items, calls, lifetimes, cfg, diagnostic lineage, SimilarFixes) with Merkle incremental invalidation is not a harness index—it is a program-analysis product.
- **Why it will hurt:** Perpetual “degraded mode,” wrong call edges, macro holes, R16 rust-analyzer skew. §18 cost savings depend on accuracy that will not exist.
- **Fix:** Persist metadata + syn + diagnostics/fixes only; query RA live. Defer incremental typed call graphs past 0.1.0.
- **Classification:** Original proposal overstated; Aider map is the proven ancestor—the jump is too large.

#### F-03 — Multiple DAG topology writers
- **Location:** §6.6, §9.2 `CapabilityOutput.follow_up_nodes`, App CP
- **Problem:** Planner replan, worker `follow_up_nodes`, and Scheduler apply/cancel can all reshape the DAG. No single authority; “validate acyclic” is not a conflict protocol.
- **Why it will hurt:** Oscillating repair loops, duplicate nodes across generations, non-reproducible runs, undebuggable provenance.
- **Fix:** Delete `follow_up_nodes`. Workers return failure IR; only one Replan/Planner service mutates topology.

#### F-04 — Dual graph access + unclear mutation ownership
- **Location:** §9.2 `Arc<dyn ProjectGraph>` + `graph_mutations`; §12.3.3 `graph_query`; App Z
- **Problem:** Workers hold a direct graph handle and also reach graph via MCP. `GraphMutation` application owner is missing from the ownership matrix.
- **Why it will hurt:** Stale reads during incremental rebuild, divergent views, corruption under parallel Analyze—R6 becomes inevitable.
- **Fix:** One read path (in-process). Graph writes only via Graph service ingest—never worker JSON blobs. Delete builtin `graph_query` MCP.

#### F-05 — Component / crate topology before a working loop
- **Location:** §5.1–5.4, App BV
- **Problem:** ~15 components and ~18 crates (including daemon, ACP, External Memory, OTel, lang plugins) peer Session/MCP/Router before any repair loop works.
- **Why it will hurt:** Interface churn, empty-lib CI tax, glue bus-factor—OpenHands V0 complexity the RFC claims to avoid.
- **Fix:** ≤5 crates for 3 months (`cli`, `runtime`, `tools`, `index`, `eval`). Split crates only when compile times or ownership force it.

#### F-06 — Roadmap honesty failure
- **Location:** §19, §19.7 gantt, App AJ, §21.4
- **Problem:** “Weekly vertical slices” vs Alloy-on-Alloy dogfood after Week 8 (before DAG/planner/semantic edits/sandbox); gantt gives Freeze ~28 days after eval; §21.4 thin day-1 contradicts §5.4 full skeleton.
- **Why it will hurt:** Theater greens; security/eval lag; 26-week 0.1.0 is fiction for this scope.
- **Fix:** Three milestones × 6–8 weeks, one falsifiable thesis each. Ban dogfood until sandbox + compile-gated repair exist. Do not scaffold 18 crates in week 1.

#### F-07 — Critical sandbox threats scheduled after dogfood
- **Location:** §14.1, §14.3, §14.8, Week 3 vs Week 17
- **Problem:** `build.rs` / proc-macro RCE is Critical in the threat model, yet MCP lands Week 3 with stub sandbox and container broker is Week 17—after App AJ dogfood.
- **Why it will hurt:** Highest-risk workspaces run first; supply-chain / confused-deputy incidents before 0.1.0 are plausible.
- **Fix:** Sandbox before dogfood. Quarantine defaults for network/deps. Defer community MCP until broker enforces allowlists.

#### F-08 — Cost model used as architecture proof
- **Location:** §1.3, §18, App AB
- **Problem:** 30–60% token savings and $0.05–$4 bands appear as differentiators while §18.5 admits unmeasured opinion. Claude subscription comparison is a category error vs BYOM API metering.
- **Why it will hurt:** Team builds graph/cache complexity for numbers that may not appear; stakeholders expect cost wins DAG overhead may erase.
- **Fix:** Strip numeric claims from §1/§18 until Eval calibrates. Keep budgets + tiers only.
- **Classification:** Emerging economics overclaimed as design justification.

---

### High

#### F-09 — MCP custom server sprawl
- **Location:** §12, App BL, §2.5
- **Problem:** Thesis warns about schema tax, then specifies ~7 custom servers plus wraps. Lazy selectors cut tokens, not process/ops/security surface.
- **Fix:** In-process cargo/test + fs; optional ra. Delete crates/git/rustdoc servers until needed.

#### F-10 — VerifyCompile dual-modeled
- **Location:** §6.2 `NodeKind` vs App BM Capability
- **Problem:** Same step is both a scheduler primitive and a Capability (possibly near ModelRouter).
- **Fix:** Verify/Test/GateHuman are runtime kinds only—not LLM capabilities.

#### F-11 — Classification label inflation
- **Location:** §1.3, §2.3, §6.1, §7.1, body-wide
- **Problem:** “Production proven” for Cursor Merkle used to bless typed graphs; “Original proposal” on synthesis (DAG+gates, capability registry, IR). App CU is more honest than the body.
- **Fix:** Relabel honestly in-line with named prior-art deltas. Ban orphan “Original proposal” stamps.
- **Classification:** Production proven / Original was overclaimed.

#### F-12 — Context domain proliferation
- **Location:** §8, App B, App BJ
- **Problem:** Eight normative domains with weights/compaction; self-critique stubs half; APIs still encode all eight.
- **Fix:** Three domains only until measured need.

#### F-13 — Capability/worker over-catalog
- **Location:** §9–10, §9.5 vs BM
- **Problem:** Eleven full worker specs; “MVP cut” still ~7; multi-impl registry unused.
- **Fix:** Plan (optional) / Repair / Edit / Review. Testing = tool node. Delete the rest from 0.1.0.

#### F-14 — Hidden write-path coupling
- **Location:** §13 + §7 + §14 + App AP.4
- **Problem:** Edit → Graph anchors → RA → MCP edit → Overlay → Sandbox check → Checkpoint with no graceful degraded mode except gated `ReplaceBody`.
- **Fix:** model → patch → apply → check. One checkpoint (git). Optional RA. Delete dual edit server/crate split.

#### F-15 — Language plugin system too early
- **Location:** §16, Phase 5, App CV
- **Problem:** Trait freeze Week 23; PY/TS sketches; cdylib discussion before Rust path proven.
- **Fix:** Internal rust module only; revisit LanguageBackend after 6 months Rust dogfood.

#### F-16 — Parallelism claims vs serial cargo/edits
- **Location:** App AH vs §6.1
- **Problem:** `max_parallel_cargo=1`, `max_parallel_edits=1` serialize the expensive stages; DAG “parallelism” mostly helps cheap Analyze nodes.
- **Fix:** Admit linear MVP; defer Hint edges, priority function, file leases.

#### F-17 — Observability / prompt retention defaults
- **Location:** §15, App BR / CM, §15.5
- **Problem:** Always-on decision logs; optional full prompts; incomplete redaction; 14-day default retains sensitive code.
- **Fix:** Metadata+hashes default; full prompts opt-in per session; do not retain file-body tool results by default.

#### F-18 — Ownership theater
- **Location:** §20, §20.1, App Z, Week 26
- **Problem:** Owners are component labels; “TPM/architect (arkadianet)”; CODEOWNERS deferred; R10 accepted.
- **Fix:** Name humans or drop Owner column. CODEOWNERS before Phase 1.

#### F-19 — Eval too late relative to claims
- **Location:** §17 vs §18 vs W8 ≥40%
- **Problem:** Meaningful eval at Week 20; early thresholds without holdout invite R15 overfitting while cost claims ship earlier.
- **Fix:** Fixtures from week 1; holdout gates every phase exit; no cost marketing until calibrated.

#### F-20 — Model Router over-engineered early
- **Location:** §11, App CN
- **Problem:** Normative multi-factor scoring, residency, health bonuses before a second provider exists.
- **Fix:** TOML capability→tier map only; scoring after measured misroutes across ≥2 providers.

---

### Medium

#### F-21 — Document sprawl past “End of RFC”
- **Location:** App BK then BL–DB
- **Problem:** Density treated as completeness; normative boundary fails.
- **Fix:** Cap at §§1–21 + A–E; move the rest out of normative RFC.

#### F-22 — Missing RunController interface
- **Location:** §5.5 vs App CJ
- **Problem:** SessionService is “primary control API” but start/cancel/replan live elsewhere without a clean Run boundary.
- **Fix:** `RunController { start, cancel, approve, replan }` owned by Scheduler; Session owns lifecycle/events/budgets only.

#### F-23 — SimilarFixes / External Memory premature
- **Location:** §7 SimilarFixes, App AG
- **Problem:** Auto-retrieved fix memory without transfer conditions; trust mis-tags risk injection.
- **Fix:** Successful patches → eval fixtures, not auto context, until precision measured.

#### F-24 — Triple checkpoint story
- **Location:** §6.4, §12.3.6, App AP.4
- **Problem:** Git stash + alloy snapshot bundles + overlay FS.
- **Fix:** One backend (git) for MVP.

#### F-25 — Design poorly hermetically testable
- **Location:** App AL, App CR/CS
- **Problem:** Thesis invariants need live rustc/MCP/models; unit tests cover serde/DAG shape only.
- **Fix:** ScriptedProvider + recorded cargo JSON fixtures from day 1; thesis tests offline.

#### F-26 — P0 lifetime goals vs stubbed ops
- **Location:** §4 vs §13 / App AQ
- **Problem:** P0 lifetime repair and `lifetime_shorten` strategies lack MVP lowering (`AddLifetime` not in admitted MVP set).
- **Fix:** Narrow P0 to locally editable diagnostics, or delay lifetime claims until RA-assisted ops exist.

---

### Low

#### F-27 — alloyd still designed as optional peer
- **Location:** §5.3, App BU, Week 25
- **Fix:** Delete from body until single-binary p95 fails on real repos.

#### F-28 — Survey length as normative ballast
- **Location:** §2 (~200 lines), App W thin sources
- **Fix:** Separate survey doc; keep one-page gap table in RFC.

---

## Contradictions & consistency failures

| Claim A | Claim B | Failure |
| --- | --- | --- |
| §3.5 semantic-only writes | §19 W8 text edits; §13.7 stubs | Identity principle vs admitted MVP path |
| §5.4 scaffold ~18 crates | §21.4 no planner/graph/daemon day 1 | Skeleton vs thin vertical slice |
| §6.1 parallel DAG benefits | App AH cargo/edit parallelism = 1 | Sold parallelism vs serialized reality |
| §12 schema-tax avoidance | §12.3 ~7 custom servers | Token policy vs process sprawl |
| §7 “Original” typed graph | App CU Aider ancestor; degraded syn mode | Novelty claim vs fallback ≈ prior art |
| §14.1 build.rs Critical | W3 MCP + stub sandbox; W17 broker | Threat severity vs schedule |
| App AJ dogfood after W8 | Sandbox/semantic/DAG incomplete | Dogfood before trustworthiness |
| §4 P0 lifetimes | MVP ops omit `AddLifetime` lowering | Goals vs admitted capabilities |
| `NodeKind::VerifyCompile` | Capability `VerifyCompile` (BM) | Dual model of same step |
| §18 cost bands in summary | §18.5 “not SLAs” / opinion savings | Marketing vs disclaimer |
| App BK “End of RFC” | App BL–DB continue | Document boundary failure |
| §9.5 cut Benchmarking etc. | §10 full specs + W18/W22 builds | Stub rhetoric vs scheduled build |
| Worker `follow_up_nodes` | Planner owns DAG (§5.2) | Ownership conflict |
| Graph via Arc + MCP | Explicit state / single truth (§3.3) | Two truth channels |
| Mermaid §5 includes ACP/daemon | §5.7 / BH “optional / deferred” | Diagram still sells deferred surface |
| Week 5 graph from syn | Week 14 “first-class” RA for RenameType | Graph claimed useful before RA anchors edits |

Spot-checks performed: component diagram (§5) vs crate list vs App Z ownership; sequence §6.5 vs capability catalog; MCP tool schemas vs security §14; cost §18 vs differentiators §1.3; gantt §19.7 vs weekly deps; App BM VerifyCompile vs §6.2 NodeKind; App BK “end” vs continued appendices.

---

## What is actually good

1. Thesis is coherent: Rust needs compile-gated loops, not bigger chats.
2. BYOM with no hardcoded model IDs in core is the right self-hosted default.
3. Fail-closed permissions and “no raw bash” default are correct instincts.
4. Lazy MCP disclosure is the right reaction to schema tax.
5. Event log / resume / budgets as control-plane state beat transcript archaeology.
6. §21.4 day-1 slice (`tool → model → edit → check → log`) is the real RFC—buried under theater.
7. Eval-as-gate culture (§17) is necessary if kept honest and early.
8. Self-critique notes correctly name several traps—they were not enforced on scope.

---

## Day-1 build recommendation under this critique

### Build for ≤3 months (thesis validation)

Single binary:

1. Session event log (SQLite append-only)
2. One openai-compatible `ModelProvider`
3. Apply unified diff / text edit
4. `cargo check --message-format=json` via in-process tool facade (sandbox on)
5. Linear retry/repair loop with budgets + decision log
6. Thin index: cargo metadata + syn symbols + diagnostic store
7. Fixtures + `ScriptedProvider` from week 1
8. Hardcoded repair pipeline template (no LLM planner)

**Validation bar:** holdout borrow-repair compile success with costs logged. If text-diff + check cannot beat a naive agent baseline, **stop**—graph/IR will not save the thesis.

### Do NOT build for 3 months

Semantic IR · typed call graph · alloyd · ACP · LanguageBackend · capability multi-impl registry · 8 context domains · External Memory embeddings · custom crates/git/rustdoc MCP · Planner LLM · Observability TUI · Benchmarking/UnsafeAudit/CargoManagement workers · file leases · OverlayFS product · Postgres · Python/TS backends · Hint edges / fancy priority · seven MCP servers · 18-crate skeleton.

### If kill list accepted: thinnest vertical slice that still validates DAG + graph + MCP + BYOM

- **DAG:** hardcoded 3–5 node template (analyze → edit → verify → gate), not a planner product.
- **Graph:** metadata + symbols + diagnostics only (not typed calls/lifetimes).
- **MCP:** host with ≤2 servers or in-process builtins with lazy disclosure.
- **BYOM:** one openai-compatible provider + tier labels in TOML.

That is enough to falsify or support the thesis without inventing a compiler.

---

## Recommended RFC disposition

| Action | Target |
| --- | --- |
| **Keep / sharpen** | Thesis, BYOM router basics, permissions fail-closed, event log, compile gates, §21.4 slice, eval culture |
| **Rewrite** | §5 topology, §7 graph MVP, §9 catalog, §12 MCP surface, §19 roadmap |
| **Demote to research backlog** | §13 IR, §16 plugins, alloyd, ACP, SimilarFixes memory, OverlayFS product |
| **Delete from normative RFC** | App BL–DB (or all post-CJ), numeric §18 marketing tables, classification stamps without prior-art deltas |

**Do not edit the original RFC in place for a “soft” fix.** Produce a short Architecture Decision: “MVP = §21.4 + kill list,” then a ≤30-page replacement RFC. The current document is a poor implementation contract precisely because it tries to be a complete one.
