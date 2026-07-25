# RFC-0003: Session Manager & RunController

| Field | Value |
| --- | --- |
| **Status** | Implemented |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md) (merged) · [RFC-0002](./RFC-0002-storage-artifacts-session-events.md) (merged) |
| **Effort** | 3–5 person-days |
| **Related RFCs** | [0004](./RFC-0004-observability-cost-metering.md) budget metering / decision writers · [0009](./RFC-0009-task-dag-templates-planner.md) planner / DAG store · [0010](./RFC-0010-scheduler-runtime-adapters.md) scheduler execution · [0015](./RFC-0015-cli-profiles-config.md) CLI / TTY approval UX |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | Draft outline of this filename (expanded to implementation grade) |

**Mental model (V2 §5.2 / ADR F-22):** Session owns lifecycle, events, and budgets only. Run control is `RunController`. Session MUST NOT execute tools or mutate DAG topology. Explicit state — if it is not in the session event log (or durable session/run rows), it did not happen.

**Authority order (highest → lowest):** current `main` source → RFC-0002 → RFC-0001 → Architecture V2. Never modify an existing public API solely to match an older V2 sketch.

---

## 1. Overview

### Purpose

Ship the MVP **control plane** inside `alloy-runtime`:

1. Concrete **`SessionService`** over RFC-0002 `SessionRows` + `EventStore` / `RuntimeHandle::append_session`.
2. Concrete **`RunController`** that records run-control transitions, emits required runtime/session events, and integrates with the existing `Scheduler` abstraction (MVP: `NullScheduler` → defined unavailable errors).
3. **Budget attachment** on session create and **budget exhaustion hooks** (signaling only; metering → RFC-0004).
4. **Resume** after process restart from SQLite session rows + gapless event sequences.
5. **Host wiring** after `install_sqlite_event_sink` with no sixth crate and no new OS service.

Day-1 developer deliverable: with runtime `Running` and SQLite installed, `create` → `submit_goal` → `events` round-trips durable rows and Appendix A envelopes; `resume` after reopen returns the same `Session`; `start` against `NullScheduler` returns `RunError::SchedulerUnavailable` with defined side effects; `approve` / `cancel` / `request_replan` obey the contracts below — without writing the user’s `.env`.

### Problem

RFC-0001 published `SessionService` / `RunController` trait signatures, `Session`, `ReplanReason`, `SessionError` / `RunError`, and required that `RunAccepted` / `RunFinished` be emitted by Session/RunController — not by `AlloyRuntime::run`. RFC-0002 shipped durable `SessionRows`, `RunRow`, `EventStore`, and `store_to_session`. No orchestration impl exists. Without this RFC, CLI (0015), scheduler (0010), and planner (0009) have no session/run control surface to drive.

### Scope

| In scope | Detail |
| --- | --- |
| `SessionService` MVP impl | `create`, `resume`, `submit_goal`, `events` with exclusive cursor + page limit |
| `RunController` MVP impl | `start`, `cancel`, `approve`, `request_replan` |
| Persistence integration | `SessionRows` + `EventStore` via `AlloyStorage`; append via `RuntimeHandle` |
| Profiles | Validate MVP ids: `default` \| `autonomous` \| `readonly` |
| Budgets | Persist `BudgetPolicy` on create; exhaustion **hooks** + `BudgetWarning` events |
| Scheduler contract | Call existing `Scheduler` / host DAG forwarder; `NullScheduler` → `SchedulerUnavailable` |
| Run control state | Pin `RunRow.state` **string vocabulary** for control-plane rows (not DAG state machine) |
| Additive errors | Extend `RunError` with `SchedulerUnavailable`, `AlreadyStarted`, `UnknownGate` |
| Additive host seam | `RuntimeHandle::run_dag` / `cancel_dag` sharing `AlloyRuntime::run` semantics |
| Observability | `tracing` + in-process counters; no OTLP |
| Tests | Unit + restart integration + concurrency |

### Non-goals

- Scheduler execution loop, retries, ready-queue → **RFC-0010**.
- Planner / DAG template load / DAG CRUD / topology mutation → **RFC-0009**.
- Cost accounting, decision writers, OTLP → **RFC-0004**.
- TTY approval UX / `alloy run` CLI → **RFC-0015**.
- MCP, ModelRouter, EditEngine, alloyd, ACP (V2 deferred / other RFCs).
- Redesigning V2, RFC-0001, or RFC-0002; new crates; new services; parallel traits.
- Changing RFC-0001 `SessionService` / `RunController` method signatures (except additive `RunError` variants and additive inherent/host helpers documented here).

---

## 2. Architecture Integration

### Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §5.2 Session Manager | Lifecycle, events, budgets, resume — **no** tool execution; **no** DAG topology mutation |
| §5.2 RunController | `start` / `cancel` / `approve` / `request_replan` — owns run control API; **not** event storage |
| §5.5 Control APIs | Distinct traits; Session MAY facade approve/cancel for CLI convenience |
| ADR F-22 | RunController separate from Session |
| §5.6 Budget exhaustion | Stop non-essential; summarize; ask user — **hooks + events** here; metering in 0004 |
| §3.3 Explicit state | Session/run truth = durable rows + session event log |

**V2 sketch superseded by RFC-0001 code:** `SessionService::events(id, after: EventSeq)` without `Option` / `limit`. **Normative:** RFC-0001 / `session/traits.rs` — `after: Option<EventSeq>`, `limit: usize`, `MAX_EVENTS_PAGE`, `clamp_events_page_limit`.

### Relationship to RFC-0001

RFC-0001 is **authoritative** for:

- `SessionService` / `RunController` trait method signatures
- `Session`, `ReplanReason`, `CreateSession`, `Goal`, `BudgetPolicy`, `Approval`, IDs
- `SessionEvent` / `NewSessionEvent` / `SessionEventType` / `RuntimeEvent`
- `RuntimeHandle::emit` / `append_session` / `handoff_event_sink` / `set_event_sink`
- `AlloyRuntime::run` = thin `Scheduler::run` forwarder (**must not** emit `RunAccepted` / `RunFinished`)
- `NullScheduler`, `Scheduler`, `DagOutcome`, `DagState`
- Phase model; single-flight DAG admit on the host forwarder

This RFC **implements** behavior behind those traits and **adds** only the seams required to wire RunController to the existing forwarder without duplicating admit logic.

### Relationship to RFC-0002

RFC-0002 is **authoritative** for:

- `AlloyStorage`, `EventStore`, `SessionRows`, `RunRow`, `Sqlite*` backends
- `install_sqlite_event_sink`, `store_to_session`, `StoreError`
- Per-session gapless `EventSeq`, exclusive cursor pagination, handoff rules
- Schema for `sessions` / `runs` / `session_events` (no redesign)

This RFC **owns when** to call `SessionRows` / append events; it does **not** fork storage.

### Already implemented | Added by RFC-0003 | Deferred

| Item | Owner |
| --- | --- |
| `SessionService` / `RunController` trait signatures | **0001** |
| `Session`, `ReplanReason`, `MAX_EVENTS_PAGE`, `clamp_events_page_limit` | **0001** |
| `SessionError` `{NotFound, Invalid, Internal}` | **0001** (retained) |
| `RunError` `{NotFound, InvalidPhase, Internal}` | **0001** (retained; **extended** here) |
| `Approval`, `CreateSession`, `Goal`, `BudgetPolicy`, `ProfileId` | **0001** |
| `EventSink` / envelopes / `RuntimeHandle` append+emit | **0001** |
| `Scheduler` / `NullScheduler` / `AlloyRuntime::run` | **0001** |
| `AlloyStorage` / `EventStore` / `SessionRows` / `RunRow` / installer | **0002** |
| `store_to_session` | **0002** |
| Concrete `SessionService` + `RunController` impls | **0003** |
| `RunRow.state` control-plane vocabulary | **0003** |
| `RunGoalRecord` envelope in `goal_json` | **0003** |
| Budget exhaustion hooks + `BudgetWarning` emission | **0003** |
| Additive `RunError` variants + `RuntimeHandle::{run_dag,cancel_dag}` | **0003** |
| Gate waiter registry (in-process; for 0010) | **0003** |
| Cost metering / decision writers / OTLP | **0004** |
| Planner / DAG load / topology mutation | **0009** |
| Real scheduler execution / Verify* / GateHuman adapters | **0010** |
| CLI / TTY approval UX | **0015** |
| alloyd / ACP | V2 deferred |

