# RFC-0010: Scheduler & Runtime Adapters

| Field | Value |
| --- | --- |
| **Status** | Draft |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0003](./RFC-0003-session-manager-run-controller.md) (merged), [RFC-0004](./RFC-0004-observability-cost-metering.md) (merged), [RFC-0006](./RFC-0006-mcp-host-builtins.md) (merged), [RFC-0009](./RFC-0009-task-dag-templates-planner.md) (merged) |
| **Effort** | 5–8 person-days |
| **Related RFCs** | [0001](./RFC-0001-alloy-runtime.md) host / `SchedError` / drain · [0002](./RFC-0002-storage-artifacts-session-events.md) artifacts / events · [0005](./RFC-0005-sandbox-broker.md) sandbox via MCP · [0007](./RFC-0007-model-router-provider.md) `RetryDisposition` / `FailureIr.retry` (consumed, not a hard dep) · [0013](./RFC-0013-capability-registry-workers.md) capability workers · [0015](./RFC-0015-cli-profiles-config.md) `alloy run` |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | The pre-revision draft of this filename (expanded to implementation grade) |

**Mental model (V2 §6.3 / §10.4 / ADR F-10 / F-16):** the Scheduler is the **first Alloy component that executes a plan**. It walks a validated `TaskDag` **serially** (`max_parallel_* = 1`), dispatches capability nodes versus runtime adapters, checkpoints same-generation state through `put_if_generation`, and returns a `DagOutcome` that RFC-0003 / RFC-0015 surface. `VerifyCompile` / `VerifyTest` / `GateHuman` are **runtime adapters**, not LLM capabilities. Capability workers land in RFC-0013; until then the scheduler MUST inject an explicit stub executor rather than fail opaquely.

**Authority order (highest → lowest):** current `main` source → merged RFCs 0001–0007, 0009, 0016 → Architecture V2 → this document → roadmaps. Never reshape a merged public API solely to match an older V2 sketch or this document's prior outline. RFC-0009 §6.5 / §6.6 and Appendix C are **binding** here.

**Reading rules.** MUST / MUST NOT / SHOULD / MAY are normative. Tables are normative unless labelled *informative*. This RFC contains no product code: every Rust block is a signature or a shape, and every algorithm is expressed as an ordered rule table.

---

## 1. Overview

### 1.1 Purpose

Ship the MVP **linear scheduler and real runtime adapters** inside `alloy-runtime`:

1. **`LinearScheduler`** implementing the merged `Scheduler` trait, replacing `NullScheduler` as the production default.
2. **Ready-set derivation and single-node selection** over a validated `TaskDag` under RFC-0009 §5.3.1 (serial honesty).
3. **Node dispatch** — capability nodes through an injected `CapabilityExecutor`; adapter nodes through `VerifyCompileAdapter` / `VerifyTestAdapter` / `GateHumanAdapter`; structural `Aggregate` through a deterministic fold.
4. **Real adapters** — `McpVerifyCompileAdapter` / `McpVerifyTestAdapter` over RFC-0006 `cargo_check` / `cargo_test` via an injected `ToolCaller`; `SessionGateHumanAdapter` bridging `WaitingApproval` to `SessionPlane::register_gate_waiter` + `RunController::approve`.
5. **Retry / backoff / tier escalation** execution of the RFC-0009 `RetryPolicy` under the RFC-0007 admission rule.
6. **Same-generation checkpointing** through `DagStore::put_if_generation`, with a pinned `artifacts → CAS → events` write order and a Conflict abort.
7. **Ownership, cancellation, drain, restart-resume**, budget enforcement, and observability (`NodeState`, `DecisionKind::{Retry, Budget, Gate}`, run-scoped cost meter).

### 1.2 Problem statement

Seven RFCs have built substrate — storage, sessions, observability, sandbox, tool bus, model router, DAG store — and nothing yet runs a plan. Current `main` registers `NullScheduler` (`SchedError::Unavailable`) and the adapter traits exist only as `Unavailable*` stubs. Without this RFC there is no ready-queue, no verify loop, no gate bridge, no checkpointed node progress, and neither RFC-0015 (`alloy run`) nor RFC-0013 (workers) has an execution contract to target.

### 1.3 Scope

| In scope | Detail |
| --- | --- |
| Linear `Scheduler` | `LinearScheduler` replacing `NullScheduler` in production wiring |
| Ready-queue | `promotable_nodes` / `ready_nodes`; select exactly one node; serial dispatch |
| Dispatch | Capability vs verify vs gate vs `Aggregate` |
| Verify adapters | MCP-backed compile/test adapters over an injected `ToolCaller` |
| Gate adapter | Real `GateHuman` ↔ `RunController` / `SessionPlane` bridge, including expiry |
| Retry execution | Attempt counters, backoff sleep, escalation, exhaustion → durable `Failed` |
| Checkpointing | Same-generation `put_if_generation`; write order; Conflict abort |
| Ownership | Process-local lease + OS advisory lock on `<data_dir>/scheduler.lock` |
| Cancel / drain | Token propagation; `Cancelled` vs `Skipped`; runtime drain composition |
| Budgets | Effective session budget (policy ∧ `Goal` `MaxUsd`); meter rebuild on resume |
| Observability | Spans, `NodeState` events, DecisionLog (no double-count of model/tool) |
| Tests | Unit + cross-subsystem `repair_local_diagnostic` against SQLite + sandboxed `cargo_check` |

### 1.4 Non-goals

| Deferred item | Owner |
| --- | --- |
| Topology mutation / replan / template selection | **RFC-0009** (scheduler may only cancel/skip existing nodes and checkpoint `ReplanRequired`) |
| Capability worker logic and prompts | **RFC-0013** |
| Concurrent / parallel node execution | **Forbidden** by RFC-0009 §6.5 — MUST NOT be built |
| File leases / priority function | Deferred pending eval (V2 §6.1 / §6.3) |
| Applying cache hits / `CachedHit` transitions | Deferred by RFC-0009 (day-1 `cache_key = None`) |
| `alloy run` CLI surface | **RFC-0015** |
| `EdgeKind::Hint` semantics | Deferred (inert per RFC-0009 §5.10) |
| Multi-process schedulers, distributed workers, Temporal-style durability | Deferred (V2 §6.3) |
| Sixth crate / writing `.env` | Forbidden |

### 1.5 Day-1 MVP (normative)

1. `LinearScheduler` MUST implement `Scheduler` and MUST be constructed from `LinearSchedulerDeps` (§3.10) with **no** `RuntimeHandle`.
2. Execution MUST be **serial**: at most one node in `NodeState::Running` per process. `BudgetPolicy.max_parallel_nodes`, `max_parallel_cargo`, and `max_parallel_edits` MUST all be `1`; any other value MUST fail construction with `SchedError::Config`.
3. Ready-set derivation MUST apply RFC-0009 §5.3.1 exactly. `EdgeKind::Hint` MUST be ignored. More than one Ready node MUST fail closed with `SchedError::Invariant`; the scheduler MUST NOT pick arbitrarily and MUST NOT run two nodes concurrently.
4. `VerifyCompile` / `VerifyTest` MUST reach `cargo_check` / `cargo_test` through `ToolCaller` (§3.4). Exit `101` returned as `Ok(ToolResult)` with `ToolError::ExecutionFailed` MUST become `VerifyOutcome { ok: false, .. }` (**normal outcome**). A sandbox denial MUST become `AdapterError::PermissionDenied` (**error**), never a compile/test failure.
5. `GateHuman` MUST checkpoint `NodeState::WaitingApproval` + `DagState::WaitingApproval`, emit `ApprovalRequested`, register a gate waiter, and **block inside `Scheduler::run`** until an approval arrives, cancel fires, or the node `timeout_ms` elapses. `Scheduler::run` MUST NOT return `DagState::WaitingApproval`.
6. Every production DAG write MUST use `put_if_generation(&dag, Some(dag.generation))` with `dag.generation` **unchanged**. On `StoreError::Conflict` the scheduler MUST stop checkpointing and MUST terminate the run per §5.8.4.
7. Retry admission MUST require `failure.retry == RetryDisposition::Retryable` **and** `failure.error_class ∈ policy.retry_on` **and** `attempts_started < policy.max_attempts` (RFC-0007 §8.4.1).
8. A durable `NodeState::Failed` means **retries exhausted or non-retryable**. A retryable soft failure MUST be checkpointed as `Ready` with `DagState::Running` (§5.8.3 C8).
9. `Scheduler::run` MUST return only `DagState ∈ {Succeeded, Failed, Cancelled, ReplanRequired}` on the `Ok` path.
10. Alloy MUST NEVER write `.env`. New knobs are documented in `example.env` comments and/or profile TOML only.

### 1.6 Review-driven pins (index)

*Informative index of the decisions the reviews demanded; each row is normative in its own section.*

| Topic | Pin | Section |
| --- | --- | --- |
| Tool boundary | `ToolCaller` in `alloy-runtime`; `ToolHandleToolCaller` in `alloy-tools` | §3.4 / §3.5 |
| Adapter home | `McpVerify*Adapter` live in `alloy-runtime` holding `Arc<dyn ToolCaller>` | §2.6 / §3.6 |
| Deps | `EventStore`, `SessionRows`, `SessionPlane` (Clone), `CostMeterFactory`, `VerifyPermissions`, `runtime_cancel`; no `RuntimeHandle` | §3.10 |
| Ownership | Process map + `<data_dir>/scheduler.lock` exclusive advisory lock | §4.5 |
| Shutdown | `set_scheduler(NullScheduler)` while `Running` **and** idle, before `drain` | §4.6 |
| Write order | artifacts → CAS → events; recovery filter on newest matching `to` | §5.8.1 / §5.3.3 |
| Retry durability | C8 single CAS to `Ready` with `DagState::Running` | §5.8.3 |
| Gate | `ApprovalResolved` scan (allow/allow_once/deny/expired), re-register-only resume, `expire_gate` | §5.7 |
| Cancel | `pending_cancels`, run-side forced C6, RAII `OwnedGuard` | §5.12 |
| Budget | Effective USD = min(policy, finite `MaxUsd`); `<= 0` ⇒ exhausted | §5.16 |
| Outcome | Total `DagState` table D1–D8 + stall; `Skipped` never succeeds alone | §5.17 |

---

## 2. Architecture Integration

### 2.1 Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §6.1 Why a DAG / ADR F-16 | Linear MVP; provenance, gates, retries — not fake parallelism |
| §6.2 Task DAG | Consume merged types; cancel/skip only; checkpoint `ReplanRequired` only |
| §6.3 Scheduler | Ready-queue, retries, budgets, cancel, RunController integration |
| §6.5 Repair sequence | CLI → Session → RunController → Planner → **Scheduler** → workers/adapters → GateHuman |
| §10.4 Runtime adapters | `VerifyCompile` / `VerifyTest` / `GateHuman` are adapters, not capabilities |
| Appendix B | `max_parallel_* = 1` honesty |
| Appendix C | Node state machine — reconciled in §5.18 |
| ADR F-10 | Verify*/Gate are not LLM capabilities |
| ADR F-03 | No `follow_up_nodes`; replan requests only |

### 2.2 Relationship to merged RFCs

| RFC | What this RFC consumes / extends |
| --- | --- |
| **0001** | `Scheduler` / `DagOutcome` / `DagState` / `SchedError` / `AdapterError` / `RuntimeHandle::{run_dag, cancel_dag, set_scheduler}` / `AlloyRuntime::drain` / `RuntimeConfig.run_timeout` / `BudgetPolicy`. **Amended** in §2.7 (drain deadline; `reconcile_terminal_run` forwarder) |
| **0002** | `ArtifactStore` (labels, `ArtifactKind`), `EventStore` (append + read + existence probes), session event envelopes |
| **0003** | `RunController::{start, cancel, approve, request_replan}`, `SessionPlane::register_gate_waiter`, outcome merge table, gate waiter lifecycle, `RunControlState`. **Amended** in §2.7 (`expire_gate`; resume reconcile; `WaitingApproval` merge rule) |
| **0004** | `DecisionLog`, `SharedCostMeter`, `BudgetCheck`, `maybe_signal_budget_warning`, `reaccumulate_cost_from_events`, `DecisionKind::{Retry, Budget, Gate}` |
| **0005** | Sandbox enforcement **through** MCP builtins — adapters never call the broker |
| **0006** | `ToolCall` / `ToolResult` / `ToolError` / `McpError` / `ToolHandle` / `cargo_check` / `cargo_test` / disclosure tags |
| **0007** | `RetryDisposition` / `FailureIr.retry` admission rule (consumed; not a Cargo dependency of the hard path) |
| **0009** | Validated DAG shapes, readiness rules, `put_if_generation`, §6.5 / §6.6, envelopes, retry field ownership, Appendix C obligations |

### 2.3 Already implemented | Added by RFC-0010 | Deferred

| Category | Contents |
| --- | --- |
| **Already implemented** | `Scheduler` / `DagState` / `DagOutcome` / `NullScheduler`; `SchedError` / `AdapterError` bases; adapter traits + `Unavailable*`; `TaskDag` / node types; `DagStore::put_if_generation`; envelopes; `RetryPolicy`; `RunController` / gate registry; `DecisionLog` / `SharedCostMeter` / `reaccumulate_cost_from_events`; MCP builtins + `ToolHandle`; runtime single-flight `run_dag` |
| **Added by RFC-0010** | `LinearScheduler` + `LinearSchedulerDeps` + `SchedConfig`; `ready_nodes` / `promotable_nodes`; `ToolCaller` / `ToolCallerError`; `ToolHandleToolCaller` (alloy-tools); `McpVerifyCompileAdapter` / `McpVerifyTestAdapter` / `SessionGateHumanAdapter`; rustc-JSON → `DiagnosticEvent` ingest; `CapabilityExecutor` + `CapabilityOutcome`; `CostMeterFactory`; `VerifyPermissions`; additive `SchedError` / `AdapterError` variants; `Scheduler::reconcile_terminal_run`; `RunController::expire_gate`; checkpoint/CAS/write-order policy; ownership lease + OS lock; cancel/drain semantics; budget + decision emission; cross-subsystem e2e |
| **Deferred** | Parallel nodes; file leases; cache-hit application; worker bodies (0013); CLI (0015); Hint edges; durable backoff timers; multi-process scheduling |

### 2.4 What RFC-0013 and RFC-0015 may rely on

| Consumer | May rely on |
| --- | --- |
| **RFC-0013** | `CapabilityExecutor` injection point; `CapabilityExecContext` fields (`effective_tier`, `budget`, `input`, `attempt`, `cost_meter`, `cancellation`); serial dispatch; retry admission owned by the scheduler (workers return one-shot `CapabilityOutcome::Failed`, never self-retry); verify diagnostics reaching repair nodes through predecessor envelopes |
| **RFC-0015** | `DagOutcome` field semantics (§5.18); `Scheduler::run` / `cancel` reached only through `RunController::start` / `cancel`; gate UX over existing `approve`; terminal mapping already implemented in RFC-0003; `failure.error_class` vocabulary for exit codes |

### 2.5 Inherited RFC-0009 constraints (normative — restated)

| Constraint | Source |
| --- | --- |
| MVP scheduler is **linear**; at most one node runs at a time | RFC-0009 §6.5, ADR F-16 |
| Concurrency safety of Ready siblings is **unmodelled**; a concurrent scheduler MUST NOT be built on this model | RFC-0009 §6.5 |
| `EdgeKind::Sequence` is ordering, **not** a lease | RFC-0009 §6.5 |
| `max_parallel_* = 1` — scheduler honesty | V2 Appendix B |
| Only `PlanService` mutates topology; the scheduler may cancel/skip **existing** nodes and write **same-generation** checkpoints | RFC-0009 §6.2 / §6.4 |
| Production DAG writes use `put_if_generation` with `expected = Some(current.generation)` and unchanged `dag.generation` | RFC-0009 §6.6 |
| On `StoreError::Conflict` the scheduler MUST stop checkpointing | RFC-0009 §6.6 |
| Replan is rejected while `DagState::Running` (`DagBusy`) — so the scheduler MUST checkpoint `ReplanRequired` | RFC-0009 §6.6 / Appendix C |
| The scheduler MUST NOT read `planner::*`, select templates, or introduce fan-out edge kinds | RFC-0009 |
| `EdgeKind::Hint` MUST NOT affect scheduling | RFC-0009 §5.10 |
| Cache-hit application is deferred; `CachedHit` MUST NOT be produced | RFC-0009 §12 |
| No attempt-counter field on `TaskNode` — attempt state stays outside the merged struct | RFC-0009 §6.4 |
| Rewrite the final `input_ref` when Data predecessors succeed | RFC-0009 §5.3.0 / Appendix C |
| Single scheduler writer per DAG — ownership / leasing is RFC-0010's responsibility | RFC-0009 Appendix C |

**Any text in this RFC implying parallel node execution is a defect.**

### 2.6 Dependency boundaries

```text
alloy-cli / host assembly / integration tests
   │  builds ToolHandle (0006) and wraps it:
   │      Arc<dyn ToolCaller> = Arc::new(alloy_tools::mcp::ToolHandleToolCaller::new(handle))
   ▼
alloy-runtime::scheduler::LinearScheduler
   ├──► DagStore / ArtifactStore / EventStore          (0002 / 0009)
   ├──► SessionRows / SessionPlane / RunController      (0003)
   ├──► DecisionLog / CostMeterFactory                  (0004)
   ├──► adapters::verify::{McpVerifyCompileAdapter, McpVerifyTestAdapter}
   │        └──► Arc<dyn ToolCaller>  (trait defined in alloy-runtime)
   ├──► adapters::gate::SessionGateHumanAdapter ──► SessionPlane
   └──► CapabilityExecutor                              (stub now; 0013 fills)

alloy-tools (already depends on alloy-runtime)
   └──► mcp::ToolHandleToolCaller : alloy_runtime::ToolCaller
            └──► ToolHandle ──► McpPlatform ──► builtins ──► sandbox broker (0005)
```

| Rule | Statement |
| --- | --- |
| B1 | `alloy-runtime` MUST NOT depend on `alloy-tools` at the Cargo level. The dependency edge stays `alloy-tools → alloy-runtime`. |
| B2 | `ToolCaller` and `ToolCallerError` MUST be defined in `alloy-runtime` (`adapters::tool_caller`). |
| B3 | `McpVerifyCompileAdapter` and `McpVerifyTestAdapter` MUST live in `alloy-runtime` (`adapters::verify`) and MUST hold `Arc<dyn ToolCaller>`. They MUST NOT name `ToolHandle`, `McpError`, or any `alloy-tools` type. |
| B4 | `ToolHandleToolCaller` and `map_mcp_error` MUST live in `alloy-tools` (`mcp::tool_caller`), the crate that defines `McpError`, so the mapping match is exhaustive without a catch-all. |
| B5 | No sixth crate. No feature flag is required for either side. |
| B6 | The scheduler MUST NOT import `planner::*` (CI grep, AC 57). |

### 2.7 Amendments to merged RFCs (normative)

Each amendment is additive and MUST land with this RFC.

| # | RFC | Amendment | Rationale |
| --- | --- | --- | --- |
| A1 | 0001 | `AlloyRuntime::drain` MUST compute `deadline = Instant::now() + grace` **before** awaiting `Scheduler::cancel`, and MUST bound that await by the remaining budget (`tokio::time::timeout`). Today the deadline is taken after the cancel await, so a slow cancel consumes the whole grace and the in-flight wait degenerates. | §5.12 makes `cancel` blocking (it awaits drain completion), so the pre-cancel deadline is required for `drain(grace)` to mean anything. |
| A2 | 0001 | Additive `RuntimeHandle::reconcile_terminal_run(dag_id, terminal) -> Result<(), RuntimeError>` forwarder to `Scheduler::reconcile_terminal_run`, allowed in phase `Running` \| `Draining`, **not** admitted through the single-flight run gate. | Lets RFC-0003 resume reconcile a DAG without becoming scheduler-aware. |
| A3 | 0001 | `SchedError` becomes `#[non_exhaustive]` and gains the §3.2 variants; `runtime_to_run` MUST cover them all plus a catch-all arm. | Additive variants otherwise break the existing exhaustive match in `session::map_err`. |
| A4 | 0003 | Additive `RunController::expire_gate(run, gate) -> Result<(), RunError>` (§3.15). | The gate timeout is scheduler-observed but control-plane-durable; `approve` cannot express "expired" and requires a live waiter. |
| A5 | 0003 | `apply_start_outcome` MUST NOT suppress a terminal scheduler outcome when durable state is `waiting_approval`: for `Ok(DagOutcome { state: Failed \| Cancelled, .. })` it MUST apply the terminal transition (events before row) instead of merging. `Running` / `WaitingApproval` / `ReplanRequired` outcomes keep merging. `replan_requested` / `cancelling` / `cancelled` keep winning. | Gate deny/expiry can terminalize the DAG while the row still says `waiting_approval` (expiry write failed, or the scheduler observed the resolution first); merging would strand the run non-terminal forever. |
| A6 | 0003 | `SessionService::resume` MUST call `RuntimeHandle::reconcile_terminal_run(dag_id, terminal)` (best effort, warn on error, never abort resume) for every run row whose durable state is terminal (`failed` / `cancelled` / `succeeded`) and whose `goal_json` yields a `dag_id`. | Closes the crash window where the control row is `failed` (Deny/expiry) but the DAG blob is still `WaitingApproval` — no `start` will ever be dispatched for a terminal row, so nothing else would terminalize the DAG. |
| A7 | 0003 | `expire_gate` MUST be idempotent with respect to a missing waiter (no `UnknownGate`), because the timeout races waiter removal. | The scheduler cannot distinguish "waiter already taken" from "gate never registered" without a durable waiter table. |

### 2.8 Trust boundary on load

RFC-0009 §6.3 promises only that *plan-path* DAGs were validated. `SchedConfig.validate_on_load` MUST be `true` in production (§3.12), so `LinearScheduler` re-validates every loaded DAG with `DagValidator::validate` before executing it.

---

## 3. Public Rust API

