# RFC-0015: CLI, Profiles & Config

| Field | Value |
| --- | --- |
| **Status** | Implemented |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0003](./RFC-0003-session-manager-run-controller.md) (merged), [RFC-0004](./RFC-0004-observability-cost-metering.md) (merged), [RFC-0010](./RFC-0010-scheduler-runtime-adapters.md) (merged), [RFC-0013](./RFC-0013-capability-registry-workers.md) |
| **Effort** | 4–6 person-days |
| **Crate (implementation)** | `alloy-cli` — clap surface, terminal rendering, composition root |
| **Crate (touched, additively)** | `alloy-runtime` — profile parsing, profile-catalog export, durable approve fallback, stderr tracing init (§5.6 amendments A1–A5) |
| **Related RFCs** | [0001](./RFC-0001-alloy-runtime.md) host lifecycle / `RuntimeConfig` · [0002](./RFC-0002-storage-artifacts-session-events.md) `AlloyStorage` · [0005](./RFC-0005-sandbox-broker.md) `[sandbox]` table · [0006](./RFC-0006-mcp-host-builtins.md) `InProcessMcpHost` · [0007](./RFC-0007-model-router-provider.md) `TomlModelRouter::from_paths` · [0008](./RFC-0008-edit-engine.md) `GitEditEngine` · [0009](./RFC-0009-task-dag-templates-planner.md) `TemplatePlanService` · [0011](./RFC-0011-project-graph.md) `alloy index` (Appendix E.4) · [0012](./RFC-0012-context-engine.md) `[context]` → `context_profile` · [0016](./RFC-0016-eval-harness-holdout-gates.md) offline holdout |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | The 115-line outline of this filename (expanded to implementation grade) |
| **Amended by** | [RFC-0017](./RFC-0017-dynamic-planning.md) §2.7 — **AM-0015-1** B1 clarification: constructing the `GenerationDriver` and injecting it as the run executor at assembly is "construct, call, render" (SQ2 needs no amendment — the CLI still calls `runs.start(run)`); any CLI-side retry loop over runs remains forbidden and the interim `--max-retries` loop is removed (MG4); **AM-0015-2** profiles gain the `[planner]` table and `[limits] max_repair_generations = 2` (`0..=8`, mapped to `RuntimeConfig.max_repair_generations` — never `SchedConfig`) |

**Mental model (V2 §5.2 / §5.3 / ADR F-18):** `alloy-cli` is a **composition root plus a terminal**. It parses argv, resolves config, builds every subsystem in one honest order, hands the resulting object graph to the control plane, and renders what the control plane returns. It contains **no** planning, scheduling, retry, budget, or verification decision. Every behaviour a user sees is a behaviour some merged crate already owns; the CLI's only original contribution is *wiring* and *presentation*.

**Authority order (highest → lowest):** current `main` source → merged RFCs → Architecture V2 → this document → roadmaps. This RFC reshapes no merged public type; the only changes to merged crates are the additive amendments explicitly authorised in §5.6.

---

## 1. Overview

### 1.1 Purpose

Ship the M7 user-visible surface — the milestone where Alloy stops being a set of libraries and becomes a program someone can run:

1. **Subcommands** — `run`, `events`, `approve`, `cancel`, `resume`, `index`, alongside the existing `host` (V2 §1.4, roadmap M7).
2. **A normative composition root** — the exact construction order that turns `RuntimeConfig` + a workspace into a live `LinearScheduler` with real verify adapters, a real MCP host over a real sandbox broker, a real capability registry, and a real graph.
3. **Three profiles** — `default | autonomous | readonly` with Appendix B defaults spelled out per table, and a normative statement of which knob each profile may move.
4. **Config resolution** — one pinned precedence order over the `ConfigPaths` / env behaviour that already exists, with `.env` never written and `example.env` the only documented template.
5. **Approval UX** — the terminal path from a scheduler `GateHuman` node to `RunController::approve`, both interactively and out-of-band.
6. **An error / exit-code taxonomy** that makes the CLI scriptable and CI-usable without reading prose.

### 1.2 Problem statement

`crates/alloy-cli/src/main.rs` is 174 lines: a clap `Cli` with exactly one subcommand (`host`), a signal task, and `graceful_shutdown`. Its own integration test says so out loud:

> "`alloy host` today configures the runtime, creates the data directory, and waits for a signal. It does *not* install storage: nothing in `alloy-cli` references `AlloyStorage`, so the SQLite log, artifact CAS, and session plane are unreachable from the binary until RFC-0015 wires the CLI."
> — `crates/alloy-cli/tests/host_e2e.rs`

Meanwhile every subsystem it should be wiring is merged and waiting: `SessionPlane::new`, `LinearSchedulerDeps` with its fifteen fields, `McpVerifyCompileAdapter::new`, `SessionGateHumanAdapter::new`, `SessionVerifyPermissions::new`, `InProcessMcpHost::new`, `NativeSandboxBroker::new`, `TomlModelRouter::from_paths`, `TemplatePlanService::from_storage`, `SqliteProjectGraph::open`, `EventDecisionLog::new`, `list_decision_events`. None of them has a caller in a shipped binary.

Two failure modes are equally bad and this RFC must avoid both:

- **Business logic creeping into `alloy-cli`.** The roadmap makes it an M7 acceptance criterion: *"CLI owns I/O only — no planner/scheduler business logic in `alloy-cli`."* A retry loop, a budget check, or a node-readiness decision written in `main.rs` would pass tests and violate the architecture. §6.5 makes this CI-greppable rather than aspirational.
- **A composition root that lies.** Building the host with `McpHostConfig::new()` (default `max_in_flight = 64`) while asserting `SchedConfig.host_parallel_honesty = true` would be a self-contradicting assembly that no single crate's tests can catch. §6 pins the values that must agree across crate boundaries.

### 1.3 Scope

| In scope | Detail |
| --- | --- |
| Subcommand grammar | `run`, `events`, `approve`, `cancel`, `resume`, `index` + existing `host`; exact clap shapes (§4) |
| Output rendering | Human (TTY) and `--json` for every subcommand; stable JSON envelope (§7.7) |
| Composition root | Construction order, shutdown order, signal handling, per-run vs per-process objects (§6) |
| Profiles | Three catalog TOMLs, per-table Appendix B defaults, override authority matrix, readonly semantics (§5) |
| Config precedence | env > flag > profile file > built-in default, pinned against merged `ConfigPaths` behaviour (§5.4) |
| Approval UX | Gate prompt rendering, interactive and out-of-band approve, non-interactive refusal (§8) |
| Exit codes | Closed taxonomy mapped from `SessionError` / `RunError` / `DagState` / `ErrorClass` (§9) |
| `alloy index` | Ingest trigger, `sessions.graph_version` write, `graph_rebuild` decision record (§10) |
| Secrets posture | `.env` never read or written; `example.env` documentation obligations (§11) |
| Tests / CI greps | Named process tests, boundary greps, snapshot help (§12) |

### 1.4 Non-goals

Each deferral names the seam that already exists for it, so nothing needs redesigning to enable it later.

| Deferred item | Seam that exists | Owner / when |
| --- | --- | --- |
| TUI | `alloy events --json` is the same data source a TUI would read | Deferred (V2 §21.2) |
| `alloyd` daemon / ACP | none — deliberately absent | Deferred (ADR F-27) |
| Eval gates in `alloy` | `alloy-eval` binary / its own subcommand | **RFC-0016** |
| `alloy doctor` / `alloy config show` | `RuntimeConfig.data_dir_rule` already records provenance | Future (§17) |
| Multi-run / parallel `run` | `BudgetPolicy.max_parallel_* = 1`, `OwnershipLock` | Forbidden in MVP (V2 §6.2) |
| Interactive goal REPL | `SessionService::submit_goal` is per-invocation | Deferred |
| Shell completions / man pages | clap generators | Future (§17) |
| Goal-blind repair (goal names no target) | RFC-0013 `RW6` + auto-replan on `FailureIr` | Deferred (Appendix C note) |
| Reading or writing `.env` | none | **Forbidden** (V2 §12.4) |
| Planner / scheduler / retry logic in `alloy-cli` | RFCs 0009 / 0010 own it | **Forbidden** (rule B1) |

### 1.5 Day-1 MVP (normative)

1. `alloy run "<goal>" --workspace <path>` MUST take a fresh workspace to a compile-verified patch or an honest failure, with every tool call sandboxed and every decision in the log.
2. `alloy events`, `alloy approve`, `alloy cancel`, `alloy resume`, `alloy index` MUST all operate against the same on-disk state a prior `alloy run` produced, in a separate process.
3. Every subcommand MUST support `--json` and MUST emit machine-readable output on stdout with human diagnostics on stderr.
4. The binary MUST refuse to start a run it cannot execute safely: missing router, missing API key, unavailable `check` sandbox backend, unknown profile, or a profile whose knobs violate §5.5 all fail **before** any model call.
5. `alloy-cli` MUST NOT import any planning, scheduling, retry, or budget-policy symbol (§6.5).
6. No `TODO`, `unimplemented!()`, or `todo!()` in scope. The word **Stub** marks the only permitted "does nothing yet" behaviours and each is pinned by a rule ID.

### 1.6 Rule-ID index

| Prefix | Domain | Section |
| --- | --- | --- |
| **CL** | Command grammar and clap shapes | §4 |
| **PF** | Profile catalog and knob authority | §5 |
| **PR** | Config precedence and resolution | §5.4 |
| **CR** | Composition root: construction, shutdown, signals | §6 |
| **B** | Boundary — I/O only in `alloy-cli` | §6.5 |
| **SQ** | Per-subcommand control-plane call sequences | §7 |
| **OUT** | Output rendering, human and JSON | §7.7 |
| **GA** | Gate / approval UX | §8 |
| **EX** | Error taxonomy and exit codes | §9 |
| **IX** | `alloy index` and graph obligations | §10 |
| **SEC** | Secrets, `.env`, `example.env` | §11 |
| **T** | Testing and CI greps | §12 |

---

## 2. Architecture integration

### 2.1 Relationship to Architecture V2

| V2 reference | Application here |
| --- | --- |
| §1.4 milestone thesis | `alloy run` → compile-verified patch with decision log under sandbox is this RFC's acceptance walkthrough (Appendix C) |
| §5.2 responsibilities | "CLI — Args, TTY, approvals, config — owns User I/O — does **not** own Planning logic" is rule **B1** |
| §5.3 process topology | Single binary; CLI, embedded runtime, in-process MCP + sandbox, ProjectGraph and Eval all in one process (§6.1) |
| §5.4 crate layout | `alloy-cli/ # binary / TTY`; no sixth crate, no `alloy-daemon` |
| §5.5 primary control APIs | `SessionService` / `RunController` are the only control surfaces the CLI calls (§7) |
| §6.5 sequence diagram | Reproduced as the `alloy run` call sequence (§7.1) |
| §12.4 secrets | "Never replace user's `.env`; use `example.env` patterns only" is rules **SEC1**–**SEC4** |
| §17 default profile posture | "Default profile: **no raw bash**" is rule **PF6** |
| §20 R14 cost overrun | Budget warnings surfaced from `BudgetWarning` events, never invented by the CLI (**OUT7**) |
| Appendix B | The three catalog profiles in §5.2 are Appendix B plus two constrained deltas |
| Appendix E permission token | The CLI never mints a `PermissionToken`; it injects `SessionVerifyPermissions`, which does (**B4**) |

### 2.2 Relationship to the roadmap (M7)

The roadmap's M7 user-visible list is this RFC's scope statement, and its acceptance criteria are reproduced in §13:

- `alloy run "fix <local diagnostic> in crate X"` — §7.1
- `alloy events` / `approve` / `cancel` / `resume` — §7.2–§7.5
- Repair → Edit → TextPatch → sandboxed check → GateHuman → decision log — Appendix C
- Profiles: `default | autonomous | readonly`; Appendix B defaults — §5
- "CLI owns I/O only — no planner/scheduler business logic in `alloy-cli`" — §6.5
- "`.env` never replaced; `example.env` documented" — §11

M7 also ships RFC-0011 **thin**. The CLI therefore MUST tolerate an empty graph and a WorkingSet with no graph projection without degrading its own behaviour (**IX7**).

### 2.3 Relationship to merged code

Every identifier below is a merged public item this RFC calls. Nothing here requires a merged signature to change *except* the additive amendments of §5.6, which are called out inline.