**RFC-0004 call edge (same crate):** metering in 0004 MUST invoke `SessionPlane::signal_budget_warning` for exhaustion signaling. This is an in-crate call, not a workspace crate dependency; the RFC index crate/dependency table for 0004 (depends on 0001, 0002) remains correct.

### Dependency boundaries

```text
alloy-cli ──► alloy-runtime
                 ├── session (0003 impl) ──► storage SessionRows/EventStore (0002)
                 │                      ──► RuntimeHandle emit/append (0001)
                 │                      ──► Scheduler via RuntimeHandle::run_dag (0003 additive)
                 ├── runtime / scheduler (0001)
                 └── storage (0002)
```

No new workspace crate. No new OS service. Session MUST NOT depend on planner/DAG store modules beyond reading/minting `DagId` values.

---

## 3. Public Rust API

All items live in `alloy-runtime` (edition 2021, Tokio 1.x, `async_trait` on public traits through M1 — same pins as RFC-0001/0002).

**Do not break** existing public signatures. Extend via additive enum variants, additive `RuntimeHandle` methods, new types in `session::`, and crate-root re-exports.

### 3.1 Existing traits (normative — do not change signatures)

```rust
// alloy-runtime/src/session/traits.rs  — AUTHORITATIVE (RFC-0001)

pub const MAX_EVENTS_PAGE: usize = 1_000;

#[must_use]
pub fn clamp_events_page_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_EVENTS_PAGE)
}

#[async_trait]
pub trait SessionService: Send + Sync {
    async fn create(&self, req: CreateSession) -> Result<SessionId, SessionError>;
    async fn resume(&self, id: SessionId) -> Result<Session, SessionError>;
    async fn submit_goal(&self, id: SessionId, goal: Goal) -> Result<RunId, SessionError>;
    /// Exclusive cursor + page limit.
    /// - `after: None` — from first event (`EventSeq(0)`).
    /// - `after: Some(seq)` — events with `seq > after`.
    /// - `limit` — clamp via `clamp_events_page_limit`.
    async fn events(
        &self,
        id: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError>;
}

#[async_trait]
pub trait RunController: Send + Sync {
    async fn start(&self, run: RunId) -> Result<(), RunError>;
    async fn cancel(&self, run: RunId) -> Result<(), RunError>;
    async fn approve(&self, run: RunId, gate: GateId, decision: Approval) -> Result<(), RunError>;
    async fn request_replan(&self, run: RunId, reason: ReplanReason) -> Result<(), RunError>;
}
```

`Approval` remains `alloy_runtime::adapters::Approval` (`Allow` \| `Deny` \| `AllowOnce`), re-exported at crate root.

### 3.2 Error taxonomy (additive `RunError` only)

```rust
// alloy-runtime/src/error.rs

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("not found: {0}")]
    NotFound(SessionId),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    #[error("not found: {0}")]
    NotFound(RunId),
    #[error("invalid phase: {0}")]
    InvalidPhase(String),
    #[error("internal: {0}")]
    Internal(String),

    // --- RFC-0003 additive ---
    /// No executable scheduler / NullScheduler / SchedError::Unavailable.
    #[error("scheduler unavailable")]
    SchedulerUnavailable,
    /// `start` called while an in-process live execution is already registered.
    #[error("already started: {0}")]
    AlreadyStarted(RunId),
    /// `approve` for a gate with no pending waiter.
    #[error("unknown gate: {0}")]
    UnknownGate(GateId),
}
```

**Rules:**

- MUST NOT remove or redefine existing variants.
- `#[non_exhaustive]` on `RunError` is REQUIRED (pre-1.0 additive safety for exhaustive matches).
- **Public API impact:** adding `#[non_exhaustive]` is an intentional source break for downstream `match` expressions that exhaust every variant. Downstream crates MUST add a wildcard arm (`_ => …`) or equivalent; existing variants and their meanings are preserved. This is required so later additive variants do not break dependents before 1.0.
- `store_to_session` (0002) remains unchanged and MUST NOT invent `SessionError::NotFound` from store misses — SessionService maps missing **session rows** itself (see §7 / §11).
- Full `RuntimeError` / `StoreError` mapping is normative in §7 and §11.

### 3.3 Run control state vocabulary (`RunRow.state`)

RFC-0002 stores `RunRow.state: String`. This RFC pins the **control-plane** vocabulary written by Session/RunController.

**This is not the DAG state machine.** `DagState` / node states remain owned by RFC-0009 / RFC-0010. `RunRow.state` is a durable control marker for resume and API guards only.

```rust
// alloy-runtime/src/session/run_state.rs

/// Control-plane values persisted in `RunRow.state` (snake_case strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunControlState {
    /// Row created by `submit_goal`; not yet accepted by `start`.
    Created,
    /// `start` wrote acceptance and emitted `RunAccepted`. May be re-dispatched
    /// when no in-process live execution is registered (see §6.3).
    Accepted,
    /// In-process execution is live (host forwarder admitted work).
    Running,
    /// Human gate outstanding (written by `register_gate_waiter`).
    WaitingApproval,
    /// Cancel requested / in progress.
    Cancelling,
    /// Terminal cancel.
    Cancelled,
    /// Terminal success (from `RunCompleted` / successful `RunFinished`).
    Succeeded,
    /// Terminal failure (deny, execution failure, budget stop).
    Failed,
    /// Replan requested; DAG mutation deferred to 0009/0010.
    ReplanRequested,
}

impl RunControlState {
    #[must_use]
    pub const fn as_str(self) -> &'static str { /* snake_case */ }

    /// Exact match on persisted vocabulary. Prefer over `Result<_, ()>` (clippy
    /// `result_unit_err` fails `-D warnings` on MSRV 1.97).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> { /* exact match */ }
}
```

| Variant | Persisted `state` string |
| --- | --- |
| `Created` | `created` |
| `Accepted` | `accepted` |
| `Running` | `running` |
| `WaitingApproval` | `waiting_approval` |
| `Cancelling` | `cancelling` |
| `Cancelled` | `cancelled` |
| `Succeeded` | `succeeded` |
| `Failed` | `failed` |
| `ReplanRequested` | `replan_requested` |

**Unknown strings on read:** treat as `RunError::InvalidPhase` when a mutating run API requires a known state; `resume` of the **session** still succeeds from the session row (run rows remain listable).

### 3.4 `goal_json` envelope

`RunRow.goal_json` MUST store:

```rust
// alloy-runtime/src/session/goal_record.rs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunGoalRecord {
    pub goal: Goal,
    /// Minted at `submit_goal`. DAG body/persistence is RFC-0009; id binding is required
    /// so `RunAccepted { run_id, dag_id }` and `Scheduler::run` have a stable id.
    pub dag_id: DagId,
}
```

No schema migration. Serde JSON into the existing `goal_json` column.

**Forward compatibility (RFC-0009):** deserialize with unknown-field tolerance (serde default: ignore unknown fields). Do **not** use `deny_unknown_fields`. New fields added by 0009 MUST use `#[serde(default)]` so 0003-written rows remain readable.

### 3.5 Profile IDs

MVP `CreateSession.profile` MUST be one of:

| ProfileId string | Meaning (V2) |
| --- | --- |
| `default` | Standard interactive profile |
| `autonomous` | Reduced gating (policy details → 0015 / later) |
| `readonly` | No mutating tools (enforcement → tools/sandbox RFCs) |

`ProfileId::new` already enforces length. This RFC MUST additionally reject other ids at `create` with `SessionError::Invalid("unsupported profile: …")`.

### 3.6 Concrete types & construction