### 3.1 Reused types (normative — unchanged fields)

| Type | Module | Rule |
| --- | --- | --- |
| `Scheduler`, `DagState`, `DagOutcome` | `scheduler` | Fields unchanged; `Scheduler` gains one **defaulted** method (§3.14) |
| `TaskDag`, `TaskNode`, `NodeKind`, `NodeState`, `EdgeKind`, `RetryPolicy`, `Backoff`, `CacheKey`, `ApprovalSpec` | `dag::types` | Unchanged |
| `NodeInputEnvelope`, `NodeInputPayload`, `NodeOutputEnvelope`, `PredecessorOutput`, `ENVELOPE_SCHEMA_VERSION` | `dag::io` | Unchanged |
| `VerifyCompileAdapter`, `VerifyTestAdapter`, `GateHumanAdapter`, `NodeExecContext`, `NodeExecRef`, `VerifyOutcome`, `Approval` | `adapters` | Signatures unchanged |
| `DagStore::put_if_generation`, `DagStore::get` | `storage::dags` | Unchanged |
| `ArtifactStore`, `ArtifactPut`, `ArtifactKind`, `ArtifactMeta` | `storage::artifacts` | Unchanged |
| `EventStore`, `EventSink`, `NewSessionEvent`, `SessionEventType` | `storage::events` / `events` | Unchanged |
| `SessionRows`, `RunRow`, `RunGoalRecord`, `RunControlState` | `storage` / `session` | Unchanged |
| `SessionPlane::register_gate_waiter`, `RunController` | `session` | `RunController` gains `expire_gate` (§3.15) |
| `FailureIr`, `ErrorClass`, `RetryDisposition`, `DiagnosticEvent`, `DiagnosticLevel`, `SpanRef` | `types::diagnostic` | Unchanged |
| `PermissionToken`, `Grant`, `ExecAllow`, `Glob` | `types::permission` | Unchanged |
| `ToolCall`, `ToolResult`, `ToolError`, `ToolName` | `types::tools` | Unchanged |
| `BudgetPolicy`, `TokenBudget`, `ModelTier`, `Goal`, `Constraint` | `types::budget` | Unchanged |
| `SharedCostMeter`, `BudgetCheck`, `DecisionLog`, `DecisionRecord`, `DecisionKind` | `obs` | Unchanged |
| `DagValidator`, `ValidateOpts` | `dag::validate` | Unchanged |

### 3.2 Additive extension — `SchedError`

Existing variants remain. The enum becomes `#[non_exhaustive]` and gains seven variants. There is **no** `Execution` variant and **no** `InvalidDagState` variant: a DAG state that cannot run is either a returnable `DagOutcome` or an `Invariant` violation.

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SchedError {
    /// No real scheduler registered (NullScheduler only).
    #[error("unavailable")]
    Unavailable,

    /// Run cancelled without a durable outcome (prefer `Ok(DagOutcome)`).
    #[error("cancelled")]
    Cancelled,

    /// Unknown DAG id.
    #[error("dag not found: {0}")]
    DagNotFound(DagId),

    /// Internal scheduler error.
    #[error("internal: {0}")]
    Internal(String),

    /// Invalid construction / parallelism / data_dir configuration.
    #[error("config: {0}")]
    Config(String),

    /// Generation CAS conflict — stop checkpointing (§5.8.4).
    #[error("generation conflict for dag {dag_id}")]
    Conflict { dag_id: DagId },

    /// Contract violation (multiple Ready, corrupt DAG, impossible state).
    #[error("invariant: {0}")]
    Invariant(String),

    /// Store / artifact / event I/O failure after mapping.
    #[error("store: {0}")]
    Store(String),

    /// Another in-process run already owns this DAG (§4.5).
    #[error("dag already owned: {0}")]
    AlreadyOwned(DagId),

    /// No run row binds this DAG (Appendix F).
    #[error("no run bound to dag {0}")]
    RunBindingMissing(DagId),

    /// Scheduler ownership could not be established (OS lock, poisoned map).
    #[error("ownership: {0}")]
    Ownership(String),
}
```

**Complete boundary table (normative).** `run_dag` wraps every non-`Unavailable` variant in `RuntimeError::Scheduler`; `runtime_to_run` (RFC-0003 `session::map_err`) MUST map every arm below and MUST end with `RuntimeError::Scheduler(other) => RunError::Internal(other.to_string())`.

| `SchedError` | `RuntimeHandle::run_dag` | `runtime_to_run` → `RunError` | Durable run row after `start` |
| --- | --- | --- | --- |
| `Unavailable` | `RuntimeError::SchedulerUnavailable` | `SchedulerUnavailable` | `accepted` retained (re-dispatchable) |
| `Cancelled` | `Scheduler(Cancelled)` | handled by the `start` **success** path (finalize cancelled); helper returns `Internal("bug: …")` | `cancelled` |
| `DagNotFound(id)` | `Scheduler(DagNotFound)` | `InvalidPhase("dag not found: {id}")` | prior state retained |
| `Config(m)` | `Scheduler(Config)` | `Internal("scheduler config: {m}")` | prior state retained |
| `Conflict { dag_id }` | `Scheduler(Conflict)` | `InvalidPhase("dag generation conflict: {dag_id}")` | prior state retained; caller MAY replan |
| `Invariant(m)` | `Scheduler(Invariant)` | `Internal("scheduler invariant: {m}")` | prior state retained |
| `Store(m)` | `Scheduler(Store)` | `Internal("scheduler store: {m}")` | prior state retained |
| `AlreadyOwned(id)` | `Scheduler(AlreadyOwned)` | `InvalidPhase("dag already owned: {id}")` | prior state retained |
| `RunBindingMissing(id)` | `Scheduler(RunBindingMissing)` | `Internal("no run bound to dag {id}")` | prior state retained |
| `Ownership(m)` | `Scheduler(Ownership)` | `Internal("scheduler ownership: {m}")` | prior state retained |
| `Internal(m)` | `Scheduler(Internal)` | `Internal(m)` | prior state retained |
| future variant | `Scheduler(other)` | `Internal(other.to_string())` | prior state retained |

**Planned failures return `Ok`.** Compile-loop exhaustion, gate deny/expiry, budget exhaustion, run timeout, and cancellation MUST return `Ok(DagOutcome { .. })` whenever a durable outcome was written. `Err(SchedError)` is reserved for "no durable outcome exists".

### 3.3 Additive extension — `AdapterError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    #[error("unavailable")]
    Unavailable,

    #[error("cancelled")]
    Cancelled,

    /// Legacy free-form tool failure (retained; new code prefers `ToolFailure`).
    #[error("tool: {0}")]
    Tool(String),

    #[error("internal: {0}")]
    Internal(String),

    /// A tool ran and failed, carrying the merged RFC-0006 taxonomy.
    #[error("tool failure: {0}")]
    ToolFailure(#[source] ToolError),

    /// Sandbox / token / disclosure denial. NOT a compile or test failure.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Adapter-observed deadline (host `call_timeout`, sandbox `exec_timeout`).
    #[error("timeout")]
    Timeout,

    /// MCP host draining or stopped.
    #[error("shutting down")]
    ShuttingDown,

    /// Artifact store failure while persisting a raw log.
    #[error("artifact: {0}")]
    Artifact(String),
}
```

### 3.4 `ToolCaller` (new, `alloy-runtime`)

```rust
/// The only tool seam runtime adapters may use. Implemented in `alloy-tools`
/// over `ToolHandle`; implemented in tests by recording doubles.
///
/// Cancellation is by dropping the returned future (RFC-0006 §3.8).
#[async_trait]
pub trait ToolCaller: Send + Sync {
    async fn call(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, ToolCallerError>;
}

/// Host-boundary failure, mirroring `McpError` without naming it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolCallerError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("permission token expired")]
    TokenExpired,
    #[error("invalid permission token: {0}")]
    InvalidToken(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("host shutting down")]
    ShuttingDown,
    #[error("cancelled")]
    Cancelled,
    #[error("timeout")]
    Timeout,
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("internal: {0}")]
    Internal(String),
}
```

| Rule | Statement |
| --- | --- |
| TC1 | `ToolCallerError` MUST carry `InvalidToken` so an uncompilable grant glob is distinguishable from a policy denial. |
| TC2 | `ToolCallerError` MUST NOT embed `SandboxError` or `PermissionDenial`; both collapse into redacted strings already produced by RFC-0006 §9.1. |
| TC3 | `Timeout` MUST NOT carry a `Duration`: the adapter reports the fact, the scheduler owns deadlines (§5.19). |

### 3.5 `ToolHandleToolCaller` + `map_mcp_error` (new, `alloy-tools`)

```rust
// crates/alloy-tools/src/mcp/tool_caller.rs
pub struct ToolHandleToolCaller {
    handle: ToolHandle,
}

impl ToolHandleToolCaller {
    #[must_use]
    pub fn new(handle: ToolHandle) -> Self;

    /// Selectors this caller was built with (host wiring assertions / tests).
    #[must_use]
    pub fn selectors(&self) -> &[ToolSelector];
}

