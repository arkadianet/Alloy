# RFC-0017: Dynamic Planning & Repair Generations

| Field | Value |
| --- | --- |
| **Status** | Draft |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0003](./RFC-0003-session-manager-run-controller.md) (merged), [RFC-0004](./RFC-0004-observability-cost-metering.md) (merged), [RFC-0009](./RFC-0009-task-dag-templates-planner.md) (merged), [RFC-0010](./RFC-0010-scheduler-runtime-adapters.md) (implemented), [RFC-0013](./RFC-0013-capability-registry-workers.md) (implemented), [RFC-0015](./RFC-0015-cli-profiles-config.md) (implemented), [RFC-0016](./RFC-0016-eval-harness-holdout-gates.md) (implemented — gate machinery) |
| **Effort** | 9–13 person-days (§17; revised upward from 6–9 by the 2026-07-29 audit response) |
| **Revision** | Rev 2 — external audit response, 2026-07-29. Disposition table in §18 |
| **Related RFCs** | [0007](./RFC-0007-model-router-provider.md) router/meter binding for the planning model call · [0012](./RFC-0012-context-engine.md) goal context for the planning prompt |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | The interim cross-run repair posture from issue #53 (pre-plan seed + operator re-runs) as the *primary* repair mechanism; it is retained as a complement (§5.6) |

**Mental model (V2 §6.2 upgrade path / §6.4 / ADR F-03):** V2's planner evolution is "swap template source for Planner behind same DAG schema … generation++ on replan with provenance." This RFC executes exactly that swap, twice over: (1) the **plan source** may now be an LLM proposal, compiled and clamped by the runtime, validated by the existing `DagValidator`, falling back to the template catalog fail-closed; (2) the **replan trigger** may now be automatic — a genuine verify `Fail` seeds a bounded generation bump whose new root actually *carries the failure diagnostics*, instead of the run dying at derivation rule D3 with the diagnostics stranded in a `failure_ir` artifact. The topology writer count stays exactly one: `PlanService`. The scheduler stays a single-generation executor (RFC-0010 RP4/B6); the generation loop lives *inside the control plane*, as the execution step of `RunController::start` (§3.8, §5.5) — **not** above it and **not** in the CLI. RFC-0015 SQ2 ("`RunController::start` is the only entry") therefore stands verbatim, and acceptance/completion events plus run-row terminalization stay single-sourced in RFC-0003 §6.3 no matter how many generations run. Workers still never mutate topology (RFC-0013 PW2 is retained verbatim).

**Authority order (highest → lowest):** current `main` source → merged RFCs 0001–0016 → Architecture V2 → this document → roadmaps. Where this RFC must change a merged RFC, the change is an explicit numbered amendment in §2.7 — nothing else in this document overrides merged text.

**Reading rules.** MUST / MUST NOT / SHOULD / MAY are normative. Tables are normative unless labelled *informative*. Every Rust block is a signature or a shape, not product code.

---

## 1. Overview

### 1.1 Purpose

Ship two coupled capabilities inside `alloy-runtime`:

1. **LLM-backed planning** — an `LlmPlanService` that drives the existing `PlanningWorker` (RFC-0013, id `planning`) to *propose* a linear task chain for an arbitrary goal, compiles the proposal into a `TaskDag` through a clamping **proposal compiler**, validates it with the existing `DagValidator`, and persists it through the existing RFC-0009 plan path. Any defect in the proposal — model unavailable, malformed payload, clamp violation, validation failure, budget denial, timeout — falls back to the template catalog, fail-closed, with an audited reason.
2. **Repair generations** — a `GenerationDriver` installed as `RunController::start`'s execution step (the `RunExecutor` seam of AM-0003-2, replacing the bare `RuntimeHandle::run_dag` call at RFC-0003 §6.3 step 8): when a run's DAG fails at a `VerifyCompile` node with a *genuine* `Fail` verdict (`ErrorClass::Compile` with diagnostics — never an Inconclusive/transient classification), the driver bumps the generation in place and re-dispatches, and — the load-bearing fix — the planner **seeds the `ReplanReason::FailureIr` into generation N+1's root input envelope** so the next Analyze actually reads the rustc errors that killed generation N. Only the *final* generation's `DagOutcome` reaches §6.3 step 10, so exactly one `RunAccepted` / `RunCompleted` / `RunFinished` triple and exactly one terminal row write happen per run. The loop is bounded by `RuntimeConfig.max_repair_generations` (profile `[limits]`, default **2**) and by one **absolute** run deadline shared across generations (AM-0010-2).

### 1.2 Problem statement