```rust
// alloy-runtime/src/session/plane.rs

/// Process session/run control plane. Cheap to clone (`Arc` inner).
#[derive(Clone)]
pub struct SessionPlane { /* Arc<SessionInner> */ }

/// In-process counters (see §13).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetrics {
    pub sessions_created: u64,
    pub sessions_resumed: u64,
    pub goals_submitted: u64,
    pub runs_started: u64,
    pub runs_start_unavailable: u64,
    pub runs_cancelled: u64,
    pub approvals_resolved: u64,
    pub replans_requested: u64,
    pub budget_warnings: u64,
}

impl SessionPlane {
    /// Construct after SQLite install. Does not take ownership of `AlloyRuntime`.
    ///
    /// REQUIREMENTS:
    /// - `handle.phase()` is `Running` for production wiring.
    /// - `storage` is the same `Arc` returned by `install_sqlite_event_sink`.
    pub fn new(handle: RuntimeHandle, storage: Arc<AlloyStorage>) -> Self;

    /// `Arc<dyn SessionService>` view (same inner).
    pub fn sessions(&self) -> Arc<dyn SessionService>;

    /// `Arc<dyn RunController>` view (same inner).
    pub fn runs(&self) -> Arc<dyn RunController>;

    /// Snapshot of §13 counters.
    #[must_use]
    pub fn metrics(&self) -> SessionMetrics;

    /// CLI convenience: forward to `RunController` (traits remain distinct).
    pub async fn approve(
        &self,
        run: RunId,
        gate: GateId,
        decision: Approval,
    ) -> Result<(), RunError>;

    pub async fn cancel(&self, run: RunId) -> Result<(), RunError>;

    /// Budget exhaustion hook. RFC-0004 metering calls this; does not compute spend.
    pub async fn signal_budget_warning(
        &self,
        session: SessionId,
        run: Option<RunId>,
        snapshot: BudgetSnapshot,
        message: impl Into<String>,
    ) -> Result<EventSeq, SessionError>;

    /// Register an in-process gate waiter and persist `waiting_approval`.
    ///
    /// RFC-0010 `GateHumanAdapter` MUST call this before awaiting the receiver.
    /// Replaces any prior waiter for `(run, gate)` (dropped sender → prior receiver errs).
    /// Returns `UnknownGate` never; returns `NotFound` / `InvalidPhase` when run missing
    /// or terminal / cancelling.
    pub async fn register_gate_waiter(
        &self,
        run: RunId,
        gate: GateId,
    ) -> Result<tokio::sync::oneshot::Receiver<Approval>, RunError>;
}
```

`SessionPlane` MUST implement both traits on internal wrapper types (or on `SessionPlane` itself via explicit `Arc` views) so `Arc<dyn SessionService>` and `Arc<dyn RunController>` remain distinct objects if required by callers — same `SessionInner`.

### 3.7 Additive `RuntimeHandle` DAG forwarder seams

To avoid splitting `AlloyRuntime` ownership and to keep single-flight admit in one place:

```rust
// alloy-runtime/src/runtime/handle.rs  — ADDITIVE

impl RuntimeHandle {
    /// Same semantics as `AlloyRuntime::run`: single-flight admit, map
    /// `SchedError::Unavailable` → `RuntimeError::SchedulerUnavailable`,
    /// does **not** emit `RunAccepted` / `RunFinished`.
    pub async fn run_dag(&self, dag_id: DagId) -> Result<DagOutcome, RuntimeError>;

    /// Cancel via current `Scheduler::cancel` (NullScheduler: Ok(())).
    /// Phase: `Running` | `Draining`. Does not emit session events.
    pub async fn cancel_dag(&self, dag_id: DagId) -> Result<(), RuntimeError>;
}
```

`AlloyRuntime::run` MUST call the same shared implementation as `RuntimeHandle::run_dag` (no divergent admit logic).

### 3.8 Crate-root re-exports (additive)

```rust
pub use session::{
    clamp_events_page_limit, ReplanReason, RunController, RunControlState, RunGoalRecord,
    Session, SessionMetrics, SessionPlane, SessionService, MAX_EVENTS_PAGE,
};
```

Existing re-exports remain. No glob exports.

---

## 4. Internal Module Design

### Module map

```text
alloy-runtime/src/session/
  mod.rs              # re-exports
  traits.rs           # existing traits / Session / ReplanReason (0001)
  plane.rs            # SessionPlane + Arc views + SessionMetrics
  service.rs          # SessionService impl
  run_controller.rs   # RunController impl
  run_state.rs        # RunControlState
  goal_record.rs      # RunGoalRecord
  profiles.rs         # MVP profile validation
  gates.rs            # in-process GateWaiterRegistry
  metrics.rs          # session/run counters
  map_err.rs          # runtime_to_run / store_to_run helpers
  inner.rs            # shared SessionInner (handle, storage, locks, registries)
```

### Responsibilities

| Module | Owns | MUST NOT |
| --- | --- | --- |
| `service` | create/resume/submit_goal/events; session row + lifecycle events | Call tools; mutate DAG topology; invent seq numbers |
| `run_controller` | start/cancel/approve/request_replan; run row state; RunAccepted/Finished | Own EventStore schema; plan DAGs |
| `gates` | oneshot waiters for `(RunId, GateId)` | Persist waiters across process death (re-register on resume via 0010) |
| `storage` (0002) | Durable bytes | Orchestration |
| `scheduler` (0001/0010) | DAG execution | Session row writes |

### Dependency injection

`SessionInner` MUST hold:

- `RuntimeHandle`
- `Arc<AlloyStorage>`
- Per-session `tokio::sync::Mutex<()>` map for mutating session APIs
- Per-run `tokio::sync::Mutex<()>` map for mutating run APIs
- Process-local `live_execution: HashSet<RunId>` (or equivalent) — true while `run_dag` await is in flight for that run
- `GateWaiterRegistry`
- Counters

**Lock-map eviction:** after releasing a per-session or per-run mutex, if the map entry has no other Arc clones / waiters, remove it. Maps MUST NOT grow without bound across a long-lived process.

Storage access:

- Sessions: `storage.sessions()` → `Arc<SqliteSessionRows>` as `dyn SessionRows`
- Events read: `storage.events()` → `EventStore::list_session_events`
- Events write: **only** `handle.append_session` / `handle.emit` (so active `EventSink` — memory or SQLite — stays authoritative)

### Ownership / Sync

- `SessionPlane: Clone` via `Arc<SessionInner>`
- `SessionInner: Send + Sync`
- Public traits remain `Send + Sync` + `async_trait`

### ID sourcing for run APIs

All RunController methods that append session events or call the host forwarder MUST obtain:

| Field | Source |
| --- | --- |
| `session_id` | `RunRow.session_id` |
| `dag_id` | `RunGoalRecord.dag_id` deserialized from `RunRow.goal_json` |
| `run_id` | method argument / `RunRow.id` |

Corrupt `goal_json` on `start` / `request_replan` ⇒ `RunError::Internal`. On `cancel`, if `goal_json` is corrupt, still transition to `cancelled` and clear waiters; skip `cancel_dag` and skip `RunFinished` (log warn).

---

## 5. Session Lifecycle

### 5.1 Preconditions

| Method class | Phase requirement | Failure |
| --- | --- | --- |
| Mutating: `create`, `submit_goal`, `signal_budget_warning` | `Running` | `SessionError::Invalid("runtime not running")` |
| Read-only: `resume`, `events` | `Running` \| `Draining` | `SessionError::Invalid("runtime not available")` |
| All | `cancellation` not cancelled for mutating ops | `SessionError::Invalid("runtime cancelled")` |
| All | Storage open (not closed) | `store_to_session` → Internal/Invalid |

### 5.2 `create`

**Normative steps (order):**

1. Validate `req.profile` ∈ {`default`,`autonomous`,`readonly`}.
2. Validate `req.workspace_root` is absolute; else `Invalid`.
3. Validate `language_backends` non-empty for MVP; else `Invalid` (MVP expects `rust`).
4. Allocate `SessionId::new()`, `created_at = Timestamp::now()`.
5. Build `Session { id, workspace_root, profile, budget: req.budget, language_backends, created_at }`.
6. `sessions.upsert_session(&session)` — on error `store_to_session`.
7. `handle.append_session(NewSessionEvent { session_id, run_id: None, type_: SessionCreated, payload })` where payload MUST include at least:

```json
{
  "workspace_root": "<utf-8 path>",
  "profile": "default",
  "budget": { /* BudgetPolicy serde */ },
  "language_backends": ["rust"]
}
```

8. Return `id`.

**Crash window:** upsert then append. If append fails after upsert, log the session id and return the append error mapped through `runtime_to_session` (§7) — a stopped or wrong-phase runtime is an `Invalid` request, not an `Internal` fault, and mislabelling it hides a retryable condition. Do not invent compensating deletes (0002 has none). `resume` succeeds from the row alone; `submit_goal` requires the row and does **not** require a `SessionCreated` event.

### 5.3 `resume`