#[async_trait]
impl alloy_runtime::ToolCaller for ToolHandleToolCaller {
    async fn call(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, ToolCallerError> {
        self.handle.call(call, perms).await.map_err(map_mcp_error)
    }
}

/// Exhaustive: no catch-all arm, so a new `McpError` variant breaks the build here.
pub fn map_mcp_error(err: McpError) -> ToolCallerError;
```

**`map_mcp_error` (normative, total over today's `McpError`):**

| `McpError` | `ToolCallerError` | Note |
| --- | --- | --- |
| `UnknownTool(name)` | `UnknownTool(name)` | Registry/wiring bug |
| `PermissionDenied(PermissionDenial::NotDisclosed)` | `PermissionDenied("tool not disclosed for handle selectors")` | Selector wiring bug (Appendix J) |
| `PermissionDenied(other)` | `PermissionDenied(other.to_string())` | `Display` is already redacted (RFC-0006 §9.1) |
| `TokenExpired` | `TokenExpired` | — |
| `InvalidToken(m)` | `InvalidToken(m)` | Uncompilable grant glob / defensive invariant |
| `InvalidArguments(m)` | `InvalidArguments(m)` | Adapter-side bug; never a verify failure |
| `Unsupported(m)` | `Unsupported(m)` | Out-of-process servers |
| `ShuttingDown` | `ShuttingDown` | — |
| `Cancelled` | `Cancelled` | Host cancel token |
| `Timeout(d)` | `Timeout` | Duration dropped by TC3; MAY be logged |
| `Sandbox(e)` | `Sandbox(e.to_string())` | Already redacted |
| `Internal(m)` | `Internal(m)` | — |

`ToolCallerError` → `AdapterError` (normative; applied inside the verify adapters):

| `ToolCallerError` | `AdapterError` |
| --- | --- |
| `PermissionDenied(m)` | `PermissionDenied(m)` |
| `TokenExpired` | `PermissionDenied("permission token expired")` |
| `InvalidToken(m)` | `PermissionDenied("invalid permission token: {m}")` |
| `UnknownTool(n)` | `Internal("tool not registered: {n}")` |
| `InvalidArguments(m)` | `Internal("adapter built invalid arguments: {m}")` |
| `Unsupported(m)` | `Internal("unsupported tool path: {m}")` |
| `ShuttingDown` | `ShuttingDown` |
| `Cancelled` | `Cancelled` |
| `Timeout` | `Timeout` |
| `Sandbox(m)` | `ToolFailure(ToolError::Permanent { code: "sandbox", message: m })` |
| `Internal(m)` | `Internal(m)` |

### 3.6 Verify adapters (new, `alloy-runtime`)

```rust
// crates/alloy-runtime/src/adapters/verify.rs
pub struct McpVerifyCompileAdapter {
    tools: Arc<dyn ToolCaller>,
    perms: Arc<dyn VerifyPermissions>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl McpVerifyCompileAdapter {
    #[must_use]
    pub fn new(
        tools: Arc<dyn ToolCaller>,
        perms: Arc<dyn VerifyPermissions>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self;
}

#[async_trait]
impl VerifyCompileAdapter for McpVerifyCompileAdapter {
    async fn check(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError>;
}

pub struct McpVerifyTestAdapter { /* identical shape; tool `cargo_test` */ }

#[async_trait]
impl VerifyTestAdapter for McpVerifyTestAdapter {
    async fn test(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError>;
}

/// rustc-JSON (NDJSON) → diagnostics. Pure; no I/O.
pub fn parse_rustc_diagnostics(stdout_utf8: &str) -> Vec<DiagnosticEvent>;

/// Stable dedupe fingerprint (§5.13.4 framing).
pub fn diagnostic_fingerprint(
    code: Option<&str>,
    level: DiagnosticLevel,
    message: &str,
    first_span: Option<&SpanRef>,
) -> Digest;
```

### 3.7 `VerifyPermissions` (new)

```rust
/// Host-owned permission minting for verify adapters. Adapters MUST NOT invent grants.
pub trait VerifyPermissions: Send + Sync {
    fn token_for(
        &self,
        ctx: &NodeExecRef,
        class: VerifyClass,
    ) -> Result<PermissionToken, AdapterError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyClass {
    /// `cargo_check` under `ExecClass::Check`.
    Compile,
    /// `cargo_test` under `ExecClass::Test`.
    Test,
}
```

| Field of the minted token | Source |
| --- | --- |
| `profile` | `Session.profile` for `ctx.session_id` |
| `run_id` | `ctx.run_id` (MUST equal the executing run) |
| `expires` | `None` in MVP, or session policy when RFC-0015 adds one |
| `grants` | MUST include a `Grant::Exec(ExecAllow { binary: "cargo", args_glob })` sufficient for RFC-0006 `match_exec_grant` on the derived argv (Appendix J). Host assembly owns the glob strings. |

`token_for` MUST return `AdapterError::PermissionDenied` (not `Internal`) when the profile catalog has no exec grant for the class, so a mis-provisioned profile is reported as a denial rather than a crash.

### 3.8 `CapabilityExecutor` (new)

```rust
#[async_trait]
pub trait CapabilityExecutor: Send + Sync {
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError>;
}

/// One capability-node attempt.
#[derive(Debug, Clone)]
pub struct CapabilityExecContext {
    pub meta: NodeExecRef,
    pub cancellation: CancellationToken,
    /// From `TaskNode.capability` (always `Some` post-validate).
    pub capability: CapabilityId,
    pub kind: NodeKind,
    /// Effective tier after escalation (§5.11.4).
    pub effective_tier: ModelTier,
    pub budget: TokenBudget,
    /// Node deadline already clamped by the remaining run budget (§5.19).
    pub timeout: Duration,
    /// Decoded input envelope (`schema_version == 1`).
    pub input: NodeInputEnvelope,
    /// Attempt index starting at 1.
    pub attempt: u32,
    /// Run-scoped meter. Workers MUST record model usage here (RFC-0004),
    /// and MUST NOT construct their own meter.
    pub cost_meter: SharedCostMeter,
}

/// Success or structured soft failure. A worker never both succeeds and fails.
#[derive(Debug, Clone)]
pub enum CapabilityOutcome {
    Succeeded { payload: serde_json::Value },
    Failed { failure: FailureIr },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CapabilityExecError {
    #[error("unavailable")]
    Unavailable,
    #[error("cancelled")]
    Cancelled,
    #[error("timeout")]
    Timeout,
    #[error("worker: {0}")]
    Worker(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableCapabilityExecutor;
```

| Rule | Statement |
| --- | --- |
| CE1 | `CapabilityOutcome` MUST be the enum above. The previous struct-with-`Option<FailureIr>` shape made "succeeded and failed" representable. |
| CE2 | The scheduler MUST overwrite `failure.node` with the dispatched `NodeId` before persisting, whatever the worker set. |
| CE3 | Workers MUST NOT retry, sleep for backoff, escalate tiers, write `TaskNode` fields, or write `NodeState` events. |
| CE4 | `UnavailableCapabilityExecutor::execute` MUST return `Err(CapabilityExecError::Unavailable)`. |
| CE5 | `CapabilityExecContext.timeout` is advisory to the worker; the scheduler enforces it independently (§5.19). |

### 3.9 `CostMeterFactory` (new)

```rust
/// Run-scoped meter provider. Injected so the host can share one meter with the
/// RFC-0007 router bridge for the same run.
pub trait CostMeterFactory: Send + Sync {
    fn meter_for(&self, run: RunId) -> SharedCostMeter;
}

/// Process-local factory: one meter per `RunId`, memoized, cleared on `release`.
#[derive(Debug, Default)]
pub struct ProcessCostMeterFactory { /* Mutex<HashMap<RunId, SharedCostMeter>> */ }

impl ProcessCostMeterFactory {
    #[must_use]
    pub fn new() -> Self;
    /// Drop the memoized meter for a finished run.
    pub fn release(&self, run: RunId);
}
```

`meter_for` MUST return the **same** `SharedCostMeter` for repeated calls with the same `RunId` within a process, so a resumed run and the router bridge accumulate into one meter.

### 3.10 `LinearScheduler` and `LinearSchedulerDeps` (new)

```rust
/// Serial ready-queue scheduler (RFC-0010).
pub struct LinearScheduler { /* private; see §4 and Appendix I */ }

pub struct LinearSchedulerDeps {
    /// DAG blobs (CAS checkpoints).
    pub dags: Arc<dyn DagStore>,
    /// Envelopes, raw verify logs, failure IR.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Append **and** read: attempt rebuild, `ApprovalResolved` scan, meter rebuild,
    /// existence probes. `EventStore: EventSink`, so this is also the append path —
    /// which is why the scheduler needs no `RuntimeHandle`.
    pub events: Arc<dyn EventStore>,
    /// Run binding + session workspace/profile/budget (Appendix F).
    pub sessions: Arc<dyn SessionRows>,
    /// Control plane. `SessionPlane` is `Clone` (Arc inner) — store the value, not an `Arc`.
    pub session_plane: SessionPlane,
    /// Gate resolution / expiry (`SessionPlane::runs()`).
    pub runs: Arc<dyn RunController>,
    pub verify_compile: Arc<dyn VerifyCompileAdapter>,
    pub verify_test: Arc<dyn VerifyTestAdapter>,
    pub gate_human: Arc<dyn GateHumanAdapter>,
    /// `UnavailableCapabilityExecutor` until RFC-0013.
    pub capabilities: Arc<dyn CapabilityExecutor>,
    pub decisions: Arc<dyn DecisionLog>,
    pub cost_meters: Arc<dyn CostMeterFactory>,
    /// Process cancellation token (formerly `RuntimeHandle::cancellation()`).
    pub runtime_cancel: CancellationToken,
    /// Session budget ceilings; `max_parallel_*` MUST all be 1.
    pub budget_policy: BudgetPolicy,
    /// Wall-clock budget for one `run`, excluding gate waits (§5.19).
    pub run_timeout: Duration,
    pub config: SchedConfig,
}

impl LinearScheduler {
    /// Validate deps, then acquire the process ownership lock (§4.5).
    pub fn new(deps: LinearSchedulerDeps) -> Result<Self, SchedError>;

    /// Test-only relaxation: permits `validate_on_load = false` and
    /// `host_parallel_honesty = false`. Serial invariants still hold.
    #[cfg(test)]
    pub(crate) fn new_for_test(deps: LinearSchedulerDeps) -> Result<Self, SchedError>;

    /// Additive scheduler-owned reconciliation for a terminal control row (§5.20).
    pub async fn reconcile_terminal_run(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError>;

    /// Debug/test counters (§9.3).
    #[must_use]
    pub fn metrics(&self) -> SchedulerMetrics;
}

#[async_trait]
impl Scheduler for LinearScheduler {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError>;
    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError>;
    async fn reconcile_terminal_run(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError>;
}
```

| Rule | Statement |
| --- | --- |
| D1 | `LinearSchedulerDeps` MUST NOT contain a `RuntimeHandle`. Phase coupling lives in the control plane; the scheduler observes only `runtime_cancel` and its own ownership state. |
| D2 | `events` MUST be `Arc<dyn EventStore>` (not `Arc<dyn EventSink>`): the recovery paths in §5.3.3, §5.7.2 and §5.16.2 are reads. `EventStore::replay_session` carries `where Self: Sized`, so `dyn EventStore` stays object-safe. |
| D3 | `LinearScheduler` MUST be `Send + Sync` and is stored as `Arc<dyn Scheduler>` through `RuntimeHandle::set_scheduler`. |
| D4 | One `LinearScheduler` per process. Constructing a second one against the same `data_dir` MUST fail with `SchedError::Ownership` (§4.5). |
| D5 | `run_timeout` SHOULD come from `RuntimeConfig.run_timeout`; `budget_policy` SHOULD come from `RuntimeConfig.budget_policy`. The scheduler does not read config files. |

### 3.11 `SchedConfig` (new)

```rust
#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// Absolute runtime data dir; owns `<data_dir>/scheduler.lock` (§4.5).
    pub data_dir: PathBuf,
    /// Run-side budget for abandoning an in-flight node after cancel (§5.12).
    pub cancel_drain_grace: Duration,
    /// Extra budget `cancel` allows the run for its forced C6 write (§5.12.3).
    pub cancel_write_grace: Duration,
    /// Upper bound on any single retry backoff sleep (§5.11.3).
    pub max_backoff: Duration,
    /// Host affirmation that every other parallelism knob is pinned to 1
    /// (MCP `max_in_flight` for cargo classes, edit path). MUST be `true`.
    pub host_parallel_honesty: bool,
    /// Re-validate every loaded DAG (§2.8). MUST be `true` in production.
    pub validate_on_load: bool,
    /// Options for the load-time validation.
    pub validate_opts: ValidateOpts,
}

impl SchedConfig {
    /// Defaults with the required `data_dir`.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self;
}
```

| Field | Default | Notes |
| --- | --- | --- |
| `data_dir` | *required* | MUST be non-empty and absolute |
| `cancel_drain_grace` | `5s` | Run stops awaiting the node future after this |
| `cancel_write_grace` | `2s` | `cancel` waits `cancel_drain_grace + cancel_write_grace` for completion |
| `max_backoff` | `60s` | Caps `Fixed` and `Exponential` sleeps |
| `host_parallel_honesty` | `true` | `false` rejected by `new` |
| `validate_on_load` | `true` | `false` rejected by `new` |
| `validate_opts` | `ValidateOpts { enforce_linear_mvp: true, require_gates: true }` | `ValidateOpts::default()` |

`SchedConfig` deliberately has **no** `Default`: a default would have to invent a `data_dir`.

### 3.12 Construction validation (normative)

Checks run in this order; the first failure wins.

| # | Check | Failure |
| --- | --- | --- |
| N1 | `config.data_dir` non-empty | `Config("data_dir must not be empty")` |
| N2 | `config.data_dir.is_absolute()` | `Config("data_dir must be absolute: {path}")` |
| N3 | `budget_policy.max_parallel_nodes == 1` | `Config("max_parallel_nodes must be 1 (serial scheduler)")` |
| N4 | `budget_policy.max_parallel_cargo == 1` | `Config("max_parallel_cargo must be 1")` |
| N5 | `budget_policy.max_parallel_edits == 1` | `Config("max_parallel_edits must be 1")` |
| N6 | `config.host_parallel_honesty == true` | `Config("host_parallel_honesty must be true")` — relaxed by `new_for_test` |
| N7 | `config.validate_on_load == true` | `Config("validate_on_load must be true in production")` — relaxed by `new_for_test` |
| N8 | `config.max_backoff > Duration::ZERO` | `Config("max_backoff must be > 0")` |
| N9 | `config.cancel_drain_grace > Duration::ZERO` | `Config("cancel_drain_grace must be > 0")` |
| N10 | `run_timeout > Duration::ZERO` | `Config("run_timeout must be > 0")` |
| N11 | `create_dir_all(data_dir)` then open+lock `scheduler.lock` (§4.5) | `Ownership("…")` |

`new_for_test` MUST relax **only** N6 and N7. N3–N5 are unconditional: no test may create a parallel scheduler.

### 3.13 Pure helpers (new, public)

```rust
/// Nodes already in `NodeState::Ready`, ascending `NodeId`.
#[must_use]
pub fn ready_nodes(dag: &TaskDag) -> Vec<NodeId>;

/// `Pending` nodes whose Data∪Sequence predecessors are satisfied
/// (RFC-0009 §5.3.1), ascending `NodeId`. `Hint` edges are ignored.
#[must_use]
pub fn promotable_nodes(dag: &TaskDag) -> Vec<NodeId>;

/// Backoff sleep before the attempt following failed attempt `attempt` (1-based),
/// capped by `max_backoff` (§5.11.3).
#[must_use]
pub fn backoff_delay(backoff: &Backoff, attempt: u32, max_backoff: Duration) -> Duration;

/// First-match-wins `DagState` derivation (§5.17). Pure over node states plus
/// the run-local flags the loop tracks.
#[must_use]
pub fn derive_dag_state(dag: &TaskDag, flags: DeriveFlags) -> DagState;

#[derive(Debug, Clone, Copy, Default)]
pub struct DeriveFlags {
    /// A cancel was requested for this DAG (user, runtime drain, or control plane).
    pub cancel_requested: bool,
    /// `RunControlState::ReplanRequested` was observed for the owning run.
    pub replan_requested: bool,
    /// A gate resolution (deny/expired) recorded an `ErrorClass::Approval` failure.
    pub approval_failure: bool,
}
```

Both node helpers are `pub` so RFC-0013 fixtures and RFC-0015 dry-run tooling can assert readiness without a scheduler instance, and so §11 can unit-test readiness on hand-built DAGs.

### 3.14 `Scheduler` trait — additive defaulted method

```rust
#[async_trait]
pub trait Scheduler: Send + Sync {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError>;

    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError>;

    /// Reconcile a DAG whose owning run row is already terminal (§5.20).
    ///
    /// Additive with a default `Ok(())` so `NullScheduler` and test doubles keep
    /// compiling. `terminal` MUST be `Succeeded | Failed | Cancelled`.
    async fn reconcile_terminal_run(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError> {
        let _ = (dag_id, terminal);
        Ok(())
    }
}
```

### 3.15 `RunController::expire_gate` (additive, RFC-0003 amendment A4)

```rust
#[async_trait]
pub trait RunController: Send + Sync {
    async fn start(&self, run: RunId) -> Result<(), RunError>;
    async fn cancel(&self, run: RunId) -> Result<(), RunError>;
    async fn approve(&self, run: RunId, gate: GateId, decision: Approval) -> Result<(), RunError>;
    async fn request_replan(&self, run: RunId, reason: ReplanReason) -> Result<(), RunError>;

    /// Terminalize a gate whose `timeout_ms` elapsed (RFC-0010 §5.7.8).
    async fn expire_gate(&self, run: RunId, gate: GateId) -> Result<(), RunError>;
}
```

**Normative behaviour (mirrors `approve(Deny)` with an `expired` decision):**

| Step | Rule |
| --- | --- |
| 1 | Phase gate `Running` (same as `approve`); acquire the per-run mutex. |
| 2 | Load the row. Missing ⇒ `NotFound(run)`. |
| 3 | State table: `WaitingApproval` ⇒ continue; `Cancelled` \| `Succeeded` \| `Failed` ⇒ `InvalidPhase("terminal")`; `Cancelling` ⇒ `InvalidPhase("cancelling")`; `ReplanRequested` ⇒ `InvalidPhase("replan pending")`; `Created` \| `Accepted` \| `Running` ⇒ `InvalidPhase("not waiting approval")`. |
| 4 | Take the waiter for `(run, gate)` if present and **drop** it (the receiver observes closure). A missing waiter is **not** an error (A7). |
| 5 | Sample acceptance (`was_accepted`), then upsert `failed` (row first, same crash-window shape as Deny). |
| 6 | Clear remaining waiters for the run. |
| 7 | Append `ApprovalResolved` `{ "gate_id": …, "decision": "expired", "reason": "approval_timeout" }`. |
| 8 | Append `RunCompleted` `{ "dag_state": "failed", "reason": "approval_timeout" }`. |
| 9 | Emit `RunFinished` with a synthetic failed outcome when accepted and not already finished. |
| 10 | Bump `approvals_resolved`; return `Ok(())`. |

Resume repair: a `failed` row missing its terminal events is repaired by the existing RFC-0003 §5.3 step 10 path, which is decision-agnostic (it writes `deny` provenance only when no `ApprovalResolved` exists at all).

### 3.16 `NullScheduler` retention

`NullScheduler` MUST stay public and MUST keep returning `SchedError::Unavailable` from `run` and `Ok(())` from `cancel`. It is (a) the pre-wiring default, (b) the shutdown parking scheduler (§4.6), and (c) the control-plane test double.

### 3.17 Crate-root re-exports

`alloy-runtime` MUST re-export: `LinearScheduler`, `LinearSchedulerDeps`, `SchedConfig`, `SchedulerMetrics`, `ready_nodes`, `promotable_nodes`, `backoff_delay`, `derive_dag_state`, `DeriveFlags`, `ToolCaller`, `ToolCallerError`, `VerifyPermissions`, `VerifyClass`, `CapabilityExecutor`, `CapabilityExecContext`, `CapabilityOutcome`, `CapabilityExecError`, `UnavailableCapabilityExecutor`, `CostMeterFactory`, `ProcessCostMeterFactory`, `McpVerifyCompileAdapter`, `McpVerifyTestAdapter`, `SessionGateHumanAdapter`, `parse_rustc_diagnostics`, `diagnostic_fingerprint`.

`alloy-tools` MUST re-export `ToolHandleToolCaller` and `map_mcp_error` from `mcp`.

---

## 4. Internal Module Design

### 4.1 Module layout

```text
crates/alloy-runtime/src/
  scheduler/
    mod.rs            # re-exports; NullScheduler retained
    traits.rs         # merged Scheduler/DagOutcome/DagState + reconcile_terminal_run default
    linear/
      mod.rs          # LinearScheduler, LinearSchedulerDeps, SchedConfig, new/new_for_test
      own.rs          # OwnedDag, OwnedGuard, ownership map, scheduler.lock
      loop_.rs        # the serial scheduling loop (§5.4 / §5.6)
      ready.rs        # ready_nodes / promotable_nodes / derive_dag_state (pure)
      checkpoint.rs   # C1..C10 catalog (Appendix A), write order, Conflict mapping
      envelopes.rs    # input assembly / input_ref rewrite / output envelope
      retry.rs        # admission, backoff_delay, escalation
      gate.rs         # gate orchestration (§5.7)
      budget.rs       # effective ceilings, meter rebuild, enforcement points
      failure.rs      # FailureIr construction + failure_ir artifacts
      outcome.rs      # DagOutcome assembly, failed_node selection
      metrics.rs      # SchedulerMetrics
  adapters/
    mod.rs            # merged traits (unchanged)
    tool_caller.rs    # ToolCaller + ToolCallerError (§3.4)
    verify.rs         # McpVerifyCompileAdapter / McpVerifyTestAdapter (§3.6)
    diagnostics.rs    # parse_rustc_diagnostics / diagnostic_fingerprint
    perms.rs          # VerifyPermissions / VerifyClass
    gate.rs           # SessionGateHumanAdapter
    capability.rs     # CapabilityExecutor / CapabilityOutcome / Unavailable*
```

```text
crates/alloy-tools/src/mcp/
  tool_caller.rs      # ToolHandleToolCaller + map_mcp_error (only place naming McpError)
```

| Rule | Statement |
| --- | --- |
| M1 | `scheduler::linear` MUST NOT be `pub`; only the types in §3.17 escape. |
| M2 | `ready.rs` MUST be pure (no `async`, no store handles) so §11.1 can table-test it. |
| M3 | `checkpoint.rs` MUST be the **only** module calling `DagStore::put_if_generation`. |
| M4 | `gate.rs` (scheduler side) owns deadlines; `adapters/gate.rs` owns only the waiter. |
| M5 | No module in `alloy-runtime` may name `ToolHandle`, `McpError`, `McpPlatform`, or `SandboxError`. |

### 4.2 Private scheduler state

```rust
struct LinearScheduler {
    deps: LinearSchedulerDeps,
    /// Process-wide DAG ownership (§4.5).
    owned: Mutex<HashMap<DagId, Arc<OwnedDag>>>,
    /// Cancels observed for DAGs this process does not (yet) own (§5.12.1).
    pending_cancels: Mutex<HashSet<DagId>>,
    /// Held for the process lifetime; released on Drop.
    _lock: OwnershipLock,
    metrics: Arc<SchedulerMetrics>,
}
```

| Rule | Statement |
| --- | --- |
| S1 | `owned` and `pending_cancels` MUST be `std::sync::Mutex` guarding only map operations. No `.await` may occur while either guard is alive. |
| S2 | A poisoned guard MUST surface as `SchedError::Ownership("ownership map poisoned")`, never a panic in `run`/`cancel`. |
| S3 | `LinearScheduler` MUST NOT cache `TaskDag` blobs across loop iterations except the single in-memory copy the loop CASes; the store row is the source of truth for generation. |

### 4.3 `OwnedDag`

```rust
struct OwnedDag {
    dag_id: DagId,
    run_id: RunId,
    session_id: SessionId,
    /// Child of `deps.runtime_cancel`; also fired by `cancel(dag_id)`.
    run_cancel: CancellationToken,
    /// Notified exactly once when the run loop has written its terminal
    /// checkpoint and dropped the in-flight node future (§5.12.2).
    completed: Arc<Notify>,
    /// Set before `completed.notify_waiters()`.
    terminal: Mutex<Option<DagState>>,
}
```

| Rule | Statement |
| --- | --- |
| O1 | `run_cancel` MUST be created as a child token of `deps.runtime_cancel` so process drain cancels every owned run. |
| O2 | `completed` MUST be notified on **every** exit path of `run`, including `Err`, panic-free early returns, and the forced-C6 path. The RAII guard (§4.4) guarantees this. |
| O3 | `terminal` MUST be written **before** `completed` is notified, so a waiting `cancel` can read the outcome without touching the store. |

### 4.4 `OwnedGuard` (RAII)

```rust
/// Released on every exit path, including panic unwind.
struct OwnedGuard<'a> {
    sched: &'a LinearScheduler,
    dag_id: DagId,
    owned: Arc<OwnedDag>,
}

impl Drop for OwnedGuard<'_> {
    fn drop(&mut self) {
        // 1. remove `dag_id` from `sched.owned`
        // 2. `owned.completed.notify_waiters()`
        // 3. `sched.deps.cost_meters` is NOT released here (§5.16.2 rule B7)
    }
}
```

| Rule | Statement |
| --- | --- |
| G1 | Ownership MUST be released only through `OwnedGuard::drop`. No code path may remove the map entry directly. |
| G2 | `Drop` MUST notify waiters even when `terminal` is `None` (a panicking or aborted run), so `cancel` cannot hang forever. |
| G3 | `Drop` MUST NOT block, `.await`, or touch the store. |

### 4.5 Ownership model (normative)

Two layers, both required.

| Layer | Mechanism | Scope | Failure |
| --- | --- | --- | --- |
| **Process** | `std::fs::File::try_lock_exclusive` on `<data_dir>/scheduler.lock` | One `LinearScheduler` per `data_dir` per host | `SchedError::Ownership("scheduler.lock held by another process")` |
| **DAG** | `Mutex<HashMap<DagId, Arc<OwnedDag>>>` insert-if-absent | One in-process run per `DagId` | `SchedError::AlreadyOwned(dag_id)` |

```rust
/// Kept alive for the scheduler's lifetime; the advisory lock is released when
/// the file handle drops (process exit or scheduler drop).
struct OwnershipLock {
    _file: std::fs::File,
    path: PathBuf,
}
```

| # | Rule |
| --- | --- |
| L1 | `new` MUST `create_dir_all(data_dir)`, then `OpenOptions::new().read(true).write(true).create(true).open(data_dir.join("scheduler.lock"))`. |
| L2 | `new` MUST call `std::fs::File::try_lock_exclusive` (stable since Rust 1.89; the toolchain is pinned at 1.97.1). The `fs4` / `fs2` crates MUST NOT be added. |
| L3 | `TryLockError::WouldBlock` MUST map to `Ownership("scheduler.lock held by another process: {path}")`. Any other error MUST map to `Ownership("scheduler.lock: {e}")`. |
| L4 | The lock file MUST NOT be deleted on drop (deleting it races a second process that already opened the same inode). It MAY be truncated and MAY have the owning pid written for debugging; correctness MUST NOT depend on its contents. |
| L5 | `run` MUST acquire DAG ownership **before** any CAS and MUST hold it until the terminal checkpoint is committed. |
| L6 | A DAG whose durable `DagState::Running` is not owned by this process is a **crash residue**: `run` MUST adopt it (§5.3.2), and a `LinearScheduler` MUST NOT leave a foreign `Running` DAG untouched while accepting new work for it (RFC-0009 Appendix C). |
| L7 | Advisory locks are advisory: L1–L3 are a same-host safety net, not a distributed lease. Multi-host execution stays out of scope (§12). |

### 4.6 Shutdown / scheduler swap ordering

`RuntimeHandle::set_scheduler` rejects `RuntimePhase::Draining`. Therefore the swap MUST happen **before** drain begins.

| Step | Action | Phase required |
| --- | --- | --- |
| 1 | Stop accepting new `start` calls at the caller (CLI/server closes its intake) | `Running` |
| 2 | Wait for in-flight runs to finish, or accept that step 4 cancels them | `Running` |
| 3 | `handle.set_scheduler(Arc::new(NullScheduler))` — new `run_dag` calls now fail fast with `SchedulerUnavailable` | `Running`, scheduler idle |
| 4 | `runtime.drain(grace)` — computes the deadline **first** (amendment A1), cancels each live DAG, then waits for the remaining budget | `Running → Draining` |
| 5 | `runtime.shutdown()` | `Draining → Stopped` |
| 6 | Drop the `LinearScheduler`; `scheduler.lock` is released | — |

| Rule | Statement |
| --- | --- |
| SD1 | Hosts MUST perform step 3 while the phase is still `Running`. Calling `set_scheduler` during drain MUST fail and MUST NOT be retried in a loop. |
| SD2 | Step 3 MUST NOT be used as a cancellation mechanism: swapping the scheduler does not cancel an in-flight `run` future, because `run_dag` already holds an `Arc<dyn Scheduler>` clone. |
| SD3 | `LinearScheduler::run` MUST tolerate being called after step 3 (a `run_dag` that captured the old `Arc`) and MUST honour `runtime_cancel` in that window. |

### 4.7 Concurrency rules

| # | Rule |
| --- | --- |
| K1 | At most one node per process may be in `NodeState::Running`. The loop dispatches, awaits, then checkpoints; it never holds two node futures. |
| K2 | `run` for two different `DagId`s MAY overlap in wall-clock time (the runtime's single-flight gate is per **run**, not per process). Each such loop MUST still dispatch serially within itself, and `budget_policy.max_parallel_cargo = 1` plus MCP host admission (`host_parallel_honesty`) is what keeps cargo serial across them. |
| K2a | Consequence of K2: `SchedulerMetrics::nodes_running` MAY exceed 1 across DAGs. The serial invariant is **per DAG**, plus host-level cargo admission. Any test asserting a global 1 MUST scope itself to one DAG. |
| K3 | No `.await` while holding `owned` / `pending_cancels` guards (S1). |
| K4 | The scheduler MUST NOT hold a `SharedCostMeter` lock across `.await` (`with_mut` closures stay allocation-light and synchronous). |
| K5 | `cancel` MUST NOT be dispatched through the runtime single-flight gate; it is reachable while a run is in flight by construction. |

---

## 5. Execution Algorithm

### 5.1 `Scheduler::run` entry sequence (normative, ordered)

| # | Step | Failure / early return |
| --- | --- | --- |
| R1 | `dags.get(dag_id)` | `None` ⇒ `Err(DagNotFound(dag_id))`; `StoreError` ⇒ `Err(Store(..))` |
| R2 | If `config.validate_on_load`: `DagValidator::validate(&dag, &config.validate_opts)` | `Err(e)` ⇒ `Err(Invariant("dag {id} failed load validation: {e}"))` |
| R3 | Resolve the run binding (Appendix F) | `None` ⇒ `Err(RunBindingMissing(dag_id))` |
| R4 | Insert `OwnedDag` into `owned` (insert-if-absent) and build `OwnedGuard` | occupied ⇒ `Err(AlreadyOwned(dag_id))`; poisoned ⇒ `Err(Ownership(..))` |
| R5 | Take `dag_id` out of `pending_cancels`; if it was present, fire `run_cancel` immediately | — |
| R6 | `sessions.get_session(dag.session_id)` for `workspace_root`, `profile`, `budget` | `None` ⇒ `Err(Invariant("session row missing for dag {id}"))` |
| R7 | Compute effective budget ceilings (§5.16.1) | non-fatal; degenerate ceilings recorded as a `Budget` decision |
| R8 | Rebuild the run cost meter from events (§5.16.2) | `ObsError` ⇒ `Err(Store(..))` |
| R9 | If `dag.state ∈ {Succeeded, Failed, Cancelled}` ⇒ return `Ok` outcome derived from the durable blob (§5.18) with **no** CAS | — |
| R10 | If `dag.state == ReplanRequired` ⇒ return `Ok(DagOutcome { state: ReplanRequired, .. })` with no CAS | — |
| R11 | Reconstruct per-node attempt counters (§5.3.1) | `Err(Store(..))` on event read failure |
| R12 | Adopt any node durably `Running` (§5.3.2) | — |
| R13 | Gate resume decision (§5.7.2 / §5.7.3) when `dag.state == WaitingApproval` | — |
| R14 | C1: if `dag.state == Pending` (or a gate resolution advanced the DAG per §5.7.6) CAS `DagState::Running` | `Conflict` ⇒ §5.8.4 |
| R15 | Start the run clock; `gate_wait_total = 0` (§5.19) | — |
| R16 | Enter the loop (§5.4) | — |
| R17 | On loop exit: derive the terminal `DagState` (§5.17), commit C7, assemble `DagOutcome` (§5.18), drop `OwnedGuard` | — |

`run` MUST NOT return `Ok` with `state ∈ {Pending, Running, WaitingApproval}` (fix 24). Producing one is an `Invariant` bug and MUST be caught by AC 43.

### 5.2 Loop step order (normative, one iteration)

| # | Step | Detail |
| --- | --- | --- |
| L1 | Cancel check | `run_cancel.is_cancelled()` or `deps.runtime_cancel.is_cancelled()` ⇒ cancel path (§5.12.2) |
| L2 | Late cancel check | `dag_id ∈ pending_cancels` ⇒ remove, fire `run_cancel`, cancel path |
| L3 | Run deadline check | remaining budget `<= 0` ⇒ run-timeout path (§5.19) |
| L4 | Replan check | run row state `replan_requested` ⇒ C10 + return `Ok(ReplanRequired)` (§5.21) |
| L5 | Budget check | exhausted ⇒ budget-failure path (§5.16.3) |
| L6 | Promote | one CAS (C2) marking every `promotable_nodes` entry `Ready` |
| L7 | Serial assertion | `ready_nodes(&dag).len() > 1` ⇒ `Err(Invariant("multiple ready nodes: {ids:?}"))` |
| L8 | Quiescence | `ready.is_empty()` ⇒ leave the loop and derive terminal state (§5.17) |
| L9 | Select | the single `Ready` node |
| L10 | Assemble input | build the envelope, put it, C5 rewrite `input_ref` if changed (§5.5) |
| L11 | Escalate | apply tier escalation + `Retry` decision when admitted (§5.11.4) — before C3 |
| L12 | Dispatch | C3 (`Ready → Running`, attempt++), then dispatch under the node deadline (§5.6 / §5.19) |
| L13 | Apply | success ⇒ C4; soft failure ⇒ retry admission (§5.11) ⇒ C8 or durable C7 failure; gate ⇒ §5.7 |
| L14 | Reload | re-read the DAG from the store only if a CAS returned a fresher blob; otherwise reuse the in-memory copy the CAS produced |

### 5.3 Resume reconstruction

#### 5.3.1 Attempt counters

`TaskNode` has no attempt field (RFC-0009 §6.4), so counters are process-local and rebuilt from events on resume.

| Source | Rule |
| --- | --- |
| Events | Count `NodeState` events for `(session, run, node)` with `payload.to == "running"` and `payload.generation == dag.generation`. Call this `n_running`. |
| Durable node state | `Running` ⇒ `attempts_started = max(n_running, 1)`; `Ready`/`Failed`/`Succeeded`/`Cancelled`/`Skipped`/`WaitingApproval` ⇒ `attempts_started = n_running` |
| Missing events | If the node is durably `Running` with `n_running == 0` the CAS committed and the event was lost (§5.8.1 crash window). The counter MUST be `1`, not `0`. |
| Other generation | Events from an earlier `generation` MUST be ignored: a replan resets the retry budget. |

`attempts_started` is the **k** used by §5.11: attempt `k` is the attempt about to start after `attempts_started = k - 1`.

#### 5.3.2 Adopting a durably `Running` node

| Situation | Action |
| --- | --- |
| Node `Running`, `attempts_started < max_attempts`, failure would be admissible | Treat the lost attempt as a soft failure with `FailureIr { error_class: Internal, retry: Retryable, notes: "adopted after restart" }`, apply §5.11 from step (a) |
| Node `Running`, retries exhausted | Terminalize: durable `Failed` (C7) with `notes: "adopted after restart; retries exhausted"` |
| Node `Running` and `NodeKind::GateHuman` | Illegal — gates never reach `Running` before approval; `Err(Invariant("gate node running"))` |
| More than one node `Running` | `Err(Invariant("multiple running nodes after restart"))` |

Adoption MUST NOT re-dispatch the lost attempt directly: the attempt is accounted for, so an infinite crash loop cannot exceed `max_attempts`.

#### 5.3.3 CAS-before-events recovery filter (normative)

Because the write order is artifacts → CAS → events (§5.8.1), a crash can leave a committed CAS with no event. Recovery MUST therefore trust the blob and repair the event log, never the reverse.

| Rule | Statement |
| --- | --- |
| RF1 | The authoritative node state is `dag.nodes[id].state` from the blob. |
| RF2 | Event-derived state MUST be filtered to the **newest** `NodeState` event for the node whose `payload.to` equals the persisted node state and whose `payload.generation` equals `dag.generation`. Newer events with a different `to` MUST be ignored (they belong to a transition that never committed). |
| RF3 | If no such event exists, the transition's event is missing. The scheduler MUST append a repair `NodeState` event with `payload.repaired = true` before continuing, so the log matches the blob. |
| RF4 | The scheduler MUST NOT roll a blob backwards to match an event. |
| RF5 | Repair appends MUST be idempotent: at most one repair event per `(node, generation, to)`; existence is probed with the RF2 filter. |

### 5.4 Ready-set derivation and selection

`promotable_nodes` implements RFC-0009 §5.3.1 verbatim:

| Edge kind into `n` | Predecessor `p` satisfied when |
| --- | --- |
| `Sequence` | `p.state ∈ {Succeeded, Skipped, CachedHit}` |
| `Data` | `p.state ∈ {Succeeded, CachedHit}` **and** `p.output_ref.is_some()` |
| `Hint` | never consulted |

| Rule | Statement |
| --- | --- |
| RS1 | `n` is promotable iff `n.state == Pending` and every Data and Sequence predecessor is satisfied. |
| RS2 | A `Skipped` predecessor MUST NOT satisfy a Data edge. The successor stays `Pending` and eventually becomes `Skipped` (§5.15). |
| RS3 | A predecessor in `Succeeded` **without** `output_ref` on a Data edge MUST fail closed: `Err(Invariant("succeeded node {id} has no output_ref"))` (RFC-0009 §5.3.2 invariant owned by this RFC). |
| RS4 | `ready_nodes` returns nodes already in `Ready`, ascending `NodeId`. Both helpers are deterministic and total. |
| RS5 | `ready_nodes(&dag).len() > 1` after promotion MUST fail closed (fix: no arbitrary pick, no concurrency). |
| RS6 | Promotion MUST be a single CAS covering all promotable nodes, so a crash cannot leave a half-promoted frontier. |

### 5.5 Input envelope assembly and `input_ref` rewrite

| Node shape | Rewrite rule |
| --- | --- |
| **Root** — no incoming `Data` **and** no incoming `Sequence` edges | MUST keep the plan-time `NodeInputPayload::Goal(..)` envelope. No rewrite, no C5. |
| **Sequence-only** — ≥1 `Sequence` edge, zero `Data` edges | MUST keep `FromPredecessors { preds: [] }`. No rewrite, no C5. |
| **Data** — ≥1 incoming `Data` edge | MUST rewrite: one `PredecessorOutput` per incoming Data edge, in ascending `NodeId`, each carrying the predecessor's real `output_ref` |

| Rule | Statement |
| --- | --- |
| E1 | The rewrite MUST only run for nodes with at least one incoming `Data` edge (fix 36). Rewriting a Sequence-only node into a populated `preds` list is a defect. |
| E2 | The new envelope MUST set `generation = dag.generation`, `schema_version = ENVELOPE_SCHEMA_VERSION`, `dag_id`, `node_id`, `kind`. |
| E3 | Before dispatching, the scheduler MUST detect a **pending placeholder**: read `ArtifactMeta.labels["alloy.envelope"]` for each referenced `output_ref` and reject `"pending_pred"` (fix 37). Content sniffing for `{"pending": true}` MUST NOT be the primary test; it MAY be a secondary defence. |
| E4 | A still-pending predecessor slot at dispatch time MUST fail closed with `Err(Invariant("pending predecessor slot for node {id}"))`. |
| E5 | The assembled envelope MUST be `put` **before** the C5 CAS (write order §5.8.1). If the CAS conflicts, the orphan artifact is acceptable (no GC in RFC-0002). |
| E6 | If the assembled envelope is byte-identical to the current `input_ref` body, the scheduler SHOULD skip both the put and C5. |
| E7 | Adapter nodes (`VerifyCompile`, `VerifyTest`, `GateHuman`) get the same treatment; their adapters ignore the payload but the envelope keeps provenance intact. |

### 5.6 Dispatch table

| `NodeKind` | Target | Context | Notes |
| --- | --- | --- | --- |
| `Plan`, `Analyze`, `Edit`, `Review` | `deps.capabilities.execute` | `CapabilityExecContext` (§3.8) | `capability` MUST be `Some`; `None` ⇒ `Invariant` |
| `VerifyCompile` | `deps.verify_compile.check` | `NodeExecContext` | `model_tier` / `budget` ignored for routing |
| `VerifyTest` | `deps.verify_test.test` | `NodeExecContext` | same |
| `GateHuman` | §5.7 (scheduler-owned) via `deps.gate_human.wait_approval` | `NodeExecContext` + `GateId` | `approval` MUST be `Some`; `None` ⇒ `Invariant` |
| `Aggregate` | internal fold (§5.14) | — | No adapter, no worker, no model call |

| Rule | Statement |
| --- | --- |
| DP1 | Dispatch MUST happen after C3 commits, so a crash mid-node is recoverable as §5.3.2. |
| DP2 | Every dispatch MUST be wrapped in `tokio::time::timeout(node_deadline, fut)` except the gate wait, which uses its own deadline (§5.19). |
| DP3 | `CapabilityExecContext.cost_meter` MUST be the run meter from `deps.cost_meters.meter_for(run_id)` (fix 9). Workers MUST NOT create their own. |
| DP4 | The scheduler MUST overwrite `failure.node` with the dispatched `NodeId` on every returned `FailureIr` (CE2). |
| DP5 | Adapter `Result` mapping to `FailureIr` is §5.10; adapter errors are never silently swallowed. |

### 5.7 Gate execution (`NodeKind::GateHuman`)

#### 5.7.1 Overview

The gate is the only node kind whose completion depends on an external actor. The scheduler MUST block inside `run`; it MUST NOT return `WaitingApproval` (fix 24). The control plane owns durability of the decision (`RunController::approve` / `expire_gate`); the scheduler owns the DAG blob, the node state, and the deadline.

| Actor | Owns |
| --- | --- |
| Scheduler | `NodeState::WaitingApproval`, `DagState::WaitingApproval`, `ApprovalRequested`, the `timeout_ms` deadline, terminalization of the DAG |
| `RunController::approve` | `RunControlState`, `ApprovalResolved`, waiter delivery |
| `RunController::expire_gate` | The same, with `decision = "expired"` |
| `SessionGateHumanAdapter` | Registering the waiter and awaiting it (`select` over receiver vs cancel only) |

#### 5.7.2 Durable resolution scan (fix 13)

Before touching state, the gate path MUST scan session events for the newest `ApprovalResolved` for `(run_id, gate_id)` in this generation.

| `payload.decision` | Meaning | Continue at |
| --- | --- | --- |
| `"allow"` | Approved, ongoing | §5.7.6 allow path |
| `"allow_once"` | Approved for this occurrence | §5.7.6 allow path |
| `"deny"` | Denied | §5.7.4 deny path |
| `"expired"` | Timed out durably | §5.7.5 expiry path — MUST NOT call `expire_gate` again (fix 18) |
| unknown string | Forward-compat guard | `Err(Invariant("unknown approval decision: {s}"))` |
| absent | Not yet resolved | §5.7.3 (resume) or §5.7.7 (first schedule) |

Scanning MUST match on `payload.gate_id == gate_id`; an `ApprovalResolved` for a different gate MUST be ignored.

#### 5.7.3 Resume with `WaitingApproval` and no resolution (fix 14)

| Rule | Statement |
| --- | --- |
| GR1 | The scheduler MUST re-register the waiter (`SessionPlane::register_gate_waiter`) and await it. |
| GR2 | The scheduler MUST NOT re-checkpoint `NodeState::WaitingApproval` / `DagState::WaitingApproval` — they are already durable. |
| GR3 | The scheduler MUST NOT re-emit `ApprovalRequested`. Re-emission would duplicate the operator-visible prompt on every resume. Existence is probed with `has_session_event_for_run(session, run, ApprovalRequested)` plus the `gate_id` filter. |
| GR4 | The remaining gate deadline MUST be recomputed as `timeout_ms - elapsed_since(first ApprovalRequested.ts)`, clamped at `>= 0`. A non-positive remainder MUST go straight to §5.7.8 expiry. |
| GR5 | `register_gate_waiter` returning `Err(RunError::InvalidPhase(..))` for a terminal row MUST be classified through §5.7.9 (crash window), not treated as an internal error. |

#### 5.7.4 Deny path (fix 17)

Ordered, single CAS for the DAG markings:

| # | Action |
| --- | --- |
| 1 | Put a `failure_ir` artifact for the gate node: `FailureIr { node: gate, error_class: Approval, retry: NonRetryable, diagnostics: [], notes: "approval denied" }` |
| 2 | CAS: gate node `WaitingApproval → Cancelled`; every non-terminal other node (`Pending` / `Ready`) → `Skipped`; `DagState → Failed` |
| 3 | Append `NodeState` `waiting_approval → cancelled` with `failure_ref` and `decision: "deny"`; append one `NodeState` `→ skipped` per skipped node |
| 4 | Record `DecisionKind::Gate` with `{ "gate_id": …, "decision": "deny" }` |
| 5 | Return `Ok(DagOutcome { state: Failed, failed_node: Some(gate), failure: Some(approval_failure) })` |

| Rule | Statement |
| --- | --- |
| GD1 | Deny MUST produce `DagState::Failed`, never `Cancelled`. The **node** is `Cancelled`; the **DAG** is `Failed` via rule D5 (§5.17). |
| GD2 | The failure MUST be attributed to the gate node, so `failed_node` is the gate even though the gate node itself is `Cancelled` (§5.18). |
| GD3 | Retries MUST NOT be admitted for `ErrorClass::Approval` even if `retry_on` lists it: the disposition is `NonRetryable`. |

#### 5.7.5 Expiry observed durably (fix 18)

When the scan finds `decision == "expired"`, the control plane already terminalized the run. The scheduler MUST terminalize the DAG with the same shape as §5.7.4 but with `notes: "approval timeout"`, `decision: "expired"`, and MUST NOT call `expire_gate` again.

#### 5.7.6 Allow path (fix 16)

Target sequence `WaitingApproval → Ready → Running → Succeeded`. Every step is skipped when the durable state already shows it (crash-tolerant):

| Durable node state on entry | Steps still required |
| --- | --- |
| `WaitingApproval` | CAS → `Ready` (DAG → `Running`); CAS → `Running`; execute fold; CAS → `Succeeded` |
| `Ready` | CAS → `Running`; execute fold; CAS → `Succeeded` |
| `Running` | execute fold; CAS → `Succeeded` |
| `Succeeded` | none — continue the loop |
| `Cancelled` / `Failed` / `Skipped` | `Err(Invariant("gate resolved allow but node is {state:?}"))` |

| Rule | Statement |
| --- | --- |
| GA1 | The gate "execution" is a deterministic fold: it MUST put a `node_output` envelope whose payload records `{ "approved": true, "decision": …, "gate_id": … }` and MUST set `output_ref` on success (§5.9). |
| GA2 | `allow` and `allow_once` are treated identically by the scheduler. Scope semantics (remembering `allow` for later gates) belong to the control plane / RFC-0015. |
| GA3 | The DAG MUST leave `WaitingApproval` in the **same** CAS that moves the node to `Ready`. |

#### 5.7.7 First schedule (fix 15)

| # | Action |
| --- | --- |
| 1 | Single CAS: gate node `Ready → WaitingApproval`, `DagState::Running → WaitingApproval` (C9) |
| 2 | Append `NodeState` `ready → waiting_approval` |
| 3 | Append `ApprovalRequested` `{ "gate_id": …, "node_id": …, "reason": approval.reason, "timeout_ms": node.timeout_ms }` |
| 4 | `register_gate_waiter(run, gate)` |
| 5 | Await under the gate deadline (§5.7.10) |

The CAS MUST precede the events (§5.8.1), so a crash between them is repaired by RF3.

#### 5.7.8 Timeout and `expire_gate` (fix 19)

On deadline expiry the scheduler MUST call `deps.runs.expire_gate(run_id, gate_id)` exactly once per `(run_id, gate_id)` (fix 20) and then act on the result:

| `expire_gate` result | Scheduler action | `run` returns |
| --- | --- | --- |
| `Ok(())` | Terminalize the DAG per §5.7.5 (`decision: "expired"`) | `Ok(Failed)` |
| `Err(RunError::InvalidPhase(_))` | Re-scan (§5.7.2). A resolution now present ⇒ follow that path. Still absent ⇒ terminalize per §5.7.5 anyway (the control row moved on without us) | `Ok(Failed \| Cancelled)` per the resolution |
| `Err(RunError::NotFound(_))` | No run row for a DAG we are executing — contract break | `Err(Internal("run row vanished during gate expiry: {run}"))` |
| `Err(RunError::UnknownGate(_))` | MUST NOT happen (A7). Treat as `InvalidPhase` | as `InvalidPhase` row |
| `Err(other)` | Best-effort terminalize the DAG per §5.7.5, then surface the error | `Err(Internal("expire_gate failed: {other}"))` |

| Rule | Statement |
| --- | --- |
| GT1 | The gate deadline MUST come from `node.timeout_ms` (RFC-0009 obligation), not from `run_timeout`. |
| GT2 | Gate wait time MUST NOT consume the run budget (§5.19, fix 44). |
| GT3 | The expiry attempt MUST be recorded once in an in-run `HashSet<(RunId, GateId)>`; a second expiry for the same key MUST skip the call and go straight to terminalization. |
| GT4 | Expiry MUST produce `ErrorClass::Approval` with `retry: NonRetryable`, never `ErrorClass::Timeout`. Node-level `Timeout` is for execution deadlines. |

#### 5.7.9 Closed receiver classification (fix 22)

`oneshot::Receiver` returning `Err(RecvError)` means the sender was dropped. That is ambiguous, so the scheduler MUST classify by reading the durable `RunControlState`:

| `RunControlState` after closure | Classification |
| --- | --- |
| `WaitingApproval` | Waiter lost without a decision — re-register once (bounded by `GATE_REREGISTER_MAX = 3`), then treat as `Internal` |
| `Cancelling` / `Cancelled` | Cancel path (§5.12) |
| `Failed` | Re-scan §5.7.2; expect `deny` or `expired`; absent ⇒ terminalize as expiry with `notes: "gate waiter closed; run failed"` |
| `Succeeded` | `Err(Invariant("run succeeded while gate pending"))` |
| `ReplanRequested` | Replan path (§5.21) |
| `Created` / `Accepted` / `Running` | Re-register once, then `Internal("gate waiter closed in state {s:?}")` |

#### 5.7.10 Adapter contract (fix 23)

```rust
pub struct SessionGateHumanAdapter {
    plane: SessionPlane,
}

impl SessionGateHumanAdapter {
    #[must_use]
    pub fn new(plane: SessionPlane) -> Self;
}

#[async_trait]
impl GateHumanAdapter for SessionGateHumanAdapter {
    /// `select!` over exactly two branches: the waiter receiver and
    /// `ctx.cancellation.cancelled()`. No timer.
    async fn wait_approval(
        &self,
        ctx: &NodeExecContext,
        gate: GateId,
    ) -> Result<Approval, AdapterError>;
}
```

| Rule | Statement |
| --- | --- |
| GC1 | The adapter MUST NOT own a timer. The scheduler wraps `wait_approval` in `tokio::time::timeout(remaining_gate_budget, ..)`. |
| GC2 | The adapter MUST `select!` on exactly two branches: receiver vs `ctx.cancellation`. |
| GC3 | Cancellation MUST return `AdapterError::Cancelled`; a closed receiver MUST return `AdapterError::Internal("gate waiter closed")` so §5.7.9 can classify it. |
| GC4 | The adapter MUST NOT append events, write the DAG, or call `approve` / `expire_gate`. |
| GC5 | `register_gate_waiter` MUST be called by the adapter (it needs the receiver), and its `RunError` MUST map: `InvalidPhase(m) → AdapterError::Internal("gate registration: {m}")`, `NotFound → Internal`, everything else → `Internal`. The scheduler re-reads `RunControlState` for classification rather than trusting the string. |

#### 5.7.11 Crash window: terminal control row, `WaitingApproval` DAG (fix 21)

| Step | Actor |
| --- | --- |
| 1 | `approve(Deny)` / `expire_gate` writes the run row `failed` and its events |
| 2 | Process crashes before the scheduler terminalizes the DAG blob |
| 3 | Restart: `SessionService::resume` sees a terminal row. It MUST NOT `start` (terminal rows are not dispatchable) |
| 4 | `resume` MUST call `RuntimeHandle::reconcile_terminal_run(dag_id, DagState::Failed)` (amendment A6), best effort, warning on error, never aborting resume |
| 5 | `LinearScheduler::reconcile_terminal_run` terminalizes the DAG per §5.20 |

Without step 4 the DAG blob stays `WaitingApproval` forever and `PlanService::replan` keeps working only because `WaitingApproval != Running` — but the run is unobservably stranded. This is why A6 is mandatory rather than advisory.

### 5.8 Checkpointing

#### 5.8.1 Write order (fix 25, normative)

```text
1. put artifacts (input envelope / output envelope / failure_ir / raw verify log)
2. put_if_generation(&dag, Some(dag.generation))          <-- the commit point
3. append events (NodeState, ApprovalRequested, Decision, …)
```

| Rule | Statement |
| --- | --- |
| W1 | Artifacts MUST be durable before the CAS references them. An artifact id in a committed blob MUST always resolve. |
| W2 | The CAS is the commit point. Events are **derived** and MAY lag. |
| W3 | Events MUST NOT be appended before their CAS. An event describing an uncommitted transition would make the log a liar and break RF2. |
| W4 | Event append failure after a successful CAS MUST NOT roll back the CAS. It MUST be logged at `warn` and MUST be repaired by RF3 on the next pass. |
| W5 | Orphaned artifacts from a failed CAS are acceptable (no GC in RFC-0002) and MUST NOT be deleted (deletion races a concurrent reader). |
| W6 | Every CAS MUST pass `expected = Some(current.generation)` and MUST leave `dag.generation` unchanged. |

#### 5.8.2 Checkpoint catalog (summary; full table in Appendix A)

| Id | Transition | DAG state after |
| --- | --- | --- |
| C1 | run adopt / start | `Running` |
| C2 | `Pending → Ready` (all promotable, one CAS) | `Running` |
| C3 | `Ready → Running` (attempt `k`) | `Running` |
| C4 | `Running → Succeeded` + `output_ref` | `Running` |
| C5 | `input_ref` rewrite (node stays `Ready`) | `Running` |
| C6 | cancel markings (`Cancelled` / `Skipped`) | `Cancelled` |
| C7 | terminal (`Succeeded` / `Failed`, durable node `Failed`) | `Succeeded` \| `Failed` |
| C8 | retry (`Running → Ready`) | `Running` |
| C9 | gate (`Ready → WaitingApproval`, resolutions) | `WaitingApproval` \| `Running` \| `Failed` |
| C10 | replan observed | `ReplanRequired` |

#### 5.8.3 C8 — retry checkpoint (fix 26, pinned sequence)

A durable `NodeState::Failed` means **retries exhausted or non-retryable**. A retryable soft failure MUST be checkpointed as `Ready` with `DagState::Running`. Exact order:

| Step | Action | Notes |
| --- | --- | --- |
| (a) | `put` the `failure_ir` artifact for failed attempt `k` | label `alloy.envelope = failure_ir`; artifacts-first (W1) |
| (b) | Append `NodeState` `running → failed` with `{ attempt: k, failure_ref, error_class, retry }` | **logical** transition: it describes the attempt, and it is appended before the CAS only in the sense that it is the *first* event of the pair — see R-note |
| (c) | **Single CAS**: node `Running → Ready`, `DagState` stays `Running` | the commit point; the node never becomes durably `Failed` |
| (d) | Append `NodeState` `failed → ready` with `{ attempt: k, next_attempt: k + 1, backoff_ms }` | records the admitted retry |
| (e) | Sleep `backoff_delay(&node.retry.backoff, k, config.max_backoff)` | interruptible by cancel (§5.11.3) |
| (f) | C3 `Ready → Running` for attempt `k + 1` | `attempts_started = k + 1` |

**R-note (ordering reconciliation).** W3 forbids appending an event for an uncommitted transition. Steps (b) and (d) describe **one** committed CAS (step c) that leaves the node `Ready`, so both events MUST be appended **after** step (c), in the order (b) then (d). The `running → failed` event is retained because the repair loop and the operator timeline need the failed attempt to be visible; `failed` is a *logical* waypoint, never a durable node state here.

| Rule | Statement |
| --- | --- |
| RT1 | Exactly one CAS per retry, and it MUST leave the node `Ready` and the DAG `Running`. |
| RT2 | The `failure_ir` artifact MUST be durable before the CAS, so the event pair can reference `failure_ref` (fix 43). |
| RT3 | The backoff sleep MUST happen **after** the CAS and after the events, so a crash during backoff resumes from a coherent blob. |
| RT4 | On restart with the node `Ready` and `attempts_started = k` (a failed attempt recorded), the scheduler MUST continue the remaining backoff (§5.11.3 rule B4) and then perform C3 for attempt `k + 1`. |
| RT5 | The `Failed → Ready` transition MUST NOT be used for non-admitted failures. Non-admitted failures go straight to durable `Failed` via C7. |

#### 5.8.4 Conflict handling

| Step | Action |
| --- | --- |
| 1 | Stop all checkpointing for this DAG immediately (RFC-0009 §6.6) |
| 2 | Cancel the in-flight node future by firing `run_cancel` |
| 3 | Do **not** attempt a compensating write, a reload-and-retry, or a terminal CAS |
| 4 | Increment `SchedulerMetrics::cas_conflicts`; log at `warn` with `dag_id` and both generations |
| 5 | Return `Err(SchedError::Conflict { dag_id })` |

`Conflict` means another writer bumped the generation — in practice `PlanService::replace_for_replan`. The new generation's DAG is a different execution; the control plane re-dispatches it.

### 5.9 Output envelope and `output_ref`

| Rule | Statement |
| --- | --- |
| OU1 | On success the scheduler MUST `put` a `NodeOutputEnvelope { schema_version: 1, dag_id, node_id, kind, generation, attempt, payload }` and set `output_ref = Some(id)` in the same C4 CAS. |
| OU2 | `Succeeded` without `output_ref` MUST fail closed (`Invariant`) — RFC-0009 §5.3.2 invariant owned here. |
| OU3 | On failure the scheduler MUST NOT set `output_ref` (fix 41). Failure artifacts are referenced from events and `FailureIr`, never from `output_ref`. |
| OU4 | Verify nodes with `ok == true` MUST set `output_ref` to an envelope whose payload is `{ "ok": true, "diagnostics": [...], "raw_artifact": … }`. With `ok == false` no `output_ref` is written. |
| OU5 | `CachedHit` MUST NOT be produced in MVP (RFC-0009 §12). If a blob is ever loaded with a `CachedHit` node, the scheduler MUST require `output_ref.is_some()` and otherwise fail closed. |
| OU6 | The payload of a capability node is the worker's `CapabilityOutcome::Succeeded { payload }` value verbatim. The scheduler MUST NOT rewrite or interpret it. |

### 5.10 `FailureIr` construction

| Source | `error_class` | `retry` | `notes` |
| --- | --- | --- | --- |
| `CapabilityOutcome::Failed { failure }` | worker's value | worker's value | worker's value |
| `CapabilityExecError::Unavailable` | `Internal` | `NonRetryable` | `"capability executor unavailable"` |
| `CapabilityExecError::Worker(m)` | `Internal` | `Retryable` | `"worker error: {m}"` |
| `CapabilityExecError::Timeout` | `Timeout` | `Retryable` | `"worker reported timeout"` |
| `CapabilityExecError::Cancelled` | `Cancelled` | `NonRetryable` | `"cancelled"` |
| `CapabilityExecError::Internal(m)` | `Internal` | `NonRetryable` | `"internal: {m}"` |
| `VerifyOutcome { ok: false, .. }` on `VerifyCompile` | `Compile` | `NonRetryable` | `"cargo check failed"` |
| `VerifyOutcome { ok: false, .. }` on `VerifyTest` | `Test` | `NonRetryable` | `"cargo test failed"` |
| `AdapterError::ToolFailure(ToolError::Transient { .. })` | `Tool` | `Retryable` | `"tool transient: {code}"` |
| `AdapterError::ToolFailure(ToolError::Permanent { .. })` | `Tool` | `NonRetryable` | `"tool permanent: {code}"` |
| `AdapterError::ToolFailure(ToolError::InvalidArgs { .. })` | `Internal` | `NonRetryable` | `"adapter built invalid args"` |
| `AdapterError::ToolFailure(ToolError::ExecutionFailed { .. })` | per §5.13.2 | per §5.13.2 | per §5.13.2 |
| `AdapterError::PermissionDenied(m)` | `Tool` | `NonRetryable` | `"permission denied: {m}"` |
| `AdapterError::Timeout` | `Timeout` | `Retryable` | `"adapter timeout"` |
| `AdapterError::ShuttingDown` | `Internal` | `NonRetryable` | `"mcp host shutting down"` |
| `AdapterError::Artifact(m)` | `Internal` | `NonRetryable` | `"artifact store: {m}"` |
| `AdapterError::Unavailable` | `Internal` | `NonRetryable` | `"adapter unavailable"` |
| `AdapterError::Cancelled` | `Cancelled` | `NonRetryable` | `"cancelled"` |
| `AdapterError::Tool(m)` (legacy) | `Tool` | `NonRetryable` | `"tool: {m}"` |
| `AdapterError::Internal(m)` | `Internal` | `NonRetryable` | `"internal: {m}"` |
| Node deadline elapsed (scheduler-side) | `Timeout` | `Retryable` | `"node timeout after {ms}ms"` |
| Run deadline elapsed (scheduler-side) | `Timeout` | `NonRetryable` | `"run timeout after {ms}ms"` |
| Budget exhausted | `Budget` | `NonRetryable` | `"budget exhausted: {check:?}"` |
| Gate deny | `Approval` | `NonRetryable` | `"approval denied"` |
| Gate expiry | `Approval` | `NonRetryable` | `"approval timeout"` |

| Rule | Statement |
| --- | --- |
| F1 | `failure.node` MUST be the dispatched `NodeId` (DP4). |
| F2 | `failure.diagnostics` MUST carry the verify diagnostics when they exist, so the next repair node sees them without re-running cargo. |
| F3 | Every `FailureIr` MUST be persisted as an artifact (`alloy.envelope = failure_ir`) **before** its CAS, and the artifact id MUST appear as `failure_ref` in the corresponding `NodeState` event (fix 43). |
| F4 | `notes` MUST NOT contain absolute paths outside the workspace, env values, or provider keys. Adapter strings arrive pre-redacted from RFC-0006 §9.1. |
| F5 | `retry` is advisory input to admission (§5.11.1); `retry_on` still has to list the class. |

### 5.11 Retry, backoff, escalation

#### 5.11.1 Admission (RFC-0007 §8.4.1)

A retry for attempt `k + 1` is admitted iff **all** hold:

| # | Condition |
| --- | --- |
| A1 | `failure.retry == RetryDisposition::Retryable` |
| A2 | `failure.error_class ∈ node.retry.retry_on` |
| A3 | `attempts_started < node.retry.max_attempts` |
| A4 | Neither `run_cancel` nor `runtime_cancel` is cancelled |
| A5 | The run budget is not exhausted (§5.16.3) |
| A6 | The remaining run budget exceeds the backoff delay plus a non-zero slice (otherwise the retry would immediately time out) |

Failing any condition ⇒ durable `Failed` via C7 with the same `FailureIr`.

#### 5.11.2 Sequence

The retry sequence is exactly §5.8.3 steps (a)–(f). One `DecisionKind::Retry` record MUST be appended per admitted retry with metadata `{ node_id, attempt: k, next_attempt: k+1, error_class, retry_admitted: true, backoff_ms, escalated_to: … }`, and one with `retry_admitted: false` plus a `reason` for each rejection (which admission condition failed).

#### 5.11.3 Backoff

```text
raw(Fixed { delay_ms })                = delay_ms
raw(Exponential { base_ms, factor }, k) = base_ms * factor^(k - 1)
delay = min(max(raw, 0), max_backoff)
```

| # | Rule |
| --- | --- |
| B1 | `k` is the **failed** attempt number (1-based). The first retry (after attempt 1) uses `factor^0 = 1`, i.e. `base_ms`. |
| B2 | `factor` MUST be treated as `1.0` when it is non-finite or `< 1.0`; the computed product MUST saturate at `max_backoff` rather than overflow. |
| B3 | The sleep MUST be `tokio::select!` over `tokio::time::sleep(delay)` and the cancel tokens, so cancel during backoff is immediate. |
| B4 | Backoff elapsed time is **not** durable (deferred, §12). On resume with a node `Ready` and `attempts_started >= 1`, the scheduler MUST sleep the **full** `backoff_delay(.., attempts_started, max_backoff)` before C3. This is deterministic and never under-waits; it can over-wait after a crash, which is acceptable. |
| B5 | Tests MUST use `tokio::time::pause()` / `advance()`. No test may sleep in real time for backoff. |
| B6 | `backoff_delay` MUST be pure and total for every `Backoff` value, including `delay_ms = 0` (returns `Duration::ZERO`). |

#### 5.11.4 Tier escalation (fix 27)

| # | Rule |
| --- | --- |
| ES1 | Escalation applies to attempt `k` iff `node.retry.escalate_after == Some(n)` **and** `k > n` **and** `node.retry.escalate_to_tier == Some(tier)`. |
| ES2 | `escalate_after = Some(n)` with `escalate_to_tier = None` MUST NOT escalate and MUST record one `Retry` decision with `{ "escalation_skipped": "no target tier" }`. |
| ES3 | The escalated tier MUST be applied to `CapabilityExecContext.effective_tier` only. The scheduler MUST NOT write `TaskNode.model_tier` (topology is planner-owned). |
| ES4 | Escalation MUST be decided and recorded **before** the C3 dispatch of attempt `k`; the C5 `input_ref` rewrite for that attempt MUST also be committed before C3. |
| ES5 | Escalation MUST be ignored for adapter nodes (`VerifyCompile`, `VerifyTest`, `GateHuman`, `Aggregate`) — there is no model call to escalate (RFC-0009 Appendix C). |
| ES6 | Escalation is monotone within a node's attempts: once escalated, later attempts MUST NOT fall back to `node.model_tier`. |

### 5.12 Cancellation and drain

#### 5.12.1 `pending_cancels` (fix 29)

| Rule | Statement |
| --- | --- |
| PC1 | `cancel(dag_id)` for a DAG this process does not own MUST insert `dag_id` into `pending_cancels` when the durable DAG is non-terminal, so a `run` that starts moments later cancels immediately (R5 / L2). |
| PC2 | `run` MUST remove its `dag_id` from `pending_cancels` at R5 and MUST fire `run_cancel` if it was present. |
| PC3 | `pending_cancels` is process-local and MUST NOT be treated as durable. Durability comes from the C6 write in §5.12.4. |
| PC4 | Entries MUST be removed when consumed (R5) or when the unowned-cancel path completes its own C6 (§5.12.4), to avoid cancelling a later re-dispatch of the same DAG. |

#### 5.12.2 Owned cancel (run-side)

| Step | Actor | Action |
| --- | --- | --- |
| 1 | `cancel` | Look up `OwnedDag`, fire `run_cancel`, increment `SchedulerMetrics::cancels`. No decision record is written: cancellation is an operator action already durable in the control plane |
| 2 | run loop | Observes cancellation (L1 or the in-flight `select!`), **drops** the node future immediately |
| 3 | run loop | C6: node `Running`/`Ready`/`WaitingApproval` → `Cancelled`; other non-terminal nodes → `Skipped`; `DagState → Cancelled` |
| 4 | run loop | Append `NodeState` events for every marked node |
| 5 | run loop | Write `terminal = Some(DagState::Cancelled)`, drop `OwnedGuard` (notifies `completed`) |
| 6 | run loop | Return `Ok(DagOutcome { state: Cancelled, failed_node: None, failure: None })` |
| 7 | `cancel` | Wakes on `completed`, reads `terminal`, returns |

| Rule | Statement |
| --- | --- |
| CN1 | Dropping the node future is the cancellation mechanism for tools (RFC-0006 §3.8) and workers. The scheduler MUST NOT wait for a cooperative acknowledgement. |
| CN2 | `cancel` MUST await `OwnedDag::completed` for at most `cancel_drain_grace + cancel_write_grace`. |
| CN3 | The run loop MUST stop awaiting the in-flight node future after `cancel_drain_grace` and MUST then force C6 (fix 30). |
| CN4 | After forcing C6 the run loop MUST notify `completed` even if the CAS failed, so `cancel` never hangs (G2). |
| CN5 | A cancel arriving after the terminal checkpoint MUST NOT rewrite it. `cancel` returns `Ok(())` observing the terminal state. |

#### 5.12.3 `cancel` return table (fix 30, normative)

| Situation | `cancel` returns |
| --- | --- |
| Owned run; C6 committed within the grace | `Ok(())` |
| Owned run; run already terminal (`Succeeded` / `Failed` / `Cancelled` / `ReplanRequired`) | `Ok(())` |
| Owned run; forced C6 after `cancel_drain_grace` committed | `Ok(())` |
| Owned run; forced C6 hit `StoreError::Conflict` | `Err(Conflict { dag_id })` |
| Owned run; forced C6 hit another store error | `Err(Store(m))` |
| Owned run; grace elapsed with **no** C6 committed and no terminal state | `Err(Internal("cancel drain grace exceeded"))` |
| Unowned; durable DAG non-terminal | acquire ownership, C6, release ⇒ `Ok(())` (§5.12.4) |
| Unowned; durable DAG terminal | `Ok(())`, no write |
| Unowned; DAG missing | `Err(DagNotFound(dag_id))` |
| Unowned; ownership contended (a `run` won the race) | insert into `pending_cancels`, fire that run's token, then behave as the owned rows above |

**Pin:** after `cancel_drain_grace` the *run* is responsible for forcing the C6 markings and CAS and then notifying `completed`. `cancel` returns `Ok(())` iff a C6 (or an equivalent terminal state) is durable, and `Err(Store | Conflict | Internal)` otherwise. `cancel` never writes the DAG for a DAG this process owns.

#### 5.12.4 Unowned non-terminal cancel (fix 31)

| Step | Action |
| --- | --- |
| 1 | `dags.get(dag_id)` ⇒ `None` ⇒ `Err(DagNotFound)` |
| 2 | Terminal state ⇒ `Ok(())` with no write |
| 3 | Insert an `OwnedDag` (transient) — occupied ⇒ fall through to the owned path |
| 4 | Resolve the run binding for event attribution; missing ⇒ still write C6, log at `warn`, skip events |
| 5 | C6 markings + `DagState::Cancelled` |
| 6 | Append `NodeState` events |
| 7 | Drop the guard (releases ownership, notifies) |
| 8 | Remove `dag_id` from `pending_cancels` |

This is the path that makes `AlloyRuntime::drain` able to terminalize DAGs whose `run` future already returned (or never started).

#### 5.12.5 Runtime drain composition (fix 33)

`AlloyRuntime::drain(grace)` MUST, per amendment A1:

```text
deadline = Instant::now() + grace          // FIRST
for dag in live_dags:
    timeout(deadline - now(), scheduler.cancel(dag))
timeout(deadline - now(), wait_for_in_flight_runs())
```

| Rule | Statement |
| --- | --- |
| DR1 | The deadline MUST be computed before the first `cancel` await. Today's order (cancel, then deadline) lets a slow cancel consume the entire grace. |
| DR2 | `SchedConfig.cancel_drain_grace` SHOULD be strictly less than the host's drain grace, so the run's forced C6 lands inside the drain window. |
| DR3 | Drain MUST NOT call `set_scheduler` (phase `Draining` rejects it). The swap is step 3 of §4.6. |
| DR4 | A `cancel` error during drain MUST be logged and MUST NOT abort the drain of other DAGs. |

### 5.13 Verify adapters

#### 5.13.1 Tool call construction

| Node kind | Tool | Arguments |
| --- | --- | --- |
| `VerifyCompile` | `cargo_check` | `{ "workspace_root": <session.workspace_root>, "message_format": "json" }` |
| `VerifyTest` | `cargo_test` | `{ "workspace_root": <session.workspace_root> }` |

| Rule | Statement |
| --- | --- |
| V1 | `workspace_root` MUST come from the session row, never from the node payload or the environment. |
| V2 | `message_format` MUST be `"json"` for `cargo_check` (the only accepted value). `cargo_test` takes no message format in RFC-0006. |
| V3 | The adapter MUST attach attribution: `ToolCall::with_attribution(Some(session), Some(run), Some(node))` and `with_call_id(format!("{node}:{attempt}"))`. |
| V4 | `package`, `features`, `all_features`, `jobs`, and `test_name_filter` MUST be omitted in MVP. Adding them is a later RFC's choice, not a scheduler default. |
| V5 | The adapter MUST NOT retry, sleep, or loop. One call per invocation. |

#### 5.13.2 Result classification (fix 39, normative and total)

`content.exit_code` is authoritative when present. `content` shape is RFC-0006's cargo envelope: `{ exit_code, signal, stdout_utf8, stderr_utf8, stdout_truncated, stderr_truncated, duration_ms, backend, policy_digest }`.

| `ToolCaller::call` result | `content.exit_code` | `signal` | Outcome |
| --- | --- | --- | --- |
| `Ok(r)`, `!r.is_error()` | `Some(0)` | — | `Ok(VerifyOutcome { ok: true, diagnostics, raw_artifact })` |
| `Ok(r)`, `!r.is_error()` | `None` | — | `Err(Internal("cargo result missing exit_code"))` |
| `Ok(r)`, `!r.is_error()` | `Some(n != 0)` | — | `Err(Internal("cargo result invariant: ok with exit {n}"))` |
| `Ok(r)`, `is_error`, `ExecutionFailed` | `Some(101)` | `None` | **soft fail**: `Ok(VerifyOutcome { ok: false, diagnostics, raw_artifact })` ⇒ `FailureIr { error_class: Compile\|Test, retry: NonRetryable }` |
| `Ok(r)`, `is_error`, `ExecutionFailed` | `Some(101)` | `None`, and `stdout_truncated == true` | `Err(ToolFailure(ToolError::Transient { code: "cargo_output_truncated", .. }))` — diagnostics are unreliable, so retry rather than mis-repair |
| `Ok(r)`, `is_error`, `ExecutionFailed` | any | `Some(sig)` | `Err(ToolFailure(ToolError::Transient { code: "cargo_signal", message: "signal {sig}" }))` — OOM/kill class, **never** `Compile` |
| `Ok(r)`, `is_error`, `ExecutionFailed` | `Some(n ∉ {0, 101})` | `None` | `Err(ToolFailure(ToolError::Permanent { code: "cargo_exit_{n}", .. }))` — usage/toolchain error |
| `Ok(r)`, `is_error`, `ExecutionFailed` | `None` | `None` | `Err(ToolFailure(ToolError::Transient { code: "cargo_no_exit", .. }))` |
| `Ok(r)`, `is_error`, `Transient` | — | — | `Err(ToolFailure(that error))` |
| `Ok(r)`, `is_error`, `Permanent` | — | — | `Err(ToolFailure(that error))` |
| `Ok(r)`, `is_error`, `InvalidArgs` | — | — | `Err(Internal("adapter built invalid arguments: {m}"))` |
| `Err(ToolCallerError::…)` | — | — | per the §3.5 mapping table |

| Rule | Statement |
| --- | --- |
| VC1 | Exit `101` with no signal is the **only** compile/test failure signal. It is a normal outcome, not an error (fix 4). |
| VC2 | A signal MUST NOT be classified as `Compile` / `Test` (fix: `signal ≠ Compile`). |
| VC3 | A sandbox / disclosure / token denial MUST surface as `AdapterError::PermissionDenied`, never as `ok: false`. |
| VC4 | The `is_error == error.is_some()` invariant is enforced by `ToolResult`; an adapter MUST NOT re-derive success from `content` alone except for the exit-code authority rule above. |
| VC5 | Truncated stdout on a `101` is `Transient` because the diagnostic stream is incomplete; truncated stdout on exit `0` is ignored. |

#### 5.13.3 Diagnostics ingest

| Rule | Statement |
| --- | --- |
| DG1 | `parse_rustc_diagnostics` MUST parse `stdout_utf8` as NDJSON, keep only objects with `reason == "compiler-message"`, and read `message.{code.code, level, message, spans, children}`. |
| DG2 | Unparseable lines MUST be skipped, counted, and reported once at `debug`. A malformed stream MUST NOT fail the node. |
| DG3 | `level` maps `error → Error`, `warning → Warning`, `note → Note`, `help → Help`; anything else MUST be skipped. |
| DG4 | Only spans with `is_primary == true` populate `SpanRef`, mapped `{ file_name, line_start, column_start, line_end, column_end }`; paths MUST stay workspace-relative when the compiler emits them that way. |
| DG5 | `package` MUST come from the enclosing cargo message's `target.name` when present. |
| DG6 | Diagnostics MUST be deduped by `fingerprint`, preserving first-seen order, and capped at `MAX_DIAGNOSTICS = 200` with a `Note`-level marker diagnostic appended when truncated. |
| DG7 | `cargo_test` output is **not** rustc JSON. `McpVerifyTestAdapter` MUST return an **empty** `diagnostics` vector and MUST rely on the raw log artifact (fix 40). It MUST NOT invent synthetic diagnostics from test names. |
| DG8 | `raw_json` SHOULD carry the original compiler message for provenance, subject to the artifact size caps of RFC-0002. |

#### 5.13.4 Fingerprint framing (fix 40)

```text
b"alloy.diag.v1" || 0x00 ||
code_or_empty              || 0x00 ||
level_serde_snake_case     || 0x00 ||   # error | warning | note | help
message_utf8               || 0x00 ||
span_path_or_empty         || 0x00 ||
start_line_le_u32          ||
start_col_le_u32           ||
end_line_le_u32            ||
end_col_le_u32
```

| Rule | Statement |
| --- | --- |
| FP1 | The digest MUST be computed over the byte framing above with a single hash pass (`Digest` = the RFC-0002 content hash). |
| FP2 | `0x00` separators are mandatory: without framing, `code="E05"` + `message="02x"` and `code="E0502"` + `message="x"` would collide. |
| FP3 | Only the **first** primary span participates. Nested `children` MUST NOT contribute. |
| FP4 | Integers MUST be little-endian `u32`; a missing span contributes an empty path plus four zero integers. |
| FP5 | The framing is versioned by the `alloy.diag.v1` prefix; changing any field ordering requires a new prefix. |

#### 5.13.5 Raw log artifact

| Rule | Statement |
| --- | --- |
| RL1 | The adapter MUST `put` one artifact containing the cargo envelope (`stdout_utf8`, `stderr_utf8`, `exit_code`, `signal`, truncation flags) with `ArtifactKind::Log` and label `alloy.envelope = verify_raw`. |
| RL2 | The put MUST happen for both `ok: true` and `ok: false`. |
| RL3 | An artifact store failure MUST surface as `AdapterError::Artifact(m)`, and the scheduler MUST treat it as a node failure — the verify result is not trustworthy without its log. |
| RL4 | `VerifyOutcome.raw_artifact` MUST carry the id. |
| RL5 | The log MUST NOT be written into `output_ref` on failure (OU3). |

### 5.14 `Aggregate` fold

| Rule | Statement |
| --- | --- |
| AG1 | `Aggregate` is structural: no worker, no adapter, no model call, no tool call. |
| AG2 | The fold MUST produce `{ "aggregate": true, "preds": [{ "node_id", "kind", "output_ref" }, …] }` in ascending `NodeId`, from the node's incoming Data edges. |
| AG3 | The result MUST be written as a `node_output` envelope and `output_ref` set (C4). |
| AG4 | An `Aggregate` node with zero incoming Data edges MUST fail closed (`Invariant`), because it can never carry provenance. |
| AG5 | Day-1 templates ship no `Aggregate` node (RFC-0009 §5.7.3); the path exists for hand-built DAGs and tests. |

### 5.15 Skip and cancel propagation

| Trigger | Marking |
| --- | --- |
| Durable node `Failed` (retries exhausted) | Every node not in a terminal state and not the failed node → `Skipped`; `DagState::Failed` |
| Gate deny / expiry | Gate node → `Cancelled`; every other non-terminal node → `Skipped`; `DagState::Failed` |
| Cancel | In-flight node (`Running` / `Ready` / `WaitingApproval`) → `Cancelled`; every other non-terminal node → `Skipped`; `DagState::Cancelled` |
| Budget exhaustion | The node that would have run → `Failed` (class `Budget`); the rest → `Skipped`; `DagState::Failed` |
| Run timeout | In-flight node → `Failed` (class `Timeout`, `NonRetryable`); the rest → `Skipped`; `DagState::Failed` |

| Rule | Statement |
| --- | --- |
| SK1 | Propagation MUST be a single CAS. Partial markings are forbidden. |
| SK2 | `Skipped` MUST NOT be applied to a node already `Succeeded`, `Failed`, `Cancelled`, `Skipped`, or `CachedHit`. |
| SK3 | Marking MUST NOT depend on graph reachability in MVP: **every** non-terminal node is skipped. Reachability-aware skipping is deferred. |
| SK4 | One `NodeState` event per marked node MUST be appended after the CAS. |

### 5.16 Budgets

#### 5.16.1 Effective ceilings (fix 34)

```text
effective_usd    = min(deps.budget_policy.max_usd_per_run,
                       session.budget.max_usd_per_run,
                       min { c | Constraint::MaxUsd(c) in goal.constraints, c.is_finite(), c >= 0.0 })
effective_tokens = min(deps.budget_policy.max_tokens_per_run,
                       session.budget.max_tokens_per_run)
```

| # | Rule |
| --- | --- |
| BG1 | `Constraint::MaxUsd(c)` participates only when `c.is_finite() && c >= 0.0`. Non-finite values MUST be ignored and MUST be recorded once as a `DecisionKind::Budget` with `{ "ignored_max_usd": "non_finite" }`. |
| BG2 | A negative `MaxUsd` MUST clamp the effective ceiling to `0.0` (which then trips BG3). |
| BG3 | `effective_usd <= 0.0` MUST be treated as **exhausted before** calling `check_budget`. `CostMeter::check_budget` reports `usd_exhausted` only when `spent >= max`, and `spent` is `None` before the first model call, so a `0.0` ceiling would otherwise let a whole run through. |
| BG4 | The scheduler MUST construct a `BudgetPolicy` carrying the effective ceilings for `check_budget`, keeping `max_parallel_* = 1`. |
| BG5 | Session-row `max_parallel_*` values other than `1` MUST be ignored for dispatch (execution stays serial) and MUST be recorded once as a `Budget` decision with `{ "ignored_parallelism": … }`. They MUST NOT fail the run: the construction-time check (N3–N5) governs the injected policy, not historical session rows. |
| BG6 | `effective_tokens == 0` behaves as exhausted through `check_budget` directly (`tokens >= 0` is always true), so no special case is required. |

#### 5.16.2 Meter rebuild on resume (fix 35)

```rust
let rebuilt = reaccumulate_cost_from_events(&*deps.events, session_id, Some(run_id)).await?;
let meter = deps.cost_meters.meter_for(run_id);
meter.with_mut(|m| *m = rebuilt);
```

| # | Rule |
| --- | --- |
| B7 | The rebuild MUST happen once per `run` invocation at R8, **before** the first budget check and before any dispatch. |
| B8 | The assignment MUST be `*m = rebuilt` inside `with_mut` — not `add_*` calls — so a resumed run does not double count. |
| B9 | `meter_for(run_id)` MUST be memoized per run so the RFC-0007 router bridge and the scheduler share one meter. `ProcessCostMeterFactory::release` MUST NOT be called from `OwnedGuard::drop` (§4.4 rule G3): the host releases the meter after the outcome is surfaced, otherwise a re-dispatch inside the same process would lose accumulated spend. |
| B10 | `ObsError` from the rebuild MUST map to `SchedError::Store` — a run must not start with an unknown spend. |

#### 5.16.3 Enforcement points

| Point | Action on exhaustion |
| --- | --- |
| L5, before selecting a node | Do not dispatch; construct a `Budget` failure for the node that would have run (or the lowest `Ready` node) |
| A5, before admitting a retry | Reject the retry; go durable `Failed` |
| After each node completes | `maybe_signal_budget_warning(&plane, session, Some(run), &meter, &effective_policy)` |

| Rule | Statement |
| --- | --- |
| BE1 | On exhaustion the scheduler MUST append `BudgetWarning` (via `maybe_signal_budget_warning`) and one `DecisionKind::Budget` record, then terminalize with `DagState::Failed`. |
| BE2 | The scheduler MUST NOT itself add model usage to the meter. Workers (RFC-0013) and the router (RFC-0007) own `add_model_usage`; tool calls are metered by RFC-0006's `ToolCall` records. Double counting is an AC (§13 AC 38). |
| BE3 | Budget exhaustion is a **planned** failure: `Ok(DagOutcome { state: Failed, .. })`, never `Err`. |

### 5.17 `DagState` derivation (fix 42, total and first-match-wins)

Evaluated in order over the post-CAS blob plus `DeriveFlags`:

| # | Condition | Result |
| --- | --- | --- |
| D1 | `flags.replan_requested` | `ReplanRequired` |
| D2 | `flags.cancel_requested` **and** ≥1 node `Cancelled` | `Cancelled` |
| D3 | Any node `Cancelled` **and not** `flags.approval_failure` | `Cancelled` |
| D4 | Any node `Failed` | `Failed` |
| D5 | Any node `Cancelled` **and** `flags.approval_failure` | `Failed` (gate deny / expiry) |
| D6 | Any node in `{Pending, Ready, Running, WaitingApproval}` | `Running` if none is `WaitingApproval`, else `WaitingApproval` — **loop-internal only**; `run` MUST NOT return either (fix 24) |
| D7 | Every node `∈ {Succeeded, CachedHit}` (and ≥1 node exists) | `Succeeded` |
| D8 | Every node `∈ {Succeeded, CachedHit, Skipped}` with ≥1 `Skipped` | `Failed` — a partially skipped DAG never "succeeds" (fix 42) |
| D9 | Empty node map | `Err(Invariant("empty dag"))` — rejected by validation, defended here |

| Rule | Statement |
| --- | --- |
| DS1 | D4 precedes D5 so a real node failure names the failing node rather than the gate. |
| DS2 | D3 before D4 is deliberately **not** the order: an explicit `Failed` node dominates a `Cancelled` sibling unless the cancel was user-requested (D2). |
| DS3 | `Skipped` alone MUST NEVER produce `Succeeded` (D8). |
| DS4 | **Stall detection.** If the loop finds no `Ready`, no `Running`, no `WaitingApproval`, and at least one `Pending` node whose Data predecessors can never be satisfied (a `Skipped`, `Failed`, or `Cancelled` predecessor), the scheduler MUST treat it as a stall: mark the remaining `Pending` nodes `Skipped` and derive again. If derivation still yields `Running`, return `Err(Invariant("dag stalled: {pending:?}"))`. |
| DS5 | `derive_dag_state` MUST be pure and MUST NOT read the store. |

### 5.18 `DagOutcome` construction (fix 43)

| Field | Rule |
| --- | --- |
| `dag_id` | The requested id |
| `generation` | The generation of the blob the terminal CAS committed (unchanged from load) |
| `state` | §5.17 result, restricted to `{Succeeded, Failed, Cancelled, ReplanRequired}` |
| `failed_node` | Selection table below |
| `failure` | The `FailureIr` matching `failed_node`, or `None` |

**`failed_node` selection (ordered):**

| # | Rule |
| --- | --- |
| FN1 | The lowest `NodeId` in `NodeState::Failed`, if any |
| FN2 | Otherwise, the lowest `NodeId` that is `Cancelled` **and** has a durable `ErrorClass::Approval` failure (gate deny / expiry) |
| FN3 | Otherwise `None` (`Succeeded`, plain `Cancelled`, `ReplanRequired`) |

| Rule | Statement |
| --- | --- |
| FO1 | `failure` MUST be recovered from the durable `failure_ref` artifact when the in-memory value is unavailable (crash resume), so a resumed terminal run still reports a structured failure. |
| FO2 | A missing `failure_ref` artifact MUST degrade to `FailureIr { node, error_class, retry: NonRetryable, diagnostics: [], notes: "failure detail unavailable" }` reconstructed from the `NodeState` event, not to `None`. |
| FO3 | `state == Failed` with `failed_node == None` is permitted only for D8 (all-skipped) and MUST carry `failure = None`. |
| FO4 | `state == Cancelled` MUST carry `failed_node = None` and `failure = None`. |

### 5.19 Timeouts (fix 44)

| Kind | Source | Budget | On elapse | Retryable |
| --- | --- | --- | --- | --- |
| **Node** | `node.timeout_ms` clamped by the remaining run budget | per attempt | `FailureIr { error_class: Timeout, retry: Retryable, notes: "node timeout after {ms}ms" }` | Yes, if `Timeout ∈ retry_on` |
| **Gate** | `node.timeout_ms` (never clamped by the run budget) | per gate | §5.7.8 expiry, `ErrorClass::Approval` | No |
| **Run** | `deps.run_timeout` | per `run` invocation, **excluding** gate wait | `FailureIr { error_class: Timeout, retry: NonRetryable, notes: "run timeout after {ms}ms" }`, DAG `Failed` | No |

```text
elapsed_charged = now - run_started - gate_wait_total
remaining_run   = run_timeout.saturating_sub(elapsed_charged)
node_deadline   = min(node.timeout_ms, remaining_run)
```

| # | Rule |
| --- | --- |
| T1 | `gate_wait_total` MUST accumulate the wall time spent inside gate waits and MUST be subtracted from the charged run elapsed (fix 44). A human taking an hour to approve MUST NOT fail the run with a run timeout. |
| T2 | Gate waits MUST NOT be clamped by `remaining_run`. |
| T3 | `node_deadline == 0` MUST short-circuit to the run-timeout path without dispatching. |
| T4 | Node timeout and run timeout MUST be distinguishable in the durable failure (`retry` and `notes` differ, per the table). |
| T5 | Run timeout MUST NOT admit a retry, even if `Timeout ∈ retry_on`. |
| T6 | `gate_wait_total` is process-local; a resumed run starts a fresh run budget. That is intentional and documented (§15 Q3). |

### 5.20 `reconcile_terminal_run` (fix 12)

```rust
async fn reconcile_terminal_run(&self, dag_id: DagId, terminal: DagState) -> Result<(), SchedError>;
```

| # | Rule |
| --- | --- |
| RC1 | `terminal` MUST be `Succeeded` \| `Failed` \| `Cancelled`; anything else ⇒ `Err(Config("reconcile_terminal_run requires a terminal state"))`. |
| RC2 | Missing DAG ⇒ `Err(DagNotFound)`. Already-terminal DAG ⇒ `Ok(())` with no write (idempotent). |
| RC3 | A DAG owned by a live run in this process ⇒ `Ok(())` with no write: the run owns terminalization. |
| RC4 | Otherwise: acquire ownership, then a single CAS marking non-terminal nodes (`WaitingApproval`/`Running`/`Ready` → `Cancelled` for `Cancelled`/`Failed` targets; the rest → `Skipped`), set `DagState = terminal`, append `NodeState` events, release ownership. |
| RC5 | For `terminal == Failed` arising from a gate decision, the reconciler MUST put an `Approval` `failure_ir` for the gate node so `failed_node` selection (FN2) still works. |
| RC6 | For `terminal == Succeeded` with non-terminal nodes remaining, the reconciler MUST NOT invent success: it MUST write `Failed` with `notes: "control row succeeded with unfinished nodes"` and log at `warn`. |
| RC7 | `Conflict` ⇒ `Err(Conflict { dag_id })`; callers (resume) log and continue. |
| RC8 | The method MUST be safe to call concurrently with `cancel` for the same DAG: whichever acquires ownership first wins, the other observes a terminal state and returns `Ok(())`. |

### 5.21 Replan observation

| # | Rule |
| --- | --- |
| RP1 | The loop MUST read the run row state at L4 (cheap `get_run`) and MUST detect `RunControlState::ReplanRequested`. |
| RP2 | On detection it MUST stop dispatch, drop the in-flight node future, CAS `DagState::ReplanRequired` (C10, node states untouched except an in-flight `Running` node marked `Cancelled`), and return `Ok(DagOutcome { state: ReplanRequired, .. })`. |
| RP3 | This is mandatory: `PlanService::replan` returns `DagBusy` while the DAG is `Running`, so without C10 the replan is permanently blocked (RFC-0009 §6.6). |
| RP4 | The scheduler MUST NOT call `PlanService` or mutate topology itself. |
| RP5 | `ReplanRequired` MUST NOT set `failed_node` / `failure`. |

---

## 6. Lifecycle & Concurrency

### 6.1 Runtime phase interaction

| Phase | `run` | `cancel` | `reconcile_terminal_run` | `set_scheduler` |
| --- | --- | --- | --- | --- |
| `Created` | rejected by `RuntimeHandle` | rejected | rejected | allowed |
| `Running` | allowed (single-flight per run) | allowed | allowed (not single-flighted, A2) | allowed |
| `Draining` | rejected (no new runs) | allowed | allowed | **rejected** |
| `Stopped` | rejected | rejected | rejected | rejected |

### 6.2 Ownership lifecycle

```text
new() ──► create_dir_all(data_dir) ──► open scheduler.lock ──► try_lock_exclusive
                                                                  │
run(dag) ──► owned.insert(dag) ──► OwnedGuard ──► loop ──► terminal CAS ──► drop(guard)
                                                                  │
                                                        owned.remove + notify_waiters
drop(LinearScheduler) ──► lock file handle drops ──► advisory lock released
```

### 6.3 Crash recovery matrix

| Durable DAG | Durable node | Durable run row | Recovery |
| --- | --- | --- | --- |
| `Pending` | all `Pending` | `accepted` | Fresh start (R14 C1) |
| `Running` | one `Running` | `running` | Adopt as a lost attempt (§5.3.2) |
| `Running` | one `Ready`, `attempts ≥ 1` | `running` | Resume backoff (B4) then C3 |
| `Running` | all terminal | `running` | Derive terminal state, C7 |
| `WaitingApproval` | gate `WaitingApproval` | `waiting_approval` | Re-register only (§5.7.3) |
| `WaitingApproval` | gate `WaitingApproval` | `failed` | `resume` → `reconcile_terminal_run` (§5.7.11) |
| `WaitingApproval` | gate `WaitingApproval` | `running` + `ApprovalResolved(allow)` | Allow path (§5.7.6) |
| `Failed` / `Succeeded` / `Cancelled` | — | any | R9 short-circuit, no CAS |
| `ReplanRequired` | — | `replan_requested` | R10 short-circuit |
| any | committed CAS, missing event | any | RF3 repair append |

### 6.4 Concurrency invariants

| # | Invariant |
| --- | --- |
| CI1 | One `LinearScheduler` per `data_dir` per host (OS lock). |
| CI2 | One `run` loop per `DagId` (ownership map). |
| CI3 | One node `Running` per DAG at any instant. |
| CI4 | One writer per DAG blob at any instant (CI2 + CAS). |
| CI5 | Cargo invocations are serialized by `max_parallel_cargo = 1` plus MCP host admission (`host_parallel_honesty`). |
| CI6 | No `.await` while holding an internal `Mutex`. |

---

## 7. Configuration

### 7.1 New knobs

| Knob | Type | Default | Source |
| --- | --- | --- | --- |
| `data_dir` | `PathBuf` | *required, absolute* | `RuntimeConfig.data_dir` |
| `cancel_drain_grace` | `Duration` | `5s` | `SchedConfig` |
| `cancel_write_grace` | `Duration` | `2s` | `SchedConfig` |
| `max_backoff` | `Duration` | `60s` | `SchedConfig` |
| `host_parallel_honesty` | `bool` | `true` | host assembly |
| `validate_on_load` | `bool` | `true` | `SchedConfig` |
| `validate_opts` | `ValidateOpts` | `default()` | `SchedConfig` |
| `run_timeout` | `Duration` | `RuntimeConfig.run_timeout` | runtime config |
| `budget_policy` | `BudgetPolicy` | `RuntimeConfig.budget_policy` | runtime config |

### 7.2 `example.env`

Documented as comments only. Alloy MUST NEVER write `.env` (§1.5 rule 10).

```text
# RFC-0010 scheduler (all optional; defaults shown)
# ALLOY_SCHED_CANCEL_DRAIN_GRACE_MS=5000
# ALLOY_SCHED_CANCEL_WRITE_GRACE_MS=2000
# ALLOY_SCHED_MAX_BACKOFF_MS=60000
```

| Rule | Statement |
| --- | --- |
| CF1 | The scheduler MUST NOT read environment variables itself. Mapping env → `SchedConfig` is host assembly / RFC-0015. |
| CF2 | Profile TOML MAY carry the same keys; precedence is RFC-0015's problem. |
| CF3 | No new key may weaken a serial invariant. There is no `max_parallel` knob. |

---

## 8. Error Handling

### 8.1 Boundary matrix

| Layer | Error type | Mapped by | Table |
| --- | --- | --- | --- |
| MCP host | `McpError` | `map_mcp_error` (alloy-tools) | §3.5 |
| Tool seam | `ToolCallerError` | verify adapters | §3.5 |
| Adapters | `AdapterError` | scheduler | §5.10 |
| Workers | `CapabilityExecError` / `CapabilityOutcome::Failed` | scheduler | §5.10 |
| Store | `StoreError` | `checkpoint.rs` | §8.2 |
| Scheduler | `SchedError` | `RuntimeHandle` | §3.2 |
| Runtime | `RuntimeError` | `runtime_to_run` | §3.2 |
| Control plane | `RunError` | RFC-0003 | RFC-0003 §6 |

### 8.2 `StoreError` mapping

| `StoreError` | `SchedError` | Note |
| --- | --- | --- |
| `Conflict` | `Conflict { dag_id }` | §5.8.4; stop checkpointing |
| `NotFound` | `DagNotFound(dag_id)` on load; `Store(..)` elsewhere | — |
| `Corrupt(m)` | `Invariant("corrupt dag blob: {m}")` | never retried |
| `Busy` | `Store("busy")` after the store's own retries | SQLite contention |
| `Internal(m)` | `Store(m)` | — |
| `Io(m)` | `Store(m)` | — |

`EventSinkError` from an append MUST NOT abort a committed transition (W4); it is logged and repaired.

### 8.3 Fail-closed catalogue

| Situation | Behaviour |
| --- | --- |
| Multiple `Ready` nodes | `Err(Invariant)` — never pick arbitrarily |
| Multiple `Running` nodes after restart | `Err(Invariant)` |
| `Succeeded` node without `output_ref` on a Data edge | `Err(Invariant)` |
| `WaitingApproval` on a non-gate node | `Err(Invariant)` |
| Gate node without `approval` | `Err(Invariant)` |
| Capability node without `capability` | `Err(Invariant)` |
| Pending placeholder at dispatch | `Err(Invariant)` |
| Unknown `ApprovalResolved.decision` | `Err(Invariant)` |
| `CachedHit` without `output_ref` | `Err(Invariant)` |
| Load-time validation failure | `Err(Invariant)` |
| Stalled DAG after skip propagation | `Err(Invariant)` |
| `run` about to return `Running` / `WaitingApproval` / `Pending` | `Err(Invariant)` |

### 8.4 What MUST NOT be an error

| Situation | Returns |
| --- | --- |
| Compile / test failure after retries | `Ok(DagOutcome { state: Failed, .. })` |
| Gate deny / expiry | `Ok(DagOutcome { state: Failed, .. })` |
| Budget exhaustion | `Ok(DagOutcome { state: Failed, .. })` |
| Run timeout | `Ok(DagOutcome { state: Failed, .. })` |
| Cancellation with a durable C6 | `Ok(DagOutcome { state: Cancelled, .. })` |
| Replan observed | `Ok(DagOutcome { state: ReplanRequired, .. })` |

---

## 9. Observability

### 9.1 Spans

| Span | Fields |
| --- | --- |
| `sched.run` | `dag_id`, `run_id`, `session_id`, `generation` |
| `sched.node` | `node_id`, `kind`, `attempt`, `effective_tier` |
| `sched.checkpoint` | `checkpoint` (`c1`…`c10`), `generation`, `nodes_changed` |
| `sched.gate` | `gate_id`, `node_id`, `remaining_ms` |
| `sched.verify` | `tool`, `exit_code`, `signal`, `diagnostics`, `truncated` |
| `sched.cancel` | `dag_id`, `owned`, `forced` |

| Rule | Statement |
| --- | --- |
| OB1 | Spans MUST NOT record goal text, prompt bodies, diagnostics messages, or env values. |
| OB2 | `sched.verify` MUST record counts and codes, never stdout. |

### 9.2 Events emitted by the scheduler

| Event | When | Payload keys |
| --- | --- | --- |
| `NodeState` | every committed node transition (after the CAS) | `node_id`, `from`, `to`, `generation`, `attempt`, optional `failure_ref`, `error_class`, `retry`, `decision`, `repaired` |
| `ApprovalRequested` | gate first schedule only | `gate_id`, `node_id`, `reason`, `timeout_ms` |
| `Decision` | retry / budget / gate decisions | via `DecisionLog::record` |
| `BudgetWarning` | exhaustion, via `maybe_signal_budget_warning` | RFC-0004 shape |

| Rule | Statement |
| --- | --- |
| OB3 | The scheduler MUST NOT emit `RunCompleted`, `ReplanRequested`, `ApprovalResolved`, `PlanProduced`, or `GoalSubmitted`. Those belong to RFC-0003 / RFC-0009. |
| OB4 | The scheduler MUST NOT emit `ModelCall` or `ToolCall` records: workers (RFC-0013) and the MCP host (RFC-0006) already do, and duplicating them double-counts cost (AC 38). |
| OB5 | Every `NodeState` event MUST carry `generation` so RF2 filtering works across replans. |

### 9.3 `SchedulerMetrics`

```rust
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerMetrics {
    pub runs_started: u64,
    pub runs_succeeded: u64,
    pub runs_failed: u64,
    pub runs_cancelled: u64,
    pub runs_replan_required: u64,
    pub nodes_dispatched: u64,
    pub nodes_succeeded: u64,
    pub nodes_failed: u64,
    pub nodes_skipped: u64,
    pub retries_admitted: u64,
    pub retries_rejected: u64,
    pub escalations: u64,
    pub gates_opened: u64,
    pub gates_allowed: u64,
    pub gates_denied: u64,
    pub gates_expired: u64,
    pub cas_conflicts: u64,
    pub event_repairs: u64,
    pub cancels: u64,
    pub forced_cancel_writes: u64,
    pub budget_stops: u64,
    pub node_timeouts: u64,
    pub run_timeouts: u64,
}
```

Counters are additive `AtomicU64` internally and snapshot through `metrics()`. They are debugging aids, not a durability mechanism.

### 9.4 Decision records

| `DecisionKind` | Metadata |
| --- | --- |
| `Retry` | `{ node_id, attempt, next_attempt, error_class, retry_admitted, reason?, backoff_ms?, escalated_to? }` |
| `Budget` | `{ check, effective_usd, effective_tokens, ignored_max_usd?, ignored_parallelism? }` |
| `Gate` | `{ gate_id, node_id, decision, waited_ms }` |

`prompt_body` MUST be `None` for every scheduler-authored decision.

---

## 10. Crate Dependencies & `unsafe`

| Crate | Change |
| --- | --- |
| `alloy-runtime` | No new external dependency. Uses `tokio` (`time`, `sync`), `tokio-util` (`CancellationToken`), `async-trait`, `serde_json`, `thiserror`, `tracing`, and `std::fs` file locking |
| `alloy-tools` | No new dependency; adds `mcp::tool_caller` |
| `alloy-cli` / host | Wires `ToolHandleToolCaller` into `LinearSchedulerDeps` |

| Rule | Statement |
| --- | --- |
| CD1 | `#![forbid(unsafe_code)]` stays. This RFC introduces no `unsafe`. |
| CD2 | `fs4` / `fs2` MUST NOT be added: `std::fs::File::try_lock_exclusive` is stable on the pinned toolchain (1.97.1). |
| CD3 | No JSON-schema crate, no state-machine crate, no retry crate. |
| CD4 | `alloy-runtime` MUST NOT gain a dependency on `alloy-tools`, `alloy-models`, or `alloy-cli` (B1). |

---

## 11. Testing Strategy

### 11.1 Pure unit tests (no I/O)

| Area | Cases |
| --- | --- |
| `ready_nodes` / `promotable_nodes` | Data vs Sequence satisfaction; `Skipped` never satisfies Data; `Hint` ignored; multi-ready detection; ascending order |
| `backoff_delay` | `Fixed`; `Exponential` growth; `max_backoff` cap; `factor < 1`/non-finite; `delay_ms = 0`; overflow saturation |
| `derive_dag_state` | D1–D9 including deny (D5), all-skipped (D8), stall (DS4) |
| `diagnostic_fingerprint` | Framing collisions (`E05`+`02x` vs `E0502`+`x`); missing span; level sensitivity |
| `parse_rustc_diagnostics` | Valid NDJSON; malformed lines skipped; non-`compiler-message` reasons; dedupe; cap + marker |
| `failed_node` selection | FN1 before FN2; FN2 for a gate-denied DAG; FN3 |
| Verify classification | Every row of §5.13.2 as a table test |
| `map_mcp_error` | Every `McpError` variant including `InvalidToken` and `NotDisclosed` |

### 11.2 Scheduler tests with in-memory / SQLite stores

| Area | Cases |
| --- | --- |
| Happy path | 4-node `repair_local_diagnostic` chain to `Succeeded`; one `Running` node at a time |
| Checkpoint order | Recorded store double asserts artifacts → CAS → events for every checkpoint |
| Crash windows | Kill after CAS, before events ⇒ RF3 repair; kill mid-`Running` ⇒ adopted attempt; kill during backoff ⇒ full re-wait then C3 |
| Retry | Admission matrix; C8 leaves `Ready` + `Running`; exhaustion writes durable `Failed`; escalation before C3 |
| Gate | First schedule; allow; allow_once; deny; expiry; resume re-register-only; closed receiver per `RunControlState`; `expire_gate` result table |
| Cancel | Owned mid-node; owned during gate; unowned non-terminal; terminal no-op; grace exceeded; forced C6; `Drop` releases ownership on panic |
| Ownership | Second `run` for the same DAG ⇒ `AlreadyOwned`; second scheduler on the same `data_dir` ⇒ `Ownership` |
| Budget | `MaxUsd` min; non-finite ignored; `effective_usd <= 0` stops before dispatch; meter rebuild does not double count |
| Conflict | Generation bumped mid-run ⇒ `Conflict`, no further writes |
| Reconcile | Terminal row + `WaitingApproval` DAG ⇒ terminalized; idempotent second call |
| Timeouts | Node timeout retryable; run timeout non-retryable; gate wait excluded from the run budget |

### 11.3 Cross-subsystem test

`tests/scheduler_repair_e2e.rs` (gated by the existing sandbox test feature): real SQLite storage, real MCP host with `cargo_check` over a tiny fixture crate with a deliberate type error, a stub capability executor that applies a fixed patch, and a gate approved by `RunController::approve`. Asserts: verify soft-fails with diagnostics, the repair node sees them through the predecessor envelope, the second verify succeeds, the gate opens and is approved, and the DAG reaches `Succeeded` with every node carrying `output_ref`.

### 11.4 Determinism

| Rule | Statement |
| --- | --- |
| TD1 | Time-dependent tests MUST use `tokio::time::pause()`. |
| TD2 | Tests MUST NOT assert on wall-clock durations. |
| TD3 | Every test creating a scheduler MUST use a per-test temp `data_dir` (the OS lock is per directory). |
| TD4 | `new_for_test` MAY relax only `validate_on_load` and `host_parallel_honesty`. |

---

## 12. MVP vs Deferred

| Capability | MVP | Deferred |
| --- | --- | --- |
| Serial ready-queue | ✅ | parallel dispatch (forbidden) |
| Verify compile/test over MCP | ✅ | other language backends |
| Gate with timeout + expiry | ✅ | multi-approver, delegation |
| Retry / backoff / escalation | ✅ | durable backoff timers |
| Same-generation checkpointing | ✅ | multi-writer leases |
| Ownership (process + OS lock) | ✅ | cross-host leases |
| Cancel / drain | ✅ | pause / resume mid-node |
| Budgets | ✅ | per-node USD ceilings |
| Cache hits | ❌ | RFC-0009 `CachedHit` application |
| Capability workers | stub | RFC-0013 |
| CLI surface | ❌ | RFC-0015 |
| `Aggregate` in templates | path only | later templates |
| Reachability-aware skipping | ❌ | later |

---

## 13. Acceptance Criteria

Each AC is a test name or a CI check.

| # | Acceptance criterion |
| --- | --- |
| 1 | `LinearScheduler::new` returns `Config` when `max_parallel_nodes != 1`. |
| 2 | `new` returns `Config` when `max_parallel_cargo != 1`. |
| 3 | `new` returns `Config` when `max_parallel_edits != 1`. |
| 4 | `new` returns `Config("data_dir must not be empty")` for an empty path. |
| 5 | `new` returns `Config("data_dir must be absolute: …")` for a relative path. |
| 6 | `new` returns `Config` when `validate_on_load == false`. |
| 7 | `new` returns `Config` when `host_parallel_honesty == false`. |
| 8 | `new_for_test` succeeds with `validate_on_load = false` **and** `host_parallel_honesty = false`, and still rejects `max_parallel_nodes = 2`. |
| 9 | A second `LinearScheduler` on the same `data_dir` fails with `Ownership`, and succeeds after the first is dropped. |
| 10 | `<data_dir>/scheduler.lock` exists after construction and is not deleted on drop. |
| 11 | Two concurrent `run` calls for one `DagId`: one proceeds, the other gets `AlreadyOwned`. |
| 12 | `run` for a DAG with no bound run row returns `RunBindingMissing`. |
| 13 | `run` with `validate_on_load` on an invalid blob returns `Invariant`, and no CAS is issued. |
| 14 | Happy path: 4-node chain reaches `Succeeded`; each node has `output_ref`; at most one node is `Running` at any observation point. |
| 15 | Two `Ready` nodes cause `Invariant`, not an arbitrary pick and not concurrent execution. |
| 16 | A `Skipped` Data predecessor never promotes its successor. |
| 17 | Promotion of multiple frontier nodes happens in exactly one CAS. |
| 18 | `input_ref` is rewritten only for nodes with ≥1 incoming Data edge. |
| 19 | A Sequence-only node keeps `FromPredecessors { preds: [] }` and produces no C5. |
| 20 | A root node keeps its `Goal` payload and produces no C5. |
| 21 | A `pending_pred`-labelled `output_ref` at dispatch time fails with `Invariant` (label-based detection, not content sniffing). |
| 22 | Recorded store asserts write order artifacts → CAS → events for C4, C7, C8, and C9. |
| 23 | Crash after CAS and before the event: on resume the RF2 filter finds no matching event and RF3 appends one `repaired = true` event exactly once. |
| 24 | Crash mid-`Running`: resume accounts the lost attempt and cannot exceed `max_attempts` across repeated crashes. |
| 25 | C8 leaves the node `Ready` and the DAG `Running` in a single CAS, after the `failure_ir` put, with the `running → failed` then `failed → ready` event pair appended afterwards. |
| 26 | Restart with a node `Ready` and one recorded failed attempt: the scheduler waits the full backoff (paused clock) then performs C3 for attempt 2. |
| 27 | Durable `NodeState::Failed` appears only when retries are exhausted or the failure is non-retryable. |
| 28 | Retry admission requires all of `Retryable`, `error_class ∈ retry_on`, and `attempts_started < max_attempts`; each rejection records a `Retry` decision with a reason. |
| 29 | `backoff_delay` respects `max_backoff` for `Fixed` and `Exponential`, saturates on overflow, and treats `factor < 1` as `1.0`. |
| 30 | Escalation applies iff `escalate_after = Some(n)`, `k > n`, and `escalate_to_tier = Some(_)`; the decision and C5 precede C3; `TaskNode.model_tier` is never written. |
| 31 | Gate first schedule: one CAS to `WaitingApproval` (node and DAG), then `NodeState`, then exactly one `ApprovalRequested`. |
| 32 | Gate allow: `WaitingApproval → Ready → Running → Succeeded` with `output_ref` set; `allow_once` behaves identically. |
| 33 | Gate allow after a crash at each intermediate state skips the already-durable steps and never double-transitions. |
| 34 | Gate deny: gate node `Cancelled`, every other non-terminal node `Skipped`, `DagState::Failed`, `failed_node` = the gate, failure `ErrorClass::Approval`. |
| 35 | Resume with `WaitingApproval` and no `ApprovalResolved`: the waiter is re-registered, no second `ApprovalRequested` is appended, and no `WaitingApproval` CAS is re-issued. |
| 36 | Resume with a durable `ApprovalResolved(expired)`: the DAG terminalizes and `expire_gate` is **not** called again. |
| 37 | Gate timeout calls `expire_gate` exactly once per `(run_id, gate_id)`; the §5.7.8 result table is covered for `Ok`, `InvalidPhase`, `NotFound`, and other errors. |
| 38 | The scheduler appends no `ModelCall` / `ToolCall` events, and a run's cost meter total equals the sum of worker/router-recorded usage (no double counting). |
| 39 | A closed gate receiver is classified by `RunControlState` per §5.7.9 (`Cancelling`, `Failed`, `WaitingApproval` cases all covered). |
| 40 | Control row terminal (`failed`) with a `WaitingApproval` DAG: `SessionService::resume` calls `reconcile_terminal_run` and the DAG becomes `Failed`; a second call is a no-op. |
| 41 | `reconcile_terminal_run` rejects a non-terminal `terminal` argument with `Config`, returns `Ok(())` for a live owned DAG, and maps `Conflict`. |
| 42 | `apply_start_outcome` applies a terminal `Failed`/`Cancelled` scheduler outcome even when the durable row is `waiting_approval` (amendment A5), while `replan_requested` / `cancelling` / `cancelled` still win. |
| 43 | `run` never returns `Ok` with `state ∈ {Pending, Running, WaitingApproval}` across the full test matrix. |
| 44 | Cancel of an owned run mid-node: the node future is dropped, C6 commits, `run` returns `Ok(Cancelled)`, `cancel` returns `Ok(())`. |
| 45 | Cancel return table: terminal ⇒ `Ok`, forced C6 ⇒ `Ok`, C6 `Conflict` ⇒ `Err(Conflict)`, store error ⇒ `Err(Store)`, grace elapsed with no C6 ⇒ `Err(Internal("cancel drain grace exceeded"))`. |
| 46 | Cancel of an unowned non-terminal DAG acquires ownership, writes C6, and releases; cancel of an unowned terminal DAG writes nothing. |
| 47 | `cancel` for an unknown DAG returns `DagNotFound`. |
| 48 | A cancel arriving before `run` starts is captured in `pending_cancels` and cancels the run at R5. |
| 49 | `OwnedGuard::drop` releases ownership and notifies waiters even when the run body panics. |
| 50 | `AlloyRuntime::drain` computes its deadline before awaiting `Scheduler::cancel` (amendment A1), verified by a slow-cancel double that must not consume the whole grace. |
| 51 | Host shutdown order test: `set_scheduler(NullScheduler)` succeeds in `Running`, fails in `Draining`, and drain still terminalizes live DAGs. |
| 52 | Exit `101` with no signal on `cargo_check` yields `VerifyOutcome { ok: false }` with parsed diagnostics — not an `AdapterError`. |
| 53 | A signal-killed cargo yields `ToolFailure(Transient { code: "cargo_signal", .. })` and `ErrorClass::Tool`, never `Compile`. |
| 54 | A non-`101` non-zero exit yields `ToolFailure(Permanent { code: "cargo_exit_{n}", .. })`; `is_error == false` with a missing or non-zero `exit_code` yields `Internal`. |
| 55 | A truncated stdout on exit `101` yields `Transient`, not a compile soft-fail. |
| 56 | A sandbox / disclosure / token denial yields `AdapterError::PermissionDenied`; `map_mcp_error` covers every `McpError` variant including `InvalidToken`, and the match has no catch-all arm. |
| 57 | CI grep: no file under `crates/alloy-runtime/src/scheduler/` or `src/adapters/` references `planner::`, and no file in `alloy-runtime` names `ToolHandle`, `McpError`, or `McpPlatform`. |
| 58 | `McpVerifyCompileAdapter` / `McpVerifyTestAdapter` are constructible from `alloy-runtime` alone (`Arc<dyn ToolCaller>` double), proving they do not live in `alloy-tools`. |
| 59 | `cargo_test` verification returns an empty `diagnostics` vector and a non-`None` `raw_artifact`. |
| 60 | `ok = true` sets `output_ref`; `ok = false` leaves `output_ref` untouched. |
| 61 | Effective USD is `min(policy, session, finite MaxUsd)`; non-finite `MaxUsd` is ignored with a `Budget` decision; `effective_usd <= 0.0` stops the run before any dispatch and before `check_budget`. |
| 62 | Meter rebuild uses `reaccumulate_cost_from_events` then a `with_mut` assignment (`*m = rebuilt`, not `add_*`); a resumed run's total equals the pre-crash total (no doubling). |
| 63 | `derive_dag_state` covers D1–D9: gate deny ⇒ `Failed` (D5), all-`Skipped` ⇒ `Failed` (D8), and `Skipped` never yields `Succeeded`. |
| 64 | A stalled DAG (unsatisfiable Data predecessor) is skipped and then, if still non-terminal, produces `Invariant`. |
| 65 | Node timeout produces `Timeout` + `Retryable`; run timeout produces `Timeout` + `NonRetryable` and admits no retry; a long gate wait does not consume the run budget. |
| 66 | `failed_node` selection prefers the lowest `Failed` node and otherwise picks a `Cancelled` gate carrying an `Approval` failure; `failure` is recovered from `failure_ref` on resume. |
| 67 | Observing `RunControlState::ReplanRequested` writes C10 `ReplanRequired` and returns `Ok(ReplanRequired)` so a subsequent `PlanService::replan` is not `DagBusy`. |
| 68 | A generation bump mid-run yields `Conflict`, and no further CAS or event append is issued for that DAG. |
| 69 | Cross-subsystem e2e (§11.3) passes against real SQLite and a real sandboxed `cargo_check`. |
| 70 | `runtime_to_run` compiles with `SchedError` `#[non_exhaustive]` and maps every §3.2 row plus a catch-all. |

---

## 14. Definition of Done

| # | Requirement |
| --- | --- |
| 1 | Every AC in §13 is implemented as a passing test or CI check. |
| 2 | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` are green. |
| 3 | `LinearScheduler` is the production default; `NullScheduler` remains for pre-wiring, shutdown parking, and tests. |
| 4 | Amendments A1–A7 have landed with their own tests. |
| 5 | `#![forbid(unsafe_code)]` holds; no new external dependency. |
| 6 | Public items in §3.17 are re-exported and documented with `#[must_use]` where applicable. |
| 7 | No `.env` writes; `example.env` comments updated. |
| 8 | RFC-0009 Appendix C obligations are each traceable to a section here (Appendix H mapping). |
| 9 | Doc comments on every public item; `cargo doc` warning-free. |
| 10 | The e2e test is runnable locally with the documented sandbox prerequisites and is skipped (not failed) when unavailable. |

---

## 15. Open Questions

| # | Question | Current answer | Owner |
| --- | --- | --- | --- |
| Q1 | Should backoff elapsed time become durable? | No for MVP: rule B4's full re-wait is deterministic and never under-waits. | RFC-0013 revisit |
| Q2 | Should a `Skipped`-only DAG be `Failed` or a new state? | `Failed` (D8). A `PartiallySucceeded` state would change the merged `DagState` enum. | RFC-0015 UX |
| Q3 | Should `run_timeout` be durable across resumes? | No: a resumed run starts a fresh budget (T6). Durable run deadlines need a durable clock. | Later |
| Q4 | Should `allow_once` differ from `allow` in the scheduler? | No; scope semantics are control-plane. | RFC-0015 |
| Q5 | Should the OS lock be a real lease with a heartbeat? | No; advisory same-host locking is enough while execution is single-process. | Later |
| Q6 | Should skipping be reachability-aware? | No for MVP (SK3); every non-terminal node is skipped. | Later |
| Q7 | Should verify adapters expose `package` / `features`? | No (V4); adding knobs invites non-deterministic verification. | RFC-0015 |

---

## 16. Estimated Implementation Effort

| Phase | Content | Effort |
| --- | --- | --- |
| P1 | `SchedConfig` / `LinearSchedulerDeps` / `new` validation / ownership + OS lock / metrics | 0.75 pd |
| P2 | Pure helpers: `ready_nodes`, `promotable_nodes`, `backoff_delay`, `derive_dag_state` + table tests | 0.5 pd |
| P3 | Checkpoint module: C1–C10, write order, `StoreError` mapping, RF1–RF5 repair | 1.0 pd |
| P4 | Serial loop: selection, envelopes + `input_ref` rewrite, dispatch, success path | 1.0 pd |
| P5 | Retry / backoff / escalation + decisions | 0.75 pd |
| P6 | `ToolCaller` seam, `map_mcp_error`, verify adapters, diagnostics parser + fingerprint | 1.25 pd |
| P7 | Gate: scheduler orchestration, adapter, `expire_gate`, resume, reconcile | 1.25 pd |
| P8 | Cancel / drain / amendments A1–A7 | 0.75 pd |
| P9 | Budgets, meter rebuild, observability | 0.5 pd |
| P10 | Cross-subsystem e2e + CI greps | 0.75 pd |
| **Total** | | **~8.5 pd raw → 5–8 pd with overlap** |

Critical path: P3 → P4 → P7. P6 can proceed in parallel with P4 once the `ToolCaller` trait lands.

---

## Appendix A — Checkpoint catalog (normative)

Every row is a single `put_if_generation(&dag, Some(dag.generation))`. `dag.generation` is never changed.

| Id | Trigger | Node transitions | `DagState` after | Events appended after the CAS |
| --- | --- | --- | --- | --- |
| **C1** | `run` start / adopt | none | `Pending → Running` (or `WaitingApproval → Running` on the allow path) | none |
| **C2** | frontier promotion | `Pending → Ready` for every promotable node | `Running` | one `NodeState` per node |
| **C3** | dispatch attempt `k` | `Ready → Running` | `Running` | `NodeState { to: running, attempt: k }` |
| **C4** | node success | `Running → Succeeded`, `output_ref = Some(id)` | `Running` | `NodeState { to: succeeded, attempt: k }` |
| **C5** | input rewrite | `input_ref = Some(new)` (state unchanged, `Ready`) | `Running` | none (artifact provenance only) |
| **C6** | cancel | in-flight → `Cancelled`; other non-terminal → `Skipped` | `Cancelled` | one `NodeState` per marked node |
| **C7** | terminal | failing node → `Failed`; remaining non-terminal → `Skipped` | `Failed` \| `Succeeded` | `NodeState` per marked node, `failure_ref` on the failed one |
| **C8** | retry admitted | `Running → Ready` | `Running` | `NodeState { to: failed, attempt: k, failure_ref }` then `NodeState { to: ready, next_attempt: k+1 }` |
| **C9a** | gate first schedule | `Ready → WaitingApproval` | `WaitingApproval` | `NodeState`, then `ApprovalRequested` |
| **C9b** | gate allow | `WaitingApproval → Ready` | `Running` | `NodeState { to: ready, decision }` |
| **C9c** | gate deny / expiry | gate → `Cancelled`; others → `Skipped` | `Failed` | `NodeState` per node with `decision` + `failure_ref` |
| **C10** | replan observed | in-flight `Running` → `Cancelled` | `ReplanRequired` | `NodeState` for the cancelled node |

| Rule | Statement |
| --- | --- |
| CA1 | No checkpoint may change `generation`, `nodes` membership, `edges`, `kind`, `capability`, `retry`, `budget`, `model_tier`, `approval`, or `timeout_ms`. Only `state`, `input_ref`, `output_ref`, and the DAG's `state` are writable. |
| CA2 | Every checkpoint MUST be reachable from the ordered rule tables in §5. There are no ad-hoc writes. |
| CA3 | `Conflict` from any checkpoint ends the run per §5.8.4. |

## Appendix B — Node state machine reconciliation

Legal transitions (superset of RFC-0009 §5.3.2, with the RFC-0010 owner named):

| From | To | Owner / checkpoint |
| --- | --- | --- |
| `Pending` | `Ready` | C2 |
| `Pending` | `Skipped` | C6 / C7 |
| `Pending` | `Cancelled` | C6 (cancel before start) |
| `Ready` | `Running` | C3 |
| `Ready` | `WaitingApproval` | C9a (gate only) |
| `Ready` | `Skipped` | C6 / C7 |
| `Ready` | `Cancelled` | C6 |
| `Ready` | `CachedHit` | **deferred** — MUST NOT be produced |
| `Running` | `Succeeded` | C4 |
| `Running` | `Ready` | C8 (retry admitted) |
| `Running` | `Failed` | C7 (exhausted / non-retryable) |
| `Running` | `Cancelled` | C6 |
| `WaitingApproval` | `Ready` | C9b (allow) |
| `WaitingApproval` | `Cancelled` | C9c (deny / expiry) or C6 (cancel) |
| `Succeeded` / `Failed` / `Cancelled` / `Skipped` / `CachedHit` | — | terminal |

| Reconciliation note |
| --- |
| RFC-0009's diagram shows `WaitingApproval → Failed: timeout (0010)`. This RFC refines it: the **node** becomes `Cancelled` and the **DAG** becomes `Failed` (D5). One shape for deny and expiry keeps `failed_node` selection (FN2) uniform. The `ErrorClass::Approval` failure record carries the distinction (`"approval denied"` vs `"approval timeout"`). |
| RFC-0009's `Failed → Ready: retry admitted` edge is preserved as a **logical** transition: the durable blob goes `Running → Ready` in one CAS (C8), and the event pair records the `failed` waypoint. Durable `Failed` therefore means "exhausted", which is what §1.5 rule 8 requires. |

## Appendix C — Gate decision matrix

| Durable `ApprovalResolved` | Node state | Control row | Scheduler action | `run` result |
| --- | --- | --- | --- | --- |
| none | `Ready` | `running` | C9a, register, wait | depends |
| none | `WaitingApproval` | `running` \| `waiting_approval` | re-register only (GR1–GR3) | depends |
| none | `WaitingApproval` | `failed` | terminalize as expiry | `Ok(Failed)` |
| none | `WaitingApproval` | `cancelled` \| `cancelling` | cancel path | `Ok(Cancelled)` |
| `allow` \| `allow_once` | `WaitingApproval` | any non-terminal | allow path from `WaitingApproval` | `Ok(Succeeded)` if the rest succeeds |
| `allow` | `Ready` | any non-terminal | allow path from `Ready` | as above |
| `allow` | `Running` | any non-terminal | allow path from `Running` | as above |
| `allow` | `Succeeded` | any | continue the loop | as above |
| `deny` | `WaitingApproval` | any | deny path (§5.7.4) | `Ok(Failed)` |
| `deny` | `Cancelled` | any | already terminal; derive | `Ok(Failed)` |
| `expired` | `WaitingApproval` | any | expiry path, no `expire_gate` call | `Ok(Failed)` |
| unknown string | any | any | fail closed | `Err(Invariant)` |

**Gate wait accounting.** `gate_wait_total += wait_elapsed` on every exit from the wait (allow, deny, expiry, cancel), so T1 holds regardless of outcome.

## Appendix D — Verify tool contract

### D.1 `cargo_check` call

```json
{
  "name": "cargo_check",
  "arguments": { "workspace_root": "/abs/workspace", "message_format": "json" },
  "call_id": "<node_id>:<attempt>",
  "session": "<session_id>", "run": "<run_id>", "node": "<node_id>"
}
```

Derived argv (RFC-0006): `["cargo", "check", "--message-format", "json"]` under `ExecClass::Check`, disclosure tag `sel.compiler`.

### D.2 `cargo_test` call

```json
{ "name": "cargo_test", "arguments": { "workspace_root": "/abs/workspace" } }
```

Derived argv: `["cargo", "test", "--", "--nocapture"]` under `ExecClass::Test`, disclosure tag `sel.test`.

### D.3 Result content (RFC-0006 cargo envelope)

| Key | Type | Use here |
| --- | --- | --- |
| `exit_code` | `Option<i32>` | authoritative classification (§5.13.2) |
| `signal` | `Option<i32>` | signal ⇒ `Transient`, never `Compile` |
| `stdout_utf8` | `String` | rustc NDJSON for `cargo_check`; opaque for `cargo_test` |
| `stderr_utf8` | `String` | raw log only |
| `stdout_truncated` | `bool` | `101` + truncated ⇒ `Transient` |
| `stderr_truncated` | `bool` | logged |
| `duration_ms` | `u64` | span field |
| `backend` | `String` | span field |
| `policy_digest` | `String` | span field |

### D.4 Exit-code decision tree

```text
Err(ToolCallerError)            -> §3.5 map
Ok, !is_error, exit == 0        -> ok: true
Ok, !is_error, exit missing     -> Internal
Ok, !is_error, exit != 0        -> Internal
Ok, is_error, signal.is_some()  -> Transient("cargo_signal")
Ok, is_error, exit == 101, truncated stdout -> Transient("cargo_output_truncated")
Ok, is_error, exit == 101       -> ok: false  (Compile | Test, NonRetryable)
Ok, is_error, exit missing      -> Transient("cargo_no_exit")
Ok, is_error, exit == n         -> Permanent("cargo_exit_{n}")
```

## Appendix E — Retry worked examples

Node: `max_attempts = 3`, `backoff = Exponential { base_ms: 1000, factor: 2.0 }`, `retry_on = [Compile, Model]`, `escalate_after = Some(1)`, `escalate_to_tier = Some(Premium)`, `max_backoff = 60s`.

| Attempt | Tier | Result | Admission | Backoff before next |
| --- | --- | --- | --- | --- |
| 1 | `Standard` (`k = 1`, not `> 1`) | `Model`, `Retryable` | admitted (`1 < 3`) | `1000ms` |
| 2 | `Premium` (`k = 2 > 1`) | `Model`, `Retryable` | admitted (`2 < 3`) | `2000ms` |
| 3 | `Premium` (monotone, ES6) | `Model`, `Retryable` | rejected (`3 == max_attempts`) | — ⇒ durable `Failed` |

Cap example: `Exponential { base_ms: 1000, factor: 10.0 }` with `max_backoff = 60s` yields `1s, 10s, 60s, 60s, …`.

Non-admission examples:

| Failure | Reason rejected |
| --- | --- |
| `Compile`, `NonRetryable` (verify soft-fail) | A1 — a compile failure is repaired by the next DAG node, not by re-running cargo |
| `Tool` `Transient` with `retry_on = [Compile]` | A2 |
| `Approval` | A1 (always `NonRetryable`) |
| `Timeout` (run-level) | T5 |
| any, with the budget exhausted | A5 |
| any, with `remaining_run < backoff + slice` | A6 |

## Appendix F — Run binding resolution

```text
resolve_run(dag_id):
  dag       = dags.get(dag_id)?                     # else DagNotFound
  session   = dag.session_id
  rows      = sessions.list_runs(session)?          # created_at ascending
  matches   = [ r for r in rows
                if serde_json::from_value::<RunGoalRecord>(r.goal_json)
                       .ok()
                       .is_some_and(|g| g.dag_id == dag_id) ]
  match matches:
    []                      -> Err(RunBindingMissing(dag_id))
    [one]                   -> Ok(one)
    many                    -> Ok(last non-terminal by created_at,
                                   else last by created_at)
```

| Rule | Statement |
| --- | --- |
| RB1 | `goal_json` that fails to deserialize MUST be skipped, not fatal: RFC-0002 treats it as opaque. |
| RB2 | Multiple runs bound to one DAG is legal (a re-dispatch after a crash). The newest non-terminal row wins. |
| RB3 | The resolved `RunId` is used for event attribution, gate calls, the cost meter, and budget checks. It MUST be resolved once at R3 and reused. |
| RB4 | The scheduler MUST NOT create or mutate run rows. |

## Appendix G — Artifact labels and envelopes

### G.1 Label table (extends RFC-0009 §5.3.0)

| `alloy.envelope` | Writer | `ArtifactKind` | Body | Referenced by |
| --- | --- | --- | --- | --- |
| `node_input` | Planner (plan time), Scheduler (C5 rewrite) | `Json` | `NodeInputEnvelope` | `TaskNode.input_ref` |
| `pending_pred` | Planner | `Json` | `{"schema_version":1,"pending":true}` | placeholder `PredecessorOutput.output_ref` |
| `dag_snapshot` | Planner | `Json` | `TaskDag` snapshot | `PlanProduced` payload |
| `node_output` | **Scheduler** | `Json` | `NodeOutputEnvelope` | `TaskNode.output_ref` |
| `verify_raw` | **Scheduler adapters** | `Log` | cargo envelope (§D.3) | `VerifyOutcome.raw_artifact`, `NodeState.raw_ref` |
| `failure_ir` | **Scheduler** | `Json` | `FailureIr` | `NodeState.failure_ref`, `DagOutcome.failure` |

Every artifact MUST also carry `alloy.dag_id`, and SHOULD carry `alloy.node_id` and `alloy.generation`.

### G.2 Rewritten input envelope (Data node)

```json
{
  "schema_version": 1,
  "dag_id": "…", "node_id": "…", "kind": "edit", "generation": 1,
  "payload": { "from_predecessors": { "preds": [
    { "node_id": "…", "kind": "analyze", "output_ref": "<real analyze output>" }
  ] } }
}
```

### G.3 Output envelope (verify success)

```json
{
  "schema_version": 1,
  "dag_id": "…", "node_id": "…", "kind": "verify_compile", "generation": 1,
  "attempt": 2,
  "payload": { "ok": true, "diagnostics": [], "raw_artifact": "…" }
}
```

### G.4 `failure_ir` (verify soft-fail)

```json
{
  "node": "…",
  "error_class": "compile",
  "retry": "non_retryable",
  "diagnostics": [ { "id": "…", "code": "E0308", "level": "error",
                     "message": "mismatched types", "spans": [ … ],
                     "children": [], "package": "demo",
                     "fingerprint": "…", "raw_json": { … } } ],
  "notes": "cargo check failed"
}
```

## Appendix H — Event payloads and RFC-0009 obligation mapping

### H.1 `NodeState` payload

| Key | Type | Required |
| --- | --- | --- |
| `node_id` | string | yes |
| `from` | node state (snake_case) | yes |
| `to` | node state (snake_case) | yes |
| `generation` | u64 | yes |
| `attempt` | u32 | when a node attempt is involved |
| `failure_ref` | artifact id | on failure transitions |
| `error_class` | snake_case | on failure transitions |
| `retry` | `retryable` \| `non_retryable` | on failure transitions |
| `next_attempt` | u32 | on `→ ready` after a failure |
| `backoff_ms` | u64 | on `→ ready` after a failure |
| `decision` | `allow` \| `allow_once` \| `deny` \| `expired` | on gate transitions |
| `raw_ref` | artifact id | on verify transitions |
| `repaired` | bool | on RF3 repair appends |

### H.2 `ApprovalRequested` payload

| Key | Type |
| --- | --- |
| `gate_id` | string |
| `node_id` | string |
| `reason` | string (from `ApprovalSpec.reason`) |
| `timeout_ms` | u64 |

### H.3 RFC-0009 Appendix C obligations → sections here

| RFC-0009 obligation | Discharged in |
| --- | --- |
| `put_if_generation(.., Some(generation))` for checkpoints | §5.8.1 W6, Appendix A |
| Stop on `Conflict` after replan | §5.8.4 |
| Reclaim a foreign `Running` DAG before accepting work | §4.5 L6, §5.3.2, §6.3 |
| Write a non-`Running` state on `ReplanRequested` | §5.21 |
| Rewrite the final `input_ref` per §5.3.0 | §5.5 |
| Enforce `output_ref` invariants on `Succeeded` / `CachedHit` | §5.9 OU2 / OU5 |
| Enforce the `GateHuman` timeout from `timeout_ms` | §5.7.8, §5.19 |
| Apply Data vs Sequence satisfaction per §5.3.1 | §5.4 |
| Ignore `model_tier` / budgets on adapter nodes | §5.6, ES5 |
| Specify `FromPredecessors` digest framing before non-root cache | deferred with cache (§12) |
| Reject non-finite `Goal` constraint values before cache hits | §5.16.1 BG1 (and cache stays off) |
| Single scheduler writer per DAG (ownership) | §4.5 |

## Appendix I — Internal state reference

```rust
struct RunCtx {
    dag: TaskDag,                       // in-memory copy the CAS advances
    dag_id: DagId,
    run_id: RunId,
    session_id: SessionId,
    workspace_root: PathBuf,
    profile: ProfileId,
    effective_policy: BudgetPolicy,      // §5.16.1 ceilings, parallel_* = 1
    effective_usd_degenerate: bool,      // BG3 short-circuit
    meter: SharedCostMeter,
    attempts: HashMap<NodeId, u32>,      // §5.3.1 attempts_started
    escalated: HashSet<NodeId>,          // ES6 monotonicity
    expired_gates: HashSet<(RunId, GateId)>, // GT3 idempotency
    run_started: Instant,
    gate_wait_total: Duration,           // T1
    flags: DeriveFlags,                  // §5.17 inputs
    owned: Arc<OwnedDag>,
}
```

| Rule | Statement |
| --- | --- |
| IS1 | `RunCtx` is stack-local to one `run` invocation and MUST NOT be shared between runs. |
| IS2 | `attempts` MUST be rebuilt from events at R11, never carried across `run` invocations. |
| IS3 | `dag` MUST be replaced by the blob each successful CAS returns; a failed CAS MUST NOT leave a partially mutated copy in play. |
| IS4 | `flags` MUST be updated at the moment the corresponding condition is observed (cancel requested, replan requested, approval failure recorded). |

## Appendix J — Permission and grant wiring

| Step | Owner | Detail |
| --- | --- | --- |
| 1 | Host assembly | Build a `ToolHandle` with selectors covering `sel.compiler` (compile) and `sel.test` (test). Missing disclosure ⇒ `McpError::PermissionDenied(NotDisclosed)` ⇒ `PermissionDenied` (never a compile failure). |
| 2 | Host assembly | Wrap it: `Arc::new(ToolHandleToolCaller::new(handle))`. |
| 3 | Host assembly | Provide a `VerifyPermissions` implementation reading the profile catalog. |
| 4 | Adapter | `perms.token_for(&ctx.meta, VerifyClass::Compile)` before each call. |
| 5 | RFC-0006 | `match_exec_grant` checks the derived argv against `Grant::Exec(ExecAllow { binary: "cargo", args_glob })`. |

| Required grant shape | Class |
| --- | --- |
| `ExecAllow { binary: "cargo", args_glob: ["check*"] }` (or a glob accepting `check --message-format json`) | `Compile` |
| `ExecAllow { binary: "cargo", args_glob: ["test*"] }` (accepting `test -- --nocapture`) | `Test` |

| Rule | Statement |
| --- | --- |
| PJ1 | Adapters MUST NOT synthesize grants or widen globs. |
| PJ2 | `PermissionToken.run_id` MUST equal the executing run. |
| PJ3 | A missing profile grant MUST be `AdapterError::PermissionDenied`, not `Internal` (§3.7). |
| PJ4 | Tokens MUST NOT be logged, spanned, or written into artifacts. |
| PJ5 | The scheduler MUST NOT hold or cache tokens; they are minted per call. |

## Appendix K — End-to-end trace: `repair_local_diagnostic`

Nodes `analyze → edit → verify → gate` with both Data and Sequence edges on each hop. Timeline (checkpoints in bold):

| # | Step |
| --- | --- |
| 1 | `RunController::start` → `RuntimeHandle::run_dag` → `LinearScheduler::run` |
| 2 | R1 load, R2 validate, R3 bind run, R4 own, R6 session, R7 ceilings, R8 meter rebuild (no events yet) |
| 3 | **C1** `DagState::Pending → Running` |
| 4 | L6 **C2** `analyze: Pending → Ready` (`edit` still has an unsatisfied Data predecessor) |
| 5 | L10 root node — no C5 (Goal payload retained) |
| 6 | L12 **C3** `analyze: Ready → Running` (attempt 1); dispatch to the capability executor |
| 7 | Worker returns `Succeeded { payload }`; put `node_output`; **C4** `Succeeded` + `output_ref` |
| 8 | **C2** `edit: Pending → Ready` (Data + Sequence satisfied) |
| 9 | **C5** `edit.input_ref` rewritten with `analyze`'s real `output_ref` (≥1 Data edge) |
| 10 | **C3** `edit: Ready → Running`; worker applies the patch; **C4** success |
| 11 | **C2** `verify: Pending → Ready`; **C5** rewrite; **C3** dispatch |
| 12 | `cargo_check` exits `101` ⇒ `VerifyOutcome { ok: false, diagnostics }`; put `verify_raw`; put `failure_ir` |
| 13 | Admission: `Compile` is `NonRetryable` ⇒ no retry. **C7** `verify: Failed`, `gate: Skipped`, `DagState::Failed` |
| 14 | `run` returns `Ok(DagOutcome { state: Failed, failed_node: Some(verify), failure })`; RFC-0003 writes the terminal row and `RunCompleted` |
| 15 | The operator (or a future auto-replan) requests a replan; RFC-0009 bumps the generation with a fresh chain |
| 16 | Second run: `analyze` and `edit` consume the diagnostics through the predecessor envelope; `verify` exits `0` ⇒ **C4** with `output_ref` |
| 17 | **C2** `gate: Pending → Ready`; **C9a** `gate: Ready → WaitingApproval`, `DagState::WaitingApproval`; `ApprovalRequested`; waiter registered |
| 18 | `gate_wait_total` accrues while the human deliberates; the run budget is not charged (T1) |
| 19 | `RunController::approve(Allow)` → `ApprovalResolved` → waiter fires |
| 20 | **C9b** `gate: WaitingApproval → Ready`, `DagState::Running`; **C3** `Running`; gate fold; **C4** `Succeeded` + `output_ref` |
| 21 | L8 no `Ready` nodes; D7 ⇒ `Succeeded`; **C7** `DagState::Succeeded` |
| 22 | `run` returns `Ok(DagOutcome { state: Succeeded, failed_node: None, failure: None })`; `OwnedGuard` drops, ownership released, `completed` notified |

Crash injection points and their recoveries are the §6.3 matrix; every step above is idempotent under RF1–RF5.