| Merged symbol | Crate / path | Used by |
| --- | --- | --- |
| `ConfigPaths::for_workspace`, `RuntimeConfig::load` | `alloy-runtime::config` | §5.4 |
| `AlloyRuntime::{new, configure, start, drain, shutdown, handle}` | `alloy-runtime::runtime::lifecycle` | §6.2, §6.3 |
| `RuntimeHandle::{phase, cancellation, config, set_scheduler, run_dag, cancel_dag}` | `alloy-runtime::runtime::handle` | §6.2 |
| `install_sqlite_event_sink` | `alloy-runtime::storage::install` | §6.2 step 3 |
| `AlloyStorage::{open, events, artifacts, sessions, dags, close}` | `alloy-runtime::storage` | §6.2 |
| `SessionPlane::{new, sessions, runs, approve, cancel, register_gate_waiter, signal_budget_warning}` | `alloy-runtime::session::plane` | §6.2, §7 |
| `SessionService::{create, resume, submit_goal, events}`, `MAX_EVENTS_PAGE`, `clamp_events_page_limit` | `alloy-runtime::session::traits` | §7 |
| `RunController::{start, cancel, approve, request_replan, expire_gate}` | `alloy-runtime::session::traits` | §7, §8 |
| `RunGoalRecord { goal, dag_id }` | `alloy-runtime::session::goal_record` | §7.1 |
| `validate_mvp_profile`, `MVP_PROFILES` | `alloy-runtime::session::profiles` — **crate-private today**; the additive re-export is amendment **A1** | §5.1 |
| `TemplatePlanService::{new, from_storage}`, `PlanService::{plan, load_template, replan}`, `PlanContext`, `PlanResult`, `TemplateId::RepairLocalDiagnostic` | `alloy-runtime::planner` | §7.1 |
| `LinearScheduler`, `LinearSchedulerDeps`, `SchedConfig::new` | `alloy-runtime::scheduler::linear` | §6.2 step 12 |
| `DagOutcome { dag_id, generation, state, failed_node, failure }`, `DagState` | `alloy-runtime::scheduler::traits` | §9.3 |
| `McpVerifyCompileAdapter::new`, `McpVerifyTestAdapter::new` | `alloy-runtime::adapters::verify` | §6.2 step 8 |
| `SessionGateHumanAdapter::new(plane)` | `alloy-runtime::adapters::gate` | §6.2 step 8, §8 |
| `SessionVerifyPermissions::new(sessions, compile_args_glob, test_args_glob)` | `alloy-runtime::adapters::perms` | §6.2 step 8 |
| `ToolCaller`, `ToolCallerError`, `UnavailableCapabilityExecutor`, `Approval` | `alloy-runtime::adapters` | §6.2, §8 |
| `EventDecisionLog::{new, from_handle}`, `DecisionKind::Custom`, `ProcessCostMeterFactory`, `SharedCostMeter`, `CostSnapshot`, `RetentionPolicy` | `alloy-runtime::obs` | §6.2, §7.7, §10 |
| `list_decision_events`, `DecisionPage`, `parse_decision_event`, `parse_model_call_event`, `parse_tool_call_event`, `reaccumulate_cost_from_events` | `alloy-runtime::obs::query` | §7.2 |
| `SessionEvent { seq, ts, session_id, run_id, type_, payload }`, `SessionEventType::*` | `alloy-runtime::events` | §7.2, §7.7 |
| `CreateSession`, `Goal`, `Constraint`, `BudgetPolicy` | `alloy-runtime::types::budget` | §7.1 |
| `ErrorClass`, `FailureIr` | `alloy-runtime::types::diagnostic` | §9.3 |
| `SessionError`, `RunError`, `RuntimeError` | `alloy-runtime::error` | §9.2 |
| `logging::init_tracing` | `alloy-runtime::logging` — **writes to stdout**; the stderr variant is amendment **A5** | §7.7 |
| `NativeSandboxBroker::{new, with_operator_homes}`, `load_sandbox_profile(profile_toml, fs_jail)`, `SandboxProfile`, `OperatorHomes::resolve` | `alloy-tools::sandbox` | §6.2 step 5 |
| `InProcessMcpHost::new(broker, homes, read_only_roots, patch_backend, config)`, `McpHostConfig`, `ToolHandle::new`, `ToolHandleToolCaller::new` | `alloy-tools::mcp` | §6.2 steps 6–7 |
| `GitEditEngine::new(GitEditEngineConfig)`, `EditEnginePatchBackend::new` | `alloy-tools::edit` | §6.2 step 6 |
| `TomlModelRouter::from_paths(router, budget, example_env, decisions, meter, bound_run)`, `RouterConfig` | `alloy-runtime::router` | §6.4, Appendix D.3 |
| `SqliteProjectGraph::{open, rebuild_reported}`, `GraphOpenOptions::for_data_dir`, `GraphLayout`, `IngestLimits`, `GraphMetricsSnapshot` | `alloy-index` | §10 |

### 2.4 Downstream RFCs this RFC wires

| RFC | Status for wiring | Boundary |
| --- | --- | --- |
| **RFC-0013** — Capability Registry & Workers | Implementation-grade spec; the CLI constructs the registry and injects it as `LinearSchedulerDeps.capabilities` | 0013 owns worker logic and `RW6` target resolution; 0015 owns wiring only. **CR12**'s honest-error fallback applies only where an executor is genuinely absent from a build |
| **RFC-0012** — Context Engine | Shipped `context_profile` flow: `RuntimeConfig::load` parses `[context]` into `context_profile`, and the CLI passes it to the `ContextEngine` it constructs for workers (**PF13**) | 0012 owns assembly and `DomainWeights`; 0015 owns parsing, validation, and injection |
| **RFC-0016** — Eval | Offline holdout runs against a loopback OpenAI-compatible scripted server (Appendix D.3) | 0016 owns fixtures and gates; `alloy-cli` never depends on `alloy-eval` (**T9**) |

---

## 3. What this RFC does not decide

To keep review focused, these are explicitly *someone else's* decisions and the CLI only surfaces their results:

- **Whether a node retries.** RFC-0010 §5.11. The CLI renders attempts from `NodeState` events.
- **Whether a gate is required.** RFC-0009 templates + RFC-0010 gate policy. The CLI renders the gate it is told about.
- **Whether the budget is exhausted.** `SharedCostMeter::check_budget` + RFC-0010. The CLI renders `BudgetWarning` events.
- **Which model is called.** `TomlModelRouter` + `router.toml`. The CLI never names a model id (**B3**).
- **How a worker finds its target.** RFC-0013 `RW6`. The CLI passes goal text through unchanged (Appendix C note).
- **Whether a patch is correct.** The verify adapters + `cargo check` under sandbox.

---

## 4. Command grammar

### 4.1 Rules