1. `get_session(id)` → `None` ⇒ `SessionError::NotFound(id)` (**do not** use `store_to_session` for this miss).
2. Store errors ⇒ `store_to_session`.
3. Return `Session` snapshot as stored.
4. MUST NOT invent missing events, except the terminal pair owed by step 9 finalization.
5. MUST NOT auto-start runs.
6. Rebuild **process-local** run→dag bindings by scanning `list_runs` and deserializing `RunGoalRecord` from `goal_json`. On per-run deser failure: `tracing::warn`, skip that run’s binding, continue. The corrupt row remains persisted and listable, is not dispatched, is not entered into `live_execution`, and MUST NOT block resume of the session or restoration of other valid runs.
7. Gate waiters start empty (0010 re-registers).
8. `live_execution` starts empty.
9. **Re-arm after restart (explicit recovery only):** for rows in `running` or `waiting_approval`, upsert durable state to `accepted` (crash recovery), then they follow the `Accepted` re-dispatch path in §6.3. Rows already `accepted` stay re-dispatchable. Rows in `cancelling` MUST be finalized to `cancelled` on resume (best-effort `cancel_dag` skipped if no live scheduler work), together with a `RunCompleted` event. Emit `RunFinished` **only when** a durable `RunAccepted` exists for the run (or this process marked acceptance): `cancelling` is reachable from `created` without acceptance, so durable state alone must not invent an unpaired finish. Do not invent DAG progress. In-process non-terminal outcomes MUST NOT be treated as crash recovery (see §6.3). Terminal cancel finalization writes events **before** the `cancelled` upsert, with existence checks so a retry stays idempotent.
10. Each row is re-read under its per-run mutex before step 9 decides anything: the `list_runs` result is a snapshot, and a concurrent `cancel` in the same process must not be rewritten from a stale state.

### 5.4 `submit_goal`

1. Load session or `NotFound`.
2. Reject empty `goal.text` (trim) with `Invalid`.
3. Allocate `RunId::new()`, `DagId::new()`, timestamps.
4. Persist `RunRow` (row first):

```text
id, session_id,
goal_json = serde(RunGoalRecord { goal, dag_id }),
state = "created",
created_at, updated_at
```

5. Append `GoalSubmitted` with `run_id: Some(run)`, payload:

```json
{
  "goal": { /* Goal */ },
  "dag_id": "<uuid>",
  "budget": { /* session BudgetPolicy snapshot */ }
}
```

6. Return `RunId`.

**MUST NOT** call Planner, mutate DAG topology, or call `Scheduler::run`.
**MUST NOT** emit `RunAccepted` here.

### 5.5 `events`

1. Ensure session exists (`get_session`) → else `NotFound`.
2. `limit = clamp_events_page_limit(limit)` (always clamp; never panic).
3. `storage.events().list_session_events(id, after, limit)` → map errors via `store_to_session`.
4. Return page in ascending `seq` order (0002 contract).

### 5.6 Budget attachment & exhaustion hooks

- **Attachment:** `BudgetPolicy` from `CreateSession` persisted on the session row and echoed in `SessionCreated` / `GoalSubmitted` payloads as specified above.
- **Enforcement accounting:** RFC-0004.
- **Hook:** `SessionPlane::signal_budget_warning` MUST:

  1. Verify session exists.
  2. Append `SessionEventType::BudgetWarning` with payload `{ "snapshot": BudgetSnapshot, "message": "…" }` and `run_id` as provided.
  3. Return assigned `EventSeq`.

- Callers (0004 / future scheduler) then MAY `request_replan(..., ReplanReason::BudgetPolicy)` or `cancel` — not automatic inside the hook.

### 5.7 Mermaid — create → submit_goal → events

```mermaid
sequenceDiagram
  participant CLI
  participant SS as SessionService
  participant SR as SessionRows
  participant H as RuntimeHandle
  participant ES as EventSink/EventStore

  CLI->>SS: create(CreateSession)
  SS->>SR: upsert_session
  SS->>H: append_session(SessionCreated)
  H->>ES: append_session
  SS-->>CLI: SessionId

  CLI->>SS: submit_goal(Goal)
  SS->>SR: upsert_run(state=created)
  SS->>H: append_session(GoalSubmitted)
  SS-->>CLI: RunId

  CLI->>SS: events(after, limit)
  SS->>SR: get_session
  SS->>ES: list_session_events
  ES-->>CLI: Vec SessionEvent
```

### 5.8 Mermaid — resume after restart

```mermaid
sequenceDiagram
  participant Proc as New process
  participant RT as AlloyRuntime
  participant Inst as install_sqlite_event_sink
  participant SP as SessionPlane
  participant SR as SessionRows
  participant ES as EventStore

  Proc->>RT: configure + start
  Proc->>Inst: open data_dir + handoff
  Proc->>SP: SessionPlane::new(handle, storage)
  Proc->>SP: resume(session_id)
  SP->>SR: get_session
  SR-->>SP: Session
  Note over SP: live_execution empty; running/waiting_approval rewritten to accepted
  SP-->>Proc: Session
  Proc->>SP: events(None, n)
  SP->>ES: list_session_events
  Note over ES: Same gapless seq/ts/payload as pre-crash
```

---

## 6. RunController Lifecycle

### 6.1 Preconditions

| Check | Failure |
| --- | --- |
| Runtime phase `Running` for `start` / `approve` / `request_replan`; `Running` \| `Draining` for `cancel` | `RunError::InvalidPhase` |
| Run row exists | `RunError::NotFound` |

### 6.2 Ownership of transitions

| Transition | Owner |
| --- | --- |
| `created` (insert run) | **Session** (`submit_goal`) |
| `created` → `accepted` | **RunController** (`start`, first dispatch) |
| `accepted` → `running` | **RunController** (`start`, after `run_dag` returns `Ok` with non-terminal running-class outcome, or when live execution begins under 0010 — MVP sets `running` only from successful non-terminal `DagState::Running`) |
| `*` → `waiting_approval` | **RunController** via `SessionPlane::register_gate_waiter` (0010 calls it) |
| `waiting_approval` → `running` / `failed` | **RunController** (`approve`) |
| `*` → `cancelling` → `cancelled` | **RunController** (`cancel`) |
| non-terminal → `replan_requested` | **RunController** (`request_replan`) |
| DAG node execution / `DagState` | **Scheduler** (RFC-0010) |
| DAG topology edits | **Planner** (RFC-0009) — **forbidden** here |
| Terminal `succeeded` / `failed` / `cancelled` from execution | **RunController** when handling `run_dag` terminal outcomes (`RunFinished` + session events) |

### 6.3 `start`

**Normative algorithm:**

1. Acquire per-run mutex.
2. Load `RunRow` → missing ⇒ `NotFound`.
3. Let `session_id = row.session_id`. Parse `RunControlState` → unknown string ⇒ `InvalidPhase`.
4. Apply the state guards below, then deserialize `RunGoalRecord` from `goal_json` → corrupt ⇒ `Internal`. State comes first: a run that is live, cancelling, replan-pending, or terminal is not dispatchable whatever its envelope says, and the state error is the one the caller can act on.

| Current state | `live_execution` | Action |
| --- | --- | --- |
| `Created` | false | First dispatch — continue |
| `Accepted` | false | Re-dispatch — continue; **do not** emit a second `RunAccepted` |
| `Created` \| `Accepted` | true | `AlreadyStarted(run)` |
| `Running` \| `WaitingApproval` | * | `AlreadyStarted(run)` — not re-dispatchable in-process; crash recovery rewrites these to `accepted` in §5.3 before `start` |
| `Cancelling` | * | `InvalidPhase("cancelling")` |
| `Cancelled` \| `Succeeded` \| `Failed` | * | `InvalidPhase("terminal")` |
| `ReplanRequested` | * | `InvalidPhase("replan pending")` |

5. If state == `Created` (first dispatch only):
   - Upsert `state = "accepted"`, bump `updated_at` (**row first**).
   - `handle.emit(RunAccepted { run_id, dag_id })` (**event second**).
6. Insert `run` into `live_execution` (execution lease). The lease is the sole in-process liveness indicator that `run_dag` is outstanding for this run.
7. **Release per-run mutex** before any host await.
8. `result = handle.run_dag(dag_id).await`.
9. Re-acquire per-run mutex. Clear the execution lease (`live_execution`) **only after** applying step 10’s durable state transition for this result (still under the same lock acquisition). Order under the lock: (a) if durable state is `replan_requested`, `cancelling`, `cancelled`, or `waiting_approval`, an explicit control call won the race — **merge**: do not overwrite that state, clear the lease, return `Ok(())`, and still count `runs_start_unavailable` when the host result was unavailable; (b) if durable state is `succeeded` or `failed`, a second writer already finalized the run: return `Ok(())` when the host outcome **agrees** with that terminal (e.g. `Ok(Failed)` over durable `failed` from `approve(Deny)` while `run_dag` was awaited), return `Ok(())` for `SchedError::Cancelled`, and return `InvalidPhase("state advanced during run")` only for a **conflicting** success/failure; skip event writes on the agreeing path because the other writer already emitted them; (c) otherwise apply the step 10 row; (d) then clear lease. This prevents a late `run_dag` completion from clobbering `request_replan` / `cancel` / a registered gate, and prevents a second `start` from admitting duplicate execution while the lease is held.