The 2026-07-29 live-model dogfood data (issue #53; commits `443bf16` #52 and `1896934` #53) measured the day-1 posture end to end:

- **Runs die with their best evidence in hand.** After #52's `cargo_exit_verdict` fix, a wrong model fix produces an *honest* verify `Fail`: the scheduler builds a `FailureIr` whose `diagnostics` carry the exact rustc errors (F2/F3), CASes the node `Failed`, derives `DagState::Failed` (rule D3), and returns. The run is over. The diagnostics survive only inside the `failure_ir` artifact and the `NodeState` event — nothing consumes them. The operator re-runs from scratch and the model guesses again. Measured cost: trivial-repair pass rate ~1/5 before #53's levers, 3/5 after, with **every remaining failure an honest model-side compile `Fail`** — i.e. exactly the class of failure a seeded second generation is built to convert.
- **The production replan path cannot carry the failure.** `PlanService::replan` exists and bumps the generation, but `TemplatePlanService::instantiate_and_persist` drops the `ReplanReason` before Phase B: `put_input_artifacts(manifest, ids, ctx, generation)` takes no reason parameter, so generation N+1's root node gets the bare `Goal` again — payload-identical to generation 1. The failure IR reaches only the `PlanProduced` event's `reason` field, which no worker reads. The `scheduler_repair_e2e` test proves seeded generation 2 works — by *hand-crafting* a `FromPredecessors` envelope around a synthetic verify predecessor, explicitly standing in for "RFC-0009's not-yet-built auto-replan." Production must do what the test fakes.
- **Nothing drives the loop.** `RunController::request_replan` records intent; `RunControlState::ReplanRequested` is currently a dead-end (no API transitions a run out of it); the scheduler is forbidden from calling the planner (RFC-0010 RP4, B6); the CLI is forbidden from containing retry logic (RFC-0015 B1). Replan is therefore an externally-requested path with no production caller.
- **Template-only planning caps the goal space.** `TemplatePlanService::select` always returns `RepairLocalDiagnostic`. Goals that need a different shape — verify-first, review-before-gate, analyze-only — cannot be expressed, even though `DagValidator`, the node-kind contract table, and `PlanningWorker` already exist for exactly this.

### 1.3 Scope

| In scope | Detail |
| --- | --- |
| Proposal wire schema | `ProposedDagManifest` v1 — ordered linear chain, shape-only (§3.4) |
| Proposal compiler | Clamps + resource assignment + `DagValidator` final gate (§5.2) |
| `LlmPlanService` | `PlanService` impl: propose → compile → validate → persist, template fallback (§3.6, §5.1) |
| `PlanningWorker` v2 | Model-backed proposal per RFC-0013 PW5 amendment path (§5.3, AM-0013-1) |
| Replan seeding | `ReplanReason::FailureIr` → sanitized root `FromPredecessors` envelope (§5.4, AM-0009-2) |
| Validated persistence | `PlanPersistence` — one named, shared plan-write API for both plan services (§3.5b, AM-0009-6) |
| `GenerationDriver` | Bounded auto-replan loop **inside** `RunController::start` (§3.8, §5.5, AM-0003-2) |
| Config | `RuntimeConfig.max_repair_generations`; `[planner]` profile table (§7) |
| Control plane | `RunExecutor` seam (AM-0003-2); `begin_repair_generation` / `complete_repair_generation` / `control_state` (AM-0003-3); `resume_after_replan` (AM-0003-1) |
| Run deadline | One absolute deadline across generations: `Scheduler::run_within` (AM-0010-2) |
| Observability | `Replan` / `PlanProposal` decision records; `PlanProduced.source` (§9) |
| Security | Proposal containment rules SEC1–SEC10 (§10) |
| Migration | Interim driver loop → in-run generations (§5.6, MG1–MG7); dependency honesty vs unmerged #54 (§2.6, IN1–IN4) |

### 1.4 Non-goals

| Deferred item | Owner / disposition |
| --- | --- |
| Non-linear proposals (fan-out, explicit edges, `Aggregate`, `Plan` nodes in DAGs) | Deferred until a concurrency RFC lifts V15; proposal schema is a chain by construction (§3.4) |
| LLM planning as any profile's **default** | Eval-gated (V2 §0.4/§19.3, RFC-0016 holdout) — this RFC ships it **opt-in** (§7.1) |
| Seeded **re-proposal** on replan (LLM re-plans generation N+1's topology from the failure) | Open Question §16.1; day-1 repair generations reuse the prior plan source (§5.5 GN7) |
| Worker-proposed nodes / `follow_up_nodes` | **Eliminated** (ADR F-03, V2 §0.8) — MUST NOT reintroduce |
| Cross-process/durable generation loop (survive host crash mid-loop) | Deferred; crash resume yields the existing single-generation semantics (§6.3) — the driver reconstructs no in-flight loop |
| `VerifyTest`-triggered repair generations | Deferred: RFC-0010 **DG7** requires `McpVerifyTestAdapter` to return an **empty** `diagnostics` vector, so GN4 can never admit a test failure. Enabling it requires an explicit amendment to DG7 (a structured test-failure IR), which this RFC does not make (§16.5) |
| Cache interaction with proposed DAGs | `enable_cache` forced false (PC10); cache remains RFC-0009/0010 deferred |
| New crates, Postgres, Temporal durability, `.env` writes | Forbidden |

### 1.5 Day-1 MVP (normative)

1. `LlmPlanService` MUST implement `PlanService` and MUST be constructed only when profile `planner.mode = "llm"`. After the RFC-0016 / §12.4 stack-driver holdout gate (template vs llm non-inferiority on local-diagnostic fixtures), shipped `default` and `autonomous` profiles use `mode = "llm"`; `readonly` stays `mode = "template"` and rejects `llm` at assembly.
2. A model proposal MUST pass the compiler clamps PC1–PC14 **and** `DagValidator::validate` with `ValidateOpts::default()` (linear + gates) before persistence; any rejection MUST fall back to `TemplatePlanService` with a `PlanProposal` decision record naming the reason (FB1–FB7). Fallback MUST NOT fail the run.
3. The proposal is **shape-only**: the model chooses node names, kinds, order, and gate reasons. Capabilities, budgets, tiers, retries, timeouts, and cache flags are assigned by the compiler from the fixed table in §5.2.3, whose values are byte-identical to `crates/alloy-runtime/src/dag/templates.rs` on `main`. A proposal has no syntax with which to escape a capability allowlist or a budget ceiling (SEC1–SEC3).
4. **Verification is terminal, not incidental.** Every compiled proposal that contains an `Edit` node MUST place a verify node after the **last** `Edit` and before the terminal `GateHuman`, with no `Edit` between them (PC8). A human gate MUST NOT be reachable over an unverified edit — that is the `Constraint::RequireCargoCheck` / `[gates].require_cargo_check` posture (RFC-0015 PF7, `crates/alloy-runtime/src/config.rs:361`) expressed in topology.
5. Both plan services MUST write topology through exactly one named API, `PlanPersistence::persist_validated` (§3.5b, AM-0009-6). No caller may reach `DagStore::put_if_generation` / `replace_for_replan` with an unvalidated `TaskDag` (`DagStore` explicitly "MUST NOT run `DagValidator`", `crates/alloy-runtime/src/storage/dags.rs:81`).
6. That API MUST seed `ReplanReason::FailureIr` into the new generation's root input envelope per SD1–SD10, through the **sanitized** `SeedDiagnostic` projection (raw tool JSON dropped, byte-capped, secret-redacted). Replan with `ReplanReason::FailureIr` and a root that still receives the bare `Goal` is a defect (AC 17).
7. The `GenerationDriver` MUST auto-replan only on GN1–GN7 admission (`VerifyCompile` node, `ErrorClass::Compile`, non-empty diagnostics, bound not exhausted, run not cancelled, budget remaining, absolute deadline not expired). `Inconclusive`-class failures (`ErrorClass::Tool`, transient cargo errors per RFC-0010 §5.13.2) MUST NOT trigger a generation bump. `VerifyTest` is **excluded day-1** (RFC-0010 DG7 empties its diagnostics — §1.4, §16.5).
8. The bound is `RuntimeConfig.max_repair_generations` (default **2**, profile `[limits]`). It MUST NOT live on `SchedConfig`: the scheduler must never read it, and a knob on a struct the scheduler owns invites exactly that. `0` disables auto-replan entirely; the driver then degrades to exactly today's single-generation behaviour.
9. One **absolute** run deadline spans all generations. Each generation is dispatched with the *remaining* share of `RuntimeConfig.run_timeout` (AM-0010-2); `N` generations MUST NOT yield `N × run_timeout` of wall clock. Today's `run_started: Instant::now()` at `crates/alloy-runtime/src/scheduler/linear/loop_.rs:386` (R12) re-zeroes per invocation, which is the defect this rule closes.
10. The scheduler MUST NOT gain any planner dependency: RFC-0010 RP4 and B6 (CI-grep) remain in force. The generation loop lives in `alloy_runtime::driver`, not in `scheduler::*`.
11. The CLI MUST NOT gain retry or planning logic (RFC-0015 B1) **and MUST NOT gain a new execution entry point**: it keeps calling `runs.start(run)` exactly as RFC-0015 §7.1 step 7 specifies. SQ2 is unmodified. The only CLI-visible change is that the assembly constructs a `GenerationRunExecutor` instead of the default direct one (MG1).
12. `#![forbid(unsafe_code)]`; five-crate map unchanged; Alloy MUST NEVER write `.env`.

---

## 2. Architecture Integration

### 2.1 Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §6.2 upgrade path | "Enable Planner capability to emit DAGs validated acyclic; same store schema; generation++ on replan with provenance" — implemented verbatim: same `dag_blobs` schema, same `DagValidator`, same `PlanProduced` audit |
| §6.2 single topology mutator | Preserved. `PlanService` remains the only writer, now through one named API (`PlanPersistence`); the driver *requests*, the planner *writes*, the scheduler *executes*, the run controller *owns the lifecycle* |
| §6.4 replanning | Workers return `FailureIr` only; scheduler emits `ReplanRequired`/`Failed`; `generation++` versioned; no `follow_up_nodes` |
| §6.6 cycle prevention | Every proposed DAG passes acyclicity at plan/replan; dynamic edges only from ReplanService with validation |
| §0.4 / §9.3 / §19.3 | "LLM planner off until eval bar" — honoured: opt-in config, default-off, holdout gate §12.4 before any default flip |
| §10.2 PlanningWorker | Still the planning capability; gains its model call through the PW5-mandated amendment (AM-0013-1) |
| ADR F-03 / F-16 | No worker topology writes; linear honesty retained — proposals are chains |

### 2.2 Relationship to merged RFCs

| RFC | Reused | This RFC adds | Untouched |
| --- | --- | --- | --- |
| 0003 | `RunController::start` and its §6.3 state guards / §6.3 step-10 outcome mapping, `request_replan`, `ReplanReason::FailureIr`, `RunControlState::ReplanRequested` | `resume_after_replan` (AM-0003-1), the `RunExecutor` seam at §6.3 step 8 (AM-0003-2), in-run generation transitions + `control_state` read (AM-0003-3) | `submit_goal`, gate APIs, run event shapes, the §6.3 state-guard table, the acceptance/terminalization ordering (events-before-row) |
| 0004 | `DecisionLog::record`, decision record conventions (`prompt_body = None`) | `DecisionKind::{Replan, PlanProposal}` (AM-0004-1) | metering, budget events |
| 0009 | `PlanService`, `TemplatePlanService`, `DagValidator`, `ValidateOpts`, envelopes, `replace_for_replan`, `PlanProducedPayload` | seed rules SD1–SD10 (AM-0009-2), `PlanProducedPayload.source`/`proposal_artifact` (AM-0009-3), `PlanResult.source`/`proposal_artifact` (AM-0009-4), `LlmPlanService` replacing the `DisabledLlmPlanService` stub as the gated path (AM-0009-5), the shared `PlanPersistence` API (AM-0009-6), `PlanContext` provenance fields (AM-0009-7) | validation rules V1–V17, store CAS semantics, template catalog contents, `TaskDag`/`TaskNode` field shapes |
| 0010 | `Scheduler::run`, `DagOutcome`, `FailureIr` construction F1–F5, verdict classification §5.13.2, D1–D9 derivation, C1–C10 checkpoints | `Scheduler::run_within` for the shared absolute deadline (AM-0010-2) | RP1–RP5, B6, DG7, the entire loop; **D3 is unchanged** — a failed verify still yields `DagState::Failed`; the *driver* converts that outcome, the scheduler does not. **No `SchedConfig` change** (the bound lives on `RuntimeConfig`) |
| 0013 | `PlanningWorker`, `PlanningProposalPayload`, `CapabilityExecutor` seam, registry RG rules, worker budget rules BG1–BG4 | PW1/PW4 amended per PW5 (AM-0013-1), `PlanningProposalPayload.proposal` additive field (AM-0013-2), `planning` `side_effects` `Pure → ReadOnly` (AM-0013-3) | `CAPABILITY_CATALOG` (still exactly 4), PW2 (worker never writes a DAG), tool allowlists, `SideEffectClass`'s meaning |
| 0015 | assembly/composition root, profiles, §7.1 step ordering **including step 7 `runs.start(run)`** | `[planner]` profile table + `[limits] max_repair_generations` (AM-0015-2), B1 clarification (AM-0015-1) | flag surface, B1 itself, **SQ2 verbatim** (CLI still never calls `Scheduler::run` / `run_dag`) |
| 0016 | holdout gate machinery, `ScriptedProvider` | the planner-mode holdout comparison (§12.4) | harness APIs |

### 2.3 Already implemented | Added by RFC-0017 | Deferred

| Category | Contents |
| --- | --- |
| **Already implemented** | `PlanService::replan` + `replace_for_replan` generation CAS; `ReplanReason::FailureIr`; `request_replan`; `DeriveFlags.replan_requested` + D1 + RP1–RP5 + C10; `PlanningWorker` (deterministic) + `PlanningProposalPayload`; `FailureIr` with diagnostics (F2/F3); verdict honesty split Fail vs Inconclusive (#52); `scheduler_repair_e2e` proving seeded generation 2 converts a real `E0308`. **Not** implemented on `main`: pre-plan `seed_graph_diagnostics` / `bootstrap_diagnostics` ship on the **unmerged** PR #54 — see §2.6 |
| **Added by RFC-0017** | `ProposedDagManifest` + proposal compiler; `PlanProposer` seam + `CapabilityPlanProposer`; `LlmPlanService`; the shared `PlanPersistence` API and its replan seeding; `GenerationDriver` behind the `RunExecutor` seam; `resume_after_replan` / `begin_repair_generation` / `complete_repair_generation` / `control_state`; `Scheduler::run_within`; config knobs; decision records; amendments §2.7 |
| **Deferred** | Non-linear proposals; seeded re-proposal; durable generation loop; cache on proposed DAGs |

### 2.4 Dependency boundaries

```text
alloy-cli (0015 composition root)
        │  wires, never decides;  calls runs.start(run) — SQ2 unchanged
        ▼
alloy_runtime::session::RunController::start        [acceptance, lease, row state, RunAccepted/Completed/Finished]
        │  §6.3 step 8 seam (AM-0003-2)
        ▼
alloy_runtime::driver::GenerationDriver ──uses──► RuntimeHandle::run_dag_within (0003/0010) [one generation]
        │  (impl RunExecutor)            ──uses──► PlanService::replan (0009/0017)           [writes topology]
        │                                ──uses──► RunController control seams (AM-0003-3)
        ▼
alloy_runtime::planner::LlmPlanService ──uses──► PlanProposer ──► CapabilityExecutor ──► PlanningWorker (0013)
        │                              ──falls back to──► TemplatePlanService
        │                              ──persists via──► PlanPersistence (the single write path)
        ▼
dag::{validate, proposal, templates, io}   storage::{DagStore, ArtifactStore}   events::EventSink
```

| Consumer | MAY rely on | MUST NOT |
| --- | --- | --- |
| `driver` | the `DagOutcome` contract as returned by `RuntimeHandle::run_dag_within`; `PlanService` trait; the AM-0003-3 control seams | import `scheduler::linear` internals; mutate DAG or run rows directly; call `Scheduler::run` (it goes through the handle, preserving `try_admit_run` single-flight); call workers; emit `RunAccepted` / `RunCompleted` / `RunFinished` (§6.3 owns all three) |
| `session::run_controller` | the `RunExecutor` trait object it is constructed with | know that a `GenerationDriver` exists (it sees only `dyn RunExecutor`); duplicate the driver's admission logic |
| `scheduler` | nothing new | depend on `planner::*` or `driver::*` (B6 extended — CI grep, AC 40); read `max_repair_generations` (it is not on `SchedConfig` at all — AC 31) |
| `planner::LlmPlanService` | `CapabilityExecutor` seam; `dag::proposal`; `TemplatePlanService`; `PlanPersistence` | call `ModelRouter` directly (prompts live in workers — RFC-0013); bypass `DagValidator`; touch `DagStore` outside `PlanPersistence` |
| workers | `NodeInputPayload::FromPredecessors` seeded roots (SD5 shape) | `PlanService` (PW2 retained; CI grep T8 retained) |

**Cycle check.** `session` → `driver` (trait object) and `driver` → `session` (control seams) would be a module cycle if both were concrete. They are not: `RunExecutor` is defined in `session` and implemented in `driver`; `driver` depends on the `RunController` *trait*, also defined in `session`. Compilation direction is `session` ← `driver`; the assembly ties the knot with `Arc`s (§4.3).

### 2.5 Trust boundary

A proposal is **model output derived from untrusted goal text and untrusted repository content**. It crosses into the trusted plane only through the proposal compiler (§5.2) and `DagValidator`. Everything downstream (scheduler, workers, adapters) may continue to treat post-validate DAGs as impossible-to-be-malformed (RFC-0009 §3.3 fail-closed posture) precisely because the compiler assigns every security-relevant field itself (SEC1–SEC3).

### 2.6 Interim driver-loop posture (issue #53) — restated

The interim repair mechanism on the #53 line is: one pre-plan `seed_graph_diagnostics` pass (CLI `bootstrap_diagnostics` → `alloy_runtime::adapters::seed::seed_graph_diagnostics` → `ProjectGraph::record_diagnostic`, read back by the repair worker's `GraphQuery::Diagnostics`), plus operator-driven whole-run re-invocation. Any interim CLI-side *bounded retry loop* (fresh runs per attempt under a `--max-retries` style flag) is **not** part of the merged surface, violates RFC-0015 B1 as written, and is superseded by this RFC before it can merge (MG4). §5.6 specifies the migration.

**Dependency honesty (normative).** Neither `seed_graph_diagnostics` nor `bootstrap_diagnostics` exists on `main`; both ship on the **open, unmerged PR #54**. This RFC therefore MUST NOT be specified as depending on them:

| # | Rule |
| --- | --- |
| **IN1** | Every normative rule in this document MUST hold against `main` as it stands, with no graph-seeded diagnostics. The envelope channel (§5.4) is the sole diagnostics path this RFC requires. |
| **IN2** | MG2's retention of the pre-plan graph seed is **conditional**: it applies iff PR #54 merges. If #54 is dropped or reshaped, MG2 is void and nothing else in this RFC changes. |
| **IN3** | No acceptance criterion, test, or slice in §12/§14/§17 may reference `seed_graph_diagnostics`, `bootstrap_diagnostics`, or `--max-retries` as an existing symbol. |
| **IN4** | The `Depends on` header lists merged/implemented RFCs only. PR #54 is a *sequencing* relationship (MG4), not a dependency. |

### 2.7 Amendments to merged RFCs (normative)

Each amendment is additive unless marked otherwise and MUST land with this RFC.

**Numbering.** `AM-<rfc>-<n>` identifiers are **globally unique across `docs/rfcs/`**, not per-amending-document. RFC-0013 §2.7 already owns `AM-0009-1`, `AM-0010-1`, `AM-0012-1`, `AM-0012-2`, and `AM-0007-1` (and `AM-0009-1` is additionally cited from a doc comment in `crates/alloy-runtime/src/planner/template_service.rs`). This RFC's identifiers are allocated above those. The renumbering from the audited draft is recorded in §18.2.

| # | RFC | Amendment | Rationale |
| --- | --- | --- | --- |
| AM-0003-1 | 0003 | Additive `RunController::resume_after_replan(run: RunId) -> Result<(), RunError>` (§3.9), scoped to the **externally requested** replan path only. Preconditions: durable state `ReplanRequested` **and** no live execution lease for the run. Effect, in order: (a) verify the stored DAG's state is not `Running`; (b) upsert `RunControlState::Accepted` — *not* `Running`, so the run re-enters §6.3's normal `Accepted` re-dispatch arm and no second `RunAccepted` is emitted; (c) append `ReplanResumed` `{ "run_id", "generation" }` (`generation` = the stored DAG's generation). Idempotent from `Accepted` (returns `Ok(())` without a second event). Every other state, or a held lease, → `RunError::InvalidPhase`. The caller then calls `start(run)` as usual. | `ReplanRequested` is a verified dead-end: `start`/`approve`/`expire_gate`/`register_gate_waiter` all reject it, `request_replan` is an idempotent no-op on it, resume rearm skips it, and `apply_start_outcome` treats it as control-protected — so today only `cancel` leaves it. Targeting `Accepted` rather than `Running` is what makes the re-entry reuse the existing, tested dispatch arm instead of inventing a second one. |
| **AM-0003-2** | 0003 | **The load-bearing amendment.** §6.3 step 8 is generalized from a hard-coded `handle.run_dag(dag_id).await` to `self.executor.execute(RunExecCtx { run, dag_id, session_id, deadline }).await` over an injected `Arc<dyn RunExecutor>` (§3.8). The default impl, `DirectRunExecutor`, is byte-equivalent to today's call. Steps 1–7 and 9–10 — the state-guard table, acceptance emission, lease acquisition, lock release, the merge/race rules of step 9, and the whole step-10 outcome→row/event mapping — are **unchanged**, and they observe only the executor's **final** `DagOutcome`. | The audited draft had the driver call `Scheduler::run` from the CLI, which (a) breaks RFC-0015 SQ2, (b) leaves the run row at `Created`/`Accepted` with no `RunAccepted`, and (c) either skips `RunCompleted`/`RunFinished` or emits them per generation. Putting the loop *behind* the one dispatch point makes acceptance, completion, and terminalization single-sourced by construction rather than by discipline. |
| **AM-0003-3** | 0003 | Three additive `RunController` methods for the in-run generation transitions and for GN admission (§3.9): `begin_repair_generation(run, reason) -> Result<(), RunError>`, `complete_repair_generation(run, generation) -> Result<(), RunError>`, and the read accessor `control_state(run) -> Result<RunControlState, RunError>`. `begin_repair_generation` drops all gate waiters for the run and appends a `ReplanRequested` **session event** carrying the reason; `complete_repair_generation` appends `ReplanResumed`. **Neither writes `RunControlState::ReplanRequested`** — the row stays `Running` for the whole loop. Both require a live execution lease for the run (i.e. they are callable only from inside `start`'s dispatch) and return `InvalidPhase` otherwise. | Two distinct needs. (1) The row must not be parked: `replan_requested` is defined as "an external party asked and the run is waiting", and §6.3 step 9(a) explicitly treats a durable `replan_requested` as *a foreign control call winning the race*, which would make the driver clobber-protect against itself. Keeping the row `Running` across generations is both truthful and race-free. (2) There is verifiably **no** way to read a run's control state — `RunController` has exactly five methods and `SessionService` four, none of them a getter — so GN6's "run not cancelled" check has no seam without one. |
| AM-0004-1 | 0004 | Additive `DecisionKind::Replan` and `DecisionKind::PlanProposal` variants with payload shapes in §9.2. `prompt_body` MUST be `None` for both (driver/planner-authored). | The generation bump and the proposal accept/reject are decisions with budget and audit consequences; the decision log is where those live. |
| **AM-0009-2** | 0009 | (Was `AM-0009-1` in the audited draft — renumbered; RFC-0013 owns `AM-0009-1`.) §5.2 step 5 / §5.3.0 amended: the input-artifact phase gains the replan reason; when it is `Some(ReplanReason::FailureIr(f))`, the root node's plan-time `input_ref` body MUST be the seed envelope of SD1–SD10 instead of `NodeInputPayload::Goal`. §5.3.0's table gains the row: "Root, replan with FailureIr → `FromPredecessors` seed envelope (RFC-0017 §5.4)". RFC-0009 AC 28 is re-scoped to non-replan plans. | This is the production gap the `scheduler_repair_e2e` test hand-crafts around: generation N+1's root currently receives the bare `Goal`, discarding the diagnostics the replan exists to exploit. |
| AM-0009-3 | 0009 | (Was `AM-0009-2`.) `PlanProducedPayload` gains three additive optional fields: `source: Option<PlanSource>` (absent ⇒ `template`, preserving old readers), `proposal_artifact: Option<ArtifactId>`, and `seeded_root: Option<bool>`. §3.13 shape extended; Appendix B example extended. | Replay/audit must distinguish a template plan from a compiled proposal, locate the raw proposal blob, and — because the event log is the only durable provenance channel — let a restarted host recover a prior generation's plan source without re-reading the DAG blob. |
| AM-0009-4 | 0009 | (Was `AM-0009-3`.) `PlanResult` gains `source: PlanSource` and `proposal_artifact: Option<ArtifactId>` fields (construction-site change inside `alloy-runtime` only; `PlanResult` is not a wire type). | The driver and a future seeded re-proposal need the plan's provenance without re-reading the event log. |
| AM-0009-5 | 0009 | (Was `AM-0009-4`.) §1.5 item 7 amended: production wiring injects `TemplatePlanService` when `planner.mode = "template"` (default) and `LlmPlanService` when `"llm"`. `DisabledLlmPlanService` is retired from the gated-path role (it remains available for tests); the "explicit future feature flag" it guarded is now the `planner.mode` profile key. | PW5 requires enablement by RFC amendment, not a flag on the stub; this is that RFC. |
| **AM-0009-6** | 0009 | New, replacing the audited draft's LP2 reference to a private method. A **named, shared** validated-persistence API is extracted: `pub(crate) struct PlanPersistence` with `persist_validated(&self, req: PersistRequest<'_>) -> Result<PlanResult, PlanError>` (§3.5b). It absorbs today's private `TemplatePlanService::instantiate_and_persist` / `put_input_artifacts` bodies unchanged in behaviour, but takes an already-built **node/edge spec list** rather than a `TemplateId`, so a compiled proposal and a catalog template enter through the same door. `TemplatePlanService` becomes a thin caller of it. RFC-0009 §5.2/§5.3's normative step lists are re-anchored on `PlanPersistence`; the three-phase order, the pre-CAS validation pass, the post-binding re-validation, the CAS semantics, and `PlanProduced` emission are all unchanged. | The audited draft claimed "exactly one persistence path: `TemplatePlanService`'s machinery", but `instantiate_and_persist` is **private**, takes a `TemplateId`, and its `expected_for_cas` parameter type (`CasExpected`) is a private enum that cannot even be named outside the module — so `LlmPlanService` could not have called it, and `DagStore`'s methods explicitly "MUST NOT run `DagValidator`". Without this extraction the single-path claim is unimplementable and the fail-closed posture of §8.3 is unfounded. |
| **AM-0009-7** | 0009 | New. `PlanContext` gains two additive fields carrying **plan provenance into `replan`**: `prior_source: Option<PlanSource>` and `prior_proposal_artifact: Option<ArtifactId>`. `PlanContext` is `#[derive(Debug, Clone)]` and not a wire type, so this is a construction-site change; existing callers set both to `None` and get today's behaviour exactly. A caller that cannot supply them in-process MUST recover them from the last `PlanProduced` event for the DAG (AM-0009-3's durable fields). | `PlanService::replan(reason, ctx)` receives only `(ReplanReason, PlanContext)`, and `PlanContext` carries just `template_override` — there is no channel by which GN7's "re-compile the same stored proposal manifest" could reach the plan service. Provenance must be in the replan input *or* durable; this amendment provides both. |
| **AM-0010-2** | 0010 | New, and the **replacement** for the audited draft's `AM-0010-1` (which added `max_repair_generations` to `SchedConfig`; that field is withdrawn entirely — see §18.2). Additive trait method `Scheduler::run_within(&self, dag_id: DagId, remaining: Duration) -> Result<DagOutcome, SchedError>`, defaulting to `self.run(dag_id)` so no implementor breaks. `LinearScheduler` overrides it by seeding `RunCtx.run_timeout` from `remaining` instead of `deps.run_timeout`; R12's `run_started: Instant::now()` and the gate-wait exclusion of §5.19 are otherwise untouched. `RuntimeHandle` gains the matching `run_dag_within(dag_id, remaining)`; `run_dag(dag_id)` is retained as `run_dag_within(dag_id, deps.run_timeout)`. | Verified: `run_started` is set per invocation at `crates/alloy-runtime/src/scheduler/linear/loop_.rs:386`, so every generation starts a fresh `run_timeout` clock and `N` generations cost up to `N × run_timeout` (default 30 min each). A shared deadline cannot be expressed without a per-invocation remaining-budget parameter. Defaulted ⇒ additive. |
| AM-0013-1 | 0013 | Per PW5, `PlanningWorker` v2: `describe().uses_model` becomes `true`; `preferred_tier()` stays `Economy` advisory but the planner invokes it at `Standard` (§5.3); `PLANNING_SYSTEM` prompt is activated (§5.3.2); PW1 is amended to "makes at most `max_model_turns` model calls when driven by `LlmPlanService`; makes none when `planner.mode = "template"` (the deterministic branch is retained)". PW2 is retained **verbatim**. PW3's "registered-but-unreached" note is amended: the worker is now reached via the `CapabilityExecutor` seam by the planner, still never via a DAG node. PW4 amended per AM-0013-2. RFC-0013 T-test `planning_worker_makes_no_model_call_and_no_tool_call` is re-scoped to the deterministic branch. | PW5: "Enabling an LLM planner MUST be a new RFC amendment that changes `uses_model` and adds a prompt — it MUST NOT be a config flag on this worker." |
| AM-0013-2 | 0013 | `PlanningProposalPayload` gains additive `proposal: Option<ProposedDagManifest>` (§3.4). `schema_version` stays `1`; absent field ⇒ deterministic template selection (old shape unchanged on the wire). | The proposal must ride the existing worker payload, not a new channel. |
| **AM-0013-3** | 0013 | New. The `planning` capability's `describe().side_effects` changes `SideEffectClass::Pure → SideEffectClass::ReadOnly` on the model branch, and RFC-0013's §6 contract table row for `planning` is updated to `ReadOnly`. `required_tools()` stays `[]`. `SideEffectClass`'s own definition is **unchanged** (`Pure` = "No tool call, no model call"; `ReadOnly` = "Model completion and read-only tools only"). The deterministic branch keeps `Pure`; because `describe()` is per-instance, the constructed variant reports truthfully. | The audited draft's PW-C said `side_effects` "stays `Pure`" while PW-A added a model call. That is a direct contradiction of the shipped doc comment on `SideEffectClass::Pure` and of RFC-0013 PW1, and `ReadOnly` is the class that exists precisely for "model completion, no writes". `ReadOnly` is also strictly less privileged than the `WorkspaceWrite` the edit path already holds, so nothing is loosened. |
| AM-0015-1 | 0015 | B1 clarification (no text weakening): constructing a `GenerationRunExecutor` in the composition root and handing it to the `RunController` is "construct, call, render" and does not breach B1. The CLI's own call sequence is unchanged — it still calls `runs.start(run)`, so **SQ2 needs no amendment at all**. Any CLI-side retry loop over runs (e.g. a `--max-retries` driver) remains forbidden by B1 and MUST NOT merge; the in-run generation loop replaces it (MG4). | Prevents the interim loop from landing in parallel with this RFC. The audited draft additionally proposed weakening SQ2; that is withdrawn — the rework makes it unnecessary. |
| AM-0015-2 | 0015 | Profiles gain the `[planner]` table (§7.1) and `[limits] max_repair_generations = 2`, mapped by config resolution to a new `RuntimeConfig.max_repair_generations: u32` field, alongside the existing `run_timeout` / `gate_timeout` `[limits]` mappings. Unknown-key rejection updated accordingly. Accepted range `0..=8`; out of range is an assembly-time config error. | Config authority stays with profiles; the CLI maps, never decides. `RuntimeConfig` is the struct that already owns run-scoped policy read by the composition root, and — unlike `SchedConfig` — the scheduler never sees it. |

---

## 3. Public Rust API

New items live under `alloy_runtime::dag::proposal`, `alloy_runtime::planner`, and `alloy_runtime::driver`. `alloy-runtime` remains `#![deny(missing_docs)]` / `#![forbid(unsafe_code)]`.

### 3.1 Reused types (normative — unchanged fields)

| Type | Source | Notes |
| --- | --- | --- |
| `PlanService`, `PlanContext`, `PlanResult`, `PlanError`, `PlanProducedPayload`, `TemplatePlanService` | planner (0009) | `PlanContext`/`PlanResult`/`PlanProducedPayload` extended per AM-0009-3/4/7 |
| `DagValidator`, `ValidateOpts`, `DagValidationError` | dag::validate (0009) | unchanged — the proposal compiler's final gate |
| `TemplateId`, `TemplateCatalog`, `TemplateNodeSpec` | dag::templates (0009) | catalog stays closed; `TemplateId` gains **no** variant |
| `NodeInputEnvelope`, `NodeOutputEnvelope`, `NodeInputPayload`, `PredecessorOutput`, `ENVELOPE_SCHEMA_VERSION` | dag::io (0009) | seed envelope reuses these — no new envelope type |
| `Scheduler`, `DagOutcome`, `DagState`, `SchedError`, `SchedConfig` | scheduler (0001/0010) | `Scheduler` gains defaulted `run_within` per AM-0010-2. **`SchedConfig` is unchanged** |
| `RuntimeHandle` | runtime (0001/0003) | gains `run_dag_within` per AM-0010-2; `try_admit_run` single-flight admission unchanged and still enforced per generation |
| `FailureIr`, `ErrorClass`, `RetryDisposition`, `DiagnosticEvent` | types/diagnostic (0001) | unchanged. Note `DiagnosticEvent.raw_json: Option<serde_json::Value>` is unbounded and unredacted — never seeded verbatim (§5.4 SD9) |
| `RunController`, `ReplanReason`, `RunControlState`, `RunError` | session (0003) | extended per AM-0003-1/2/3; `RunControlState` gains **no** variant |
| `SharedCostMeter`, `BudgetPolicy`, `BudgetCheck`, `CostSnapshot` | obs (0004/0007) | unchanged. There is no "remaining" accessor; GN6 uses `check_budget(&policy) -> BudgetCheck` |
| `redact_secrets`, `redact_json_strings` | obs::redact (0004) | unchanged — the seed sanitizer reuses them (SD9) |
| `SideEffectClass` | capabilities (0013) | definition unchanged; the `planning` descriptor's *value* changes per AM-0013-3 |
| `CapabilityExecutor`, `NodeExecContext`, `NodeExecRef` | adapters/capabilities (0010/0013) | the proposer's invocation seam |
| `PlanningWorker`, `PlanningProposalPayload`, `WorkerConfig` | capabilities::workers (0013) | extended per AM-0013-1/2 |
| `DecisionLog`, `DecisionKind` | obs (0004) | extended per AM-0004-1 |

### 3.2 `PlanSource`

```rust
/// Provenance of a persisted plan generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanSource {
    /// Instantiated from the closed template catalog.
    Template,
    /// Compiled from an accepted `ProposedDagManifest`.
    LlmProposed,
}
```

### 3.3 `PlannerMode` and `PlannerConfig`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerMode {
    Template,
    Llm,
}

/// Validated planner knobs (profile `[planner]` table, §7.1).
#[derive(Debug, Clone)]
pub struct PlannerConfig {
    pub mode: PlannerMode,                 // default Template
    pub max_proposed_nodes: u32,           // default 8;  2..=16 accepted
    /// Cap on the raw proposal bytes (PC2). Default 16_384; accepted 1_024..=32_768.
    ///
    /// **Hard ceiling rationale (OC7).** The proposal rides inside
    /// `PlanningProposalPayload`, and RFC-0013 OC7 bounds the *total serialized
    /// worker payload* at `MAX_PAYLOAD_TOTAL_BYTES = 64 KiB`
    /// (`crates/alloy-runtime/src/capabilities/payload.rs:20`), enforced
    /// fail-closed inside the worker. A 32 KiB ceiling leaves the payload's
    /// other fields (`template_id`, `confidence`, `notes`, `truncated`) and
    /// JSON framing a full 32 KiB of headroom. Values above 32 KiB are not
    /// merely unwise, they are unreachable: the worker would clamp or drop the
    /// proposal before the compiler ever saw it.
    pub proposal_max_bytes: u32,
    pub planning_budget: TokenBudget,      // default { max_input: 16_384, max_output: 4_096 }
    pub planning_timeout_ms: u64,          // default 120_000; > 0
}

impl PlannerConfig {
    /// Defaults above; out-of-range values are a construction error
    /// (`PlanError::Internal` at assembly — fail closed, no clamping-to-valid).
    pub fn new() -> Self;
}
```

### 3.4 Proposal wire schema (`dag::proposal`)

```rust
/// Wire schema version for planning proposals (MUST be 1).
pub const PROPOSAL_SCHEMA_VERSION: u32 = 1;

/// A model-proposed **linear chain**, shape-only. Serialized inside
/// `PlanningProposalPayload.proposal` and as the `plan_proposal` CAS artifact.
///
/// Deliberately cannot express: capabilities, budgets, tiers, retries,
/// timeouts, cache keys, edges, fan-out, `Plan` or `Aggregate` nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposedDagManifest {
    /// MUST equal `PROPOSAL_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Execution order. The compiler emits dual Data+Sequence edges between
    /// consecutive entries (RFC-0009 §5.7.2 convention).
    pub nodes: Vec<ProposedNodeSpec>,
    /// Free-text model rationale; audit only. MUST NOT influence compilation.
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposedNodeSpec {
    /// `[a-z0-9_]{1,64}`, unique within the manifest (PC5).
    pub name: String,
    /// Only `Analyze | Edit | Review | VerifyCompile | VerifyTest | GateHuman`
    /// are accepted (PC3).
    pub kind: NodeKind,
    /// Required non-empty (after trim, ≤ 500 chars) iff `kind == GateHuman`;
    /// MUST be `None` otherwise (PC6).
    pub approval_reason: Option<String>,
}
```

### 3.5 Proposal compiler

```rust
/// Why a proposal was rejected. One variant per clamp rule (§5.2.2).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ProposalRejection {
    #[error("unsupported proposal schema_version {got}")]
    SchemaVersion { got: u32 },
    #[error("proposal exceeds {max} bytes")]
    TooLarge { max: u32 },
    #[error("node count {got} outside 2..={max}")]
    NodeCount { got: usize, max: u32 },
    #[error("node kind {kind:?} not allowed in proposals")]
    KindForbidden { kind: NodeKind },
    #[error("node name invalid or duplicate: {name}")]
    BadName { name: String },
    #[error("approval_reason constraint violated on {name}")]
    BadApproval { name: String },
    #[error("terminal node must be GateHuman")]
    NoTerminalGate,
    #[error("no verify node precedes the terminal gate")]
    NoVerify,
    /// PC8 — an `Edit` is not covered by a later verify (the gate would
    /// approve an unverified patch).
    #[error("edit node {name} is not followed by a verify node before the terminal gate")]
    UnverifiedEdit { name: String },
    /// PC13 — an `Edit` with no preceding `Analyze` or verify in the chain.
    #[error("edit node {name} has no preceding Analyze or verify node")]
    UngroundedEdit { name: String },
    #[error("compiled DAG failed validation: {0}")]
    Validation(#[from] DagValidationError),
}

/// Pure, sync, no I/O. Applies PC1–PC14, assigns resources per §5.2.3,
/// builds the `TaskDag` through the RFC-0009 three-phase machinery, and runs
/// `DagValidator::validate(&dag, ValidateOpts::default())` as the final gate.
/// `input_refs` are supplied by the caller exactly as in RFC-0009 §5.3
/// (ephemeral for the pre-CAS validation pass, real after Phase B).
pub fn compile_proposal(
    manifest: &ProposedDagManifest,
    args: CompileArgs<'_>,
) -> Result<TaskDag, ProposalRejection>;

#[derive(Debug)]
pub struct CompileArgs<'a> {
    pub dag_id: DagId,
    pub session_id: SessionId,
    pub generation: u64,
    pub ids: &'a TemplateIdMap,               // from allocate-ids over proposal names
    pub input_refs: &'a BTreeMap<NodeId, ArtifactId>,
    pub cfg: &'a PlannerConfig,
}

/// Allocate `NodeId`s/`GateId`s for a proposal (name-keyed, mirrors
/// `templates::allocate_ids`). Rejects rather than panics: proposals are
/// untrusted, unlike embedded manifests.
pub fn allocate_proposal_ids(
    manifest: &ProposedDagManifest,
) -> Result<TemplateIdMap, ProposalRejection>;
```

### 3.5b `PlanPersistence` — the single validated write path (AM-0009-6)

The audited draft asserted "exactly one persistence path: `TemplatePlanService`'s three-phase machinery". That path is `TemplatePlanService::instantiate_and_persist`, which is **private**, keyed on `TemplateId`, and whose `expected_for_cas` parameter has a private type — it is not callable by a second plan service, and `DagStore` deliberately does not validate (`put_if_generation`: "MUST NOT run `DagValidator`"). The claim is made true by extracting the machinery behind one named API that both services call.

```rust
/// The only code in the workspace that writes a DAG row for a plan or replan.
/// Owns the RFC-0009 three-phase order: build → pre-CAS validate with
/// ephemeral input refs → Phase B input puts → re-validate with real refs →
/// CAS (`put_if_generation` for gen 1, `replace_for_replan` for gen N+1) →
/// snapshot artifact → `PlanProduced`.
pub(crate) struct PlanPersistence { /* private: dags, artifacts, events */ }

/// What to persist. `specs`/`edges` are already resource-assigned: a template
/// instantiation and a compiled proposal are indistinguishable here by design.
pub struct PersistRequest<'a> {
    pub ctx: &'a PlanContext,
    pub specs: &'a [ResolvedNodeSpec],
    pub edges: &'a [ResolvedEdgeSpec],
    /// Recorded on `PlanResult` / `PlanProduced`; never affects the write.
    pub source: PlanSource,
    pub template_id: TemplateId,
    pub proposal_artifact: Option<ArtifactId>,
    /// `None` for generation 1; `Some(reason)` drives the CAS mode **and**
    /// the SD1–SD10 root seeding.
    pub reason: Option<&'a ReplanReason>,
    pub generation: u64,
}

impl PlanPersistence {
    /// Validates (`DagValidator::validate`, `ValidateOpts::default()`) before
    /// every write and returns `PlanError::Validation` on failure. There is no
    /// argument that skips validation and no constructor that accepts a
    /// pre-built `TaskDag` (§8.3).
    pub(crate) async fn persist_validated(
        &self,
        req: PersistRequest<'_>,
    ) -> Result<PlanResult, PlanError>;
}
```

| # | Rule |
| --- | --- |
| PS1 | `TemplatePlanService` and `LlmPlanService` MUST both write exclusively through `persist_validated`. Neither may call `DagStore::put`, `put_if_generation`, or `replace_for_replan` directly. CI grep (AC 46). |
| PS2 | Behaviour for a `PersistRequest` derived from a catalog template MUST be byte-identical to today's `instantiate_and_persist` — same node ids for the same inputs, same artifact labels, same `PlanProduced` payload modulo AM-0009-3's optional fields. RFC-0009's existing plan/replan tests are the regression suite (AC 47). |
| PS3 | `persist_validated` is `pub(crate)`. It is not a public API: exposing a DAG-writing seam outside the crate would create a second topology writer in exactly the sense V2 §6.4 forbids. Both plan services stay the only callers. |
| PS4 | The root-seeding decision (SD1–SD10) lives inside `persist_validated`, not in either service, so **every** `FailureIr` replan is seeded regardless of which service ran. |

### 3.6 `PlanProposer` and `LlmPlanService`

```rust
/// Seam between the plan service and the planning capability. Exactly one
/// production impl; tests inject scripted proposers.
#[async_trait]
pub trait PlanProposer: Send + Sync {
    /// Obtain a proposal for `ctx.goal`. `Err` values are *fallback triggers*,
    /// never run failures (§5.1 FB rules).
    async fn propose(&self, ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError>;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProposeError {
    #[error("planning capability unavailable: {0}")]
    Unavailable(String),          // registry resolve failure, router down, model 5xx
    #[error("planning call failed: {0}")]
    Model(String),                // completion error after admission
    #[error("proposal payload malformed: {0}")]
    Malformed(String),            // payload missing/undecodable
    #[error("planning budget denied")]
    Budget,
    #[error("planning timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

/// Production proposer: drives the `planning` capability through the
/// RFC-0010 `CapabilityExecutor` seam with a synthetic Plan-node context
/// (§5.3.1). Holds no prompt: prompts live in the worker (RFC-0013).
pub struct CapabilityPlanProposer { /* private: executor, deps, cfg */ }

/// Everything `CapabilityExecContext` requires that a `PlanContext` cannot
/// supply. Verified against the shipped structs: `NodeExecRef` requires
/// `workspace_root: PathBuf` and `attempt: u32`; `NodeExecContext` and
/// `CapabilityExecContext` each require a `CancellationToken`; and
/// `CapabilityExecContext` requires a `cost_meter: SharedCostMeter`.
/// Without these fields the proposer literally cannot construct its call
/// (blocker 5), and inventing a fresh meter would break RFC-0013 BG2.
pub struct ProposerDeps {
    /// From `Session.workspace_root` (the same value the scheduler puts on
    /// `NodeExecRef` at dispatch). The proposer MUST NOT read the process CWD.
    pub workspace_root: PathBuf,
    /// The run-scoped token. In production this is the run's child token, so a
    /// `RunController::cancel` aborts an in-flight planning call exactly as it
    /// aborts a node.
    pub cancellation: CancellationToken,
    /// The **run's** meter, not a new one: the planning call's tokens and USD
    /// are charged to the run (PP4, FB6). Passed through to
    /// `CapabilityExecContext.cost_meter`; the router bound to it records the
    /// usage (RFC-0013 BG1/BG2 — the proposer never calls `add_model_usage`).
    pub cost_meter: SharedCostMeter,
    /// Run-level ceilings, for the pre-call admission check (FB6).
    pub budget_policy: BudgetPolicy,
}

impl CapabilityPlanProposer {
    pub fn new(
        executor: Arc<dyn CapabilityExecutor>,
        deps: ProposerDeps,
        cfg: PlannerConfig,
    ) -> Self;
}

/// LLM-backed `PlanService`. Delegates template selection and fallback to
/// `TemplatePlanService`, and *all* persistence to `PlanPersistence` — the one
/// validated write path (LP2, AM-0009-6).
pub struct LlmPlanService { /* private: inner, persist, proposer, artifacts, decisions, cfg */ }

impl LlmPlanService {
    pub fn new(
        inner: TemplatePlanService,
        proposer: Arc<dyn PlanProposer>,
        artifacts: Arc<dyn ArtifactStore>,
        decisions: Arc<dyn DecisionLog>,
        cfg: PlannerConfig,
    ) -> Self;
}

#[async_trait]
impl PlanService for LlmPlanService {
    async fn plan(&self, ctx: PlanContext) -> Result<PlanResult, PlanError>;          // §5.1
    async fn load_template(&self, id: TemplateId, ctx: PlanContext)
        -> Result<PlanResult, PlanError>;                                             // delegates to inner (LP6)
    async fn replan(&self, reason: ReplanReason, ctx: PlanContext)
        -> Result<PlanResult, PlanError>;                                             // §5.5 GN7 (source-preserving)
}
```

### 3.7 Generation bound: `RuntimeConfig`, not `SchedConfig` (AM-0015-2)

```rust
pub struct RuntimeConfig {
    // ... existing fields, including run_timeout / gate_timeout / budget_policy ...
    /// Maximum automatic generation bumps per run. Total generations ≤ 1 + this
    /// value. `0` disables auto-replan and makes the driver a pass-through.
    /// Default 2; profile `[limits] max_repair_generations`; accepted 0..=8.
    pub max_repair_generations: u32,
}
```

The audited draft put this on `SchedConfig` while simultaneously forbidding the scheduler to read it. That is a standing invitation to a future contributor and needs a grep rule to hold. `SchedConfig` is constructed by the assembly and handed to `LinearScheduler`; `RuntimeConfig` is the struct the composition root already reads run policy from and the scheduler never sees, so the invariant becomes structural rather than aspirational (AC 31).

### 3.8 `RunExecutor` seam and `GenerationDriver`

The driver is not a top-level orchestrator. It is the *implementation of one step* of `RunController::start`.

```rust
// ---- defined in `alloy_runtime::session` (AM-0003-2) ----

/// What `RunController::start` awaits at RFC-0003 §6.3 step 8, in place of a
/// hard-coded `handle.run_dag(dag_id)`. Everything around it — the state
/// guards, `RunAccepted`, the execution lease, the step-9 race merge, the
/// step-10 outcome mapping — is unchanged and sees only the value this
/// returns.
#[async_trait]
pub trait RunExecutor: Send + Sync {
    async fn execute(&self, ctx: RunExecCtx) -> Result<DagOutcome, RuntimeError>;
}

#[derive(Debug, Clone)]
pub struct RunExecCtx {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub dag_id: DagId,
    /// **Absolute** wall-clock deadline for the whole run, computed once by
    /// `start` as `Instant::now() + cfg.run_timeout`. Every generation is
    /// dispatched with the *remaining* share (AM-0010-2). Generations do not
    /// each get a fresh `run_timeout`.
    pub deadline: Instant,
}

/// Today's behaviour, preserved verbatim: one `run_dag_within` call, no loop.
/// Used whenever `max_repair_generations == 0` or the RFC-0017 wiring is absent.
pub struct DirectRunExecutor { /* private: handle */ }

// ---- defined in `alloy_runtime::driver` ----

/// Bounded repair-generation loop. Implements `RunExecutor`, so it is reached
/// only from inside `RunController::start`. Not a scheduler and not a planner:
/// it holds neither's write authority.
pub struct GenerationDriver { /* private */ }

pub struct GenerationDriverDeps {
    /// Dispatch seam. Deliberately the **handle**, not `Arc<dyn Scheduler>`:
    /// `RuntimeHandle::run_dag_within` keeps the `try_admit_run` single-flight
    /// admission and the `SchedError → RuntimeError` mapping that §6.3 step 10
    /// is written against.
    pub handle: RuntimeHandle,
    pub plans: Arc<dyn PlanService>,
    /// For `begin_repair_generation` / `complete_repair_generation` /
    /// `control_state` (AM-0003-3) — never for `start` (re-entrancy) and
    /// never for `request_replan` (that is the external path, GN9).
    pub runs: Arc<dyn RunController>,
    /// Read-only: GN2's failed-node kind lookup, and the goal/fingerprints
    /// needed to rebuild the replan `PlanContext`.
    pub dags: Arc<dyn DagStore>,
    pub sessions: Arc<dyn SessionRows>,
    pub decisions: Arc<dyn DecisionLog>,
    /// GN6 budget admission. Read via `check_budget(&budget_policy)`; there is
    /// no "remaining" accessor on `SharedCostMeter`, so the verdict enum is the
    /// seam (`BudgetCheck::Ok` ⇒ admit).
    pub cost_meter: SharedCostMeter,
    pub budget_policy: BudgetPolicy,
    /// GN6's second half: the run's cancellation token.
    pub cancellation: CancellationToken,
    pub policy: GenerationPolicy,
}

/// The driver's own policy struct. One field today; it exists so the bound has
/// a home that is neither `SchedConfig` nor a bare `u32` argument.
#[derive(Debug, Clone, Copy)]
pub struct GenerationPolicy {
    pub max_repair_generations: u32,     // from RuntimeConfig; default 2
}

#[async_trait]
impl RunExecutor for GenerationDriver {
    /// Executes generations until a non-admissible outcome, the bound, or the
    /// absolute deadline. Returns the **final** generation's `DagOutcome`
    /// (which may be `Failed` — exhaustion is an outcome, not an error).
    /// `Err(RuntimeError)` is infrastructure only, and is mapped by §6.3
    /// step 10 exactly as a direct `run_dag` error is today.
    async fn execute(&self, ctx: RunExecCtx) -> Result<DagOutcome, RuntimeError>;
}

/// Internal to the driver; folded into `RuntimeError::Internal` at the
/// `RunExecutor` boundary so RFC-0003 §6.3 needs no new error arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DriveError {
    #[error("plan: {0}")]
    Plan(#[from] PlanError),
    #[error("run control: {0}")]
    Run(#[from] RunError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("internal: {0}")]
    Internal(String),
}
```

| # | Rule |
| --- | --- |
| RX1 | `RunController::start` MUST call `executor.execute(..)` exactly once per dispatch and MUST NOT interpret intermediate generations — the executor surfaces one `DagOutcome`. |
| RX2 | The driver MUST NOT call `RunController::start` (re-entrancy: `start` holds the per-run lease and would return `AlreadyStarted`), MUST NOT emit `RunAccepted` / `RunCompleted` / `RunFinished`, and MUST NOT write a run row. §6.3 owns all four. AC 48 pins this by grep. |
| RX3 | `DirectRunExecutor` MUST be the default when no executor is injected, so RFC-0003's behaviour without RFC-0017 wiring is bit-for-bit today's. |
| RX4 | The executor is injected at assembly (`SessionServiceDeps`), not selected at runtime by config. `max_repair_generations = 0` is expressed by the driver short-circuiting after generation 1, not by swapping executors — one code path, always exercised. |

### 3.9 `RunController` additive methods (AM-0003-1 / AM-0003-3)

```rust
#[async_trait]
pub trait RunController: Send + Sync {
    // ... existing five methods (RFC-0003): start, cancel, approve,
    //     request_replan, expire_gate — all unchanged ...

    /// Re-arm an **externally** replanned run. `ReplanRequested → Accepted`
    /// (not `Running`: re-entry reuses §6.3's existing `Accepted` arm, so no
    /// second `RunAccepted` is emitted). Requires no live lease. Idempotent
    /// from `Accepted`; `InvalidPhase` from every other state or with a lease
    /// held. Appends `ReplanResumed`. The caller then calls `start(run)`.
    async fn resume_after_replan(&self, run: RunId) -> Result<(), RunError>;

    /// In-run generation bump, step 1 of 2. Drops all gate waiters for the run
    /// and appends a `ReplanRequested` **session event** carrying `reason`.
    /// Leaves the row `Running`. Requires a live execution lease for `run`
    /// (i.e. callable only from inside `start`'s dispatch); otherwise
    /// `InvalidPhase`.
    async fn begin_repair_generation(
        &self,
        run: RunId,
        reason: &ReplanReason,
    ) -> Result<(), RunError>;

    /// In-run generation bump, step 2 of 2. Appends `ReplanResumed`
    /// `{ run_id, generation }`. Same lease precondition. Leaves the row
    /// `Running`.
    async fn complete_repair_generation(
        &self,
        run: RunId,
        generation: u64,
    ) -> Result<(), RunError>;

    /// Read the durable control state. Additive because none exists: the trait
    /// has exactly five methods and `SessionService` four, so today the only
    /// way to answer "is this run cancelled?" is to bypass the control plane
    /// and parse `SessionRows::get_run().state` directly — which the driver
    /// MUST NOT do.
    async fn control_state(&self, run: RunId) -> Result<RunControlState, RunError>;
}
```

| # | Rule |
| --- | --- |
| RC1 | `begin_repair_generation` / `complete_repair_generation` MUST NOT write `RunControlState::ReplanRequested`. The row stays `Running` for the whole loop. Writing it would trip §6.3 step 9(a)'s control-protected merge — the driver would be treated as a foreign control call winning a race against itself, and the final outcome would be silently discarded. |
| RC2 | Both MUST be lease-gated. A caller without the run's execution lease is not inside `start` and has no business bumping a generation mid-run; that caller wants `request_replan` (GN9). |
| RC3 | `request_replan` is **unchanged and still supported**: it parks the run at `ReplanRequested` for an external replanner, exactly as RFC-0003 specifies. The driver never calls it. `resume_after_replan` services that path and only that path. |
| RC4 | `control_state` is a read; it takes the per-run mutex, returns the parsed durable state, and writes nothing. |

### 3.10 Crate-root re-exports

MUST re-export: `PlanSource`, `PlannerMode`, `PlannerConfig`, `PROPOSAL_SCHEMA_VERSION`, `ProposedDagManifest`, `ProposedNodeSpec`, `ProposalRejection`, `compile_proposal`, `allocate_proposal_ids`, `PlanProposer`, `ProposeError`, `ProposerDeps`, `CapabilityPlanProposer`, `LlmPlanService`, `RunExecutor`, `RunExecCtx`, `DirectRunExecutor`, `GenerationDriver`, `GenerationDriverDeps`, `GenerationPolicy`, `DriveError`.

MUST **not** be re-exported: `PlanPersistence`, `PersistRequest` (PS3 — a public DAG-writing seam would be a second topology writer).

---

## 4. Internal Module Design

### 4.1 Hierarchy

```text
crates/alloy-runtime/src/
  dag/
    proposal.rs        # wire types, compiler, clamps (pure, no I/O)
  planner/
    mod.rs             # + pub use llm_service::*, proposer::*
    persist.rs         # PlanPersistence — the single validated write path (AM-0009-6)
                       #   absorbs instantiate_and_persist / put_input_artifacts
    seed.rs            # SD1–SD10: SeedDiagnostic projection + seed envelope build
    template_service.rs# thin caller of PlanPersistence after the extraction
    llm_service.rs     # LlmPlanService (propose → compile → PlanPersistence; fallback)
    proposer.rs        # PlanProposer trait + CapabilityPlanProposer + ProposerDeps
    llm_stub.rs        # DisabledLlmPlanService retained for tests (AM-0009-5)
  session/
    run_executor.rs    # RunExecutor trait + RunExecCtx + DirectRunExecutor (AM-0003-2)
    run_controller.rs  # §6.3 step 8 → executor.execute(); AM-0003-3 methods
  driver/
    mod.rs             # GenerationDriver (impl RunExecutor) + deps + DriveError
  capabilities/workers/
    planning.rs        # v2 body: model branch + deterministic branch (AM-0013-1/3)
```

### 4.2 Responsibilities

| Module | MUST | MUST NOT |
| --- | --- | --- |
| `dag::proposal` | PC clamps; resource assignment; final `DagValidator` gate | I/O; model calls; reading config files |
| `planner::persist` | the one validated write path; three-phase order; CAS; snapshot; `PlanProduced`; invoking the seeder on `FailureIr` replans | selecting templates; calling proposers; being public (PS3) |
| `planner::seed` | `SeedDiagnostic` sanitization (SD9); seed envelope + root `FromPredecessors` construction | reading raw verify artifacts; enriching with tool stdout |
| `planner::llm_service` | propose→compile→persist orchestration; fallback; decision records; proposal CAS artifact | prompts; direct router calls; any `DagStore` call |
| `planner::proposer` | synthetic Plan-node context; executor invocation; payload decode | clamping (compiler's job); retries beyond the worker's own turns; constructing a router or a `CostMeter` |
| `planner::template_service` | template selection; delegating to `planner::persist` | writing rows itself after the AM-0009-6 extraction; new templates |
| `driver` | GN admission; begin → replan → complete → re-dispatch ordering; bound and deadline enforcement; decision records | topology writes; run-row writes; lifecycle events; calling `start`; node-level decisions; reading `scheduler::linear` internals |
| `session::run_executor` | defining the seam; `DirectRunExecutor` | knowing that a generation loop exists |
| `capabilities::workers::planning` | prompt ownership; structured-output-first parse with one repair turn (RFC-0013 house pattern); proposal in payload | writing DAGs (PW2); calling `PlanService` (T8 grep) |

### 4.3 Dependency direction

```text
session   → runtime::RuntimeHandle, storage (traits)         # defines RunExecutor
driver    → session (RunExecutor + RunController traits), runtime::RuntimeHandle,
            planner (PlanService trait), storage (traits), obs
planner   → dag::{proposal, validate, templates, io}, obs::redact,
            capabilities (executor seam only), storage (traits)
proposal  → dag::{types, validate, templates(id-map type)}   # pure
scheduler → (unchanged; no planner, no driver, no session-executor)   # CI grep, AC 40
```

Acyclic: `session` never names `driver`; `driver` depends on traits `session` owns. The assembly wires `Arc<GenerationDriver>` into `SessionServiceDeps.executor` as `Arc<dyn RunExecutor>`.

---

## 5. Execution Algorithm

### 5.1 `LlmPlanService::plan` (normative, ordered)

| # | Rule |
| --- | --- |
| LP1 | If `cfg.mode == Template`, delegate to `inner.plan(ctx)` unchanged (constructed this way only in tests; production wiring selects the service by mode — AM-0009-5). |
| LP2 | There is exactly one persistence path, and it is the named API `PlanPersistence::persist_validated` (§3.5b, AM-0009-6), which both plan services call. `LlmPlanService` never calls `DagStore` or `EventSink` itself; it hands `persist_validated` a resource-assigned spec list plus `source = LlmProposed`. PS1's CI grep is what makes this checkable rather than aspirational. |
| LP3 | `proposal ← proposer.propose(&ctx)` bounded by `cfg.planning_timeout_ms` (outer `tokio::time::timeout`; the worker's own deadline is set to the same value) **and** by `deps.cancellation` — whichever fires first. A cancelled token yields `ProposeError::Cancelled`, which is *not* a fallback trigger (FB2b). |
| LP4 | On `Ok(manifest)`: put the raw manifest JSON as a CAS artifact (`ArtifactKind::Blob`, `content_type = application/json`, labels `alloy.envelope = plan_proposal`, `alloy.dag_id`, session/run attribution per RFC-0009 §3.11) — *before* compilation, so rejected proposals are still auditable. |
| LP5 | Compile: `allocate_proposal_ids` → `compile_proposal` (PC1–PC14, including the pre-CAS `DagValidator` pass over ephemeral refs) → hand the resulting resource-assigned specs to `persist_validated`, which runs the RFC-0009 three phases (Phase B input puts: root gets `Goal`, non-roots get pending-pred placeholders) and re-validates with real refs before the CAS. Set `PlanResult.source = LlmProposed`, `proposal_artifact = Some(id)`, `PlanProducedPayload.source = Some(LlmProposed)`, `template_id` = the template the day-1 selector would have chosen (fallback identity — informative, so old consumers keep a valid catalog id). |
| LP6 | `load_template(id, ctx)` always delegates to `inner.load_template` — an explicit template request is never second-guessed by a model. |
| LP7 | On any `ProposeError` or `ProposalRejection`: fall back (FB1–FB6). |
| LP8 | Every plan call records exactly one `PlanProposal` decision (§9.2), accepted or not. |
| LP9 | Proposal artifacts orphaned by fallback are acceptable (RFC-0002 has no GC; same posture as RFC-0009 §8.5). |
| LP10 | `PlanError` surfaced to the caller can only originate from the LP2 persistence path or from a cancellation (FB2b) — proposer/compiler failures are otherwise consumed by fallback. `PlannerDisabled` is never returned by `LlmPlanService`. |
| LP11 | **Observability failures never fail a plan.** A `DecisionLog::record` error, a metrics-counter failure, or a tracing-span failure MUST be logged at `warn` with the run/dag id and then dropped; it MUST NOT convert into `PlanError`, MUST NOT trigger fallback, and MUST NOT abort a generation. The audit trail is best-effort *after* the durable write; the durable write is not best-effort. (Contrast: `EventSink` failures after a durable DAG write remain `PlanError::Event`, unchanged from RFC-0009 — session events are part of the contract, decision records are not.) |

**Fallback (fail-closed) rules:**

| # | Rule |
| --- | --- |
| FB1 | Fallback target is `inner.plan(ctx)` (day-1 selector: `RepairLocalDiagnostic`), honouring `ctx.template_override`. |
| FB2 | Fallback MUST be attempted for every `ProposeError` variant **except `Cancelled`**, and for every `ProposalRejection` variant. Apart from cancellation there is no proposer error that fails the run. |
| FB2b | **Cancellation is not a fallback trigger.** `ProposeError::Cancelled` MUST propagate as `PlanError::Internal("cancelled")` (or a dedicated variant if RFC-0009 gains one) and MUST NOT be converted into a template plan. Falling back on cancel means an operator who pressed Ctrl-C, or a `RunController::cancel`, gets a *newly planned run* instead of a stop — the request to stop would be answered by starting work. The same rule holds for a timeout that fires *because* the token was cancelled: classify by token state first, elapsed time second. |
| FB3 | The `PlanProposal` decision record MUST name the trigger (`rejected_reason` = enum variant rendering) before the fallback plan call. |
| FB4 | If the fallback itself fails, its `PlanError` propagates unchanged — template-path errors keep template-path semantics. |
| FB5 | Fallback plans have `source = Template` (they are ordinary template plans; the failed proposal is visible only via the decision record and the `plan_proposal` artifact). |
| FB6 | The planning model call's tokens/cost are metered against the run like any worker call (RFC-0013 BG rules) via `ProposerDeps.cost_meter`; a `Budget` denial triggers fallback (FB2) and MUST NOT be retried at a lower tier (BG4). |
| FB7 | The `PlanProposal` decision record is best-effort per LP11: if it cannot be written, the fallback still happens. |

### 5.2 Proposal compilation

#### 5.2.1 Pipeline

```text
bytes ──decode──► ProposedDagManifest ──PC1..PC8──► shaped chain
      ──§5.2.3──► resource-assigned TemplateNodeSpec-equivalents + dual edges
      ──build_topology (RFC-0009 §5.3)──► TaskDag (all Pending, gen = ctx)
      ──DagValidator::validate(default opts)──► accepted | ProposalRejection::Validation
```

#### 5.2.2 Clamp rules (normative, ordered; first violation wins)

| # | Rule | Rejection |
| --- | --- | --- |
| PC1 | `schema_version == PROPOSAL_SCHEMA_VERSION` | `SchemaVersion` |
| PC2 | Serialized manifest ≤ `cfg.proposal_max_bytes` (checked on the raw payload bytes before decode) | `TooLarge` |
| PC3 | Every `kind ∈ {Analyze, Edit, Review, VerifyCompile, VerifyTest, GateHuman}` — `Plan` (no recursive planning) and `Aggregate` (no structural nodes in proposals) are forbidden | `KindForbidden` |
| PC4 | `2 <= nodes.len() <= cfg.max_proposed_nodes` | `NodeCount` |
| PC5 | Names match `[a-z0-9_]{1,64}` and are unique | `BadName` |
| PC6 | `approval_reason` present, non-empty after trim, ≤ 500 chars iff `kind == GateHuman`; absent otherwise | `BadApproval` |
| PC7 | Last node is `GateHuman` | `NoTerminalGate` |
| PC8 | **Verify-after-final-Edit clamp.** ≥ 1 node with `kind ∈ {VerifyCompile, VerifyTest}` at an index before the terminal gate (`NoVerify`); **and**, if the chain contains any `Edit`, there MUST be a verify node at an index strictly greater than the index of the **last** `Edit` and strictly less than the terminal gate's (`UnverifiedEdit`) | `NoVerify` / `UnverifiedEdit` |
| PC9 | Compiler emits **dual Data+Sequence** edges between consecutive nodes and nothing else — no Hint, no extra edges | (by construction) |
| PC10 | Compiler assigns every resource field from §5.2.3; `enable_cache = false`, `cache_key = None` on every node; the proposal has no syntax to say otherwise | (by construction) |
| PC11 | `GateHuman` nodes other than the terminal node are permitted (mid-chain gates) but count toward PC4. A mid-chain gate does **not** satisfy PC8: only the terminal gate is exempt from needing a verify after it | — |
| PC12 | `DagValidator::validate(&dag, ValidateOpts::default())` — linearity (V15) and gate presence (V11) included — as the final gate; any error is `Validation` | `Validation(_)` |
| PC13 | **Grounding clamp.** Every `Edit` MUST be preceded in the chain by at least one `Analyze` or verify node. An edit whose only input is the bare goal is the blind-edit shape this RFC exists to eliminate, and it is not a shape the shipped `repair_local_diagnostic` chain (analyze → edit → verify → gate) ever produces | `UngroundedEdit` |
| PC14 | The compiled chain MUST satisfy PC8 **as compiled**, i.e. after resource assignment and edge emission — the check runs on the built topology, not only on the manifest, so a future manifest form cannot smuggle past it | `UnverifiedEdit` |

**Why PC8 is normative and not merely advisable.** The audited PC8 required only "a verify somewhere before the gate", which admits `[analyze, verify, edit, gate]` and `[edit, verify, edit, gate]` — chains where a human is asked to approve a patch that was never compiled. Every shipped profile sets `[gates].require_cargo_check = true` and RFC-0015 **PF7** forbids setting it false in any catalog profile (`crates/alloy-runtime/src/config.rs:361`), which the CLI turns into `Constraint::RequireCargoCheck` on the goal. PC8 is that constraint expressed where a proposal can actually violate it: in topology. `Constraint::RequireCargoCheck` itself is unchanged and still carried on the `Goal`.

#### 5.2.3 Resource assignment table (normative — compiler-owned, model-invisible)

Values are transcribed from `crates/alloy-runtime/src/dag/templates.rs` on `main` (`llm_retry()` :179, `adapter_retry()` :189, `verify_retry()` :199, node specs :218–276) so a compiled proposal and a template instantiation are resource-indistinguishable. **Any divergence between this table and that file is a defect in this table** — AC 4b pins it with a test that compares a compiled proposal's nodes against `TemplateCatalog::get(RepairLocalDiagnostic)` field by field.

| Kind | capability | budget | model_tier | timeout_ms | retry (`max_attempts`, backoff, `retry_on`) | source ctor |
| --- | --- | --- | --- | --- | --- | --- |
| Analyze | `repair` | `{32768, 8192}` | Standard | 300_000 | 2, `Fixed{1000}`, `[Model]`, no escalate | `llm_retry()` |
| Edit | `edit` | `{32768, 8192}` | Standard | 300_000 | 2, `Fixed{1000}`, `[Model]`, no escalate | `llm_retry()` |
| Review | `review` | `{32768, 8192}` | Standard | 300_000 | 2, `Fixed{1000}`, `[Model]`, no escalate | `llm_retry()` |
| VerifyCompile / VerifyTest | none | `{0, 0}` | Economy (ignored) | 600_000 | **2, `Fixed{1000}`, `[ErrorClass::Tool]`**, no escalate | `verify_retry()` |
| GateHuman | none | `{0, 0}` | Economy (ignored) | 3_600_000 | 1, `Fixed{0}`, `[]` | `adapter_retry()` |

**Verify retry — corrected.** The audited draft said `max_attempts = 1, Fixed 0, retry_on = []` for verify nodes. `main` says `max_attempts = 2, Fixed{delay_ms: 1000}, retry_on = [ErrorClass::Tool]`, and the shipped doc comment states the reason: an `Inconclusive` verdict (signal-killed cargo, truncated output) "is an infrastructure no-answer, and one bounded re-run is the correct response. Genuine `Compile`/`Test` failures stay non-retryable." This is not a detail — the retry is what *creates* the Fail/Inconclusive split that GN3 depends on. A proposal compiled with `retry_on = []` would surface transient cargo noise as a genuine `Fail`, and GN3 would admit a repair generation for a failure the model cannot fix. The table now matches, and the two rules are consistent: verify retries `Tool`, the driver bumps only on `Compile`.

Note `Review` has no catalog instance (the shipped template is analyze → edit → verify → gate), so its row is derived from `expected_capability(Review) = "review"` plus the LLM-node defaults, not transcribed. Capability strings come from `dag::validate::expected_capability` (RFC-0009 Appendix A / RFC-0013 RG3) — the compiler MUST derive, never accept, capability ids.

### 5.3 Planning capability invocation

#### 5.3.1 Synthetic Plan-node context (normative)

| # | Rule |
| --- | --- |
| PP1 | `CapabilityPlanProposer` builds `NodeExecRef { session_id, run_id, dag_id, node_id, workspace_root, attempt }` with ids from `PlanContext`, a **fresh** `NodeId` (the node exists in no DAG — `Plan` nodes remain absent from all persisted topologies), `attempt = 1`, and `workspace_root` from `ProposerDeps` (sourced from `Session.workspace_root`, the same value the scheduler uses at dispatch). It MUST NOT read the process working directory. |
| PP1b | The full `CapabilityExecContext` is `{ meta, cancellation, capability, kind, effective_tier, budget, timeout, input, attempt, cost_meter }`. `cancellation` and `cost_meter` come from `ProposerDeps`; `attempt` MUST equal `meta.attempt` (RFC-0010 CE3). Every field is supplied — there is no field the proposer may default or fabricate. |
| PP2 | Dispatch parameters: capability `planning`, kind `Plan`, `effective_tier = Standard`, `budget = cfg.planning_budget`, `timeout = cfg.planning_timeout_ms`. |
| PP3 | The input envelope is `NodeInputEnvelope::new(dag_id, node_id, Plan, 1, NodeInputPayload::Goal(ctx.goal.clone()))` — the same root shape workers already parse. |
| PP4 | The call goes through the production `CapabilityExecutor` (RFC-0010 §3.8 X1–X9), so router binding, run-scoped metering, budget admission, and cancellation behave exactly as for scheduled nodes. The proposer MUST NOT construct a router or a `CostMeter` itself — it passes `ProposerDeps.cost_meter` through, and the router bound to that meter records the usage (RFC-0013 BG1/BG2; a proposer write would double-count). |
| PP5 | Executor/registry failures map to `ProposeError::Unavailable`; capability `Failed` outcomes map by `error_class`: `Budget → Budget`, `Timeout → Timeout`, **`Cancelled → Cancelled`** (which does *not* fall back — FB2b), everything else → `Model`. A fired `ProposerDeps.cancellation` MUST be classified `Cancelled` even if the observable failure was a timeout. |
| PP6 | A `Succeeded` payload that does not decode as `PlanningProposalPayload`, or decodes with `proposal: None` while `mode == Llm`, is `ProposeError::Malformed`. |

#### 5.3.2 `PlanningWorker` v2 body (normative)

| # | Rule |
| --- | --- |
| PW-A | The worker branches on its input: when driven through the proposer seam (kind `Plan`, `uses_model` path active) it makes model calls; when constructed for the deterministic path it behaves exactly as today (template selection, `proposal: None`, no model call). The deterministic branch keeps RFC-0013's `planning_worker_makes_no_model_call_and_no_tool_call` test honest (re-scoped by AM-0013-1). |
| PW-B | Prompting follows the RFC-0013 house pattern: `PLANNING_SYSTEM` system prompt (activated; owns the JSON schema of `ProposedDagManifest` and the closed kind list), goal text + assembled context as user content, structured-output-first parsing with at most **one** repair turn on parse failure, bounded by `WorkerConfig.max_model_turns`. |
| PW-C | `required_tools()` stays `[]`. `side_effects` on the **model branch** becomes `SideEffectClass::ReadOnly` (AM-0013-3); the deterministic branch keeps `Pure`. The audited draft's "stays `Pure`" was wrong on the shipped definition: `Pure` is documented as "No tool call, **no model call**", and `ReadOnly` as "Model completion and read-only tools only" — `ReadOnly` is exactly the class a model-calling, non-writing worker belongs to. The planning model call still reads and never writes, which is what `ReadOnly` asserts; declaring `Pure` while calling a model would make the descriptor lie to every consumer that orders `SideEffectClass` by privilege. |
| PW-D | On success the worker emits `PlanningProposalPayload` with `proposal: Some(manifest)`, `template_id` = day-1 selector's answer (fallback identity), `confidence` from the model turn, `replan_requested: false`. The worker performs **no clamping** — containment is the compiler's, so it is enforced even against a compromised worker (SEC5). |
| PW-E | PW2 verbatim: no `Arc<dyn PlanService>`, no DAG writes, no store handles. CI grep T8 retained and extended to `llm_service`/`proposer` imports (AC 41). |

### 5.4 Replan seeding (AM-0009-2 — the production fix)

`PlanPersistence::persist_validated` (and therefore every `PlanService::replan` call, template or LLM — PS4) changes as follows.

| # | Rule |
| --- | --- |
| SD1 | The input-artifact phase gains the parameter `reason: Option<&ReplanReason>`; `persist_validated` threads its `PersistRequest.reason` through instead of dropping it. (Today's `put_input_artifacts(manifest, ids, ctx, generation)` takes no reason, which is precisely why generation N+1's root is payload-identical to generation 1.) |
| SD2 | When `reason` is `None` or `Some` of a non-`FailureIr` variant (`UserRequested`, `BudgetPolicy`, `Other`), behaviour is byte-identical to today: root gets `NodeInputPayload::Goal(ctx.goal)`. |
| SD3 | When `reason = Some(FailureIr(f))`: the planner first puts a **seed predecessor artifact** — a `NodeOutputEnvelope` with `schema_version = 1`, `dag_id = ctx.dag_id`, `node_id = f.node`, `kind` = the failed node's kind looked up in the replan probe blob (`probe.nodes[f.node].kind`; if absent — failure from an older generation — default `VerifyCompile`), `generation` = the **prior** generation (`next_gen - 1`), `attempt = 1`, and `payload = { "ok": false, "diagnostics": <SD9 projection of f.diagnostics>, "notes": <SD9-sanitized f.notes>, "error_class": f.error_class, "truncated": <bool> }`. Labels: `alloy.envelope = replan_seed`, `alloy.dag_id`, session/run attribution. |
| SD4 | The `"ok": false` payload mirrors the verify success shape (RFC-0010 OU4 `{ ok, diagnostics, raw_artifact }`) minus `raw_artifact`, superset of the `{ diagnostics, notes }` body the `scheduler_repair_e2e` fixture already proved the repair worker consumes. |
| SD5 | The new generation's **root** input envelope is then `NodeInputEnvelope::new(ctx.dag_id, root_id, root_kind, next_gen, NodeInputPayload::FromPredecessors { preds: vec![PredecessorOutput { node_id: f.node, kind: <SD3 kind>, output_ref: <SD3 artifact> }] })`. The predecessor is *synthetic*: `f.node` belongs to generation N, not to this topology. Readers MUST NOT resolve `PredecessorOutput.node_id` against the current node map (workers already do not). |
| SD6 | Root identification is unchanged from RFC-0009 §5.3.0 (the unique node with zero Data∪Sequence template/proposal predecessors). Exactly one node receives the seed. Non-root wiring (pending-pred placeholders) is unchanged. |
| SD7 | The scheduler needs no change: the root has no in-DAG Data predecessors, so its plan-time `input_ref` is dispatched as-is (RFC-0010 C5 rewrite applies only to nodes with Data predecessors). |
| SD8 | `goal_content_digest` / cache framing are unaffected: seeded roots ship `cache_key = None` (day-1 posture; a seeded root MUST NOT reuse a `Goal`-framed cache key — restated for whenever cache lands). |
| **SD9** | **Sanitized seed projection (normative).** The seed MUST NOT embed `FailureIr.diagnostics` verbatim. Each `DiagnosticEvent` is projected into a `SeedDiagnostic` by, in order: (a) **drop `raw_json` entirely** — it is `Option<serde_json::Value>`, unbounded, and holds the compiler's original JSON; (b) keep `code`, `level`, `message`, `spans`, `package`, `fingerprint`; (c) flatten `children` to depth 1 and at most **8** entries, each projected by the same rules; (d) at most **32** `spans` per diagnostic, at most **64** diagnostics per seed, ordered as received; (e) `message` and every `notes` string through `obs::redact::redact_secrets`; (f) each string truncated at **4 KiB** on a UTF-8 boundary; (g) the whole serialized seed payload capped at **64 KiB**, dropping whole trailing diagnostics (never truncating one mid-structure) until it fits. Any drop or truncation sets `"truncated": true`. |
| **SD9a** | The caps and the redaction reuse the existing seams rather than inventing new ones: `redact_secrets` / `redact_json_strings` (`crates/alloy-runtime/src/obs/redact.rs:125`, `:178`), the 4 KiB string bound and 64 KiB total of RFC-0013 **OC7** (`crates/alloy-runtime/src/capabilities/payload.rs:18-23`), and UTF-8-safe truncation as already done by `truncate_utf8_bytes` (`redact.rs:162`). If a shared helper is warranted, promote `truncate_utf8_bytes` to `pub(crate)` — do **not** re-implement it. |
| **SD9b** | Dropping `raw_json` is consistent with the precedent already shipped: RFC-0012 D17/SEC10 has the context engine render diagnostics with "`children` flattened … and `raw_json` never" rendered (`crates/alloy-runtime/src/context/working_set.rs:291`, test `diagnostic_raw_json_is_never_rendered`). A model-facing seed is a model-facing surface; it gets the same treatment. The full, unprojected diagnostics remain durable in the `failure_ir` artifact for audit — nothing is lost, it is simply not fed to a model. |
| **SD10** | SEC7's premise is corrected accordingly: RFC-0010 **F4** constrains only `notes` ("`notes` MUST NOT contain absolute paths outside the workspace, env values, or provider keys"). It says nothing about `diagnostics`, and `DiagnosticEvent.raw_json` is neither bounded nor redacted anywhere in the type. Seed integrity therefore rests on SD9's projection, not on an inherited guarantee. |

### 5.5 `GenerationDriver::execute` (normative, ordered)

Called by `RunController::start` at RFC-0003 §6.3 step 8, with the per-run mutex **already released** (§6.3 step 7) and the execution lease **held**.

```text
execute(RunExecCtx { run_id, session_id, dag_id, deadline }):
  bumps ← 0
  loop:
    remaining ← deadline.saturating_duration_since(now())      # GN7: one absolute clock
    if remaining == 0: return Ok(last_outcome_or_timeout)      # never dispatch with a zero budget
    outcome ← handle.run_dag_within(dag_id, remaining)         # one generation; AM-0010-2
    if outcome.state != Failed: return Ok(outcome)             # Succeeded / Cancelled / ReplanRequired pass through
    if !admit(outcome, bumps, deadline): record Replan{admitted:false, reason}; return Ok(outcome)
    record Replan{admitted:true, from, to}
    runs.begin_repair_generation(run_id, &FailureIr(f))        # GN8: waiters dropped, ReplanRequested event, row stays Running
    plan  ← plans.replan(FailureIr(f), ctx')                   # seeds per §5.4; ctx' per GN10
    runs.complete_repair_generation(run_id, plan.dag.generation)# ReplanResumed event
    bumps += 1
```

The value returned here is the *only* thing §6.3 steps 9–10 ever see, so exactly one `RunCompleted` / `RunFinished` pair and one terminal row write occur per run regardless of generation count. The driver emits none of them.

**Admission (GN rules; all MUST hold, first failure names the rejection reason):**

| # | Rule |
| --- | --- |
| GN1 | `outcome.state == DagState::Failed` and `outcome.failure == Some(f)` and `outcome.failed_node == Some(n)`. Derivation rule D3 itself is untouched — the driver converts the *outcome*, the scheduler never re-routes it. |
| GN2 | The failed node's kind, looked up in the post-run blob (`dags.get(dag_id)`), is **`VerifyCompile`**. `VerifyTest` is excluded day-1: RFC-0010 **DG7** requires `McpVerifyTestAdapter` to return an **empty** `diagnostics` vector and to rely on the raw log artifact, so a test failure can never satisfy GN4 — admitting it would produce exactly the blind generation GN4 exists to prevent. Enabling it needs an amendment to DG7 that gives test failures a structured IR; that amendment is **not** made here (§16.5). Failures of LLM nodes, gates, or structural invariants never auto-replan. |
| GN3 | `f.error_class == ErrorClass::Compile` — a genuine verify `Fail` verdict. (`Test` is reachable in the enum but unreachable through GN2 today; the rule is written as `Compile` alone so the two do not silently disagree.) By RFC-0010 §5.13.2 (as amended by #52's `fail_requires_diagnostics`), Inconclusive conditions (signal kills, truncated output, non-{0,101} exits, bare 101 without rustc diagnostics) surface as `ErrorClass::Tool`/`Timeout` and are excluded by construction — and `verify_retry()` has already spent its one `Tool` re-run on them (§5.2.3). |
| GN4 | `f.diagnostics` is non-empty **after the SD9 projection** — no diagnostics, no seed, no bump (an empty seed would recreate the blind generation this RFC exists to kill). Checking post-projection matters: a diagnostic whose only content was `raw_json` projects to nothing. |
| GN5 | `bumps < policy.max_repair_generations`. |
| GN6 | The run is not cancelled and the budget is not exhausted. Two concrete reads, both now available: `runs.control_state(run_id)` MUST NOT be `Cancelling` or terminal (AM-0003-3 — no such accessor exists today), **and** `deps.cancellation.is_cancelled()` MUST be false, **and** `cost_meter.check_budget(&deps.budget_policy) == BudgetCheck::Ok`. Note `SharedCostMeter` exposes no "remaining" quantity — only `check_budget`'s verdict and `snapshot()`'s totals — so the verdict enum is the seam; the audited draft's "remaining > 0" was not expressible. Mirror of retry admission A5. |
| **GN7** | **One absolute deadline.** `deadline` is computed once by `start` (`Instant::now() + RuntimeConfig.run_timeout`) and carried on `RunExecCtx`; each generation is dispatched via `run_dag_within(dag_id, remaining)` where `remaining = deadline - now()`. Admission additionally requires `remaining > 0`. `N` generations therefore consume ≤ one `run_timeout` in total, not `N × run_timeout`. Gate-wait exclusion (RFC-0010 §5.19) still applies *within* each generation; time spent replanning between generations is charged to the run. |
| GN8 | Ordering is mandatory: `begin_repair_generation` (drops gate waiters, appends the `ReplanRequested` audit event, row stays `Running`) **before** `replan` (topology write) **before** `complete_repair_generation` (appends `ReplanResumed`) **before** the next `run_dag_within`. The DAG is `Failed` at replan time, which `replace_for_replan` permits (RFC-0009 §5.6.2 — only `Running` is rejected; AC 16b already covers `Failed → Pending`). The driver MUST NOT call `request_replan`: that writes `RunControlState::ReplanRequested`, which §6.3 step 9(a) treats as a foreign control call winning the race, causing the final outcome to be discarded (RC1). |
| GN9 | A user/external replan surfacing as `outcome.state == ReplanRequired` passes through unchanged (`execute` returns it, §6.3 step 10 maps it to `replan_requested`) — the externally-requested path keeps its RFC-0003/0009/0010 semantics and remains available, serviced by `resume_after_replan` + a fresh `start`. Auto-replan is additive. |
| GN10 | The replan `PlanContext` preserves provenance: `template_override = Some(prior template_id)`, plus AM-0009-7's `prior_source` and `prior_proposal_artifact`. If `prior_source == LlmProposed`, the plan service MUST re-compile **the same stored proposal manifest** (fetched from `prior_proposal_artifact`) at the new generation rather than re-selecting a template — repair generations change *inputs*, not *shape*. In-process the driver has these from the last `PlanResult`; after a restart it recovers them from the last `PlanProduced` event's AM-0009-3 fields. If neither is available, the driver MUST fall back to `template_override` alone and record `Replan.provenance = "degraded"` — it MUST NOT silently re-select. Seeded re-*proposal* is deferred (§16.1). |
| GN11 | Exhaustion is not an error: when GN5 or GN7 fails, `execute` returns the final `Failed` outcome with its `FailureIr` intact, after a `Replan { admitted: false, reason: "exhausted" \| "deadline" }` decision. `DriveError` is reserved for infrastructure faults (store, control plane) and is folded into `RuntimeError::Internal` at the seam. |
| GN12 | Decision-record and metric failures are best-effort (LP11): they are logged and dropped, never converted into a `DriveError` and never used to deny admission. An audit-log outage must not silently disable repair. |

### 5.6 Migration from the interim driver loop (MG rules)

| # | Rule |
| --- | --- |
| MG1 | The RFC-0015 composition root's **call sequence is unchanged** — §7.1 steps 1–11 stay exactly as merged, including step 7 `runs.start(run)`. The CLI never called `Scheduler::run` and still does not (SQ2 intact). The only change is at assembly: build a `GenerationDriver` and pass it as `SessionServiceDeps.executor: Arc<dyn RunExecutor>` instead of letting the default `DirectRunExecutor` stand. That is construct-and-inject, permitted by B1 (AM-0015-1). The audited draft's "swap `Scheduler::run` for `GenerationDriver::drive`" described a call the CLI is forbidden to make and does not make. |
| MG2 | **Conditional on PR #54 (IN2).** If and when the pre-plan `seed_graph_diagnostics` pass merges, it is **retained**: it cures generation-1 blindness (the model sees real diagnostics before the first edit) through the graph channel, while §5.4 cures generation-N+1 blindness through the envelope channel. For generations ≥ 2 the **envelope is authoritative**; the graph channel is supplementary context. If #54 does not merge, this RFC is unaffected (IN1) — generation 1 is simply as blind as it is on `main` today, and the first verify `Fail` is what opens the model's eyes. |
| MG2b | Should #54 merge, its `ProjectGraph::record_diagnostic` path stores the **full** `DiagnosticEvent` including `raw_json` (`crates/alloy-index` serializes the whole event), and the repair worker reads it back via `GraphQuery::Diagnostics`. That channel is out of this RFC's scope, but SD9's projection MUST NOT be presented as covering it: a sanitized envelope beside an unsanitized graph read is not a sanitized system. Tracked as §16.6. |
| MG3 | Cross-run behaviour is unchanged: each `alloy run` remains one fresh run (new `RunId`, new `DagId`), and operators may still re-invoke after a fully exhausted run. Cross-run retries stay operator-driven; no CLI flag automates them. |
| MG4 | Any in-flight CLI-side bounded retry loop (`--max-retries`-style, fresh runs per attempt) MUST NOT merge; where present on a working branch it MUST be dropped in favour of the in-run loop before that branch lands (RFC-0015 B1; AM-0015-1). |
| MG5 | The `scheduler_repair_e2e` test's hand-crafted generation-2 seeding (synthetic `FromPredecessors` in `build_generation`) MUST be replaced by calls through `PlanService::replan` once AM-0009-2 lands — the test then proves the production path instead of simulating it (AC 22). |
| MG6 | `expire`d/manual approval flows, cancellation, and budget behaviour inside each generation are untouched — the driver only acts between generation dispatches, never during one. |
| MG7 | Shipping both this RFC and a cross-run retry mechanism is forbidden, not merely discouraged: two independent bounded loops multiply (`cross_run_attempts × (1 + max_repair_generations)` generations, each previously entitled to its own `run_timeout`). MG4 plus GN7's absolute deadline close both halves. Order of landing: this RFC's driver first, then #54 rebased without its retry loop. |

---

## 6. Lifecycle & Concurrency

### 6.1 Run lifecycle with generations (informative diagram, normative transitions)

```mermaid
sequenceDiagram
  participant CLI as CLI (0015)
  participant RC as RunController::start (0003)
  participant DRV as GenerationDriver (RunExecutor)
  participant SCH as Scheduler via RuntimeHandle (0010)
  participant PS as PlanService (0009/0017)
  CLI->>RC: start(run)
  Note over RC: guards; row Created→Accepted; emit RunAccepted; take lease;<br/>deadline = now + run_timeout; release per-run mutex
  RC->>DRV: execute(RunExecCtx{run, dag, deadline})
  DRV->>SCH: run_dag_within(dag, remaining)
  SCH-->>DRV: DagOutcome Failed (verify_compile, FailureIr)
  DRV->>DRV: GN1..GN7 admit
  DRV->>RC: begin_repair_generation(run, FailureIr)
  Note over RC: drop gate waiters; append ReplanRequested event;<br/>row STAYS Running
  DRV->>PS: replan(FailureIr, ctx' with provenance)
  Note over PS: PlanPersistence: generation++, root seeded (SD1–SD10)
  DRV->>RC: complete_repair_generation(run, 2)
  DRV->>SCH: run_dag_within(dag, remaining')
  SCH-->>DRV: DagOutcome Succeeded (gen 2)
  DRV-->>RC: Ok(Succeeded)
  Note over RC: §6.3 step 10, ONCE: RunCompleted → RunFinished → row succeeded
  RC-->>CLI: Ok(())
```

**What the rework buys, precisely.** In the audited design the CLI called `Scheduler::run` directly: the run row would sit at `Created` (never `Accepted`), no `RunAccepted` would ever be emitted, and either no `RunFinished` would be emitted or one would be emitted per generation. Here the loop is strictly *interior* to the one dispatch point, so RFC-0003 §6.3's acceptance, race-merge, and terminalization logic is reached exactly as often as it is today — once — and needs no new arms.

### 6.2 Writer inventory (unchanged counts)

| Surface | Writers |
| --- | --- |
| DAG topology / generation | `PlanService` only, through `PlanPersistence::persist_validated` only (LP2/PS1) |
| Node state / refs, same-generation | Scheduler only (`put_if_generation`) |
| Run control state | `RunController` only. The driver calls control methods; it never writes a row (RX2) |
| Run lifecycle events (`RunAccepted`/`RunCompleted`/`RunFinished`) | `RunController::start` / `cancel` / `approve` only — **unchanged count, unchanged sites**. The driver emits none |
| Generation-audit events (`ReplanRequested`/`ReplanResumed`) | `RunController` (AM-0003-3 methods, driver-invoked) |
| Proposal / seed artifacts | Planner only (CAS puts; append-only) |

### 6.3 Crash recovery (corrected — no durable loop is claimed)

The generation loop is **process-local and not resumable**, deliberately. The audited draft claimed a restarted host's driver would "detect this shape at `drive` start … and continue the loop"; that is stronger than the durable state supports and stronger than this RFC implements.

| # | Rule |
| --- | --- |
| CR1 | The driver reconstructs **no** in-flight loop. It has no start-up scan, no shape detection, and no `bumps` reconstruction. `bumps` is in-memory and dies with the process. |
| CR2 | A crash mid-generation is governed entirely by existing machinery: RFC-0010's ownership/adoption rules (§5.3) for the DAG, and RFC-0003 §5.3 resume rearm for the run row (`running`/`waiting_approval` → `accepted`, `cancelling` → finalized `cancelled`). A rearmed run is re-dispatchable via `start`, which runs the driver again — **from generation N as persisted**, with `bumps = 0`. |
| CR3 | CR2's `bumps = 0` means a crash-and-resume can, in the worst case, grant a further `max_repair_generations` bumps on top of those already spent. That is bounded (each requires a fresh operator/host action and a fresh absolute deadline), it is honest, and it is the same class of bound as re-running `alloy run`. The audited draft's `min(bumps, generation - 1)` accounting is **withdrawn**: it assumed the driver re-enters an interrupted loop, which CR1 says it does not. |
| CR4 | A crash between `replan` and `complete_repair_generation` leaves a durable, coherent state — row `running`, DAG `Pending` at generation N+1 with a seeded root — because AM-0003-3 never parks the row. Resume rearm writes `accepted`, `start` dispatches, and generation N+1 executes with its seed intact. No `resume_after_replan` call is involved; that method serves only the external path (RC3). |
| CR5 | Making the loop durable (a persisted bump counter, replan-intent replay) is deferred (§16.3). Nothing in this RFC's normative text depends on it. |

### 6.4 Concurrency invariants

| Rule | Detail |
| --- | --- |
| One driver per run | The driver holds no lock of its own. Exclusivity comes from **two** existing mechanisms, and the audited draft named only the weaker one: the run's **execution lease** in `RunController::start` (a second `start` for the same run returns `AlreadyStarted` while the lease is held), and the scheduler's `OwnershipLock` per DAG. The lease is what actually serializes drivers, because the driver only runs behind it. |
| Handle admission | Each generation goes through `RuntimeHandle::run_dag_within`, which keeps `try_admit_run`'s single-flight admission per `DagId`; the driver does not bypass it by holding a `Scheduler` directly. |
| Lock discipline | The driver runs after §6.3 step 7 released the per-run mutex, so `cancel` / `approve` remain live throughout the loop — including across `begin_repair_generation`, which takes the mutex briefly and releases it. RFC-0003's rule "never hold control-plane locks across host/scheduler awaits" is preserved. |
| Cancel during replan | A `cancel` landing between generations writes `Cancelling` and drops waiters. GN6 observes it on the next admission check and declines the bump; if the bump already started, the next `run_dag_within` observes the fired token and returns `Cancelled`, which §6.3 step 10 finalizes normally. |
| Replan race | `replace_for_replan` remains the atomic guard; the DAG is `Failed` (not `Running`) at every driver-initiated replan, so `DagBusy` indicates a foreign writer and surfaces as `DriveError::Plan(DagBusy)` → `RuntimeError::Internal` — fail closed, no retry loop around it. |
| Planning call concurrency | One proposer call per `plan`; the proposer MUST NOT retry (the worker's internal repair turn is the only second model call — PW-B). |

---

## 7. Configuration

### 7.1 Profile `[planner]` table (AM-0015-2)

```toml
[planner]
mode = "llm"                 # "template" | "llm"; default/autonomous flipped after §12.4
max_proposed_nodes = 8       # 2..=16
proposal_max_bytes = 16384   # 1024..=32768 — hard-capped well under OC7's 64 KiB
                             # total worker-payload bound (§3.3)
planning_max_input = 16384   # tokens, planning capability call
planning_max_output = 4096
planning_timeout_ms = 120000

[limits]
run_timeout_secs = 1800      # unchanged; now an ABSOLUTE bound across generations (GN7)
max_repair_generations = 2   # 0..=8; 0 disables auto-replan.
                             # Maps to RuntimeConfig.max_repair_generations — NOT SchedConfig
```

Shipped `default` and `autonomous` use `mode = "llm"` after the §12.4 stack-driver holdout comparison. `readonly` MUST keep `mode = "template"` and MUST reject `mode = "llm"` at assembly (a read-only profile has no business proposing edit chains — fail closed at config validation).

`[gates].require_cargo_check` stays `true` in every profile and PF7 is untouched; PC8 is its topological counterpart for proposed chains (§5.2.2).

**Wall-clock note (normative).** `run_timeout_secs` does not change meaning, but its *scope* is now explicit: it bounds the whole run including every repair generation and the replanning between them, not each generation separately. Operators who previously reasoned "worst case = `run_timeout`" keep that guarantee; they do not silently acquire a `3 × run_timeout` worst case by enabling repair generations.

### 7.2 `example.env`

Comment-only additions: `ALLOY_MAX_REPAIR_GENERATIONS=2` (the knob is run policy, not scheduler policy — the audited draft's `ALLOY_SCHED_*` prefix would have advertised the wrong owner). Alloy MUST NEVER write `.env`.

---

## 8. Error Handling & Failure Taxonomy

### 8.1 Taxonomy

| Error | Producer | Consumed by | Fail-open/closed |
| --- | --- | --- | --- |
| `ProposeError::{Unavailable, Model, Malformed, Budget, Timeout}` | proposer / worker / executor | `LlmPlanService` → fallback (FB2) | closed onto template path |
| `ProposeError::Cancelled` | proposer (token fired) | **propagates** as `PlanError` (FB2b) | closed — a stop request is never answered with a new plan |
| `ProposalRejection::*` (PC1–PC14, incl. `Validation`) | compiler | `LlmPlanService` → fallback | closed onto template path |
| `PlanError::*` | `PlanPersistence` (unchanged 0009 semantics) | caller (driver / `start`) | propagates |
| `DriveError::{Plan, Run, Store, Internal}` | driver | folded to `RuntimeError::Internal` at the `RunExecutor` seam; §6.3 step 10 maps it | infrastructure only; never encodes "repair failed" |
| `SchedError::*` | scheduler | `RuntimeHandle` maps to `RuntimeError` exactly as today; the driver does not intercept | unchanged — §6.3 step 10's existing arms keep working |
| Exhausted generations / expired deadline | driver (GN11) | caller | **an outcome** — final `Failed` `DagOutcome` with `FailureIr` intact |
| `RunError::InvalidPhase` from an AM-0003-3 method | control plane | driver → `DriveError::Run` | closed (no forced transition) |
| Decision-log / metrics failure | obs | logged at `warn`, dropped (LP11/GN12) | **never** fails a plan or a generation |

### 8.2 What MUST NOT be an error

| Condition | Handling |
| --- | --- |
| Proposal rejected | Decision record + fallback; run proceeds |
| Auto-replan not admitted (GN1–GN7 miss) | Decision record + final outcome returned |
| `max_repair_generations = 0` | Driver executes exactly one generation and returns — behaviourally identical to `DirectRunExecutor` |
| Seed lookup miss (SD3 kind fallback) | Default `VerifyCompile`, seed still written |
| Seed projection dropped diagnostics for size (SD9) | `truncated: true`; seed still written; still admissible if ≥1 diagnostic survives (GN4) |
| Provenance unavailable on replan (GN10) | `template_override` alone + `provenance: "degraded"` in the decision record |

### 8.3 Fail-closed catalogue (delta)

| Surface | Rule |
| --- | --- |
| Unvalidated proposal reaching `DagStore` | Closed by AM-0009-6, not by convention: `PlanPersistence::persist_validated` is the only caller of `put_if_generation`/`replace_for_replan` for plans (PS1, CI grep AC 46), it always validates, and `PersistRequest` takes specs rather than a `TaskDag`, so there is no argument shape that carries a pre-built unvalidated DAG in. AC 8 pins it |
| Human gate over an unverified edit | Closed by PC8/PC14 at compile time and by V11 at validate time; AC 5b |
| `readonly` profile + `mode = "llm"` | Assembly-time config error (§7.1) |
| Non-finite / oversized planner config | `PlannerConfig::new` construction error; no clamping-to-valid (§3.3) |
| `proposal_max_bytes` above the OC7 ceiling | Config range rejection at assembly (§3.3); the value is unreachable in practice, so accepting it would be a silent lie |
| Driver observing `Conflict`/`DagBusy` | Propagate; MUST NOT replan over a foreign writer, MUST NOT retry |
| Cancellation during planning | `ProposeError::Cancelled` propagates (FB2b); MUST NOT fall back to a template plan |

---

## 9. Observability

### 9.1 Tracing spans

| Span | Fields |
| --- | --- |
| `planner.propose` | `session_id`, `run_id`, `dag_id`, `outcome ∈ accepted\|rejected\|unavailable`, `node_count`, `bytes` — never goal text, never rationale |
| `planner.compile` | `dag_id`, `node_count`, `rejection_variant?` |
| `driver.generation` | `run_id`, `dag_id`, `generation`, `bumps`, `remaining_ms`, `admitted?`, `reject_reason?` |
| `planner.seed` | `dag_id`, `generation`, `diagnostic_count`, `bytes`, `truncated` — never diagnostic text |

**Failure policy for the observability surfaces (normative).** Spans, metrics, and decision records are best-effort in exactly the sense LP11 and GN12 define: a failure is logged at `warn` with run/dag identifiers and dropped. It never fails a plan, never denies an admission, never aborts a generation, and never converts into `PlanError` or `DriveError`. Session events (`PlanProduced`, `ReplanRequested`, `ReplanResumed`, `RunCompleted`, `RunFinished`) are **not** in this category — they are contractual, and their failures keep their existing RFC-0003/0009 semantics.

### 9.2 Decision records (AM-0004-1; `prompt_body = None`)

| Kind | When | Payload keys |
| --- | --- | --- |
| `PlanProposal` | every `LlmPlanService::plan` (LP8) | `dag_id`, `generation`, `accepted: bool`, `rejected_reason?`, `node_count?`, `proposal_artifact?`, `fallback_template?` |
| `Replan` | every driver admission decision (admit or reject) | `run_id`, `dag_id`, `from_generation`, `to_generation?`, `failed_node`, `error_class`, `diagnostic_count`, `admitted: bool`, `reason?` (`exhausted` \| `deadline` \| `kind` \| `class` \| `no_diagnostics` \| `cancelled` \| `budget`), `provenance?` (`preserved` \| `degraded`, GN10) |

### 9.3 Session events

| Event | Emitter | Delta |
| --- | --- | --- |
| `PlanProduced` | planner | payload gains `source` / `proposal_artifact` / `seeded_root` (AM-0009-3); `replan: true` generations now imply a seeded root when `reason` is `failure_ir` |
| `ReplanRequested` | `RunController` — `request_replan` (external) **or** `begin_repair_generation` (in-run) | unchanged shape; now has production emitters on both paths. The in-run emitter does **not** write the `replan_requested` row state (RC1) |
| `ReplanResumed` | `RunController::resume_after_replan` (external) or `complete_repair_generation` (in-run) | new (AM-0003-1 / AM-0003-3) |
| `RunAccepted` / `RunCompleted` / `RunFinished` | `RunController::start` only, once per run | **unchanged** — the whole point of AM-0003-2. A three-generation run emits the same lifecycle events as a one-generation run |
| Scheduler events | scheduler | unchanged (OB3 still forbids the scheduler emitting plan events) |

### 9.4 Metrics

Additive counters on the existing metrics surfaces: `planner.proposals_accepted`, `planner.proposals_rejected`, `driver.replans_admitted`, `driver.replans_rejected`, `driver.generations_run`. No changes to `StorageMetricsSnapshot`.

---

## 10. Security

| # | Rule |
| --- | --- |
| SEC1 | **Capability containment by construction.** A proposal cannot name a capability: the compiler derives capability ids from `expected_capability(kind)` over the closed kind set (PC3). `CAPABILITY_CATALOG` (RG1/RG2) is untouched; there is no path from model output to a capability string. |
| SEC2 | **Budget/tier containment by construction.** Budgets, tiers, retries, and timeouts come from the fixed table §5.2.3. A proposal cannot raise a ceiling, add attempts, or select a tier. Run-level budget ceilings (profile `[budgets]`, RFC-0013 BG1–BG4) apply to proposed DAGs identically — more nodes never means more total budget, only earlier exhaustion. |
| SEC3 | **Gate containment.** PC7 + V11: every persisted DAG — proposed or template — terminates in `GateHuman`; a model cannot plan its way around human approval. Approval reasons are length-capped (PC6) and rendered as text, never interpreted. |
| SEC4 | **No recursive planning.** `Plan` nodes are forbidden in proposals (PC3) and remain absent from every persisted topology (PP1) — a proposal cannot schedule more planning. |
| SEC5 | **Compiler, not worker, is the boundary.** Clamps run in `dag::proposal` on the trusted side of the seam, so a prompt-injected or compromised `PlanningWorker` can at worst emit a rejected proposal (fallback), never a hostile DAG. |
| SEC6 | **Prompt-injection posture.** Goal text and repo content are untrusted; the proposal inherits that taint and is treated as data until PC12 passes. `rationale` is audit-only (never parsed, never echoed into prompts of downstream workers). |
| SEC7 | **Seed integrity — by projection, not by inheritance.** The audited premise ("seed fields are already subject to RFC-0010 F4 redaction") is **false**: F4 constrains `notes` only, and `DiagnosticEvent.raw_json: Option<serde_json::Value>` carries the tool's original JSON with no size bound and no redaction anywhere in the type. Seeds therefore MUST be built through the **SD9 projection**: `raw_json` dropped outright, `children` flattened, spans/diagnostics count-capped, strings run through `obs::redact::redact_secrets` and truncated at 4 KiB, whole payload capped at 64 KiB with `truncated: true`. The planner MUST NOT enrich seeds with raw tool stdout (raw logs stay in `verify_raw` artifacts behind their own labels), and MUST NOT pass a `DiagnosticEvent` into a seed unprojected. This mirrors the shipped RFC-0012 D17/SEC10 rule that `raw_json` is never rendered into model-facing context. |
| SEC7b | **Scope honesty.** SEC7 covers the envelope channel only. If PR #54's graph channel merges, its `record_diagnostic` path stores the full event including `raw_json`, and closing that is a separate change (MG2b, §16.6). This RFC does not claim to have closed it. |
| SEC8 | **Sandbox unchanged.** Proposed nodes execute the same workers under the same `WorkerPermissions`/tool allowlists (`fs_read`, `apply_patch`) — topology choice grants no new I/O. |
| SEC9 | **Bounded automation — three independent bounds.** (1) `max_repair_generations` (`0..=8`, driver-enforced per `execute` call, GN5). (2) The **absolute** run deadline, which bounds total wall clock regardless of generation count (GN7) — the audited design had no such bound and would have allowed `N × run_timeout`. (3) Run-level budget admission (GN6). No configuration yields an unbounded plan/execute loop. The claim is *not* made that the bound is crash-proof: a resume restarts `bumps` at zero (CR3), which is bounded by operator action and by a fresh absolute deadline, and is stated plainly rather than papered over. |
| SEC9b | **No approval laundering across generations.** Each generation's terminal `GateHuman` is a fresh gate on a fresh generation; `begin_repair_generation` **drops** the prior generation's gate waiters. An approval granted in generation N never carries into generation N+1's patch. |
| SEC10 | **No `.env` writes; no new crates; `#![forbid(unsafe_code)]`.** |

---

## 11. Crate Dependencies & `unsafe`

**New dependencies: none.** All work reuses existing workspace crates. `#![forbid(unsafe_code)]` preserved; five-crate map unchanged.

---

## 12. Testing Strategy

### 12.1 Unit (pure)

- One test per `ProposalRejection` variant (PC1–PC8, PC12–PC14), plus golden: a valid 4-node proposal compiles to a DAG that is resource-identical to `repair_local_diagnostic` modulo names — asserted field by field against `TemplateCatalog::get(RepairLocalDiagnostic)`, including `verify_retry()`'s `max_attempts = 2 / Fixed{1000} / [Tool]`, so the §5.2.3 table cannot drift from `templates.rs` undetected.
- PC8 adversarial table: `[analyze, verify, edit, gate]` → `UnverifiedEdit`; `[edit, verify, edit, gate]` → `UnverifiedEdit`; `[analyze, edit, verify, gate]` → accept; `[analyze, edit, verify, gate_mid, verify2, gate]` → accept; `[verify, gate]` (no edit) → accept.
- `allocate_proposal_ids` rejects duplicates/bad names without panicking (untrusted input — contrast with template `allocate_ids`).
- Seed envelope golden (SD3/SD5): fixed `FailureIr` fixture → stable seed `NodeOutputEnvelope` JSON and root `FromPredecessors` shape; non-`FailureIr` reasons leave the root `Goal` byte-identical to today (SD2).
- **SD9 sanitizer suite:** a `DiagnosticEvent` whose `raw_json` holds a sentinel → sentinel absent from the seed bytes; a secret-bearing `message` → `[REDACTED]`; 200 diagnostics → capped at 64 with `truncated: true`; a 1 MiB message → 4 KiB on a UTF-8 boundary; nested `children` 5 deep → flattened to 1; a diagnostic whose only content was `raw_json` → projects empty and GN4 declines the bump.
- GN admission table: one test per GN1–GN7 rejection and per `Replan` decision payload, including the `deadline` reason.
- `PlannerConfig`/profile validation incl. `readonly` + `llm` rejection and `proposal_max_bytes > 32768` rejection.

### 12.2 Service tests (in-memory stores; scripted proposer)

- `LlmPlanService`: accepted proposal → `PlanProduced.source = llm_proposed` + `proposal_artifact` resolves; each `ProposeError` variant **except `Cancelled`** → fallback plan with `PlanProposal{accepted:false}` decision; **`Cancelled` propagates and produces no plan** (FB2b); fallback failure propagates template-path `PlanError` (FB4); `load_template` never consults the proposer (LP6).
- `PlanPersistence` equivalence (PS2): for every existing RFC-0009 plan/replan test, the post-extraction `TemplatePlanService` produces identical DAG rows, artifact labels, and `PlanProduced` payloads (modulo AM-0009-3's optional fields). A compiled proposal and an equivalent template enter through the same call.
- A `DecisionLog` that always errors: plans and generations still succeed (LP11/GN12).
- Replan seeding through the real `TemplatePlanService` against SQLite: replan with `FailureIr` → root input artifact decodes to the SD5 shape; RFC-0009 AC 31 (every `input_ref` resolves) holds for seeded generations.
- `resume_after_replan` state machine: `ReplanRequested → Accepted`, idempotent from `Accepted`, `InvalidPhase` from every other state **and** while a lease is held, `ReplanResumed` appended, and a following `start` emits **no** second `RunAccepted`.
- AM-0003-3 methods: lease-gated (`InvalidPhase` without a lease); `begin_repair_generation` drops gate waiters and appends `ReplanRequested` **without** writing `replan_requested` to the row; row reads `running` throughout.

### 12.3 Driver + control-plane integration

- **Run-control integration (the blocker-1 regression):** a scripted 2-generation run driven through `RunController::start` asserts exactly one `RunAccepted`, exactly one `RunCompleted`, exactly one `RunFinished`, and a terminal row written once — with the run row observed as `running` (never `created`, never `replan_requested`) between generations.
- `DirectRunExecutor` parity: RFC-0003's existing `start` test suite passes unchanged against it (RX3).
- Scripted driver loop: Fail(compile, diags) → bump → Succeed returns `Succeeded` after exactly one replan; exhaustion returns final `Failed` outcome (GN11); `ErrorClass::Tool` (Inconclusive) never bumps (GN3); a `VerifyTest` failure never bumps (GN2); `max_repair_generations = 0` executes exactly one generation.
- **Absolute-deadline test (GN7):** with `run_timeout` = T and a generation that consumes 0.6 T, the second generation is dispatched with ≈0.4 T remaining and a third is refused with reason `deadline`; total wall clock ≤ T + ε. A scripted scheduler asserts the `remaining` argument it receives strictly decreases.
- Cancellation: `cancel` between generations → next admission declines (GN6) and the run finalizes `cancelled` once; `cancel` during a generation → token fires, `Cancelled` outcome, normal §6.3 finalization.
- Crash shape (CR2/CR4): a run killed after `replan` and before the next dispatch, then resumed → rearm to `accepted`, `start` re-dispatches generation N+1 **with its seed intact**, `bumps` restarts at 0, no `ReplanRequested` row was ever written.
- **`scheduler_repair_e2e` rewrite (MG5/AC 22):** generation 2 is produced by `PlanService::replan` driven from the `GenerationDriver` inside `start`, instead of the hand-built `build_generation(.., Some(diagnostics))` branch; the test still asserts the real `E0308` reaches the repair worker's prompt path and the run converts.

### 12.4 Eval gate (RFC-0016; blocking for default-on only)

Holdout comparison on the local-diagnostic fixture set: `planner.mode = "llm"` vs `"template"` under identical budgets, plus `max_repair_generations ∈ {0, 2}` ablation. Flipping any shipped profile to `mode = "llm"` requires the holdout gate green with LLM-mode pass-rate ≥ template-mode (non-inferiority) — V2 §19.3's eval bar, mechanized.

**Status (2026-07-31, branch `cursor/mvp-live-holdout-beta-7632`):** gate **green**. Live stack-driver holdout under Landlock (`ALLOY_REQUIRE_LANDLOCK=1 cargo test -p alloy-eval --features stack-driver --test stack_driver_holdout`) — control `success_rate = 1.0`, `compile_success_rate = 1.0`; `holdout_template_vs_llm_planner_non_inferior` asserts both arms Pass on `e0502_holdout_01` with `llm_pass_rate >= template_pass_rate` (ScriptedProposer `repair_local_diagnostic` shape; gen2 repair/edit unchanged; replan preserves prior LLM source). Shipped flip: `profiles/default.toml` and `profiles/autonomous.toml` → `mode = "llm"`; `profiles/readonly.toml` stays `mode = "template"` (AC 34).

**Scope of the gate, stated unambiguously.** V2 §19.3 says "LLM Planner behind eval bar"; §0.4/§9.3 say "LLM planner off until eval bar". Both are satisfied by this RFC as written, and the two things being gated are distinct:

| Action | Gated by §12.4? |
| --- | --- |
| Merging this RFC's code with `mode = "template"` in every shipped profile | **No** (historical). Pre-flip AC 34 kept the planner *off* until the holdout comparison landed |
| Setting `mode = "llm"` in shipped `default` / `autonomous` | **Yes** — holdout green, non-inferiority, one-line citation from `stack_driver_holdout` (**satisfied**; flip landed) |
| Setting `mode = "llm"` on `readonly` | **Forbidden** — assembly rejects; AC 34 still forbids |
| An operator setting `mode = "llm"` in their own profile | Not a repo gate. It is opt-in, audited (`PlanProposal` records), and fail-closed to templates |
| Enabling repair generations (`max_repair_generations > 0`, default) | **No.** Repair generations are not the LLM planner; the ablation in this section measures them but does not gate them |

The last row is the one the audit read as ambiguous, and it is now explicit: the eval bar attaches to the *plan source*, not to the *generation loop*.

---

## 13. MVP vs Deferred

| Item | Status |
| --- | --- |
| Proposal schema v1 (linear, shape-only) + compiler + validator gate | **MVP** |
| `LlmPlanService` + fallback + audit | **MVP** (opt-in) |
| `PlanningWorker` model branch + prompt | **MVP** (reached only via opt-in) |
| `PlanPersistence` extraction (AM-0009-6) | **MVP** — everything else's single-write-path claim rests on it |
| Replan seeding SD1–SD10 incl. the SD9 sanitizer | **MVP** (active for *all* `FailureIr` replans, including user-requested ones — the fix is unconditional) |
| `RunExecutor` seam + `GenerationDriver` inside `start` + AM-0003-3 methods + knob | **MVP** |
| Absolute run deadline (`run_within`, AM-0010-2) | **MVP** — the loop must not ship without it |
| `VerifyCompile`-triggered generations | **MVP** |
| `VerifyTest`-triggered generations | Deferred — blocked on an RFC-0010 DG7 amendment (§16.5) |
| e2e rewrite onto production seeding | **MVP** |
| LLM default-on (`default` / `autonomous`) | **Landed** — §12.4 stack-driver holdout non-inferiority; `readonly` stays template |
| Seeded re-proposal; non-linear proposals; durable loop; cache; graph-channel sanitization | Deferred (§1.4, §16) |

---

## 14. Acceptance Criteria

Every criterion is independently testable by a named test or mechanical check.

- [ ] 1. `ProposedDagManifest`/`ProposedNodeSpec` serde round-trip; `schema_version` pinned to 1; unknown fields rejected (`deny_unknown_fields`).
- [ ] 2. Each `ProposalRejection` variant is produced by exactly one clamp rule, in PC order, first violation wins (unit per variant).
- [ ] 3. `Plan` and `Aggregate` kinds in a proposal → `KindForbidden` (SEC4).
- [ ] 4. Compiled proposals carry compiler-assigned resources exactly per §5.2.3; a proposal has no field that can alter them (type-level check + golden).
- [ ] 4b. §5.2.3's table equals `crates/alloy-runtime/src/dag/templates.rs` on `main`, asserted field by field — in particular verify nodes compile to `max_attempts = 2`, `Fixed { delay_ms: 1000 }`, `retry_on = [ErrorClass::Tool]`, and gate nodes to `adapter_retry()`. A drift in `templates.rs` fails this test.
- [ ] 5. Compiled proposals always end in `GateHuman` with a validated reason (PC6/PC7) and contain ≥1 verify node (PC8).
- [ ] 5b. **PC8/PC14:** a chain whose last `Edit` is not followed by a verify before the terminal gate is rejected with `UnverifiedEdit` — `[analyze, verify, edit, gate]` and `[edit, verify, edit, gate]` both rejected, `[analyze, edit, verify, gate]` accepted. No compiled proposal can present an unverified patch to a human gate.
- [ ] 5c. **PC13:** an `Edit` with no preceding `Analyze` or verify is rejected with `UngroundedEdit`.
- [ ] 6. Every accepted proposal passed `DagValidator::validate` with `ValidateOpts::default()` (PC12); a diamond/fan-out proposal cannot be expressed (schema has no edges) and a hand-built `TaskDag` bypass has no path into persistence.
- [ ] 7. Proposal CAS artifact written with `alloy.envelope = plan_proposal` labels *before* compilation; rejected proposals remain auditable (LP4).
- [ ] 8. `LlmPlanService` falls back to `TemplatePlanService` on every `ProposeError` variant **except `Cancelled`** and on every `ProposalRejection` variant (FB2; parameterized test).
- [ ] 8b. `ProposeError::Cancelled` propagates as a `PlanError` and produces **no** plan and **no** DAG row (FB2b). A cancelled proposer whose failure surfaces as a timeout is still classified `Cancelled` (PP5).
- [ ] 9. Fallback plans have `source = Template`; accepted proposals `source = LlmProposed` with `proposal_artifact` resolving via `ArtifactStore::get`.
- [ ] 10. Exactly one `PlanProposal` decision per `plan` call with the §9.2 payload; `prompt_body = None`.
- [ ] 11. `load_template` never invokes the proposer (LP6).
- [ ] 12. `PlanningProposalPayload` old wire shape (no `proposal` field) still decodes (AM-0013-2 back-compat).
- [ ] 13. `PlanningWorker` deterministic branch makes no model/tool call (re-scoped RFC-0013 test stays green); model branch is bounded by `max_model_turns` with at most one repair turn (PW-B).
- [ ] 14. Proposer uses the production `CapabilityExecutor` (router/meter/budget via X-steps); planning-call cost appears in the **run's** meter, not a fresh one (PP4) — asserted with `SharedCostMeter::shares_state_with`.
- [ ] 14b. `CapabilityPlanProposer` constructs a complete `CapabilityExecContext`: `workspace_root` from `ProposerDeps` (never the process CWD — grep for `current_dir` under `planner/`), `cancellation` from `ProposerDeps`, `cost_meter` from `ProposerDeps`, `attempt == meta.attempt` (PP1/PP1b). Firing the token aborts an in-flight planning call.
- [ ] 15. `ProposeError` mapping from executor/capability outcomes matches PP5 (unit per arm).
- [ ] 16. Planning call bounded by `planning_timeout_ms` → `Timeout` → fallback (LP3).
- [ ] 17. Replan with `ReplanReason::FailureIr` seeds the root: input artifact decodes as `FromPredecessors` with one synthetic pred whose `output_ref` decodes as the SD3 `NodeOutputEnvelope` (`ok: false`, prior generation, failed node id/kind).
- [ ] 18. Replan with `UserRequested`/`BudgetPolicy`/`Other` leaves the root envelope byte-identical to the pre-RFC shape (SD2 regression).
- [ ] 19. SD3 kind lookup falls back to `VerifyCompile` when the failed node is absent from the probe blob.
- [ ] 20. Seeded generation satisfies RFC-0009 AC 31 (all `input_ref`s resolve) and validates under default opts.
- [ ] 20b. **SD9 sanitizer:** a seeded `DiagnosticEvent` carrying a `raw_json` sentinel produces seed bytes not containing the sentinel; a secret in `message` is `[REDACTED]`; 200 diagnostics cap to 64 with `truncated: true`; a 1 MiB message truncates to 4 KiB on a UTF-8 boundary; `children` flatten to depth 1.
- [ ] 20c. A `FailureIr` whose diagnostics project to empty under SD9 does **not** bump (GN4), and the seed is not written.
- [ ] 21. Driver loop: scripted Fail(Compile, diags) then Succeed → one bump, final `Succeeded`, decisions `Replan{admitted:true}` then none.
- [ ] 21b. **Run-control integration (blocker 1).** A 2-generation run driven through `RunController::start` emits exactly one `RunAccepted`, one `RunCompleted`, one `RunFinished`, and writes a terminal row once. The run row reads `running` between generations — never `created`, never `replan_requested` (RC1). A 1-generation and a 3-generation run emit the same lifecycle-event multiset.
- [ ] 21c. `DirectRunExecutor` is the default and RFC-0003's existing `start` suite passes against it unchanged (RX3). The driver never emits a lifecycle event and never calls `start` (grep, AC 48).
- [ ] 22. `scheduler_repair_e2e` produces generation 2 via `PlanService::replan` (no hand-crafted seed remains in the test) and still converts a genuine `E0308` (MG5).
- [ ] 23. GN2: a Failed `Edit` node (ErrorClass::Model) never bumps; a Failed **`VerifyTest`** node never bumps (day-1 exclusion). GN3: `ErrorClass::Tool` (cargo signal/truncation/config classes) never bumps. GN4: empty diagnostics never bump.
- [ ] 24. GN5/GN11: with `max_repair_generations = 2`, the third verify Fail returns the final `Failed` outcome with `FailureIr` intact and `Replan{admitted:false, reason:"exhausted"}` recorded.
- [ ] 25. GN6: cancelled run (`control_state == Cancelling` **or** a fired token) / `BudgetCheck != Ok` → no bump, reason recorded.
- [ ] 25b. **GN7 absolute deadline:** with `run_timeout = T` and a first generation consuming 0.6 T, the second is dispatched with ≈0.4 T remaining, a third is refused with `reason: "deadline"`, and total wall clock ≤ T + ε. The `remaining` argument passed to `run_dag_within` strictly decreases across generations. A run with `max_repair_generations = 2` MUST NOT be able to consume 3 T.
- [ ] 25c. `Scheduler::run_within`'s default body equals `run(dag_id)`, so no existing implementor breaks (AM-0010-2); `LinearScheduler` seeds `RunCtx.run_timeout` from the argument.
- [ ] 26. GN10: template-sourced runs replan with `template_override = prior template_id`; proposal-sourced runs re-compile the stored manifest via `PlanContext.prior_proposal_artifact` (same shape, new generation, new seed); with provenance unavailable, `provenance: "degraded"` is recorded and no silent re-selection occurs.
- [ ] 26b. Provenance survives a process restart: the last `PlanProduced` event's `source` / `proposal_artifact` (AM-0009-3) are sufficient to reconstruct GN10's context.
- [ ] 27. GN8 ordering observable in the event log: `ReplanRequested` → `PlanProduced{replan:true}` → `ReplanResumed`, then scheduler `NodeState` events at the new generation — **with the run row `running` throughout**. The driver never calls `request_replan` (grep).
- [ ] 28. GN9: an externally requested replan (`DagOutcome::ReplanRequired`) passes through `execute` unconverted and §6.3 step 10 maps it to `replan_requested`.
- [ ] 29. `resume_after_replan`: `ReplanRequested → Accepted`; idempotent from `Accepted`; `InvalidPhase` from every other state and while a lease is held; `ReplanResumed` appended; a following `start` emits no second `RunAccepted` (AM-0003-1).
- [ ] 29b. AM-0003-3: `begin_repair_generation` / `complete_repair_generation` require a live lease (`InvalidPhase` otherwise), drop gate waiters, append their events, and **never** write `RunControlState::ReplanRequested`. `control_state` reads without writing.
- [ ] 30. Crash-shape recovery (CR2/CR4): a run killed after `replan` and before the next dispatch resumes via the existing rearm to `accepted`; `start` re-dispatches generation N+1 with its seed intact; `bumps` restarts at 0; no `replan_requested` row was ever written. No start-up loop reconstruction exists (grep: the driver has no recovery scan).
- [ ] 31. `RuntimeConfig.max_repair_generations` defaults to `2`; `0` makes the driver execute exactly one generation; **`SchedConfig` has no such field** and `max_repair_generations` appears nowhere under `crates/alloy-runtime/src/scheduler/` (CI grep).
- [ ] 32. `derive_dag_state` D1–D9 unchanged (RFC-0010 regression suite untouched and green).
- [ ] 33. Profile `[planner]` parsing incl. range rejection (`proposal_max_bytes > 32768` rejected — OC7 headroom); `readonly` + `mode = "llm"` fails assembly; `[limits] max_repair_generations` maps to `RuntimeConfig` and range-rejects outside `0..=8`.
- [x] 34. Shipped `default` + `autonomous` have `mode = "llm"`; `readonly` stays `mode = "template"` and forbids llm (CI grep `ac34_shipped_profiles_llm_default_except_readonly`; §12.4 flip).
- [ ] 35. `PlanProducedPayload` with absent `source`/`proposal_artifact` decodes (old events replay — AM-0009-2 back-compat).
- [ ] 36. Decision kinds `Replan`/`PlanProposal` exist with §9.2 payloads (AM-0004-1).
- [ ] 37. Planning budget denial → `ProposeError::Budget` → fallback; no tier downgrade retry (FB6/BG4).
- [ ] 38. `rationale` is never fed into any downstream prompt (grep + unit on context assembly inputs) (SEC6).
- [ ] 39. Seed payloads contain no raw tool stdout and **no `raw_json`** (SEC7/SD9) — asserted on serialized bytes, not on the struct.
- [ ] 40. CI grep: `scheduler/**` imports neither `planner::` nor `driver::` (B6 extended).
- [ ] 41. CI grep: `capabilities/**` imports no `PlanService`/`LlmPlanService`/`GenerationDriver` (PW2/T8 extended).
- [ ] 42. CI grep: `alloy-cli` contains no retry loop over runs, no `max_retries`/`max-retries` symbol, and no `Scheduler::run` / `run_dag` call (MG4/B1/**SQ2 still holds unamended**).
- [ ] 43. No `.env` writes in new modules (`rg` CI check); `#![forbid(unsafe_code)]`; no new crates; no sixth crate.
- [ ] 44. `DisabledLlmPlanService` still returns `PlannerDisabled` (test-only role retained, AM-0009-5).
- [ ] 45. Metrics counters §9.4 increment on accept/reject/bump paths; a failing `DecisionLog` or metrics sink does not fail a plan or a generation (LP11/GN12).
- [ ] 46. **PS1 CI grep:** `DagStore::{put, put_if_generation, replace_for_replan}` is called from `planner::persist` only. No other module — including `llm_service`, `template_service`, and `driver` — names them.
- [ ] 47. **PS2 equivalence:** every existing RFC-0009 plan/replan test passes unchanged against the post-extraction `TemplatePlanService`; DAG rows, artifact labels, and `PlanProduced` payloads are identical modulo AM-0009-3's optional fields.
- [ ] 48. **RX2 CI grep:** `driver/**` contains no `RunAccepted`, `RunCompleted`, `RunFinished`, `upsert_state`, `request_replan`, or `RunController::start` reference.
- [ ] 49. AM-0013-3: the `planning` descriptor reports `side_effects = ReadOnly` on the model branch and `Pure` on the deterministic branch; `SideEffectClass`'s own definition is unchanged; `required_tools()` stays `[]`.
- [ ] 50. IN3: no test, AC, or module in this RFC's scope references `seed_graph_diagnostics`, `bootstrap_diagnostics`, or `--max-retries`; the full suite is green on a checkout of `main` + this RFC alone, without PR #54.

---

## 15. Definition of Done

Merge only when the series [Definition of Done](./README.md#definition-of-done-merge-gate) is fully met:

- [ ] Architecture compliance: **PASS** (single topology writer through one named API; scheduler planner-free; run lifecycle single-sourced in `RunController::start`; LLM planner opt-in behind the V2 eval bar; no deferred item un-deferred beyond this RFC's scope)
- [ ] RFC acceptance criteria: **100% satisfied** (§14, 1–50 including lettered sub-criteria)
- [ ] Unit tests: **passing**
- [ ] Integration tests: **passing** (§12.2–12.3, including the `scheduler_repair_e2e` rewrite)
- [ ] Documentation: **complete** (module docs; amendment cross-notes added to RFCs 0003/0004/0009/0010/0013/0015; the `AM-0009-1` reference in `crates/alloy-runtime/src/planner/template_service.rs`'s doc comment still resolves to RFC-0013's amendment and is not shadowed)
- [ ] Public APIs: **reviewed and stable** (§3 signatures match implementation)
- [ ] Clippy: **clean**
- [ ] Formatting: **clean**
- [ ] No TODO or placeholder implementations left in this RFC's scope (explicit **Stub** / deferred only)
- [ ] All fifteen amendments (AM-0003-1/2/3, AM-0004-1, AM-0009-2…7, AM-0010-2, AM-0013-1/2/3, AM-0015-1/2) landed with their own tests, and every identifier verified globally unique across `docs/rfcs/` (§18.2)
- [ ] Code review: **approved**

The §12.4 eval gate is **not** a merge gate for this RFC; it gates the later default-flip PR.

---

## 16. Open Questions

1. **Seeded re-proposal.** Should generation N+1 of an LLM-proposed run re-*propose* (model sees the `FailureIr` and may change shape) instead of re-compiling the stored manifest (GN10)? Deferred until the §12.4 holdout can measure shape-change value; requires a seed-aware `PlanProposer::propose` overload.
2. **Per-goal template selection.** With `LlmPlanService` landed, should the *template* selector also become goal-sensitive (multi-template catalog) before LLM mode graduates? Cheap intermediate; needs ≥2 catalog templates first.
3. **Driver durability.** CR1–CR5 make the loop explicitly process-local: `bumps` dies with the process and a resume restarts it at zero (CR3). Should the bump count and the replan intent become durable (a `repair_generations_used` column, replay of the `ReplanRequested` event's reason)? That would make the bound crash-proof and let a resume continue an interrupted loop. MVP accepts operator re-run and states the looser bound honestly rather than claiming an accounting it does not implement.
4. **`ReplanReason` for exhaustion.** Should exhaustion append a distinct terminal event (`RepairExhausted`) beyond the `Replan{admitted:false}` decision? MVP says the decision record suffices.
5. **`VerifyTest` repair generations.** Blocked by RFC-0010 **DG7**, which requires `McpVerifyTestAdapter` to return an empty `diagnostics` vector and forbids synthesizing diagnostics from test names. Unblocking needs a structured test-failure IR (harness JSON → `DiagnosticEvent`s with real spans), which is an RFC-0010 amendment with its own parsing and redaction surface. Until then GN2 admits `VerifyCompile` only, and the exclusion is by *derivation* (no diagnostics ⇒ GN4 declines) as well as by rule, so the two cannot drift apart.
6. **Graph-channel sanitization.** If PR #54 merges, `ProjectGraph::record_diagnostic` persists whole `DiagnosticEvent`s including `raw_json`, and the repair worker reads them back. SD9 sanitizes the envelope channel only (SEC7b). Should the graph channel get the same projection at write time, at read time, or both? Out of scope here; must not be forgotten if #54 lands.
7. **Deadline attribution across generations.** GN7 charges replanning time to the run. Should the planning model call be excluded from the run clock the way gate waits are (RFC-0010 §5.19)? Arguments both ways; MVP charges it, which is the conservative choice.

---

## 17. Estimated Implementation Effort

| Slice | Work | Effort |
| --- | --- | --- |
| A | `dag::proposal` types + compiler + clamps PC1–PC14 + unit suite | 1.0–1.5 pd |
| A2 | **`PlanPersistence` extraction (AM-0009-6)** — move the private machinery behind the named API, re-anchor `TemplatePlanService`, prove PS2 equivalence against the existing RFC-0009 suite | 0.75–1.0 pd |
| B | Seeding fix in `planner::persist` + the SD9 sanitizer in `planner::seed` + goldens + AC 17–20c | 1.0–1.5 pd |
| C | `PlanningWorker` v2 (prompt, parse, repair turn, `ReadOnly` descriptor) + proposer seam with full `ProposerDeps` | 1.0–1.5 pd |
| D | `LlmPlanService` + fallback + cancellation propagation + decisions + artifacts | 1.0–1.5 pd |
| E | **`RunExecutor` seam (AM-0003-2) + AM-0003-3 control methods + `run_within` (AM-0010-2)** — the control-plane rework; touches `run_controller.rs`'s §6.3 step 8, the `Scheduler`/`RuntimeHandle` traits, and `RuntimeConfig` | 1.5–2.0 pd |
| E2 | `GenerationDriver` itself (GN1–GN12) + config plumbing | 1.0–1.5 pd |
| F | `scheduler_repair_e2e` rewrite + run-control integration tests (AC 21b/21c) + deadline test (25b) + CI greps | 1.25–1.75 pd |
| G | Eval-mode fixtures for §12.4 (non-blocking) | 0.25–0.5 pd |

**Total: 8.75–12.75 person-days** (was 6–9 before the audit; the increase is A2 and the E split — the control-plane rework and the persistence extraction are real work the audited draft had assumed away as "delegate to existing machinery"). Sequencing: A → A2 → B; C → D (needs A); E is independent and on the critical path; E2 needs B + E; F needs B + E2.

---

## Audit disposition (2026-07-29)

*Referenced elsewhere in this document as §18.*

External audit (GPT 5.6, relayed by the operator) on PR #55, verdict FAIL. Every finding was re-verified against `main` and the merged RFCs before action; verified-wrong claims are rebutted with citations rather than absorbed. **Tally: 20 accepted, 1 rebutted, 7 confirmed-sound (untouched), 6 risks mapped to mitigations.**

### 18.1 Findings

| # | Finding | Verdict | Disposition | Citation |
| --- | --- | --- | --- | --- |
| **B1** | `GenerationDriver` bypasses run control; RFC-0015 SQ2 requires execution via `RunController::start`; row left `Created`, acceptance/completion events skipped | **Accepted** | Reworked at design level. The driver is no longer a caller of the scheduler; it *implements* `RunExecutor` and is invoked from §6.3 step 8 (AM-0003-2). §6.3 steps 1–7 and 9–10 are untouched and see only the final `DagOutcome`, so lifecycle events and terminalization stay single-sourced. **SQ2 is not amended** — the CLI still calls `runs.start(run)`. §1.1, §2.4, §3.8, §5.5, §6.1, AC 21b/21c | SQ2 at `docs/rfcs/RFC-0015-cli-profiles-config.md:530`; §6.3 algorithm at `RFC-0003:759-800`; `start` impl `crates/alloy-runtime/src/session/run_controller.rs:739-801` |
| **B2** | PC8 needs only one verify *somewhere*; `Edit → GateHuman` and verify-before-edit pass; every profile requires cargo-check on the final patch | **Accepted** — reproduced: `[analyze, verify, edit, gate]` satisfied the audited PC8 | PC8 rewritten as a verify-after-**final**-`Edit` clamp, with PC14 re-checking on the built topology and new rejection `UnverifiedEdit`. `Constraint::RequireCargoCheck` preserved unchanged. §1.5 item 4, §5.2.2, AC 5b | PF7 at `RFC-0015:363`; enforcement `crates/alloy-runtime/src/config.rs:361`; constraint `crates/alloy-runtime/src/types/budget.rs:105` |
| **B3** | GN7 source-preserving replan is not expressible — `replan` receives no proposal artifact or prior source | **Accepted** | AM-0009-7 adds `PlanContext.prior_source` / `prior_proposal_artifact` (non-wire, additive); AM-0009-3 makes the same provenance durable on `PlanProducedPayload` so a restarted host can recover it. GN10 adds an explicit `provenance: "degraded"` path rather than silent re-selection. AC 26/26b | `PlanService::replan` at `crates/alloy-runtime/src/planner/template_service.rs:164`; `PlanContext` fields `:25-42` |
| **B4** | "Single persistence path" undefined — `instantiate_and_persist` is private + template-only; `DagStore::put*` does not validate | **Accepted** | AM-0009-6 extracts `PlanPersistence::persist_validated`, keyed on resource-assigned specs rather than a `TemplateId`, called by both plan services. PS1 grep + PS2 equivalence suite. §3.5b, AC 46/47 | private method `template_service.rs:215-222`; private `CasExpected` `:482`; "MUST NOT run `DagValidator`" `crates/alloy-runtime/src/storage/dags.rs:81` |
| **B5** | Missing deps: no workspace/cancel/`CostMeter` for the proposer; no run-state/budget read for GN6 | **Accepted** | `ProposerDeps { workspace_root, cancellation, cost_meter, budget_policy }` (§3.6, PP1/PP1b/PP4). GN6 gets real seams: AM-0003-3's `control_state` (no accessor exists today) plus `cost_meter.check_budget(&policy)` — the audited "remaining > 0" is not expressible. AC 14b/25 | `NodeExecRef`/`NodeExecContext` `crates/alloy-runtime/src/adapters/mod.rs:53-78`; `CapabilityExecContext` `adapters/capability.rs:33-55`; `RunController`'s five methods `session/traits.rs:46-62`; `SharedCostMeter` API `obs/cost.rs:230-311` |
| **B6** | `VerifyTest` auto-repair unreachable — DG7 empties diagnostics | **Accepted** | MVP restricted to `VerifyCompile` (GN2/GN3). The RFC-0010 DG7 amendment path is named but **not** taken here; tracked as §16.5 and listed in §1.4. Exclusion holds by derivation too (empty diagnostics ⇒ GN4 declines), so rule and reality cannot drift. AC 23 | DG7 at `RFC-0010:1787` |
| **B7** | Global timeout resets per generation | **Accepted** | AM-0010-2 adds defaulted `Scheduler::run_within(dag_id, remaining)` + `RuntimeHandle::run_dag_within`. `start` computes one absolute deadline; GN7 dispatches each generation with the remaining share and refuses at zero. AC 25b/25c | `run_started: Instant::now()` (R12) at `crates/alloy-runtime/src/scheduler/linear/loop_.rs:386`; `remaining_run` `:120-127`; `run_timeout` source `config.rs:90-92` |
| **B8** | SEC7 redaction premise false — seeds copy `raw_json`; F4 covers only `notes` | **Accepted** | SD9/SD9a/SD9b add the `SeedDiagnostic` projection: `raw_json` dropped, children flattened, counts capped, `redact_secrets` applied, 4 KiB strings / 64 KiB payload. SEC7 rewritten to rest on the projection, SEC7b scopes it honestly. AC 20b/39 | `DiagnosticEvent.raw_json` `crates/alloy-runtime/src/types/diagnostic.rs:56`; F4 at `RFC-0010:1596`; existing seams `obs/redact.rs:125,162,178`; precedent `context/working_set.rs:291` |
| **B9** | Compiler resource table ≠ current source (verify retry is 2, not 1) | **Accepted** | §5.2.3 corrected to `max_attempts = 2, Fixed{1000}, retry_on = [ErrorClass::Tool]`, with the rationale spelled out (the `Tool` retry is what *creates* the Fail/Inconclusive split GN3 depends on). Gate row corrected to `adapter_retry()`. AC 4b pins the table against `templates.rs` | `verify_retry()` `crates/alloy-runtime/src/dag/templates.rs:199-211`; `llm_retry()` `:179`; `adapter_retry()` `:189` |
| **H1** | Cancellation swallowed by template fallback | **Accepted** | FB2 excludes `Cancelled`; FB2b makes it propagate. PP5 classifies a fired token as `Cancelled` even when it surfaces as a timeout. AC 8b | FB2 as drafted covered "every `ProposeError` variant" |
| **H2** | No semantic chain clamps (Edit needs Analyze) | **Accepted** | PC13 + `UngroundedEdit`. AC 5c | shipped chain is analyze → edit → verify → gate, `templates.rs:218-276` |
| **H3** | Obs failure policy missing | **Accepted** | LP11 + GN12 + §9.3 note: decision/metric/span failures log at `warn` and are dropped; session events keep contractual semantics. AC 45 | — |
| **H4** | Crash/concurrency overstated | **Accepted** | §6.3 replaced by CR1–CR5: no loop reconstruction, no `min(bumps, generation-1)` accounting (withdrawn), resume restarts `bumps` at 0 and says so. §6.4 corrects the exclusivity story — the **execution lease**, not just `OwnershipLock`, is what serializes drivers. AC 30 | rearm at `crates/alloy-runtime/src/session/service.rs:109-118`; lease/`AlreadyStarted` `run_controller.rs:750-767, 786` |
| **H5** | `resume_after_replan` underspecified | **Accepted** | Fully specified in AM-0003-1: preconditions (state + no lease), target `Accepted` rather than `Running` (so re-entry reuses §6.3's tested arm and emits no second `RunAccepted`), ordering, idempotency, event. Scoped to the external path only (RC3). AC 29 | `ReplanRequested` rejected by `start` `run_controller.rs:764-766`; only `cancel` leaves it today |
| **H6** | `Pure` vs model-calling conflict | **Accepted** | AM-0013-3: `planning` model branch becomes `ReadOnly`; `SideEffectClass`'s definition unchanged; PW-C rewritten. AC 49 | `SideEffectClass::Pure` = "No tool call, no model call", `crates/alloy-runtime/src/capabilities/traits.rs:95-105`; RFC-0013 PW1 `:914` |
| **H7** | Proposal size vs OC7 65 KB | **Accepted** | `proposal_max_bytes` default 65_536 → **16_384**, range `1_024..=32_768`, with the OC7 arithmetic in the doc comment. AC 33 | `MAX_PAYLOAD_TOTAL_BYTES = 64 * 1024`, fail-closed, `crates/alloy-runtime/src/capabilities/payload.rs:20,223`; OC7 `RFC-0013:730` |
| **H8** | Eval-gate ambiguity vs V2 | **Rebutted** | No spec change. The draft already ships the planner **off** (`mode = "template"` in all three profiles, `readonly` rejecting `llm`, AC 34's CI grep) and gates only the default flip — which is exactly V2 §19.3's "LLM Planner behind eval bar". §12.4 gains a four-row table making the four distinct actions and their gating explicit, including the one the audit actually found ambiguous (repair generations are **not** LLM planning and are not eval-gated). Clarification, not amendment | V2 §19.3 `docs/architecture/alloy-architecture-v2.md:1315`; profiles `profiles/*.toml` |
| **H9** | Depends on unmerged #54 | **Accepted** | Verified: neither `seed_graph_diagnostics` nor `bootstrap_diagnostics` exists anywhere in the tree, and PR #54 is `OPEN`. IN1–IN4 added: every normative rule must hold against `main` alone; MG2 is explicitly conditional; no AC may name those symbols. AC 50 | `rg seed_graph_diagnostics crates/` → no matches; `gh pr view 54` → OPEN |
| **H10** | Amendment ID collisions (`AM-0009-1` / `AM-0010-1` already in 0013) | **Accepted** | Full `docs/rfcs/` survey performed; renumbering map in §18.2; §2.7 gains a global-uniqueness note; Appendix C records the rule | `RFC-0013:170-171`, plus the `AM-0009-1` citation in `crates/alloy-runtime/src/planner/template_service.rs`'s doc comment |
| **A1** | AM-0003-1 and AM-0015-1 insufficient without fixing the start/terminalization contract | **Accepted** | Correct, and it was the sharpest observation in the audit. AM-0003-2 (the `RunExecutor` seam) and AM-0003-3 (in-run transitions + state read) are what actually fix it; AM-0003-1 shrinks to the external path. AM-0015-1's proposed SQ2 weakening is **withdrawn as unnecessary** — the rework removes the conflict rather than legislating around it | §2.7, §3.9 |
| **A2** | AM-0010-1's bound shouldn't live on `SchedConfig` if the scheduler must never read it | **Accepted** | The `SchedConfig` field is withdrawn entirely. The bound lives on `RuntimeConfig` (beside `run_timeout`, invisible to the scheduler) and reaches the driver via `GenerationPolicy`. `AM-0010-2` is reused for the deadline seam. AC 31's grep now asserts absence rather than non-use | §3.7; `SchedConfig` fields `crates/alloy-runtime/src/scheduler/linear/mod.rs:40-58` |

### 18.1b "Sound — don't churn" list: confirmed, untouched

| Item | Status |
| --- | --- |
| Replan seed drop is real | Confirmed — `put_input_artifacts(manifest, ids, ctx, generation)` takes no reason (`template_service.rs:400-406`). §5.4 unchanged in intent |
| Scheduler stays planner-free | Unchanged; strengthened only by moving the bound off `SchedConfig` (AC 31/40) |
| Shape-only proposals + compiler-owned resources | Unchanged; SEC1–SEC5 intact. New clamps constrain *shape*, never add proposal syntax |
| Single topology writer | Unchanged in count; AM-0009-6 makes it enforceable rather than conventional |
| Template default + eval-gated LLM | Unchanged; §12.4 clarified only (H8) |
| Tool/Inconclusive excluded from repair admission | Unchanged; GN3 narrowed further to `Compile` for internal consistency with GN2 |
| #54 CLI retry correctly flagged as interim/B1-incompatible | Unchanged; MG4 retained, MG7 added for the multiplication hazard |

### 18.1c Top implementation risks → mitigations

| Risk | Mitigation |
| --- | --- |
| Split-brain run state | AM-0003-2 puts the loop inside the one dispatch point; RC1 keeps the row `Running` so §6.3 step 9(a) never mistakes the driver for a foreign writer; AC 21b asserts one lifecycle-event triple per run |
| Human-approved unverified edits | PC8/PC14 + `UnverifiedEdit`; AC 5b's adversarial table; SEC9b (no approval laundering across generations) |
| Raw rustc JSON in model seeds | SD9 projection; AC 39 asserts on serialized bytes; SEC7b names the graph channel as *not* covered (§16.6) |
| Budget / wall-clock × generations | GN7's absolute deadline (AC 25b), GN6's budget admission, GN5's bound — three independent limits (SEC9) |
| Lost LLM provenance on replan | AM-0009-7 (in-process) + AM-0009-3 (durable) + GN10's explicit degraded path |
| Shipping both #54 cross-run retry and in-run generations | MG4 forbids the CLI loop; MG7 states the multiplication and the landing order; AC 42's grep |

### 18.2 Amendment renumbering map

Allocated after a full `docs/rfcs/` survey. RFC-0013 §2.7 owns `AM-0007-1`, `AM-0009-1`, `AM-0010-1`, `AM-0012-1`, `AM-0012-2`; no other document allocated amendment ids before this one.

| Audited draft | This revision | Note |
| --- | --- | --- |
| AM-0003-1 | AM-0003-1 | Kept; rescoped to the external replan path and fully specified |
| — | **AM-0003-2** | New — the `RunExecutor` seam at §6.3 step 8 |
| — | **AM-0003-3** | New — `begin_repair_generation` / `complete_repair_generation` / `control_state` |
| AM-0004-1 | AM-0004-1 | Unchanged |
| AM-0009-1 | **AM-0009-2** | Collided with RFC-0013's `AM-0009-1` (`RFC-0013:170`, also cited from `template_service.rs`) |
| AM-0009-2 | **AM-0009-3** | Shift; also gains `seeded_root` |
| AM-0009-3 | **AM-0009-4** | Shift |
| AM-0009-4 | **AM-0009-5** | Shift |
| — | **AM-0009-6** | New — `PlanPersistence` (blocker 4) |
| — | **AM-0009-7** | New — `PlanContext` provenance (blocker 3) |
| AM-0010-1 | **withdrawn** | The `SchedConfig.max_repair_generations` field is removed entirely; the id also collided with RFC-0013's `AM-0010-1` (`RFC-0013:171`). Bound relocated to `RuntimeConfig` under AM-0015-2 |
| — | **AM-0010-2** | New — `Scheduler::run_within` / absolute deadline (blocker 7) |
| AM-0013-1 | AM-0013-1 | Unchanged |
| AM-0013-2 | AM-0013-2 | Unchanged |
| — | **AM-0013-3** | New — `planning` `side_effects` `Pure → ReadOnly` |
| AM-0015-1 | AM-0015-1 | Kept; the proposed SQ2 weakening is withdrawn |
| AM-0015-2 | AM-0015-2 | Kept; retargeted from `SchedConfig` to `RuntimeConfig`, range `0..=8` |

Post-renumbering, the union of amendment ids across `docs/rfcs/` is: `AM-0003-1/2/3`, `AM-0004-1`, `AM-0007-1`, `AM-0009-1` (0013), `AM-0009-2…7` (0017), `AM-0010-1` (0013), `AM-0010-2` (0017), `AM-0012-1/2`, `AM-0013-1/2/3`, `AM-0015-1/2` — no duplicates.

### 18.3 Not resolved here

| Item | Why |
| --- | --- |
| `VerifyTest` repair generations | Needs an RFC-0010 DG7 amendment (structured test-failure IR) with its own parsing and redaction surface — out of scope, §16.5 |
| Graph-channel `raw_json` sanitization | The channel ships on unmerged PR #54; sanitizing it is that PR's or a follow-up's work, §16.6 / SEC7b |
| Durable bump accounting | Deliberately not implemented; §6.3 CR3 states the looser bound rather than claiming crash-proofness, §16.3 |
| RFC-0013's own `AM-0009-1` / `AM-0010-1` | Left as-is. They are correct and already merged; this document moved instead |

---

## Appendix A — Seed envelope wire example (normative shape)

Root input artifact of a seeded generation 2 (`repair_local_diagnostic`, prior verify `Fail`):

```json
{
  "schema_version": 1,
  "dag_id": "<uuid>",
  "node_id": "<gen-2 analyze uuid>",
  "kind": "analyze",
  "generation": 2,
  "payload": {
    "from_predecessors": {
      "preds": [{
        "node_id": "<gen-1 verify uuid>",
        "kind": "verify_compile",
        "output_ref": "<seed artifact uuid>"
      }]
    }
  }
}
```

Seed artifact (`alloy.envelope = replan_seed`) — note the diagnostics are **`SeedDiagnostic` projections** (SD9), not raw `DiagnosticEvent`s:

```json
{
  "schema_version": 1,
  "dag_id": "<uuid>",
  "node_id": "<gen-1 verify uuid>",
  "kind": "verify_compile",
  "generation": 1,
  "attempt": 1,
  "payload": {
    "ok": false,
    "error_class": "compile",
    "truncated": false,
    "diagnostics": [
      {
        "code": "E0308",
        "level": "error",
        "message": "mismatched types: expected `u32`, found `String`",
        "spans": [
          { "path": "src/lib.rs", "start_line": 42, "start_col": 9,
            "end_line": 42, "end_col": 21 }
        ],
        "package": "alloy-runtime",
        "fingerprint": "<digest>",
        "children": [
          { "level": "help", "message": "try `s.parse::<u32>()?`", "spans": [] }
        ]
      }
    ],
    "notes": "cargo check failed"
  }
}
```

**Absent by construction (SD9):** `raw_json` on the diagnostic and on every child; `children` beyond depth 1 or 8 entries; spans beyond 32 per diagnostic; diagnostics beyond 64; any string beyond 4 KiB; any payload beyond 64 KiB. `message` and `notes` have passed `redact_secrets`. Comparing this against `FailureIr`'s wire form is the point: `FailureIr.diagnostics` is `Vec<DiagnosticEvent>` and `DiagnosticEvent` carries `raw_json: Option<serde_json::Value>` — the seed is a *narrower* type by design, and AC 39 asserts the sentinel's absence on the serialized bytes.

## Appendix B — Proposal wire example (informative)

```json
{
  "schema_version": 1,
  "rationale": "goal names a failing test, so verify first, then repair narrowly",
  "nodes": [
    { "name": "precheck",  "kind": "verify_test",    "approval_reason": null },
    { "name": "analyze",   "kind": "analyze",        "approval_reason": null },
    { "name": "edit",      "kind": "edit",           "approval_reason": null },
    { "name": "verify",    "kind": "verify_compile", "approval_reason": null },
    { "name": "gate",      "kind": "gate_human",     "approval_reason": "Approve test fix before completion" }
  ]
}
```

The compiler turns this into a dual-edged linear chain with §5.2.3 resources; note this expresses issue #53's "verify-first" template variant without adding a catalog entry.

It also illustrates the clamps: the chain passes **PC8** because `verify` follows the last `edit` before the terminal gate (the leading `precheck` alone would not have sufficed), and passes **PC13** because `analyze` precedes `edit`. Moving `verify` before `edit` — a plausible model output — is rejected with `UnverifiedEdit`, because the human gate would then be approving a patch nothing compiled.

## Appendix C — What future RFCs may assume

- Exactly one plan persistence path, and it has a **name**: `PlanPersistence::persist_validated`. Every DAG row written for a plan or replan goes through it, and it always validates. `PlanSource` is on every `PlanResult`/`PlanProduced`.
- Seeded roots are `FromPredecessors` with synthetic preds; consumers never resolve seed `node_id`s against the live node map.
- Seeds are `SeedDiagnostic` projections, never raw `DiagnosticEvent`s: no `raw_json`, bounded, redacted. A future consumer may rely on a seed being small and safe to put in a prompt.
- **Run lifecycle is single-sourced in `RunController::start`.** However many generations execute, a run emits one `RunAccepted`, one `RunCompleted`, one `RunFinished`, and one terminal row write. Anything that needs to execute differently plugs in as a `RunExecutor`, not as a second caller of the scheduler.
- `run_timeout` is an **absolute** per-run bound. Anything that adds inner iterations must consume the remaining share via `run_within`, not restart the clock.
- The generation loop is driver-owned and process-local; a future concurrent scheduler slots under `execute` unchanged as long as `DagOutcome` keeps its shape. Bump accounting is *not* durable (CR3) — do not build on it until §16.3 is answered.
- Proposals are chains until an RFC lifts V15; when it does, `ProposedDagManifest` gains an edges field via a `schema_version` bump — never by reinterpreting v1. PC8's verify-after-final-Edit property must survive that lift: on a general DAG it becomes "every path from an `Edit` to the terminal gate crosses a verify node."
- `AM-<rfc>-<n>` identifiers are globally unique across `docs/rfcs/`. Before allocating one, grep the whole directory.