| Rule | Statement |
| --- | --- |
| **CL1** | The binary name stays `alloy` (`[[bin]] name = "alloy"`). The existing `host` subcommand and its lifecycle behaviour are preserved unchanged; this RFC only adds siblings. |
| **CL2** | Every subcommand accepts `--workspace <PATH>` (default `.`) and resolves config via `ConfigPaths::for_workspace`. There is no implicit workspace discovery by walking parents. |
| **CL3** | Every subcommand accepts `--json`. When set, stdout carries exactly one JSON document (or one per line for streaming subcommands, §7.7) and nothing else. |
| **CL4** | Every subcommand accepts `--profile <default\|autonomous\|readonly>`. The value is validated by `validate_mvp_profile` before any I/O. Unknown values exit `EX_USAGE`. |
| **CL5** | Ids on the command line are UUID strings parsed with the merged `*::parse` associated functions (`SessionId::parse`, `RunId::parse`, `GateId::parse`). A malformed id is a usage error, not a not-found error. |
| **CL6** | No subcommand takes a model id, endpoint, tier, template id, retry count, or timeout as a flag. Those live in `router.toml` and the profile. Exception: `run --template <id>` is permitted **only** behind `--dry-run` (**CL12**). |
| **CL7** | `--quiet` suppresses progress rendering on stderr; it never changes stdout or the exit code. `--verbose` raises the tracing filter; it never changes stdout in `--json` mode. |
| **CL8** | Long-running subcommands (`run`, `index`) install the same SIGINT/SIGTERM handler the merged `host` command uses (§6.3). Short subcommands install none. |
| **CL9** | Argument parsing MUST complete before any file, network, or process I/O, so `--help` and `--version` work in a directory with no config at all. |
| **CL10** | Help text is snapshot-tested (§12). Changing a flag name or default is a deliberate act that updates a snapshot. |
| **CL11** | Unknown flags, missing required args, and conflicting flags exit `2` (clap's default `EX_USAGE`). |
| **CL12** | `run --dry-run` plans and prints the DAG without dispatching it: `PlanService::plan` runs, `RunController::start` does not. |

### 4.2 Grammar

```text
alloy [--workspace PATH] [--profile ID] [--json] [--quiet|--verbose] <SUBCOMMAND>

alloy run <GOAL>
    [--workspace PATH]           # default "."
    [--profile default|autonomous|readonly]
    [--session ID]               # reuse an existing session instead of creating one
    [--max-usd FLOAT]            # Constraint::MaxUsd; may only lower the profile ceiling
    [--require-cargo-check]      # Constraint::RequireCargoCheck (implied by default profile)
    [--yes]                      # pre-approve gates non-interactively (refused by readonly)
    [--no-input]                 # never prompt; a gate becomes EX_GATE_REQUIRED
    [--dry-run]                  # plan + print DAG; do not start
    [--template ID]              # dry-run only (CL12)
    [--no-index]                 # skip the bootstrap graph rebuild (IX3)
    [--json]

alloy events
    [--session ID]               # default: the last_session marker (SQ5b, SQ8b)
    [--run ID]                   # filter to one run
    [--after SEQ]                # exclusive cursor
    [--limit N]                  # default 100, clamped by clamp_events_page_limit
    [--decisions-only]           # Decision | ModelCall | ToolCall via list_decision_events
    [--follow]                   # poll until run terminal or Ctrl-C
    [--json]

alloy approve
    --run ID
    --gate ID
    --decision allow|deny|allow-once
    [--json]

alloy cancel
    --run ID
    [--json]

alloy resume
    --session ID
    [--run ID]                   # default: the session's single non-terminal run
    [--json]

alloy index
    [--workspace PATH]
    [--rebuild]                  # force full rebuild (default: rebuild if stale)
    [--stats]                    # print GraphMetricsSnapshot and exit
    [--json]

alloy host                       # unchanged from RFC-0001
    [--workspace PATH]
```

### 4.3 Flag semantics that are easy to get wrong

| Flag | Normative meaning |
| --- | --- |
| `--profile` | Selects the **catalog profile**: which `profiles/<id>.toml` is loaded *and* the `ProfileId` written into `CreateSession.profile`. |
| `--max-usd` | Appends `Constraint::MaxUsd(v)`. Rejected when `v > profile [budgets].max_usd_per_run` — a constraint may only tighten (**PF11**). |
| `--yes` | Answers *future* gates with `Approval::Allow`. It does not retroactively approve a denied gate, and is refused under `readonly` (**PF9**). |
| `--no-input` | Mutually exclusive with `--yes`. Makes a gate terminal with `EX_GATE_REQUIRED` and the gate id on stdout, so CI can approve out of band. |
| `--follow` | Polls `SessionService::events` with the merged cursor contract; it does not open a subscription (no such API exists) and does not tail the SQLite file. |
| `--after` | Exclusive, matching `SessionService::events`: `after: Some(seq)` returns `seq > after`. |

---

## 5. Profiles and configuration

### 5.1 Catalog

| Rule | Statement |
| --- | --- |
| **PF1** | The catalog is exactly `MVP_PROFILES = ["default", "autonomous", "readonly"]`. `validate_mvp_profile` is the single validator; the CLI MUST NOT maintain a second list. Both items are re-exported additively by amendment **A1** — today `session::profiles` is crate-private and `alloy-cli` cannot name either. |
| **PF2** | Each catalog id maps to `<workspace>/profiles/<id>.toml`. `profiles/default.toml` already exists and is the Appendix B baseline; this RFC adds `profiles/autonomous.toml` and `profiles/readonly.toml`. |
| **PF3** | A profile file MUST parse into the same struct for all three ids. There is no per-profile schema; profiles differ only in values. |
| **PF4** | A profile file's `[profile].id` MUST equal the catalog id selected. A mismatch is a config error (`EX_CONFIG`), never a silent reinterpretation. |
| **PF5** | The `[sandbox]` table is parsed **twice** from the same file — once by `RuntimeConfig::load` (amendment A1) and once by `load_sandbox_profile(profile_toml, fs_jail)` in `alloy-tools`. The composition root MUST pass the identical path to both and MUST assert the readings agree on `network` and `quarantine_deps` (**CR6**). |

### 5.2 The three profiles, table by table

Baseline is Architecture V2 Appendix B. Cells marked **=** are identical to `default`.

**`[profile]`**

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `id` | `"default"` | `"autonomous"` | `"readonly"` |
| `description` | `"Correctness-first Rust profile"` | `"Fewer human gates; same verification and sandbox"` | `"Inspect and plan only; no workspace writes"` |

**`[gates]`**

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `require_cargo_check` | `true` | **=** (MUST stay `true`) | **=** |
| `require_human_on_public_api` | `true` | `false` | `true` (moot — no edits) |
| `require_human_on_new_unsafe` | `true` | `false` | `true` (moot) |
| `require_human_on_new_dependency` | `true` | `false` | `true` (moot) |
| `allow_raw_bash` | `false` | **=** (MUST stay `false`) | **=** (MUST stay `false`) |

**`[sandbox]`**

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `check` | `"landlock"` (`"seatbelt"` on macOS; `"container"` acceptable) | **=** | **=** |
| `test` | `"container"` | **=** | **=** |
| `network` | `"deny"` | **=** (MUST stay `"deny"`) | **=** (MUST stay `"deny"`) |
| `quarantine_deps` | `true` | **=** (MUST stay `true`) | **=** (MUST stay `true`) |

**`[budgets]`**

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `max_usd_per_run` | `5.0` | operator-set, ≥ `0.0` | `0.0` recommended |
| `max_tokens_per_run` | `2_000_000` | operator-set | operator-set |
| `max_parallel_nodes` | `1` | **=** (MUST stay `1`) | **=** (MUST stay `1`) |
| `max_parallel_cargo` | `1` | **=** (MUST stay `1`) | **=** (MUST stay `1`) |
| `max_parallel_edits` | `1` | **=** (MUST stay `1`) | **=** (MUST stay `1`) |

**`[context]`** — parsed into `RuntimeConfig.context_profile`, consumed by the RFC-0012 `ContextEngine` (**PF13**)

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `total_token_budget` | `32_000` | operator-set | **=** |
| `weights.conversation` | `0.20` | operator-set | **=** |
| `weights.working_set` | `0.55` | operator-set | **=** |
| `weights.artifacts` | `0.25` | operator-set | **=** |

**`[observability]`**

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `retain_full_prompts` | `false` | **=** | **=** |
| `retain_tool_bodies` | `false` | **=** | **=** |

**`[limits]`** (new table, amendment A2)

| Key | default | autonomous | readonly |
| --- | --- | --- | --- |
| `run_timeout_secs` | `1800` | operator-set | **=** |
| `gate_timeout_secs` | unset (wait indefinitely) | operator-set | **=** |

### 5.3 Override authority

| Rule | Statement |
| --- | --- |
| **PF6** | `[gates].allow_raw_bash` MUST be `false` in every catalog profile. A profile setting it `true` is a config error (V2 §12.1). Not an operator-tunable knob in MVP. |
| **PF7** | `[gates].require_cargo_check` MUST be `true` in every catalog profile. Autonomy removes *human* gates, never *verification* (V2 §3.4). |
| **PF8** | `[sandbox].network` MUST be `"deny"` and `[sandbox].quarantine_deps` MUST be `true` in every catalog profile. `load_sandbox_profile` already rejects `network = "allow"`; the CLI MUST also reject it before broker construction so the error names the profile file. |
| **PF9** | Under `readonly`, `alloy run` without `--dry-run` MUST exit `EX_PROFILE_REFUSED` before creating a session, because the only MVP template (`RepairLocalDiagnostic`) contains an `Edit` node. `--yes` under `readonly` is a usage error. |
| **PF10** | Under `readonly`, the composition root MUST construct `SessionVerifyPermissions::new(sessions, compile_args_glob, None)` — no test-class glob — and MUST NOT construct a `GitEditEngine` or register `apply_patch`'s write path. Refusal is structural, not a check inside a handler. |
| **PF11** | A CLI constraint may only tighten a profile ceiling. `--max-usd` above `[budgets].max_usd_per_run` is a usage error naming both numbers. |
| **PF12** | `[budgets].max_parallel_*` MUST all equal `1`. Any other value is a config error, because `SchedConfig.host_parallel_honesty = true` would then be a lie (RFC-0010 §3.12). |
| **PF13** | `[context]` is parsed by `RuntimeConfig::load` into `context_profile` and passed to the `ContextEngine` the composition root builds for workers (RFC-0012 downstream contract, Appendix D.2). **`RuntimeConfig::load` owns the sum-to-one check**: the merged `DomainWeights::validate` enforces finiteness and non-negativity but **not** that the three live-domain weights sum to `1.0`, so a profile with `0.2 / 0.2 / 0.2` passes 0012's validator and silently under-fills every pack. `RuntimeConfig::load` MUST additionally reject weights not summing to `1.0 ± 1e-6`, and MUST reject `total_token_budget == 0`. Both are config errors. |
| **PF14** | Unknown top-level tables in a profile file are an error, not a warning. Unknown *keys* inside a known table are likewise rejected (`#[serde(deny_unknown_fields)]`). |

### 5.4 Precedence

The merged code already fixes part of this order and the RFC MUST NOT contradict it. `RuntimeConfig::load` checks `ALLOY_DATA_DIR` **before** the programmatic `ConfigPaths.data_dir` override that a CLI flag would populate, and `ConfigPaths::for_workspace` lets `ALLOY_PROFILE` / `ALLOY_ROUTER` displace the workspace defaults. Therefore:

| Rule | Statement |
| --- | --- |
| **PR1** | Precedence, highest first: **(1)** process environment (`ALLOY_DATA_DIR`, `ALLOY_PROFILE`, `ALLOY_ROUTER`, other `ALLOY_*` knobs); **(2)** CLI flags; **(3)** the profile TOML; **(4)** built-in defaults in merged code. Env beats flags because that is what merged `resolve_data_dir` does today, and a divergence between "what the CLI says" and "what the runtime does" is worse than an unusual order. |
| **PR2** | `--workspace` is not subject to PR1: it is the *input* to path resolution. Relative paths resolve against the process CWD; `ConfigPaths::for_workspace` then joins once (never twice — merged regression `for_workspace_does_not_double_join_relative_root`). |
| **PR3** | `--profile <id>` sets the *catalog id*; `ALLOY_PROFILE` sets the *file path*. When both are present, `ALLOY_PROFILE`'s file is loaded and its `[profile].id` MUST equal `--profile`'s id, else `EX_CONFIG` (**PF4**). |
| **PR4** | Resolution is reported, not guessed: every `--json` invocation MUST include `config.data_dir`, `config.data_dir_rule` (the merged `RuntimeConfig.data_dir_rule` string), `config.profile_path`, and `config.router_path`. |
| **PR5** | Alloy MUST NOT read a `.env` file at any precedence level. Environment means *process* environment. |
| **PR6** | No config value is cached across processes. Each invocation re-resolves. There is no `~/.alloyrc`, no user-global profile, and no XDG *config* directory in MVP — only the XDG *data* fallback `resolve_data_dir` already implements. |

### 5.5 Fail-closed config validation

Validation order, first failure wins, all before any session row is written:

1. `--profile` in `MVP_PROFILES` (`validate_mvp_profile`) — else `EX_USAGE`.
2. Profile file exists and parses (`RuntimeConfig::load`) — else `EX_CONFIG` naming the path and `example.env`.
3. `[profile].id` matches the selected catalog id (**PF4**).
4. `[gates]`, `[sandbox]`, `[budgets]`, `[context]`, `[limits]` satisfy **PF6**–**PF13**.
5. `router.toml` exists (`RuntimeConfig::load` already requires the file) — else `EX_CONFIG` naming `router.toml.example`.
6. Router constructs, meaning the `api_key_env` variable is set and non-empty (`TomlModelRouter::from_paths`) — else `EX_CONFIG` naming the variable and `example.env`. Skipped under `--dry-run` and by `events` / `approve` / `cancel` / `index` (**CR11**).
7. Sandbox `check` backend is available on this host (`NativeSandboxBroker::new` fails closed) — else `EX_SANDBOX`. Skipped under `--dry-run`.

### 5.6 Authorised amendments to merged crates

Five additive amendments to `alloy-runtime`. None reshapes an existing field; each is required for the CLI to be more than a stub.

| ID | Amendment | Justification |
| --- | --- | --- |
| **A1** | `RuntimeConfig` gains parsed `[gates]`, `[sandbox]` (an opaque echo used only for the **CR6** cross-check), `[context]` → `context_profile` **including PF13's sum-to-one check**, and the full `[budgets]` table including `max_parallel_*`. Additionally, `session::profiles` is re-exported so `validate_mvp_profile` and `MVP_PROFILES` are nameable from `alloy-cli` (**PF1**). Today `config.rs` parses only `max_usd_per_run` / `max_tokens_per_run` plus `[observability]`, so a profile declaring `max_parallel_nodes = 4` is silently ignored; after A1 it is an error (**PF12**). |
| **A2** | `RuntimeConfig.run_timeout` becomes profile-driven via a new `[limits]` table, defaulting to the merged hard-coded `Duration::from_secs(60 * 30)`. `LinearSchedulerDeps.run_timeout` is otherwise unsettable from config. |
| **A3** | `SessionRows` gains `async fn set_graph_version(&self, id: SessionId, version: GraphVersion) -> Result<(), StoreError>`. The `sessions.graph_version` column exists and `upsert_session` writes `NULL` with no other writer; RFC-0011 Appendix E.4 item 2 assigns the write to this RFC. Until A3 lands, **IX5** applies. |
| **A4** | `RunController::approve` gains a **durable fallback**: when no in-process waiter is registered for `(run, gate)` but a durable `ApprovalRequested` event for that pair exists, `approve` persists the resolution (row transition + `ApprovalResolved`) instead of returning `RunError::UnknownGate`. This follows the merged `expire_gate` precedent (amendment A7: a missing waiter is not an error). **Cross-process `alloy approve` depends on it** — the merged implementation resolves an in-process waiter only, so a second process could never approve, and no CI workflow could use `--no-input` + `approve`. `UnknownGate` remains the answer when there is neither a waiter nor a durable `ApprovalRequested`. |
| **A5** | `alloy-runtime::logging` gains `init_tracing_stderr()` alongside the merged `init_tracing()`. The merged initializer writes to **stdout**, interleaving tracing lines into `alloy … --json` output and corrupting it. Every CLI subcommand uses the stderr variant (**OUT1**, **T11**); `host` may keep either. |

All five are backward compatible: absent tables keep today's defaults, `set_graph_version` is additive on a trait with one production impl, A4 only converts a former error into a success on a strictly narrower condition, and A5 adds a function without changing `init_tracing`.

---

## 6. The composition root

### 6.1 Shape

```text
alloy (single process)
  ├── clap parse ────────────────► argv only, no I/O            (CL9)
  ├── config resolve ────────────► ConfigPaths → RuntimeConfig   (§5.4)
  ├── composition root ──────────► assemble_read | assemble_full (CR1)
  │     ├── AlloyRuntime + storage + SessionPlane          [both]
  │     ├── ProjectGraph (alloy-index)                     [both]
  │     ├── SandboxBroker → InProcessMcpHost → ToolCaller   [full]
  │     ├── verify / gate / perms adapters                  [full]
  │     ├── ContextEngine (RFC-0012, from context_profile)  [full]
  │     ├── CapabilityRegistry (RFC-0013)                   [full]
  │     └── LinearScheduler → RuntimeHandle::set_scheduler   [full]
  ├── subcommand handler ────────► control-plane calls only      (B1)
  └── renderer ──────────────────► human or JSON                 (§7.7)
```

### 6.2 Construction order (normative)

The order below is forced by merged phase constraints, not chosen for taste. `set_scheduler` and `install_sqlite_event_sink` both accept `Configured | Running`; `SessionPlane`'s mutating operations require `Running`; `LinearSchedulerDeps` requires a `SessionPlane`. The only order satisfying all three builds everything at `Configured` and starts last.

| Step | Action | Merged call | Phase | In `assemble_read` |
| --- | --- | --- | --- | --- |
| 1 | Resolve paths and config | `ConfigPaths::for_workspace`, `RuntimeConfig::load` | — | yes |
| 2 | Create + configure the runtime | `AlloyRuntime::new`, `rt.configure(cfg)` | `Created → Configured` | yes |
| 3 | Open storage, install durable sink | `install_sqlite_event_sink(&handle, None)` | `Configured` | yes |
| 4 | Open the project graph | `SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(cfg.data_dir))` | `Configured` | yes |
| 5 | Build the sandbox broker | `load_sandbox_profile(&cfg.profile_path, workspace_root)` → `NativeSandboxBroker::with_operator_homes(profile, homes)` | `Configured` | no |
| 6 | Build edit engine + patch backend | `GitEditEngine::new(GitEditEngineConfig { broker, path_policy, trusted_path, artifacts, events, .. })` → `EditEnginePatchBackend::new(engine)` | `Configured` | no |
| 7 | Build MCP host and tool seam | `InProcessMcpHost::new(broker, homes, read_only_roots, patch_backend, McpHostConfig::new().max_in_flight(1))` → `ToolHandle::new(host, selectors)` → `ToolHandleToolCaller::new(handle)` | `Configured` | no |
| 8 | Build the control plane, then adapters | `SessionPlane::new(handle.clone(), Arc::clone(&storage))`; then `SessionVerifyPermissions::new(...)`, `McpVerifyCompileAdapter::new(...)`, `McpVerifyTestAdapter::new(...)`, `SessionGateHumanAdapter::new(plane)` | `Configured` | plane only |
| 9 | Build observability | `EventDecisionLog::from_handle(handle, storage)`, `ProcessCostMeterFactory::new()` | `Configured` | yes |
| 10 | Build the context engine | RFC-0012 engine from `cfg.context_profile` + storage + graph | `Configured` | no |
| 11 | Build the capability registry | RFC-0013 registry over `(router factory, ToolHandle, GraphViewHandle, ContextEngine)` | `Configured` | no |
| 12 | Build and install the scheduler | `LinearScheduler::new(LinearSchedulerDeps { .. })` → `handle.set_scheduler(sched)` | `Configured` | no |
| 13 | Start | `rt.start().await` | `Configured → Running` | yes |
| 14 | Run the subcommand handler | §7 | `Running` | yes |

| Rule | Statement |
| --- | --- |
| **CR1** | Construction is **two** functions over one `Assembly` type: `assemble_read` (steps 1–4, the `SessionPlane` half of 8, 9, 13) and `assemble_full` (all steps). The split is forced by **CR11** — a single constructor would make `alloy events` probe sandbox backends. Neither uses a lazy `OnceCell`, a global, or a `static`; `assemble_full` MUST be written as `assemble_read` plus the remaining steps, not as a parallel copy that can drift. |
| **CR2** | `LinearSchedulerDeps.runs` MUST be `session_plane.runs()` — the same `Arc`, satisfying merged rule D6. Constructing a second `RunController` is forbidden. |
| **CR3** | `LinearSchedulerDeps.runtime_cancel` MUST be `handle.cancellation()`, the same token the signal task cancels (§6.3). |
| **CR4** | `LinearSchedulerDeps.budget_policy` MUST be `cfg.budget_policy` (profile-derived), and `run_timeout` MUST be `cfg.run_timeout` (amendment A2). |
| **CR5** | `SchedConfig::new(cfg.data_dir)` is used unmodified except that `data_dir` MUST be made absolute first — merged check N2 rejects a relative `data_dir`, and `--workspace .` yields a relative one. |
| **CR6** | The `[sandbox]` values `RuntimeConfig` parsed (A1) and those `load_sandbox_profile` parsed MUST agree on `network` and `quarantine_deps`. Disagreement is an internal error: two parsers read one file differently. |
| **CR7** | `McpHostConfig.max_in_flight` MUST be `1`, not the crate default `64`, because `SchedConfig.host_parallel_honesty = true` asserts the host is pinned. |
| **CR8** | `McpHostConfig.cancel` MUST be a child of `handle.cancellation()` so one Ctrl-C reaches in-flight tool calls. |
| **CR9** | `read_only_roots` passed to `InProcessMcpHost::new` MUST be empty in MVP beyond the workspace jail. |
| **CR10** | `OperatorHomes::resolve()` is called **once** and the same value passed to both `NativeSandboxBroker::with_operator_homes` and `InProcessMcpHost::new`, per the merged requirement that exec pre-checks resolve against the same trusted roots. |
| **CR11** | Subcommands that make no model call (`events`, `approve`, `cancel`, `index`, `run --dry-run`) MUST use `assemble_read`: no broker probe, no MCP host, no context engine, no registry, no scheduler. `alloy events` on a laptop without a container runtime MUST work. |
| **CR12** | Where a `CapabilityExecutor` is genuinely unavailable in a build — not merely unconfigured — the assembly injects `UnavailableCapabilityExecutor` and the CLI surfaces its error class honestly (`ErrorClass::Internal` → `EX_INTERNAL` naming the missing component), never as a fake success. On a tree where RFC-0013 is present the real registry is wired and this path is unreachable; the rule exists so a stripped or misconfigured build fails loudly rather than silently succeeding at nothing. |
| **CR13** | Nothing constructed in step 5 or later may be constructed twice per process. Two brokers means two policy digests; two MCP hosts means two in-flight budgets. |

### 6.3 Shutdown and signals

Reuse the merged `host` lifecycle verbatim — it is already correct and already tested by `host_e2e.rs`:

| Rule | Statement |
| --- | --- |
| **CR14** | The SIGINT/SIGTERM task is armed **before** `rt.start()`, so a signal during startup I/O aborts startup (merged `run_host`: "Arm SIGINT/SIGTERM → cancellation before start so startup I/O can abort"). |
| **CR15** | The signal handler's only action is `cancel.cancel()`. It does not write, does not print, and does not exit the process. |
| **CR16** | Shutdown order reverses construction: (1) cancel token; (2) `rt.drain(grace)` when phase is `Running`; (3) `rt.shutdown()`; (4) `storage.close()`; (5) graph close. `graceful_shutdown` already implements (2)+(3) and MUST be extended, not replaced. |
| **CR17** | Default grace is `Duration::from_secs(10)`, matching the merged `host` path. A second signal during drain escalates to immediate `shutdown()` with `EX_CANCELLED`. |
| **CR18** | A cancelled run exits `EX_CANCELLED` (not `0`, not `EX_INTERNAL`) and prints the run id so `alloy resume` can be used. |
| **CR19** | On cancel, the CLI MUST print that the workspace may be modified when **an `EditApplied` session event exists for the run**, naming the checkpoint. RFC-0010 FOW5 lists three dirty-if legs; only this one is derivable here, because merged `NodeState` payloads carry `node_id` but **no node kind**, so "an `Edit` node reached `Succeeded`" and "a `NodeState → running` for an `Edit` node" cannot be evaluated without a DAG topology lookup the CLI must not perform (**B2**). Absence of an `EditApplied` event MUST NOT be printed as proof the tree is clean: the notice is one-directional, and its absence is silence, not a clean bill of health. |

### 6.4 Per-run versus per-process objects

`TomlModelRouter::from_paths` takes `bound_run: RunId`. The router is therefore **not** a process singleton.

| Rule | Statement |
| --- | --- |
| **CR20** | The router is constructed **after** `submit_goal` returns a `RunId`, once per run, with `cost_meter = cost_meters.for_run(run)` and `decision_log` the process `EventDecisionLog`. The capability registry (step 11) therefore receives a router *factory*, not a router. |
| **CR21** | The cost meter is per run, obtained from `ProcessCostMeterFactory`, and released on run terminal. |
| **CR22** | Everything else in §6.2 is per process. |

### 6.5 The boundary rule

| Rule | Statement |
| --- | --- |
| **B1** | `alloy-cli` contains **no** planning, scheduling, retry, budget-policy, verification, or graph-mutation logic. Its functions parse, resolve, construct, call, and render. |
| **B2** | `alloy-cli` MUST NOT import `scheduler::linear::*` internals or inspect DAG topology. Permitted scheduler-facing imports are `Scheduler`, `SchedConfig`, `LinearScheduler`, `LinearSchedulerDeps`, `DagOutcome`, `DagState`, and `TemplateId` — construction and result types only. |
| **B3** | `alloy-cli` MUST NOT contain a model id, endpoint URL, provider name, tier name, or price literal. |
| **B4** | `alloy-cli` MUST NOT construct a `PermissionToken` or a `Grant`. |
| **B5** | `alloy-cli` MUST NOT call `EditEngine::apply` or `EditEngine::rollback`, and MUST NOT call `ProjectGraph::record_diagnostic` / `record_fix` (RFC-0011 IN1/SEC4). It MAY call `rebuild` — that trigger is this RFC's (§10). |
| **B6** | `alloy-cli` MUST NOT read the SQLite files directly. Every read goes through `SessionService`, `obs::query`, or `SqliteProjectGraph`. |
| **B7** | `alloy-cli` MUST NOT spawn a process other than through the sandbox broker — no `std::process::Command`, no `git`, no `cargo`. |
| **B8** | Rules B2–B7 are enforced by CI greps (§12.3), not by review vigilance. |

---

## 7. Subcommand semantics

### 7.1 `alloy run`

| Step | Call | Notes |
| --- | --- | --- |
| 1 | §5.5 validation | Fail closed before any write |
| 2 | `assemble_full` (§6.2) | Steps 1–13 |
| 3 | `sessions.create(CreateSession { workspace_root, profile, budget, language_backends: vec![LanguageId::new("rust")?] })` | Skipped when `--session` names an existing session; the provenance seam of Appendix E attaches here when it merges |
| 3b | Write the `last_session` marker (**SQ5b**) | So a later bare `alloy events` finds this session |
| 4 | `sessions.submit_goal(session, Goal { text, constraints, attachments: vec![] })` | Returns `RunId`; persists `RunGoalRecord { goal, dag_id }` |
| 5 | `TomlModelRouter::from_paths(router_path, budget, example_env, decisions, meter, run)` | Per-run (**CR20**) |
| 6 | `plan_service.plan(PlanContext { session_id, run_id, dag_id, goal, template_override, policy_hash, tool_versions, compiler_fingerprint })` | Emits `PlanProduced`. Under `--dry-run` the handler stops here |
| 7 | `runs.start(run)` | Dispatches to `RuntimeHandle::run_dag(dag_id)` → `LinearScheduler`; blocks until `DagOutcome` |
| 8 | Progress rendering | Poll `sessions.events(session, after, limit)`; render `NodeState`, `EditApplied`, `ApprovalRequested`, `BudgetWarning` (§7.7) |
| 9 | Gate handling | §8 |
| 10 | Terminal render + exit code | §9.3 |
| 11 | Shutdown | §6.3 |

| Rule | Statement |
| --- | --- |
| **SQ1** | The CLI MUST NOT select the template itself except by passing `--template` through as `PlanContext.template_override` under `--dry-run`. |
| **SQ2** | The CLI MUST NOT call `Scheduler::run` or `RuntimeHandle::run_dag` directly. `RunController::start` is the only entry (RFC-0010 §2.4). |
| **SQ3** | The CLI MUST NOT retry a failed run. `ReplanRequired` is rendered with the suggested `alloy resume` command. |
| **SQ4** | Progress polling MUST use the merged cursor contract, including that empty `events` with `Some(next_after)` means "keep paging", not "done". |
| **SQ5** | `--session <id>` reuses a session; the profile on that session row wins over `--profile`, and a mismatch is `EX_USAGE` rather than silent re-profiling. |
| **SQ5b** | On successful session create, the CLI writes `<data_dir>/cli/last_session` containing the `SessionId`. This is a CLI-owned convenience marker, permitted by **SEC5** (inside the data dir; nothing in the workspace, nothing in `$HOME`). It is advisory only: it may be stale, it is never a source of truth, and no other crate reads it. |
| **SQ5c** | The goal text is passed to `submit_goal` **verbatim**. The CLI MUST NOT rewrite it, append a target, or infer a file (RFC-0013 `RW6` owns targeting). Where the goal names no target, see the Appendix C note. |

### 7.2 `alloy events`

| Step | Call |
| --- | --- |
| 1 | Config resolve; `assemble_read` (**CR11**) |
| 2 | Session selection: `--session`, else the `<data_dir>/cli/last_session` marker |
| 3 | `sessions.events(session, after, limit)` — or `obs::query::list_decision_events(store, session, after, limit)` with `--decisions-only` |
| 4 | Optional `--run` display filter on `SessionEvent.run_id` |
| 5 | Render (§7.7); print the resume cursor |

| Rule | Statement |
| --- | --- |
| **SQ6** | `--limit` is clamped by `clamp_events_page_limit` (1..=`MAX_EVENTS_PAGE` = 1000). A larger value is clamped and reported on stderr, not rejected. |
| **SQ7** | `--follow` polls with a fixed backoff (250 ms → 2 s), stops on a `RunCompleted` event for the followed run, and exits `0`. It never holds a write transaction. |
| **SQ8** | Payloads are rendered as stored. The CLI MUST NOT re-derive costs, durations, or verdicts; `reaccumulate_cost_from_events` is the only permitted derivation and only for the cost summary line. |
| **SQ8b** | Session selection without `--session` reads `<data_dir>/cli/last_session`. **No merged API enumerates sessions** — `SessionRows` exposes `get_session` and `list_runs`, not `list_sessions` — so "the most recent session in this workspace" is not derivable and MUST NOT be faked by scanning the database (**B6**). When the marker is absent, unreadable, or names a session that no longer exists, the CLI MUST say so plainly ("no recent session recorded for this workspace; pass `--session <id>`") and exit `EX_USAGE`. It MUST NOT guess. |

### 7.3 `alloy approve`

| Step | Call |
| --- | --- |
| 1 | `assemble_read` (**CR11**) |
| 2 | `Approval` from `--decision`: `allow` → `Approval::Allow`, `deny` → `Approval::Deny`, `allow-once` → `Approval::AllowOnce` |
| 3 | `plane.approve(run, gate, decision)` → `RunController::approve` |
| 4 | Render the resulting `ApprovalResolved` event |

| Rule | Statement |
| --- | --- |
| **SQ9** | `approve` is valid from a **different process** than the one running the DAG; that is its primary use. This requires amendment **A4**: the merged `RunController::approve` resolves an *in-process* waiter and returns `RunError::UnknownGate` when none is registered, so without A4 a second process can never approve and the `--no-input` + `approve` CI workflow (**GA5**) is unreachable. With A4, a missing waiter plus a durable `ApprovalRequested` persists the resolution, and the running process observes it through the durable path. |
| **SQ10** | Approving an unknown `(run, gate)` — no waiter *and* no durable `ApprovalRequested` — maps `RunError::UnknownGate` → `EX_NOT_FOUND`; approving a terminal run maps `RunError::InvalidPhase` → `EX_STATE`. Neither is retried. |
| **SQ11** | `allow-once` is passed through unchanged. RFC-0010 GA2 says the scheduler treats `allow` and `allow_once` identically and that scope semantics are the control plane's; the CLI adds no scope logic. |

### 7.4 `alloy cancel`

| Step | Call |
| --- | --- |
| 1 | `assemble_read` |
| 2 | `plane.cancel(run)` → `RunController::cancel` |
| 3 | Render terminal state; apply **CR19**'s workspace-modified notice |

**SQ12** — `cancel` is idempotent from the user's view: cancelling an already-terminal run prints its terminal state and exits `0`.

### 7.5 `alloy resume`

| Step | Call |
| --- | --- |
| 1 | `assemble_full` — resume can dispatch, so it needs the scheduler |
| 2 | `sessions.resume(session)` — merged crash recovery rewrites `running` / `waiting_approval` rows back to `accepted` and finalizes `cancelling` rows |
| 3 | Run selection: `--run`, else the session's single non-terminal run (from `list_runs`); ambiguity is `EX_USAGE` listing candidates |
| 4 | `runs.start(run)` |
| 5 | As `run` steps 8–11 |

| Rule | Statement |
| --- | --- |
| **SQ13** | The CLI MUST NOT re-register gate waiters itself; merged waiters are not durable and `SessionGateHumanAdapter` re-registers when the node re-enters the gate. |
| **SQ14** | `resume` MUST NOT replan. `RunController::request_replan` is not called by any subcommand in MVP. |

### 7.6 `alloy index`

See §10.

### 7.7 Output

| Rule | Statement |
| --- | --- |
| **OUT1** | stdout is *results*; stderr is *progress and diagnostics*. Every CLI subcommand MUST initialise tracing with `logging::init_tracing_stderr()` (amendment **A5**) — the merged `init_tracing()` writes to **stdout**, so with any active log filter it interleaves tracing lines into `--json` output and produces a document no parser accepts. |
| **OUT2** | `--json` emits one JSON object per invocation for `run` / `approve` / `cancel` / `resume` / `index`, and **JSON Lines** for `events` and for `run --json`'s progress stream on stderr. |
| **OUT3** | Every JSON document carries `{"schema": "alloy.cli/v1", "command": "<name>", "ok": <bool>, "exit_code": <int>, "config": { … per PR4 … }, …}`. |
| **OUT4** | JSON MUST NOT contain prompt bodies, tool bodies, API keys, or absolute paths outside the workspace. Redaction is the merged obs layer's (`RetentionPolicy`, `obs::redact`); the CLI MUST NOT re-widen it. |
| **OUT5** | Human rendering of an event is one line: `<seq>  <ts>  <type>  <summary>`. Summaries come from the typed payload parsers and fall back to the type name when a payload does not parse — never to a raw JSON dump. |
| **OUT6** | Cost is rendered from `CostSnapshot` / `reaccumulate_cost_from_events` and labelled as measured spend. No savings, efficiency, or comparative claims (V2 §0.9). |
| **OUT7** | Budget warnings are rendered **only** from `SessionEventType::BudgetWarning` events. |
| **OUT8** | When stdout is not a TTY, colour and spinners are disabled and progress collapses to periodic stderr status lines. |
| **OUT9** | `ApprovalRequested` rendering is specified in §8.2 and is identical in `run`'s live stream and in `events`. |

---

## 8. Approval UX

### 8.1 The two paths

```mermaid
sequenceDiagram
  participant S as LinearScheduler
  participant GA as SessionGateHumanAdapter
  participant SP as SessionPlane
  participant CLI as alloy run (TTY)
  participant CLI2 as alloy approve (other process)

  S->>GA: wait_approval(ctx, gate)
  GA->>SP: register_gate_waiter(run, gate)
  SP-->>GA: oneshot::Receiver<Approval>
  Note over SP: row → waiting_approval; ApprovalRequested appended
  alt interactive
    CLI->>CLI: sees ApprovalRequested in its poll
    CLI->>SP: approve(run, gate, Allow|Deny|AllowOnce)
  else out of band (requires amendment A4)
    CLI2->>SP: RunController::approve(run, gate, decision)
    Note over SP: no local waiter → durable ApprovalRequested found → resolution persisted
  end
  SP-->>GA: oneshot resolves (or the durable resolution is observed)
  GA-->>S: Approval
  Note over SP: ApprovalResolved appended
```

### 8.2 Rules

| Rule | Statement |
| --- | --- |
| **GA1** | The CLI learns about a gate **only** from an `ApprovalRequested` event. It MUST NOT call `register_gate_waiter` and MUST NOT poll the run row. |
| **GA2** | The prompt renders, in order: run id, gate id, node id, the gate's reason from the `ApprovalRequested` payload, and — as the change under review — **the patch artifact from the most recent `EditApplied` event for this run**, fetched through `ArtifactStore` and summarised (files touched, `+`/`-` counts). The merged `ApprovalRequested` payload carries **no artifact reference**, so the subject MUST be derived from the `EditApplied` event rather than from the gate payload. When no `EditApplied` event precedes the gate, the prompt says so explicitly ("no patch applied yet in this run") rather than rendering an empty diff. |
| **GA3** | Accepted interactive answers: `y` / `a` → `Approval::Allow`; `o` → `Approval::AllowOnce`; `n` / `d` → `Approval::Deny`; `?` reprints; EOF → treated as `--no-input` (**GA5**). Anything else reprompts. There is no default-on-Enter. |
| **GA4** | With `--yes`, the CLI answers `Approval::Allow` immediately, prints the same block it would have prompted with (so the log still shows what was approved), and continues. Refused under `readonly` (**PF9**). |
| **GA5** | With `--no-input`, or with no TTY on stdin, the CLI does not prompt: it prints the gate id (JSON field `gate_required`), leaves the run in `waiting_approval`, and exits `EX_GATE_REQUIRED`. The run remains resumable and approvable out of band (**SQ9**, amendment A4). |
| **GA6** | A `Deny` is a legitimate outcome, not a CLI error: the run terminalizes per RFC-0003 and the process exits `EX_GATE_DENIED`. |
| **GA7** | The CLI MUST NOT call `RunController::expire_gate`. Gate expiry is RFC-0010's timer. If `[limits].gate_timeout_secs` is set, the CLI reports the deadline in the prompt; it does not enforce it. |
| **GA8** | Ctrl-C at a prompt cancels the **run** (via the cancel token), not just the prompt. |
| **GA9** | The prompt is written to `/dev/tty` when available so `alloy run --json > out.json` remains interactive. When `/dev/tty` cannot be opened, **GA5** applies. |

---

## 9. Errors and exit codes

### 9.1 Rules

| Rule | Statement |
| --- | --- |
| **EX1** | Exit codes are a closed set (§9.2). No subcommand invents a code. |
| **EX2** | `0` means the requested operation completed **and** its subject succeeded. A run that ended `Failed` exits non-zero even though the CLI worked perfectly. |
| **EX3** | Error messages name the file, the variable, or the id the user must act on, plus the next command to run. |
| **EX4** | An error message MUST NOT contain an API key, a prompt body, or a tool body, even at `--verbose`. |
| **EX5** | The mapping from merged error types to exit codes (§9.3) is table-driven and **pinned by a variant-list test**, not by an exhaustive `match`. `RunError`, `SchedError`, and the other upstream error enums are `#[non_exhaustive]`, so a downstream crate *cannot* write a match that fails to compile when a variant is added — the wildcard arm is mandatory and would silently swallow new variants. The test therefore enumerates the variants this RFC maps (a checked-in list of variant names compared against the upstream set, e.g. via `Debug` discriminants of constructed samples) and fails when upstream's set changes, forcing a deliberate mapping decision. The wildcard arm MUST map to `EX_INTERNAL` and log the unmapped variant. |

### 9.2 Taxonomy

| Code | Name | Meaning | Typical source |
| --- | --- | --- | --- |
| `0` | `EX_OK` | Success | — |
| `1` | `EX_INTERNAL` | Unexpected internal error; unmapped upstream variant | `RuntimeError::Internal`, wildcard arm (**EX5**) |
| `2` | `EX_USAGE` | Bad arguments (clap default) | clap; **CL4**, **PF11**, **SQ5**, **SQ8b** |
| `3` | `EX_CONFIG` | Config missing or invalid | `RuntimeConfig::load`, `RouterError::Config`, §5.5 |
| `4` | `EX_SANDBOX` | Sandbox unavailable / fails closed | `SandboxError::BackendUnavailable` at broker construction |
| `5` | `EX_RUN_FAILED` | The run itself failed | `DagState::Failed` |
| `6` | `EX_CANCELLED` | Cancelled by signal or `alloy cancel` | `DagState::Cancelled`, **CR18** |
| `7` | `EX_GATE_REQUIRED` | A gate needs a human; none available | **GA5** |
| `8` | `EX_GATE_DENIED` | A human denied the gate | **GA6** |
| `9` | `EX_BUDGET` | Budget ceiling reached | `ErrorClass::Budget` |
| `10` | `EX_REPLAN` | Run needs a replan; MVP does not auto-replan | `DagState::ReplanRequired` |
| `11` | `EX_TIMEOUT` | Run timeout elapsed | `ErrorClass::Timeout` |
| `12` | `EX_NOT_FOUND` | Session / run / gate not found | `SessionError::NotFound`, `RunError::NotFound`, `RunError::UnknownGate` |
| `13` | `EX_PROFILE_REFUSED` | Profile forbids the operation | **PF9**, **PF10** |
| `14` | `EX_STATE` | Operation invalid for the current state | `RunError::InvalidPhase`, `RunError::AlreadyStarted` |
| `15` | `EX_GRAPH` | Graph open / rebuild failed | `GraphError` (§10) |

### 9.3 Mapping

**From `DagOutcome`:**

| `DagState` | Exit | Notes |
| --- | --- | --- |
| `Succeeded` | `EX_OK` | Render artifacts and the checkpoint |
| `Failed` | by `failure.error_class`, else `EX_RUN_FAILED` | See below |
| `Cancelled` | `EX_CANCELLED` | Plus **CR19** notice |
| `ReplanRequired` | `EX_REPLAN` | Print `alloy resume --session <id>` |
| `Pending` / `Running` / `WaitingApproval` | `EX_GATE_REQUIRED` when a gate is outstanding, else `EX_INTERNAL` | A non-terminal outcome from a blocking `start` is otherwise a bug |

**From `FailureIr.error_class`:**

| `ErrorClass` | Exit |
| --- | --- |
| `Compile`, `Test` | `EX_RUN_FAILED` |
| `Tool` | `EX_RUN_FAILED` (or `EX_SANDBOX` when the tool error is a broker unavailability) |
| `Model` | `EX_RUN_FAILED` |
| `Budget` | `EX_BUDGET` |
| `Approval` | `EX_GATE_DENIED` |
| `Timeout` | `EX_TIMEOUT` |
| `Cancelled` | `EX_CANCELLED` |
| `Internal` | `EX_INTERNAL` |

**From control-plane errors:**

| Variant | Exit | Notes |
| --- | --- | --- |
| `SessionError::NotFound`, `RunError::NotFound` | `EX_NOT_FOUND` | |
| `RunError::UnknownGate` | `EX_NOT_FOUND` | Post-A4: no waiter *and* no durable `ApprovalRequested` |
| `SessionError::Invalid` | `EX_USAGE` | Includes profile rejection and empty goal text |
| `RunError::InvalidPhase`, `RunError::AlreadyStarted` | `EX_STATE` | |
| `RunError::SchedulerUnavailable` | `EX_INTERNAL` | The CLI installed the scheduler; seeing this means assembly failed |
| `SessionError::Internal`, `RunError::Internal` | `EX_INTERNAL` | |
| `RuntimeError::Config` | `EX_CONFIG` | |
| `RuntimeError::InvalidPhase` | `EX_INTERNAL` | The CLI owns phase ordering, so this is the CLI's bug |
| *(any variant added upstream)* | `EX_INTERNAL` | Mandatory wildcard arm + log; the **EX5** test flags the drift |

### 9.4 Failure modes

| Failure | Handling |
| --- | --- |
| Missing profile TOML | `EX_CONFIG`; names the path and `example.env`; `.env` untouched |
| Missing `router.toml` | `EX_CONFIG`; names `router.toml.example` |
| `ALLOY_API_KEY` unset/empty | `EX_CONFIG` from `TomlModelRouter::from_paths`; names the variable and `example.env`; never offers to write one |
| `check` sandbox backend unavailable | `EX_SANDBOX` **before** the session row is written; suggests `check = "container"` |
| `test` backend unavailable | Not fatal — merged broker allows it; `VerifyTest` nodes fail with `BackendUnavailable` and the CLI reports which class degraded |
| Non-interactive + gate | `EX_GATE_REQUIRED` with the gate id (**GA5**) |
| Capability executor genuinely absent | `EX_INTERNAL` naming the missing component (**CR12**) |
| Scheduler lock held by another process | `EX_STATE` naming `<data_dir>/scheduler.lock` |
| Graph DB corrupt | Merged store self-quarantines and rebuilds; the CLI reports the quarantine path; `EX_GRAPH` only if the retry also fails |
| SQLite busy | `StoreError::Busy` → `EX_INTERNAL` with the `ALLOY_SQLITE_BUSY_TIMEOUT_MS` hint |
| Ctrl-C during drain | Second signal escalates; `EX_CANCELLED` (**CR17**) |

---

## 10. `alloy index` and graph obligations

RFC-0011 Appendix E.4 assigns four obligations to this RFC. Each is a rule here.

| Rule | Statement |
| --- | --- |
| **IX1** | This RFC owns the ingest trigger (RFC-0011 IN1): the `alloy index` subcommand, plus an optional bootstrap at session create. No capability worker triggers ingest. |
| **IX2** | `alloy index` opens with `SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(cfg.data_dir))` and rebuilds with `rebuild_reported(&workspace_root)`, which returns an `IngestReport`. |
| **IX3** | Bootstrap-at-create is **on** by default for `alloy run` and skippable with `--no-index`; it runs `rebuild_reported` before `submit_goal`. A bootstrap failure is a warning, never fatal (**IX7**). |
| **IX4** | The CLI records a `DecisionKind::Custom("graph_rebuild")` decision through `EventDecisionLog`, carrying the `IngestReport` counts (RFC-0011 OB5). `alloy-index` emits no session events itself. |
| **IX5** | The returned `GraphVersion` is written into `sessions.graph_version` via `SessionRows::set_graph_version` (amendment A3). **Stub** until A3 lands: the version is reported in output and the column stays `NULL`. |
| **IX6** | Any `ALLOY_GRAPH_*` knob the implementation introduces MUST be documented in `example.env` as a commented line (RFC-0011 E.4 item 4). MVP introduces none, so `example.env` gains a comment saying so. |
| **IX7** | An empty or stale graph MUST NOT change CLI behaviour. M7 ships RFC-0011 thin; `Callers` / `SimilarFixes` are empty by design. Rendering "0 nodes" is correct output, not an error. |
| **IX8** | `alloy index --stats` prints `GraphMetricsSnapshot` and exits without writing. |
| **IX9** | The CLI MUST NOT write graph rows by any path other than `rebuild` / `apply_incremental`. |

---

## 11. Secrets and files on disk

| Rule | Statement |
| --- | --- |
| **SEC1** | Alloy MUST NOT create, write, truncate, or append `.env` — ever, under any flag, including a hypothetical `--init`. `RuntimeConfig::load` already has a regression test (`load_never_writes_dotenv_and_preserves_sentinel`) that this RFC extends to the process level (§12.2). |
| **SEC2** | Alloy MUST NOT *read* `.env` either. Credentials come from the process environment. |
| **SEC3** | `example.env` is the single documentation surface for environment knobs. Every env variable any Alloy crate reads MUST appear there, commented, with its default. |
| **SEC4** | The CLI MUST NOT print the value of any variable named in `router.toml`'s `api_key_env`, nor any variable whose name matches `*KEY*`, `*TOKEN*`, `*SECRET*`, `*PASSWORD*` — including at `--verbose` and in JSON. |
| **SEC5** | The CLI creates exactly these paths: `<data_dir>` and its subtrees — including `<data_dir>/cli/last_session` (**SQ5b**) — and the graph DB under `<data_dir>/graph/`. It creates nothing in the user's workspace and nothing in `$HOME`. |
| **SEC6** | `router.toml.example` and `profiles/*.toml` are shipped templates. The CLI MUST NOT copy `router.toml.example` to `router.toml` automatically; it prints the `cp` command. |
| **SEC7** | Tool and model bodies obey the profile's `[observability]` retention. `--verbose` raises the *tracing filter*, not the retention policy. |

---

## 12. Testing

### 12.1 Conventions

`crates/alloy-cli/tests/` already has two integration files with distinct styles, and this RFC keeps both:

- `cli_smoke.rs` — `assert_cmd::Command::cargo_bin("alloy")` + `predicates`, for argv-level behaviour.
- `host_e2e.rs` — a real spawned process with real signals (`rustix` for SIGTERM), `#![cfg(unix)]`, for lifecycle and on-disk effects.

New tests follow the same split: argv/rendering in `assert_cmd`, process/lifecycle in the spawned-binary style, and composition-root wiring in in-crate unit tests that call `assemble_read` / `assemble_full` directly (as `main.rs`'s existing `signal_path` module does with `graceful_shutdown`).

### 12.2 Named tests

**Argv and rendering — `tests/cli_grammar.rs`:**

| Test | Asserts |
| --- | --- |
| `help_snapshot_matches` | Top-level and per-subcommand `--help` (**CL10**) |
| `every_subcommand_accepts_json_and_workspace` | (**CL2**, **CL3**) |
| `unknown_profile_is_usage_error` | Exits `2` naming the catalog (**CL4**) |
| `malformed_id_is_usage_not_not_found` | Exits `2` (**CL5**) |
| `yes_and_no_input_conflict` | Exits `2` |
| `max_usd_above_profile_ceiling_rejected` | Exits `2` naming both numbers (**PF11**) |
| `help_works_without_any_config` | Empty temp dir, exits `0` (**CL9**) |
| `json_stdout_has_no_tracing_lines` | With `RUST_LOG=debug` set, stdout still parses as JSON (**OUT1**, A5) |

**Config and profiles — `tests/config_profiles.rs`:**

| Test | Asserts |
| --- | --- |
| `three_catalog_profiles_parse` | All three load |
| `default_profile_matches_appendix_b` | Every value in §5.2's `default` column |
| `profile_id_mismatch_is_config_error` | (**PF4**) |
| `allow_raw_bash_true_is_rejected` | (**PF6**) |
| `require_cargo_check_false_is_rejected` | (**PF7**) |
| `network_allow_is_rejected_before_broker` | No broker probe ran (**PF8**) |
| `parallel_knobs_must_be_one` | (**PF12**) |
| `context_weights_must_sum_to_one_in_config_load` | `0.2/0.2/0.2` passes `DomainWeights::validate` but fails `RuntimeConfig::load` (**PF13**) |
| `context_profile_reaches_engine` | The constructed engine receives the parsed `context_profile` (**PF13**) |
| `unknown_table_is_rejected` | (**PF14**) |
| `env_beats_flag_for_data_dir` | `data_dir_rule == "ALLOY_DATA_DIR"` (**PR1**) |
| `alloy_profile_env_and_profile_flag_must_agree` | (**PR3**) |
| `json_reports_config_provenance` | (**PR4**) |
| `relative_workspace_not_double_joined` | (**PR2**) |
| `mvp_profiles_is_reachable_from_cli` | The re-export of A1 compiles and is the only catalog list (**PF1**) |

**Composition root — `tests/composition.rs` + in-crate unit tests:**

| Test | Asserts |
| --- | --- |
| `assemble_full_extends_assemble_read` | Read-path construction is identical between the two entry points (**CR1**) |
| `assembly_constructs_in_documented_order` | Recording assembly logs match §6.2 |
| `scheduler_runs_arc_is_plane_runs_arc` | `Arc::ptr_eq` (**CR2**) |
| `scheduler_cancel_token_is_runtime_token` | (**CR3**) |
| `mcp_max_in_flight_is_one` | (**CR7**) |
| `operator_homes_resolved_once` | (**CR10**) |
| `sched_data_dir_is_absolute` | `--workspace .` still absolute (**CR5**) |
| `sandbox_table_cross_check` | Divergent readings → error (**CR6**) |
| `read_only_subcommands_use_assemble_read` | `events` on a host with no backends exits `0` (**CR11**) |
| `absent_executor_is_honest` | Injecting `UnavailableCapabilityExecutor` yields `EX_INTERNAL` naming the component (**CR12**) |

**Lifecycle — extends `tests/host_e2e.rs`:**

| Test | Asserts |
| --- | --- |
| `run_sigterm_drains_and_exits_cancelled` | Exit `6` (**CR16**, **CR18**) |
| `signal_during_startup_aborts_cleanly` | (**CR14**) |
| `second_signal_escalates` | (**CR17**) |
| `cancel_notice_only_on_edit_applied` | Notice appears when an `EditApplied` event exists, is absent otherwise, and its absence prints no clean-tree claim (**CR19**) |
| `no_dotenv_written_by_any_subcommand` | Sentinel `.env` byte-identical (**SEC1**) |
| `no_dotenv_read` | A `.env` setting `ALLOY_API_KEY` does not satisfy the router (**SEC2**) |

**Behaviour — `tests/run_flow.rs` (loopback scripted server, offline):**

| Test | Asserts |
| --- | --- |
| `run_dry_run_plans_without_dispatch` | (**CL12**) |
| `readonly_refuses_run` | `EX_PROFILE_REFUSED` before any session row (**PF9**) |
| `readonly_builds_no_edit_engine` | (**PF10**) |
| `goal_text_passed_verbatim` | `RunGoalRecord.goal.text` equals argv (**SQ5c**) |
| `gate_prompt_shows_latest_edit_applied_patch` | Patch summary from the `EditApplied` artifact; explicit message when none (**GA2**) |
| `gate_without_tty_exits_gate_required` | Exit `7`; gate id on stdout (**GA5**) |
| `approve_from_second_process_unblocks_run` | Two processes; run reaches terminal — depends on A4 (**SQ9**) |
| `approve_without_waiter_or_event_is_not_found` | `EX_NOT_FOUND` (**SQ10**) |
| `deny_exits_gate_denied` | Exit `8` (**GA6**) |
| `cancel_is_idempotent` | (**SQ12**) |
| `resume_redispatches_after_kill` | (**SQ13**) |
| `events_cursor_resumes_exactly` | (**SQ4**, **SQ6**) |
| `events_without_session_uses_marker` | Marker present → works; marker absent → `EX_USAGE` with the honest message, no DB scan (**SQ8b**) |
| `exit_code_variant_list_is_pinned` | Upstream variant set matches the checked-in list; wildcard maps to `EX_INTERNAL` (**EX5**) |
| `json_contains_no_secrets` | Property test over field names (**SEC4**, **OUT4**) |
| `index_records_graph_rebuild_decision` | (**IX4**) |
| `index_stats_writes_nothing` | (**IX8**) |
| `empty_graph_does_not_change_behaviour` | (**IX7**) |

### 12.3 CI greps

| ID | Grep over `crates/alloy-cli/src/` | Rationale |
| --- | --- | --- |
| **T1** | No `std::process::Command` (outside `#[cfg(test)]`) | **B7** |
| **T2** | No `scheduler::linear::` imports beyond the **B2** allow-list | **B2** |
| **T3** | No `PermissionToken` / `Grant::` construction | **B4** |
| **T4** | No `rusqlite` / `sqlx` / direct `.sqlite` path literals | **B6** |
| **T5** | No `EditEngine::apply` / `rollback`, no `record_diagnostic` / `record_fix` | **B5** |
| **T6** | No `.env` literal except in a refusal message; no `fs::write` whose path expression contains `.env` | **SEC1** |
| **T7** | No literal model id, `https://` provider URL, or `usd_per_mtok` | **B3** |
| **T8** | No `retry`, `backoff`, `max_attempts` identifiers | **B1** |
| **T9** | `alloy-cli/Cargo.toml` dependencies remain a subset of `{alloy-runtime, alloy-tools, alloy-index, clap, tokio, tracing, serde_json, anyhow-or-equivalent}` — **no `alloy-eval`**, no HTTP client, no TOML parser of its own | **B1**, Appendix D.3 |
| **T10** | No `unsafe` | Workspace policy |
| **T11** | No bare `init_tracing()` call in `alloy-cli` — only `init_tracing_stderr()` | **OUT1**, A5 |

---

## 13. Acceptance criteria

**Grammar and rendering**

- [ ] AC1 — `alloy run`, `events`, `approve`, `cancel`, `resume`, `index` exist alongside the unchanged `host`, with the §4.2 shapes.
- [ ] AC2 — Every subcommand accepts `--workspace` and `--json` (**CL2**, **CL3**).
- [ ] AC3 — `--profile` is validated by the re-exported `validate_mvp_profile`; no second catalog list exists in `alloy-cli` (**CL4**, **PF1**).
- [ ] AC4 — Ids parse with the merged `parse` functions; malformed ids are usage errors (**CL5**).
- [ ] AC5 — No model/tier/retry/timeout flag; `--template` exists only under `--dry-run` (**CL6**, **CL12**).
- [ ] AC6 — `--help` / `--version` work with no config present (**CL9**).
- [ ] AC7 — Help text is snapshot-tested (**CL10**).
- [ ] AC8 — stdout carries results only; tracing goes to stderr via `init_tracing_stderr`, verified with a log filter active (**OUT1**, amendment A5, **T11**).
- [ ] AC9 — JSON documents carry the `alloy.cli/v1` envelope with config provenance (**OUT3**, **PR4**).
- [ ] AC10 — Event lines render through the typed payload parsers, never as raw JSON dumps (**OUT5**).
- [ ] AC11 — Cost is rendered from the meter with no savings or comparison claims (**OUT6**).
- [ ] AC12 — Budget warnings come only from `BudgetWarning` events (**OUT7**).

**Profiles and config**

- [ ] AC13 — The three catalog profile files exist and parse into one struct (**PF2**, **PF3**).
- [ ] AC14 — `default` matches Architecture V2 Appendix B value-for-value across every table.
- [ ] AC15 — `[profile].id` must equal the selected catalog id (**PF4**).
- [ ] AC16 — `allow_raw_bash = true` is rejected in every profile (**PF6**).
- [ ] AC17 — `require_cargo_check = false` is rejected in every profile (**PF7**).
- [ ] AC18 — Non-`deny` network or `quarantine_deps = false` is rejected before broker construction (**PF8**).
- [ ] AC19 — `max_parallel_*` other than `1` is a config error (**PF12**).
- [ ] AC20 — `RuntimeConfig::load` rejects `[context]` weights that do not sum to `1.0 ± 1e-6` — the check `DomainWeights::validate` does not perform — and passes `context_profile` to the engine (**PF13**).
- [ ] AC21 — Unknown tables and unknown keys are rejected, not ignored (**PF14**).
- [ ] AC22 — `readonly` refuses a non-`--dry-run` run before creating a session (**PF9**).
- [ ] AC23 — `readonly` assembles no `GitEditEngine` and no test-class permission glob (**PF10**).
- [ ] AC24 — `--max-usd` may only tighten the profile ceiling (**PF11**).
- [ ] AC25 — Precedence is env > flag > profile > default, matching merged `resolve_data_dir`, and is documented in `example.env` (**PR1**).
- [ ] AC26 — `ALLOY_PROFILE` path and `--profile` id disagreement is a config error (**PR3**).
- [ ] AC27 — Relative `--workspace` is joined exactly once (**PR2**).
- [ ] AC28 — Amendments A1–A5 are implemented additively with no merged signature reshaped (§5.6).

**Composition root**

- [ ] AC29 — Construction is `assemble_read` / `assemble_full` over one `Assembly`, with `assemble_full` extending rather than duplicating the read path (**CR1**).
- [ ] AC30 — `LinearSchedulerDeps.runs` is `Arc::ptr_eq` to `session_plane.runs()` (**CR2**).
- [ ] AC31 — `runtime_cancel` is `handle.cancellation()` (**CR3**).
- [ ] AC32 — `budget_policy` and `run_timeout` come from the profile (**CR4**).
- [ ] AC33 — `SchedConfig.data_dir` is absolute even for `--workspace .` (**CR5**).
- [ ] AC34 — The two `[sandbox]` readings are cross-checked (**CR6**).
- [ ] AC35 — `McpHostConfig.max_in_flight == 1` (**CR7**).
- [ ] AC36 — The MCP host cancel token is a child of the runtime token (**CR8**).
- [ ] AC37 — `OperatorHomes::resolve()` is called once and shared (**CR10**).
- [ ] AC38 — Read-only subcommands construct no broker, host, context engine, registry, or scheduler (**CR11**).
- [ ] AC39 — A build with a genuinely absent capability executor exits `EX_INTERNAL` naming the missing component rather than faking success. Where RFC-0013 is present the real registry is wired and this path is unreachable, so the criterion is satisfied by the injected-stub test, not by shipping a stub (**CR12**).
- [ ] AC40 — Shutdown reverses construction; `graceful_shutdown` is extended, not replaced (**CR16**).
- [ ] AC41 — The signal task is armed before `start` and only cancels the token (**CR14**, **CR15**).
- [ ] AC42 — A second signal during drain escalates (**CR17**).
- [ ] AC43 — Cancel prints the workspace-modified notice from the `EditApplied` leg only, and its absence claims nothing about the tree (**CR19**).
- [ ] AC44 — The router is per run, built after `submit_goal`, bound to that `RunId`; the registry receives a factory (**CR20**).

**Boundary**

- [ ] AC45 — `alloy-cli` contains no planner, scheduler, retry, budget, or verification logic (**B1**).
- [ ] AC46 — `alloy-cli` imports no scheduler internals beyond the **B2** allow-list.
- [ ] AC47 — `alloy-cli` names no model id, endpoint, or price (**B3**).
- [ ] AC48 — `alloy-cli` mints no `PermissionToken` (**B4**).
- [ ] AC49 — `alloy-cli` calls no `EditEngine::apply` / `rollback` and no graph ingest write beyond `rebuild` (**B5**).
- [ ] AC50 — `alloy-cli` opens no database directly (**B6**) and spawns no process outside the broker (**B7**).
- [ ] AC51 — CI greps **T1**–**T11** are wired and failing-closed (**B8**).

**Control-plane sequences and approvals**

- [ ] AC52 — `run` reaches the scheduler only through `RunController::start` (**SQ2**).
- [ ] AC53 — The CLI never retries or replans on its own (**SQ3**, **SQ14**), and never rewrites goal text (**SQ5c**).
- [ ] AC54 — Event paging honours the merged cursor contract including empty-page-with-cursor (**SQ4**).
- [ ] AC55 — `approve` from a second process unblocks a run started by a first, via amendment A4's durable fallback (**SQ9**).
- [ ] AC56 — Gates are learned only from `ApprovalRequested`; the CLI registers no waiter and calls no `expire_gate` (**GA1**, **GA7**).
- [ ] AC57 — Non-interactive gate → exit `7`, gate id printed, run resumable (**GA5**).
- [ ] AC58 — The gate prompt shows the latest `EditApplied` patch artifact, and says so plainly when none exists (**GA2**).
- [ ] AC59 — Ctrl-C at a prompt cancels the run (**GA8**); prompting uses `/dev/tty` so `--json` stays interactive (**GA9**).
- [ ] AC60 — Bare `alloy events` resolves the session from `<data_dir>/cli/last_session` and reports honestly when the marker is absent, with no database scan (**SQ5b**, **SQ8b**).

**Errors, graph, secrets**

- [ ] AC61 — Exit codes are exactly §9.2, table-driven, with a variant-list-pinning test and a wildcard arm mapping to `EX_INTERNAL` (**EX1**, **EX5**).
- [ ] AC62 — Every error message names a file, variable, or id and a next command (**EX3**), and leaks no secret (**EX4**, **SEC4**).
- [ ] AC63 — Sandbox `check` unavailability fails before any session row is written (§9.4).
- [ ] AC64 — `alloy index` owns the ingest trigger and records a `graph_rebuild` decision with `IngestReport` counts (**IX1**, **IX4**).
- [ ] AC65 — `GraphVersion` is written to `sessions.graph_version` once amendment A3 lands; until then the Stub is reported honestly (**IX5**).
- [ ] AC66 — Any `ALLOY_GRAPH_*` knob is documented in `example.env`; MVP introduces none and says so (**IX6**).
- [ ] AC67 — An empty or thin graph changes no CLI behaviour (**IX7**).
- [ ] AC68 — No subcommand writes or reads `.env`, proven by a process-level sentinel test (**SEC1**, **SEC2**).
- [ ] AC69 — `example.env` documents every environment variable any Alloy crate reads (**SEC3**); the only path the CLI creates outside `<data_dir>` subtrees is none (**SEC5**).
- [ ] AC70 — The Appendix C walkthrough runs end to end offline against the loopback OpenAI-compatible scripted server (Appendix D.3), and with a live provider when configured.

---

## 14. Definition of Done

Merge only when the series [Definition of Done](./README.md#definition-of-done-merge-gate) is fully met:

- [ ] Architecture compliance: **PASS**
- [ ] RFC acceptance criteria: **100% satisfied**
- [ ] Unit tests: **passing**
- [ ] Integration tests: **passing** (if applicable)
- [ ] Documentation: **complete**
- [ ] Public APIs: **reviewed and stable**
- [ ] Clippy: **clean**
- [ ] Formatting: **clean**
- [ ] No TODO or placeholder implementations left in this RFC's scope (explicit **Stub** / deferred only)
- [ ] Code review: **approved**

---

## 15. Estimated implementation effort

**4–6 person-days.**

| Slice | Estimate |
| --- | --- |
| Clap surface, flags, help snapshots, JSON envelope | 0.75 pd |
| Profile parsing + amendments A1 / A2 / A5, three catalog TOMLs, validation | 1.0 pd |
| Composition root (`assemble_read` / `assemble_full`) + shutdown/signal extension | 1.25 pd |
| Subcommand handlers + rendering + exit-code table | 1.0 pd |
| Approval UX + amendment A4 (durable approve fallback) | 0.75 pd |
| `alloy index` + amendment A3 | 0.5 pd |
| Tests, CI greps, `example.env` audit | 1.0 pd |

---

## 16. Open questions

| # | Question | Current answer | Decide by |
| --- | --- | --- | --- |
| Q1 | Should env really beat flags? | Yes — **PR1** matches merged `resolve_data_dir`. Changing it means changing merged behaviour. | Before A1 lands |
| Q2 | Should `alloy run` default to `--dry-run` on first use? | No. A silent mode difference between invocations is worse than an explicit flag. | Resolved |
| Q3 | Should `readonly` allow `run` with a future read-only template? | Yes, when one exists. **PF9** refuses on the *template's* side effects, so no rule change is needed. | When such a template exists |
| Q4 | Does `--yes` belong in the product, given V2's gate posture? | Kept: CI needs it and it is auditable (**GA4**). Revisit if holdout shows it masking bad patches. | MVP gate review |
| Q5 | `alloy doctor` / `alloy config show`? | Deferred; **PR4** puts provenance in `--json` today. | Beta |
| Q6 | Should `events --follow` become a real subscription? | Not in MVP; no subscription API exists and polling is honest. | Beta, with the TUI |
| Q7 | Should `last_session` become a real `list_sessions` API? | Probably — the marker is a workaround for a missing query (**SQ8b**). `SessionRows::list_sessions` would let the CLI drop it. Deliberately not amended here to keep the storage surface stable for M7. | Beta |
| Q8 | Should the CLI auto-`resume` a crashed run it finds on startup? | No. Silent resumption of a run that touched a workspace is a surprise. | Resolved |
| Q9 | Should A4's durable fallback extend to externally driven expiry? | No — RFC-0010 owns expiry (**GA7**); A4 is scoped to human approval only. | Resolved |
| Q10 | Should the loopback scripted server move into a shared test crate? | Probably, once RFC-0016 needs it from two places; keeping it in the harness avoids a dependency edge into `alloy-cli` (**T9**). | RFC-0016 holdout work |

---

## 17. Future extensions

- **TUI** reading the same `alloy events --json` stream, no new data path.
- **`alloyd` / ACP** only if single-binary p95 fails on real repos (ADR F-27).
- **Shell completions and man pages** from clap generators.
- **`alloy doctor`** — config provenance, backend availability, graph freshness (Q5).
- **`SessionRows::list_sessions`** retiring the `last_session` marker (Q7).
- **Goal-blind repair** — when auto-replan on `FailureIr` lands, the Appendix C goal need not name a file.
- **Profile inheritance** (`extends = "default"`) if the three catalog files drift toward duplication.
- **Multiple concurrent runs** when `max_parallel_*` may exceed 1; §6.4 already separates per-run from per-process objects.

---

## Appendix A — V2 obligation mapping

| V2 obligation | Where satisfied | Rule / AC |
| --- | --- | --- |
| §1.4 "engineer can run `alloy run …` and get a compile-verified patch with full decision log under sandbox" | Appendix C | AC70 |
| §5.2 "CLI — owns User I/O — does not own Planning logic" | §6.5 | B1–B8, AC45–AC51 |
| §5.3 single-binary topology | §6.1 | AC29 |
| §5.5 `SessionService` / `RunController` are the control APIs | §7 | AC52 |
| §5.5 `CreateSession.profile: ProfileId // default \| autonomous \| readonly` | §5.1 | AC3, AC13 |
| §6.5 repair sequence | §7.1, Appendix C | AC70 |
| §12.1 "Default profile: no raw bash" | **PF6** | AC16 |
| §12.4 "Never replace user's `.env`; use `example.env` patterns only" | §11 | AC68, AC69 |
| §17 "Autonomous mode opt-in, still gated" | **PF7**, §5.2 `[gates]` | AC17 |
| §19.1 quarantine profile default | **PF8** | AC18 |
| §20 R14 cost overrun | **OUT6**, **OUT7** | AC11, AC12 |
| Appendix B default profile TOML | §5.2 | AC14 |
| Appendix E permission token minted by the runtime, not the CLI | **B4** | AC48 |
| Roadmap M7 "CLI owns I/O only" | §6.5 | AC45 |
| Roadmap M7 "`.env` never replaced; `example.env` documented" | §11 | AC68, AC69 |
| RFC-0011 E.4 items 1–4 | §10 | AC64–AC66 |
| RFC-0010 §2.4 "what RFC-0015 may rely on" | §7.1, §9.3, **CR19** | AC43, AC52, AC61 |

## Appendix B — Catalog profile files

`profiles/default.toml` is the existing file extended to the full Appendix B table set; the other two are new. Comments are normative documentation and MUST be kept.

```toml
# profiles/default.toml — Author: arkadianet
# Architecture V2 Appendix B baseline. Parsed by RuntimeConfig::load (RFC-0015 A1)
# and, for [sandbox], by alloy-tools load_sandbox_profile. Both readings are
# cross-checked at assembly (RFC-0015 CR6).
[profile]
id = "default"
description = "Correctness-first Rust profile"

[gates]
require_cargo_check = true            # MUST stay true in every profile (PF7)
require_human_on_public_api = true
require_human_on_new_unsafe = true
require_human_on_new_dependency = true
allow_raw_bash = false                # MUST stay false in every profile (PF6)

[sandbox]
check = "landlock"                    # seatbelt on macOS; "container" acceptable
test = "container"
network = "deny"                      # MUST stay deny (PF8)
quarantine_deps = true                # MUST stay true (PF8)

[budgets]
max_usd_per_run = 5.0
max_tokens_per_run = 2_000_000
max_parallel_nodes = 1                # MUST stay 1 (PF12)
max_parallel_cargo = 1                # MUST stay 1 (PF12)
max_parallel_edits = 1                # MUST stay 1 (PF12)

[context]
# Weights must sum to 1.0 ± 1e-6 — checked by RuntimeConfig::load, NOT by
# DomainWeights::validate, which only checks finiteness and sign (PF13).
total_token_budget = 32_000
weights = { conversation = 0.20, working_set = 0.55, artifacts = 0.25 }

[observability]
retain_full_prompts = false
retain_tool_bodies = false

[limits]
run_timeout_secs = 1800
```

```toml
# profiles/autonomous.toml — Author: arkadianet
# Fewer HUMAN gates. Identical verification, sandbox, and parallelism honesty.
[profile]
id = "autonomous"
description = "Fewer human gates; same verification and sandbox"

[gates]
require_cargo_check = true
require_human_on_public_api = false
require_human_on_new_unsafe = false
require_human_on_new_dependency = false
allow_raw_bash = false

[sandbox]
check = "landlock"
test = "container"
network = "deny"
quarantine_deps = true

[budgets]
max_usd_per_run = 5.0
max_tokens_per_run = 2_000_000
max_parallel_nodes = 1
max_parallel_cargo = 1
max_parallel_edits = 1

[context]
total_token_budget = 32_000
weights = { conversation = 0.20, working_set = 0.55, artifacts = 0.25 }

[observability]
retain_full_prompts = false
retain_tool_bodies = false

[limits]
run_timeout_secs = 1800
```

```toml
# profiles/readonly.toml — Author: arkadianet
# Inspect and plan only. `alloy run` without --dry-run is refused (PF9); no
# EditEngine and no test-class exec grant are assembled (PF10).
[profile]
id = "readonly"
description = "Inspect and plan only; no workspace writes"

[gates]
require_cargo_check = true
require_human_on_public_api = true
require_human_on_new_unsafe = true
require_human_on_new_dependency = true
allow_raw_bash = false

[sandbox]
check = "landlock"
test = "container"
network = "deny"
quarantine_deps = true

[budgets]
max_usd_per_run = 0.0
max_tokens_per_run = 2_000_000
max_parallel_nodes = 1
max_parallel_cargo = 1
max_parallel_edits = 1

[context]
total_token_budget = 32_000
weights = { conversation = 0.20, working_set = 0.55, artifacts = 0.25 }

[observability]
retain_full_prompts = false
retain_tool_bodies = false

[limits]
run_timeout_secs = 900
```

## Appendix C — Acceptance walkthrough (roadmap M7 demo)

The roadmap's M7 demo, made executable. Each step names the rules it exercises.

> **The goal must name its target.** The goal below names the file: *"fix the compile error in `src/lib.rs`"*. Under RFC-0013 **RW6**, a repair worker resolves its target from the goal or from ingested diagnostics; on a blind generation-1 run there are no diagnostics yet, so a goal naming neither a file nor a symbol leaves the worker with nothing to act on. The roadmap's illustrative phrasing ("fix the compile error in this crate") is therefore **not** a runnable MVP goal, and the CLI does not paper over this — it passes goal text verbatim (**SQ5c**). Goal-blind repair arrives with auto-replan on `FailureIr`, when the first `VerifyCompile` failure supplies the diagnostics a second generation can target (§1.4, §17).

```bash
# 0. Toy crate with an E0502-class error fixable by a text patch.
cargo new /tmp/alloy-e0502 --lib
# (seed known-broken src/lib.rs from the alloy-eval fixtures)

# 1. Config. Alloy never writes .env; you export into the process environment.
cd /tmp/alloy-e0502
cp <alloy>/router.toml.example router.toml     # SEC6: printed, never auto-copied
mkdir -p profiles && cp <alloy>/profiles/default.toml profiles/
export ALLOY_API_KEY=...                       # or point router.toml's base_url at the
                                               # loopback scripted server for offline (D.3)

# 2. Index the workspace (IX1, IX2, IX4).
alloy index --workspace /tmp/alloy-e0502
#   graph: 1 workspace, 1 crate, 3 modules, version 1
#   decision recorded: graph_rebuild

# 3. Plan without executing (CL12).
alloy run --workspace /tmp/alloy-e0502 --dry-run "fix the compile error in src/lib.rs"
#   template repair_local_diagnostic: Analyze → Edit → VerifyCompile → GateHuman

# 4. The real run (SQ1-SQ5c, CR1-CR13).
alloy run --workspace /tmp/alloy-e0502 "fix the compile error in src/lib.rs"
#   session 0f2c…  run 7a91…
#   [node analyze]        running → succeeded
#   [node edit]           running → succeeded   (EditApplied, patch artifact 3b1d…)
#   [node verify_compile] running → succeeded   (cargo check, sandboxed: landlock, network deny)
#   [gate  approve_patch] approval required
#
#   ── approval required ────────────────────────────────
#   run   7a91…   gate 2c4e…   node approve_patch
#   reason: template gate before completion
#   patch:  3b1d…  (1 file, +7 -3)      ← from the latest EditApplied event (GA2)
#   [y] allow  [o] allow once  [n] deny  [?] details
#   ─────────────────────────────────────────────────────
#   > y                                     (GA3)
#   run succeeded — cost $0.031 measured    (OUT6)
# exit 0                                    (EX2)

# 4b. Same run in CI, approving out of band (GA5, SQ9 — requires amendment A4).
alloy run --workspace /tmp/alloy-e0502 --no-input --json \
  "fix the compile error in src/lib.rs" || test $? -eq 7   # EX_GATE_REQUIRED
alloy approve --run 7a91… --gate 2c4e… --decision allow    # second process
alloy resume --session 0f2c…

# 5. Inspect (SQ6-SQ8b, OUT5). Bare `events` uses the last_session marker.
alloy events
alloy events --session 0f2c… --decisions-only --json | jq -r '.type'
#   session_created / goal_submitted / plan_produced / node_state … / edit_applied /
#   approval_requested / approval_resolved / run_completed

# 6. The workspace now compiles.
cargo check --manifest-path /tmp/alloy-e0502/Cargo.toml

# 7. Nothing was written that should not have been (SEC1, SEC5).
test ! -e /tmp/alloy-e0502/.env
ls /tmp/alloy-e0502/.alloy            # data dir, graph/, artifacts, cli/last_session

# 8. Holdout gate (RFC-0016).
cargo test -p alloy-eval -- holdout_local_diagnostic
```

## Appendix D — Downstream obligations

### D.1 RFC-0013 (Capability Registry & Workers)

1. MUST expose a registry constructible from `(router factory, ToolHandle, GraphViewHandle, ContextEngine)` without the CLI knowing any worker's internals.
2. MUST provide a `CapabilityExecutor` implementation usable as `LinearSchedulerDeps.capabilities`, so §6.2 step 11 is a substitution and not a rewrite.
3. MUST NOT require the CLI to select a capability for a node; the scheduler resolves by `CapabilityId`.
4. Worker-side router errors MUST already be classified into `FailureIr.error_class` so §9.3's exit mapping needs no CLI-side classification.
5. **`RW6` target resolution is 0013's.** The CLI passes goal text verbatim (**SQ5c**) and documents the naming requirement to the user (Appendix C note). It MUST NOT rewrite, augment, or infer a target, and 0013 MUST NOT assume the CLI has enriched the goal.

### D.2 RFC-0012 (Context Engine)

1. MUST consume the `context_profile` this RFC parses (`total_token_budget`, `weights.{conversation,working_set,artifacts}`) without renaming those keys.
2. MUST be constructible at §6.2 step 10 from `(context_profile, storage, graph)` alone, so the composition-root order is stable.
3. `DomainWeights::validate` remains 0012's (finiteness, sign). The **sum-to-one check is `RuntimeConfig::load`'s** (**PF13**) — 0012 MUST NOT assume its own validator is the only gate, and MUST NOT silently renormalise weights, which would mask the config error the CLI is trying to report.
4. MUST tolerate an empty graph (**IX7**).

### D.3 RFC-0016 (Eval) — offline provider

`RouterConfig` supports exactly one provider kind, `openai_compatible`, and **T9** forbids `alloy-cli` depending on `alloy-eval`. A `ScriptedProvider` therefore **cannot** be selected through `router.toml` alone, and no CLI flag may select one (**B3**).

1. The offline path runs a **loopback HTTP server speaking the OpenAI-compatible API** (bound to `127.0.0.1:0`, replaying scripted completions), with `router.toml`'s `base_url` pointed at it. The CLI sees an ordinary provider and needs no knowledge of eval at all.
2. The test harness — not `alloy-cli` — owns starting the server, writing the ephemeral `router.toml`, and exporting a dummy value for `api_key_env`.
3. `[sandbox].network = "deny"` governs *sandboxed child processes*; the loopback server is reached by the parent runtime's HTTP client, which the sandbox does not mediate. Deployments that additionally restrict the parent's egress MUST permit loopback for this test.
4. This keeps AC70 honest: the offline run exercises the same code path a live provider does, differing only in `base_url`.

## Appendix E — Verification status of referenced identifiers

Statuses are as observed in the worktree this RFC was written against, where `alloy-cli` is unimplemented. Where the implementation pass observed different merged behaviour, **the implementation is authoritative**; each pin above is written to be correct under the implementation's reading, and the amendment column names what closes the gap.

| Identifier / behaviour | Status observed | Handling here |
| --- | --- | --- |
| `session::profiles` (`validate_mvp_profile`, `MVP_PROFILES`) | Module exists; **crate-private** | Additive re-export folded into **A1** (**PF1**) |
| `RunController::approve` cross-process | Requires an in-process waiter; `UnknownGate` otherwise | Amendment **A4** (durable fallback), **SQ9**, AC55 |
| `logging::init_tracing` | Writes to **stdout** | Amendment **A5** (`init_tracing_stderr`), **OUT1**, **T11**, AC8 |
| `DomainWeights::validate` | Finiteness and sign only; no sum-to-one | **PF13** puts the check in `RuntimeConfig::load`, AC20 |
| `ApprovalRequested` payload | Carries no artifact reference | **GA2** derives the subject from the latest `EditApplied` event, AC58 |
| `NodeState` payload | Carries `node_id`, no node kind | **CR19** restricted to FOW5's `EditApplied` leg, AC43 |
| `RunError` / `SchedError` | `#[non_exhaustive]` upstream | **EX5** variant-list-pinning test + mandatory wildcard arm, AC61 |
| Session enumeration | No `list_sessions` on `SessionRows` | `<data_dir>/cli/last_session` marker (**SQ5b**, **SQ8b**), AC60; Q7 revisits |
| `RouterConfig` provider kinds | Only `openai_compatible` | Loopback scripted server (Appendix D.3), AC70 |
| `sessions.graph_version` | Column exists; the only writer sets `NULL` | Amendment **A3**; **IX5** Stub until then |
| `RuntimeConfig` profile parsing | `[budgets]` partial + `[observability]` only | Amendment **A1** |
| RFC-0013 target resolution (`RW6`) | Worker resolves from goal text or ingested diagnostics | Appendix C goal names a file; **SQ5c** forbids CLI enrichment |
| `alloy-runtime::context` (`ContextEngine`, `context_profile`) | Shipped per the implementation pass | **PF13** and step 10 written against the shipped `context_profile` flow |
| Capability registry (RFC-0013) | Implementation-grade spec; wired at step 11 | **CR12** narrowed to genuinely-absent builds; AC39 scoped accordingly |
| Provenance-at-session-creation | Absent; RFC-0018 / research §7.11 item 4 | Seam named at §7.1 step 3; nothing depends on it |

---

**End of RFC-0015.**