**Lease ownership:** the lease is an RAII guard held across the step 8 await, so a `start` future that is dropped (task abort, caller cancellation, panic) releases it instead of stranding the run behind `AlreadyStarted` for the life of the process.
10. Map `result` while holding the lock (when step 9(a) does not suppress):

| Host result | RunController result | Required side effects |
| --- | --- | --- |
| `Ok(outcome)` with **terminal** `DagState` (`Succeeded` / `Failed` / `Cancelled`) | `Ok(())` | Append `RunCompleted` then emit `RunFinished`, then upsert matching `RunControlState` (events before row; existence checks keep a failed upsert retry-safe). **Not** `SessionEventType::Error` for user/host cancel |
| `Ok(outcome)` with `DagState::WaitingApproval` | `Ok(())` | Upsert `waiting_approval`; **do not** emit `RunFinished`; lease cleared after upsert — further `start` rejected until §5.3 recovery rewrites to `accepted` |
| `Ok(outcome)` with `DagState::Running` | `Ok(())` | Upsert `running`; **do not** emit `RunFinished`; same re-dispatch rule as `WaitingApproval` |
| `Ok(outcome)` with `DagState::ReplanRequired` | `Ok(())` | Upsert `replan_requested`; **do not** emit `RunFinished` |
| `Ok(outcome)` with `DagState::Pending` | `Err(Internal("unexpected pending outcome"))` | Leave durable state `accepted`; **do not** emit `RunFinished` |
| `Err(SchedulerUnavailable)` | `Err(SchedulerUnavailable)` | Keep `accepted`; append `Error` `{ "class": "scheduler_unavailable" }`; **no** `RunFinished`; run remains re-dispatchable |
| `Err(SchedulerBusy)` | `Err(InvalidPhase("scheduler busy"))` | Keep prior durable state (`accepted` if first dispatch completed step 5); **no** `RunFinished`; re-dispatchable when busy clears |
| `Err(InvalidPhase { .. })` | `Err(InvalidPhase(..))` | Keep prior durable state; no `RunFinished` |
| `Err(Scheduler(SchedError::Cancelled))` | `Ok(())` | Append `RunCompleted` / emit `RunFinished`, then upsert `cancelled` (same events-before-row order). **Do not** call `runtime_to_run` for this arm |
| `Err(Scheduler(SchedError::DagNotFound(_)))` | `Err(InvalidPhase("dag not found"))` | Keep `accepted`; append `Error` `{ "class": "dag_not_found" }` |
| `Err(EventSinkBusy)` or `Err(EventSink(_))` | `Err(Internal(..))` | Keep prior state; log |
| Other `RuntimeError` | `Err(Internal(..))` | Keep prior state; append `Error` `{ "class": "internal", "message": "…" }` when session append still possible |

**NullScheduler MVP:** first `start` performs step 5, then step 8 returns `SchedulerUnavailable`, step 10 returns that error with `accepted` retained. A later `start` (or after RFC-0010 installs a real scheduler) re-dispatches from `Accepted` without a second `RunAccepted`.

**Lock rule:** the per-run mutex MUST NOT be held across `run_dag` or `cancel_dag` awaits. The same rule applies as for handoff: never hold control-plane locks across host/scheduler awaits so `approve` / `cancel` can proceed (0010 `GateHumanAdapter::wait_approval` resumes via `approve`).

### 6.4 `cancel`

1. Acquire per-run mutex.
2. Load run → `NotFound` if missing. Read `session_id`. Attempt `RunGoalRecord` parse from `goal_json`; retain `goal_ok: bool` and `dag_id: Option<DagId>` (`goal_ok == false` ⇒ `dag_id == None`, `tracing::warn`).
3. If `Cancelled` \| `Succeeded` \| `Failed` ⇒ `Ok(())` (idempotent).
4. If state is `Created`: never-started runs were never admitted — drop waiters, append `RunCompleted` `{ "dag_state": "cancelled" }`, upsert `cancelled`, **do not** write `cancelling`, **do not** call `cancel_dag`, **do not** emit `RunFinished`. Return `Ok(())`.
5. Sample acceptance: if state is already `Cancelling`, consult the durable `RunAccepted` log (and process-local marker); otherwise use `was_accepted` on the current durable state. Do **not** treat `cancelling` alone as proof of prior acceptance (`created → cancelling` is a historical shape).
6. If state is not already `Cancelling`, upsert `cancelling`. Drop **all** gate waiters for this `run` (senders dropped).
7. Release per-run mutex.
8. If `goal_ok`: `handle.cancel_dag(dag_id).await` (map errors via §7; NullScheduler `Ok(())`). If `!goal_ok`, skip `cancel_dag`. On `cancel_dag` failure, leave durable state `cancelling` and return the mapped error; a later `cancel` completes idempotently.
9. Re-acquire per-run mutex and **re-read the row**: the mutex was released in step 7, so a resume finalizing this same cancel (§5.3 step 9) may already have written the row. If the fresh state is terminal, return `Ok(())` without repeating steps 10–11.
10. Append `RunCompleted` `{ "dag_state": "cancelled" }` (skip if one already exists), then emit `RunFinished` when `goal_ok` and acceptance from step 5 hold (skip if already finished), **then** upsert `cancelled` from the fresh row and clear `live_execution`. Events precede the terminal upsert so a failed append cannot leave a permanently `cancelled` row without them.
11. If `!goal_ok`, skip `RunFinished` (permitted cancellation updates in steps 6–10 still apply). Synthetic `DagOutcome { state: Cancelled, … }` (generation `0` / empty failure) when emitting without a scheduler outcome.
12. Return `Ok(())`.

### 6.5 `approve`

**State validation (before any waiter lookup):**

| State | Result |
| --- | --- |
| missing run | `NotFound` |
| `WaitingApproval` | continue to waiter lookup |
| `Cancelled` \| `Succeeded` \| `Failed` | `InvalidPhase("terminal")` |
| `Cancelling` | `InvalidPhase("cancelling")` |
| `Created` \| `Accepted` \| `Running` \| `ReplanRequested` | `InvalidPhase("not waiting approval")` |

**Algorithm:**

1. Acquire per-run mutex.
2. Load run; apply state table above (every existing non-`WaitingApproval` run returns its `InvalidPhase` **regardless of waiter presence**).
3. Only if `WaitingApproval`: take waiter for `(run, gate)` from registry. None ⇒ `UnknownGate(gate)`.
4. Persist before notify (row/event first):
   - On `Deny`: upsert `failed`; clear any remaining waiters for run; append `ApprovalResolved`; append `RunCompleted` `{ "dag_state": "failed", "reason": "approval_denied" }`; emit `RunFinished` with failed outcome if the run had been accepted, under the §6.4 step 11 rule — durable state is `waiting_approval` here, so acceptance is implied even when the `RunAccepted` emission happened in an earlier process.
   - On `Allow` / `AllowOnce`: upsert `running`; append `ApprovalResolved` with `{ "gate_id": "…", "decision": "allow"|"deny"|"allow_once" }` using `RunRow.session_id`.
   - On any persistence / emit failure: map via §7, **do not** call `sender.send` (waiter remains registered only if take was reverted — **pin:** on persist failure after take, put sender back or drop without send and return error so the gate is not consumed as approved).
5. Only after durable persistence succeeds: `sender.send(decision)` — if receiver dropped ⇒ `Internal("gate waiter dropped")` (decision is already durable).
6. Release lock. Return `Ok(())`.

A second `approve` for the same gate finds no waiter ⇒ `UnknownGate`.

`ApprovalRequested` emission remains owned by GateHuman execution (0010). This RFC writes `waiting_approval` only in `register_gate_waiter` and resolves via `approve`.

### 6.6 `request_replan`

**Allowed states:** `Accepted` \| `Running` \| `WaitingApproval`.

| State | Result |
| --- | --- |
| `Accepted` \| `Running` \| `WaitingApproval` | continue |
| `Created` | `InvalidPhase("not started")` |
| `Cancelling` | `InvalidPhase("cancelling")` |
| `Cancelled` \| `Succeeded` \| `Failed` | `InvalidPhase("terminal")` |
| `ReplanRequested` | `Ok(())` (idempotent) |

**Algorithm:**

