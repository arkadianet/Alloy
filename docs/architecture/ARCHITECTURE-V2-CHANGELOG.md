# Architecture V2 Changelog

| Field | Value |
| --- | --- |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **From** | `docs/architecture/ai-coding-harness-architecture-rfc.md` (v1) |
| **To** | `docs/architecture/alloy-architecture-v2.md` (canonical) |
| **Decision basis** | `docs/architecture/rfc-architect-response.md` |
| **Stub** | `docs/architecture/ai-coding-harness-architecture-v2.md` → points to canonical |

Section-by-section delta (v1 → V2). Not a redesign narrative—implementation-contract changes only.

---

## Document identity / branding

- Title category: **AI Coding Harness** → **AI Engineering Runtime**
- Product never called “Harness” in V2 body
- Mission statement + four pillars (Intelligence / Execution / Knowledge / Tooling) added as framing
- Mental model: **Runtime → Scheduler → Capability Workers** (Planner reframed under runtime; not deleted)
- Differentiation pillars: compiler-aware, execution-first, model-agnostic
- Canonical path: `alloy-architecture-v2.md`; harness-named path is stub only

## §0 (new)

- Decision application summary (18 Accept / 10 Partial / rejected kill-list remedies)
- Revised MVP / long-term / removed-from-MVP / eliminated lists
- MVP thesis bar; subsystem pattern mandate

## §1 Executive Summary

- Reframed as engineering runtime; models as plugins
- Differentiators: stripped numeric cost claims; EditEngine envelope; sandbox/eval emphasis
- Normative boundary stated (§§0–21 + A–E)

## §2 Survey

- Collapsed to gap table + borrow/avoid (F-28); full survey non-normative

## §3 Principles

- §3.5 reframed: prefer semantic when available; **TextPatch first-class** (F-01)
- Cost principle: metering yes, unmeasured % no (F-08)

## §4 Problem Analysis

- P0 narrowed to **locally editable diagnostics**; lifetime stretch (F-26)

## §5 High-Level Architecture

- Mermaid: single-binary; four pillars; no alloyd/ACP/External Memory in topology (F-05, F-27)
- Crate layout → **≤5 crates** (cli/runtime/tools/index/eval)
- Added **RunController** (F-22); Session lifecycle-only
- Day-1 slice vs product architecture clarified

## §6 DAG & Scheduler

- Hardcoded templates MVP; linear `max_parallel=1` honesty (F-16)
- Deleted `follow_up_nodes`; single Replan writer (F-03)
- Git-only checkpoints (F-24)
- Hint/leases/priority deferred
- Verify/Test/GateHuman = runtime kinds (F-10)

## §7 Project Intelligence Graph

- Trait kept; thin metadata+syn+diagnostics/fixes (F-02)
- Single in-process read; ingest-only writes; no worker `graph_query` MCP (F-04)
- SimilarFixes/auto memory deferred (F-23)

## §8 Context Engine

- Three live domains only; embeddings out (F-12)
- Domain enum may reserve future IDs as empty stubs

## §9 Capability System

- Registry kept; ≤4 LLM caps (F-13)
- Output: no topology mutation / no worker graph_mutations

## §10 Workers

- Repair / Edit / optional Review / template Planning only
- Deferred catalog workers removed from 0.1.0 schedule
- Runtime adapters section for verify/test/gate

## §11 Model Router

- Trait kept; MVP = TOML tier map + one openai-compatible provider (F-20)
- Multi-factor scoring deferred

## §12 MCP Platform

- Host kept; in-process builtins; 0–1 external servers (F-09)
- Custom server fleet deferred; `graph_query` removed for Alloy workers

## §13 Semantic Editing

- Renamed emphasis to **EditEngine**; `EditRequest { TextPatch | SemanticOps }` (F-01, F-14)
- MVP TextPatch + git; SemanticEditOp unstable/fail-closed
- OverlayFS / dual edit stack deleted from MVP

## §14 Security

- Sandbox-before-dogfood mandatory (F-07)
- Quarantine defaults; community MCP deferred

## §15 Observability

- Metadata+hashes default; prompts/bodies opt-in (F-17)
- TUI deferred

## §16 Language Plugins

- Trait kept; Rust-only internal module; no PY/TS/cdylib (F-15)
- Trait freeze after dogfood, not ceremony week

## §17 Evaluation

- Fixtures + ScriptedProvider from week 1; holdout gates (F-19, F-25)
- Correct falsification target (control plane, not “text-diff or stop”)

## §18 Cost Model

- Budgets/metering kept; numeric marketing tables stripped (F-08)

## §19 Roadmap

- Three milestones × 6–8 weeks replace 26-week weekly fiction (F-06)
- Weekly slices still within milestones, V2-scoped
- Dogfood banned until sandbox+holdout
- 0.1.0 = M1 complete + M2 started

## §20 Risk Register

- Owners → arkadianet humans; CODEOWNERS before M1 (F-18)
- Risks adjusted for V2 (sandbox timing, thin graph, scope); no invented new risk themes

## §21 Final Review

- Checklist updated for V2 commitments
- Day-1 build: 5 crates + sandboxed vertical slice

## Appendices

- **Normative:** A–E only (session events, profile TOML, DAG state machine, Diagnostic/FailureIr, PermissionToken) — updated for V2 defaults
- **Relocated / non-normative:** F–DB and survey ballast (F-21, F-28); not reproduced in V2

## Explicit non-changes (rejected kill-list remedies)

Interfaces **kept** (thinned): Task DAG, ProjectGraph, Capability registry, MCP host, EditEngine, ModelRouter, LanguageBackend. §21.4 is day-1 slice, not forever architecture.