1. Acquire per-run mutex.
2. Load run; apply table.
3. Drop **all** gate waiters for this `run` (invalidate outstanding approvals).
4. Upsert `replan_requested` (**row first**).
5. Append `ReplanRequested` with `{ "reason": /* serde ReplanReason */ }` using `RunRow.session_id`.
6. MUST NOT mutate DAG topology, nodes, or edges.
7. Return `Ok(())` (release lock). A concurrent `start` still awaiting `run_dag` MUST observe `replan_requested` in §6.3 step 9(a) and MUST NOT overwrite it with a later success/failure transition; merge by preserving `replan_requested` and skipping clobbering side effects while still clearing the execution lease.

### 6.7 `register_gate_waiter` (SessionPlane)

1. Acquire per-run mutex.
2. Load run; reject terminal / cancelling / created with `InvalidPhase` / `NotFound`. Also reject `replan_requested` with `InvalidPhase("replan pending")`: the replan discarded the DAG that owns the gate, and step 3 would otherwise rewrite the pending replan back to `waiting_approval`. Allowed states are exactly `Accepted` \| `Running` \| `WaitingApproval`.
3. Upsert `waiting_approval` (**row first**).
4. Replace registry entry for `(run, gate)` with a new oneshot; return receiver.
5. Release lock.

0010 SHOULD also append `ApprovalRequested` via `handle.append_session` (out of scope payloads beyond noting the type exists).

### 6.8 Mermaid — run control

```mermaid
stateDiagram-v2
  [*] --> Created: Session.submit_goal
  Created --> Accepted: start first dispatch (row then RunAccepted)
  Accepted --> Accepted: start re-dispatch when not live (NullScheduler unavailable)
  Accepted --> Running: run_dag Ok Running
  Accepted --> WaitingApproval: register_gate_waiter / run_dag WaitingApproval
  Running --> WaitingApproval: register_gate_waiter
  Running --> Accepted: resume crash recovery (§5.3)
  WaitingApproval --> Accepted: resume crash recovery (§5.3)
  WaitingApproval --> Running: approve Allow/AllowOnce
  WaitingApproval --> Failed: approve Deny
  Running --> Cancelling: cancel
  Accepted --> Cancelling: cancel
  WaitingApproval --> Cancelling: cancel
  Cancelling --> Cancelled
  Running --> Succeeded: terminal RunFinished
  Running --> Failed: terminal RunFinished
  Accepted --> Succeeded: terminal RunFinished
  Accepted --> Failed: terminal RunFinished
  Accepted --> ReplanRequested: request_replan
  Running --> ReplanRequested: request_replan
  WaitingApproval --> ReplanRequested: request_replan
```

---

## 7. Persistence Integration

### Writes

| Operation | SessionRows | EventSink via handle | RuntimeEvent |
| --- | --- | --- | --- |
| create | `upsert_session` | `SessionCreated` | — |
| resume | `get_session` (+ list_runs for re-arm) | `RunCompleted` cancelled when finalizing a `cancelling` row (§5.3 step 9) | `RunFinished` with the same finalization, when applicable |
| submit_goal | `upsert_run` | `GoalSubmitted` | — |
| events | `get_session` + EventStore list | — | — |
| start (first) | `upsert_run` → `accepted` **before** emit | `Error` on unavailable | `RunAccepted` after row |
| start (outcome) | `upsert_run` terminal/running/waiting | `RunCompleted` on terminal | `RunFinished` on terminal only |
| cancel | `upsert_run` | `RunCompleted` cancelled | `RunFinished` when applicable |
| approve | `upsert_run` | `ApprovalResolved` (+ `RunCompleted` on Deny) | `RunFinished` on Deny when applicable |
| replan | `upsert_run` | `ReplanRequested` | — |
| register_gate_waiter | `upsert_run` → `waiting_approval` | — (0010 may append `ApprovalRequested`) | — |
| budget hook | — | `BudgetWarning` | — |

### Ordering guarantees

- Per-session event `seq` is assigned only by the active `EventSink` (0001/0002) — Session MUST NOT assign seq.
- Mutating APIs that both upsert and append/emit MUST persist the **row first**, then append/emit, under the per-session or per-run lock for that critical section.
- Host awaits (`run_dag`, `cancel_dag`) happen **outside** the lock (see §6.3 / §8).
- Readers of `events` MAY race with appenders; pagination is cursor-based and MUST tolerate concurrent appends (same as 0002).

### Transactions

- RFC-0002 does not expose multi-table transactions to Session. MVP MUST tolerate crash between upsert and append (see §5.2 / §10).
- MUST NOT bypass `RuntimeHandle` to write session events directly into SQLite when a sink is installed (dual-write forbidden by 0002).

### Error mapping helpers

```rust
// alloy-runtime/src/session/map_err.rs

fn store_to_run(e: StoreError) -> RunError {
    match e {
        // Typed NotFound requires a RunId; callers map `get_run` → Ok(None) →
        // RunError::NotFound(id) themselves. StoreError::NotFound is a stringly miss.
        StoreError::NotFound(s) => RunError::Internal(format!("store not found: {s}")),
        StoreError::Conflict(s) => RunError::InvalidPhase(s),
        StoreError::Corrupt(s) | StoreError::Migration(s) => {
            RunError::Internal(s)
        }
        StoreError::Busy => RunError::Internal("store busy".into()),
        StoreError::Closed => RunError::Internal("store closed".into()),
        StoreError::DigestMismatch => RunError::Internal("digest mismatch".into()),
        StoreError::Io(s) | StoreError::Internal(s) => RunError::Internal(s),
    }
}

fn runtime_to_run(e: RuntimeError) -> RunError {
    match e {
        RuntimeError::SchedulerUnavailable => RunError::SchedulerUnavailable,
        RuntimeError::SchedulerBusy => RunError::InvalidPhase("scheduler busy".into()),
        RuntimeError::InvalidPhase { current, op } => {
            RunError::InvalidPhase(format!("{op} in phase {current:?}"))
        }
        // `run_dag` → SchedError::Cancelled is a **success** path in §6.3 step 10
        // (RunFinished + RunCompleted). Callers MUST match that arm before invoking
        // this helper. Reaching here is a programming error.
        RuntimeError::Scheduler(SchedError::Cancelled) => RunError::Internal(
            "bug: SchedError::Cancelled must be handled by start success path (§6.3)".into(),
        ),
        RuntimeError::Scheduler(SchedError::DagNotFound(id)) => {
            RunError::InvalidPhase(format!("dag not found: {id}"))
        }
        RuntimeError::Scheduler(SchedError::Unavailable) => RunError::SchedulerUnavailable,
        RuntimeError::Scheduler(SchedError::Internal(s)) => RunError::Internal(s),
        RuntimeError::EventSinkBusy => RunError::Internal("event sink busy".into()),
        RuntimeError::EventSink(e) => RunError::Internal(e.to_string()),
        RuntimeError::AlreadyStopped => RunError::InvalidPhase("runtime stopped".into()),
        RuntimeError::Config(s) | RuntimeError::Internal(s) => RunError::Internal(s),
        RuntimeError::Io(e) => RunError::Internal(e.to_string()),
    }
}

fn runtime_to_session(e: RuntimeError) -> SessionError {
    match e {
        RuntimeError::InvalidPhase { current, op } => {
            SessionError::Invalid(format!("{op} in phase {current:?}"))
        }
        RuntimeError::EventSinkBusy => SessionError::Internal("event sink busy".into()),
        RuntimeError::EventSink(e) => SessionError::Internal(e.to_string()),
        RuntimeError::AlreadyStopped => SessionError::Invalid("runtime stopped".into()),
        other => SessionError::Internal(other.to_string()),
    }
}
```

| Situation | Mapping |
| --- | --- |
| `get_session` → `Ok(None)` | `SessionError::NotFound(id)` |
| `get_run` → `Ok(None)` | `RunError::NotFound(id)` |
| Other `StoreError` on session APIs | `store_to_session` |
| Other `StoreError` on run APIs | `store_to_run` |
| `RuntimeError` from `emit` / `append_session` / `run_dag` / `cancel_dag` | `runtime_to_run` or `runtime_to_session` |
| `RunError` from a run-control helper reused by `resume` (§5.3 step 9) | `run_to_session`: `InvalidPhase` → `Invalid`, everything else → `Internal` |

Note: `RuntimeHandle` returns `RuntimeError` (including `EventSink` via `#[from]`). Callers MUST map `RuntimeError`, not raw `EventSinkError`.

### Host wiring (normative)

```text
AlloyRuntime::new()
  → configure(RuntimeConfig)
  → start() -> handle
  → install_sqlite_event_sink(&handle, None) -> storage
  → SessionPlane::new(handle.clone(), storage)
```

### Recovery

- After reopen + install, `resume` + `events` MUST observe durable pre-crash commits.
- In-flight oneshot gate waiters are **not** durable; 0010 MUST call `register_gate_waiter` again after resume when re-entering gates.
- Durable `accepted` / `running` / `waiting_approval` without `live_execution` remain **re-dispatchable** via `start` (§6.3) — the MVP Unavailable path MUST NOT permanently poison a run.
- `cancelling` rows are finalized to `cancelled` on resume (§5.3).

---

## 8. Concurrency Model

- Tokio async; `SessionPlane` is `Send + Sync`.
- **Per-session lock** for `submit_goal` / budget warning / conflicting session mutations: lock by `SessionId`.
- **Per-run lock** for the critical sections of `start` / `cancel` / `approve` / `request_replan` / `register_gate_waiter` only.
- **MUST NOT** hold per-run or per-session locks across `run_dag`, `cancel_dag`, or event-sink handoff awaits.
- `events` SHOULD NOT take the write lock for the whole list; MAY take a brief lock to verify session existence then read EventStore.
- Concurrent `events` readers: allowed.
- Concurrent appenders: serialized per session/run by locks above; EventSink provides its own safety (0001/0002).
- Single-flight DAG admit remains inside `run_dag` (0001 metrics/busy behavior preserved).
- Lock-map eviction: see §4.

---

## 9. Async Model

- All trait methods are `async` via `async_trait` (M1).
- SQLite remains on `spawn_blocking` inside storage (0002); Session MUST NOT add nested blocking on the async worker beyond awaiting storage/handle futures.
- `register_gate_waiter` is **async** (persists `waiting_approval`).
- Shutdown: mutating Session APIs fail when phase ≠ `Running`; `resume` / `events` allowed in `Draining`; `cancel` allowed in `Draining`.
- MVP: `start` awaits `run_dag` directly **without** holding the per-run lock (matches host `run`, enables `approve` during gate waits).

---

## 10. Shutdown and Durability

| Event | Guarantee |
| --- | --- |
| Graceful `drain` / `shutdown` | In-flight `run_dag` cancelled via host drain path; reject new `start` / `submit_goal` when not `Running`; `cancel` still allowed in `Draining` |
| Crash after successful append commit | Events durable (0002 WAL/fsync policy) |
| Crash between upsert and append/emit | Row may exist without matching event; resume uses row; `start` re-dispatch rules apply |
| Crash after `RunAccepted` before outcome | Row stays `accepted`; restart → re-dispatchable |
| Restart | `resume` + `events` restore control truth; waiters empty; `live_execution` empty |

Durability of session events equals RFC-0002 EventStore durability. This RFC adds no weaker path.

---

## 11. Error Handling

### SessionError

| Variant | When |
| --- | --- |
| `NotFound` | Missing session row on resume/submit/events/budget hook |
| `Invalid` | Bad profile, relative workspace, empty goal, wrong phase, unsupported state |
| `Internal` | Store/sink/`RuntimeError` failures after mapping |

### RunError

| Variant | When |
| --- | --- |
| `NotFound` | Missing run row |
| `InvalidPhase` | Illegal transition; scheduler busy; runtime phase; dag not found; cancelling/terminal |
| `Internal` | Corrupt `goal_json` on start/replan; sink failures; dropped waiter channel |
| `SchedulerUnavailable` | NullScheduler / Unavailable |
| `AlreadyStarted` | `start` while `live_execution` contains the run |
| `UnknownGate` | `approve` without waiter |

### Recoverable vs fatal

| Class | Examples | Caller action |
| --- | --- | --- |
| Recoverable | `SchedulerUnavailable`, `UnknownGate`, `AlreadyStarted`, `NotFound`, busy `InvalidPhase` | Surface to CLI; re-dispatch when applicable |
| Fatal process | Runtime `Failed` phase | Host shutdown path |

---

## 12. Configuration

**No new configuration keys required.**

Reuse:

- `RuntimeConfig` / `ConfigPaths` / `ALLOY_DATA_DIR` / profile + router paths (0001)
- Storage keys already in `example.env` (0002)

**MUST NOT** write or overwrite `.env`.
**MUST NOT** modify `example.env` unless a new key becomes absolutely required (none expected for this RFC).

Profile **catalog files** (`profiles/default.toml`, etc.) remain 0015/0001 config concerns; this RFC only validates the `ProfileId` string on `CreateSession`.

---

## 13. Observability

### Logging (`tracing`)

| Event | Level | Fields |
| --- | --- | --- |
| create ok | info | `session_id`, `profile` |
| create fail | warn | `error` |
| resume ok | info | `session_id` |
| resume miss | info | `session_id` |
| submit_goal ok | info | `session_id`, `run_id`, `dag_id` |
| start | info | `run_id`, `dag_id`, `redispatch` |
| start unavailable | warn | `run_id` |
| cancel | info | `run_id` |
| approve | info | `run_id`, `gate_id`, `decision` |
| replan | info | `run_id` |
| budget warning | warn | `session_id`, `run_id` |

### MVP metrics

`SessionMetrics` (§3.6) on `SessionInner`, exposed via `SessionPlane::metrics()`. No OTLP. No new crate.

---

## 14. Testing Strategy

### Unit tests (`alloy-runtime`)

| Test | Expect |
| --- | --- |
| `session_create_persists_row_and_event` | Row + `SessionCreated` seq 0 |
| `session_submit_goal_creates_run` | `RunRow.state == created`, `GoalSubmitted` |
| `session_events_pagination_exclusive` | `after`/`limit` semantics + clamp |
| `session_resume_not_found` | `SessionError::NotFound` |
| `session_reject_unknown_profile` | `Invalid` |
| `session_events_allowed_while_draining` | resume/events ok in `Draining` |
| `run_start_null_scheduler_unavailable` | `RunAccepted` once + `SchedulerUnavailable` + state `accepted` |
| `run_start_redispatch_after_unavailable` | second `start` no second `RunAccepted`; still Unavailable |
| `run_start_scheduler_cancelled_emits_finished` | `SchedError::Cancelled` → `Ok(())` + `RunFinished` + `RunCompleted`; not `InvalidPhase` |
| `runtime_to_run_cancelled_is_bug_internal` | bare `runtime_to_run(Scheduler(Cancelled))` → `Internal` (start must special-case) |
| `run_double_start_while_live_already_started` | `AlreadyStarted` when `live_execution` set |
| `run_running_outcome_not_redispatchable` | after `Ok(Running)`, second `start` → `AlreadyStarted` |
| `run_approve_unknown_gate` | `UnknownGate` only when `WaitingApproval` and no waiter |
| `run_approve_requires_waiting_approval` | `InvalidPhase` if not waiting (even if waiter present) |
| `run_approve_persists_before_notify` | fail append → waiter not notified |
| `run_approve_with_waiter` | oneshot resolves + `ApprovalResolved`; second approve `UnknownGate` |
| `run_cancel_clears_waiters` | approve after cancel → `UnknownGate` |
| `run_cancel_corrupt_goal_skips_run_finished` | `goal_ok == false` → cancelled row, no `RunFinished` |
| `run_cancel_idempotent` | second cancel `Ok(())`; uses `RunCompleted` not `Error` |
| `run_request_replan_rejects_terminal` | `InvalidPhase` |
| `run_request_replan_records_event` | state + `ReplanRequested`; waiters cleared |
| `run_request_replan_not_overwritten_by_late_start` | concurrent `run_dag` Ok does not clobber `replan_requested` |
| `budget_warning_hook_appends_event` | `BudgetWarning` |
| `store_miss_session_maps_to_not_found` | not `Invalid` via `store_to_session` |
| `store_to_run_corrupt_is_internal` | `Corrupt`/`Migration` → `Internal`; `Conflict` → `InvalidPhase` |
| `runtime_error_invalid_phase_maps` | `runtime_to_run` preserves `InvalidPhase` |
| `start_lock_not_held_across_run_dag` | with a mock scheduler that blocks until approve, approve succeeds |
| `session_resume_skips_corrupt_goal_json` | warn + skip binding; other runs restored |
| `run_start_abort_clears_lease` | aborting a `start` task releases the lease; next `start` is a fresh dispatch, not `AlreadyStarted` |
| `run_approve_deny_emits_run_finished_after_redispatch` | `Deny` emits `RunFinished` even when this process never emitted `RunAccepted` |
| `register_gate_waiter_rejects_replan_requested` | `InvalidPhase("replan pending")`; pending replan not rewritten |
| `session_resume_finalizes_cancelling_with_run_completed` | never-accepted `cancelling` → `cancelled` + one `RunCompleted`, **no** `RunFinished` |
| `session_resume_finalizes_accepted_cancelling_emits_finished` | accepted then `cancelling` → `cancelled` + `RunCompleted` + `RunFinished` |
| `session_resume_does_not_clobber_concurrent_cancel` | resume racing `cancel` yields one terminal write and one `RunCompleted` in any order |
| `run_approve_deny_during_run_dag_joins_cleanly` | Deny mid-`run_dag` → durable `failed`; `start` returns `Ok(())` with one terminal pair |
| `run_approve_deny_drops_waiter_when_append_fails` | Deny append fail after row commit drops sender (`Closed`); no stranded waiter |
| `lock_maps_evict_after_drop` | lock maps return to empty once guards/tickets drop |

### Integration tests

| Test | Expect |
| --- | --- |
| `session_resume` (roadmap M5 name) | create/submit → reopen storage/runtime → resume + events bit-identical seq/payload |
| `session_sqlite_cursor_after_restart` | exclusive cursor continues |
| `run_accepted_survives_restart_and_redispatch` | accepted row re-dispatchable after reopen |
| `cancelling_run_is_finalized_after_restart` | accepted then `cancelling` finalized once on resume with `RunFinished`; second resume is a no-op |
| `created_cancelling_resume_skips_run_finished` | `created → cancelling` without `RunAccepted` finalizes with **zero** `RunFinished` |
| Concurrent: N readers `events` + M `submit_goal` on distinct sessions | no deadlocks; seq gapless per session |

### Commands

```bash
cargo test -p alloy-runtime -- session_
cargo test -p alloy-runtime -- run_
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

---

## 15. MVP vs Deferred

### MVP (this RFC)

- `SessionPlane` + `SessionService` + `RunController` impls
- SQLite integration via existing 0002 types
- Lifecycle events: `SessionCreated`, `GoalSubmitted`, `BudgetWarning`, `ApprovalResolved`, `ReplanRequested`, `RunCompleted`, `Error` (scheduler/internal failures only)
- Runtime events: `RunAccepted`; `RunFinished` for **terminal** outcomes only
- `NullScheduler` / `SchedulerUnavailable` with re-dispatchable `accepted`
- Async gate waiter registry writing `waiting_approval`
- Tests listed above

### Deferred (do not implement here)

| RFC | Items |
| --- | --- |
| **0004** | Cost metering, decision writers, OTLP |
| **0009** | Planner, DAG load/persist, topology mutation, follow-up nodes |
| **0010** | Real scheduler, Verify*/GateHuman adapters, retries, ready-queue |
| **0015** | CLI UX, TTY approval rendering, profile TOML product behavior |
| **V2 deferred** | alloyd, ACP |

Do not invent additional deferred subsystems in this RFC.

---

## 16. Acceptance Criteria

Merge only when all items hold:

- [ ] `SessionService` / `RunController` method signatures match RFC-0001 / `session/traits.rs` (including `events(after: Option<EventSeq>, limit)`)
- [ ] Architecture V2 §5.2 / §5.5 / ADR F-22 intent preserved: Session ≠ tools/DAG mutation; RunController owns start/cancel/approve/replan
- [ ] Session does **not** execute tools or mutate DAG topology
- [ ] Persistence uses RFC-0002 `SessionRows` / `EventStore` / `RuntimeHandle` append — no dual-write, no sixth crate
- [ ] Row-then-event ordering for start acceptance; locks not held across `run_dag` / `cancel_dag`
- [ ] `accepted` remains re-dispatchable when `live_execution` is false (Unavailable path does not poison runs)
- [ ] `Running`/`WaitingApproval` not in-process re-dispatchable; resume rewrites to `accepted`
- [ ] Execution lease cleared only after durable transition; late outcomes cannot clobber `replan_requested`/`cancelled`
- [ ] `approve` / `request_replan` / `cancel` state guards and waiter lifecycle defined and tested
- [ ] Approve persists before waiter notify
- [ ] `RuntimeError` → `RunError` / `SessionError` mapping table implemented; `SchedError::Cancelled` handled as start success
- [ ] `store_to_run`: Corrupt/Migration → Internal; Conflict → InvalidPhase
- [ ] `BudgetPolicy` attached on create; `signal_budget_warning` hook defined and tested
- [ ] Restart recovery: `resume` + `events` + re-dispatch rules defined and integration-tested
- [ ] Corrupt `goal_json` on resume skipped with warn; cancel skips `RunFinished` when `!goal_ok`
- [ ] `RunError::SchedulerUnavailable` / `AlreadyStarted` / `UnknownGate`; `RunError` is `#[non_exhaustive]` with downstream catch-all guidance
- [ ] `RunRow.state` vocabulary pinned (`RunControlState::parse` → `Option`); not a second DAG state machine
- [ ] `RunGoalRecord` stored in `goal_json` with minted `DagId`; unknown fields tolerated
- [ ] `RuntimeHandle::run_dag` / `cancel_dag` additive seams share `AlloyRuntime::run` admit semantics
- [ ] `SessionMetrics` defined and re-exported
- [ ] Unit + integration tests in §14 passing
- [ ] `cargo fmt --check` clean; `clippy -D warnings` clean on touched crates
- [ ] Crate root re-exports updated explicitly (no glob)
- [ ] `.env` never written; `example.env` policy preserved
- [ ] Series [Definition of Done](./README.md#definition-of-done-merge-gate) satisfied

## Definition of Done

Merge only when the series [Definition of Done](./README.md#definition-of-done-merge-gate) is fully met:

- [ ] Architecture compliance: **PASS**
- [ ] RFC acceptance criteria: **100% satisfied**
- [ ] Unit tests: **passing**
- [ ] Integration tests: **passing**
- [ ] Documentation: **complete**
- [ ] Public APIs: **reviewed and stable**
- [ ] Clippy: **clean**
- [ ] Formatting: **clean**
- [ ] No TODO or placeholder implementations left in this RFC’s scope (explicit **Stub** / deferred only)
- [ ] Code review: **approved**

---

## 17. Open Questions

Only genuine implementation spikes — settled V2/0001/0002 decisions are not reopened.

1. **Create crash between `upsert_session` and `SessionCreated` append:** MVP accepts rare row-without-event; spike only if dogfood shows operator pain (then consider a single storage-level txn API in a follow-up — out of scope unless required).
2. **CLI backgrounding of long `start`:** MVP awaits `run_dag` without holding the per-run lock (approve/cancel remain responsive). If 0015 needs non-blocking CLI UX, that RFC may introduce backgrounding — not this RFC.

**Settled (do not reopen):**

- RFC-0001 `events(after: Option<EventSeq>, limit)` wins over V2 two-arg sketch
- Distinct `SessionService` vs `RunController` (F-22)
- Session does not store events itself (RunController does not own EventStore)
- `AlloyRuntime::run` does not emit `RunAccepted` / `RunFinished`
- SQLite MVP; ≤5 crates; never write `.env`
- `RunRow.state` is control-plane vocabulary here; `DagState` remains scheduler/DAG RFCs
- Scheduler execution and DAG topology belong to 0010 / 0009
- Budget **metering** belongs to 0004; this RFC only attaches policy + warning hooks
- Per-run locks MUST NOT span `run_dag` / `cancel_dag`
- `accepted` + `!live_execution` is re-dispatchable (Unavailable does not poison)
- `Running` / `WaitingApproval` are not in-process re-dispatchable; crash recovery rewrites them to `accepted`
- Row then event for acceptance; `RunFinished` only for terminal outcomes
- User/host cancel recorded as `RunCompleted`, not `SessionEventType::Error`
- `SchedError::Cancelled` from `run_dag` is handled as success in §6.3, not via `runtime_to_run` → `InvalidPhase`
- Approve persists before notifying the gate waiter
- `#[non_exhaustive]` on `RunError` requires downstream catch-all match arms

---

## Estimated implementation effort

**3–5 person-days** (aligned with RFC index / roadmap M5 session slice).

Suggested split: SessionPlane + create/resume/events (1d) · submit_goal + RunGoalRecord/state (0.5–1d) · RunController start/cancel/approve/replan + handle seams (1–1.5d) · budget hook + metrics (0.5d) · tests/recovery/concurrency (1d).

---

**End of RFC-0003.**
