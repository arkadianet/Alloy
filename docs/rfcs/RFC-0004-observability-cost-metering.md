# RFC-0004: Observability & Cost Metering

| Field | Value |
| --- | --- |
| **Status** | Implemented |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md) (merged) · [RFC-0002](./RFC-0002-storage-artifacts-session-events.md) (merged) |
| **Effort** | 2–4 person-days |
| **Related RFCs** | [0003](./RFC-0003-session-manager-run-controller.md) budget warning hook (merged; in-crate call) · [0007](./RFC-0007-model-router-provider.md) route/complete attribution · [0010](./RFC-0010-scheduler-runtime-adapters.md) scheduler decision/node producers · [0013](./RFC-0013-capability-registry-workers.md) `WorkerMetrics` producers · [0015](./RFC-0015-cli-profiles-config.md) `alloy events` UX · [0016](./RFC-0016-eval-harness-holdout-gates.md) calibrated cost bands |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | Draft outline of this filename (expanded to implementation grade) |

**Mental model (V2 §15 / ADR F-17):** Always-on decision recording and cost metering. Default retention = metadata + content hashes + redacted decision records. Full prompts and tool bodies are opt-in. Persistence is the existing session event log — never a second database. Numeric savings claims are forbidden until Eval calibrates (V2 §18).

**Authority order (highest → lowest):** current `main` source → RFC-0003 → RFC-0002 → RFC-0001 → Architecture V2. Never modify an existing public API solely to match an older V2 sketch or this document’s draft outline.

---

## 1. Overview

### Purpose

Ship the MVP **observability & metering substrate** inside `alloy-runtime`:

1. **`DecisionLog`** — append attributable decisions through existing `SessionEventType::{Decision, ModelCall, ToolCall}` envelopes via `RuntimeHandle::append_session`.
2. **`CostMeter` / `CostSnapshot`** — always-on incremental token and optional USD accounting; convert to `BudgetSnapshot` for RFC-0003 budget hooks.
3. **Hash + redaction helpers** — SHA-256 content hashes via existing `Digest`; secret/path deny-list redaction before any body retention.
4. **Query helpers** — filter/parse decision-related session events for CLI (RFC-0015) without owning CLI UX.
5. **Budget signaling edge** — when spend meets or exceeds `BudgetPolicy`, invoke existing `SessionPlane::signal_budget_warning` (RFC-0003). Metering does not own session lifecycle.

### Problem Statement

RFC-0001 published `WorkerMetrics`, `RuntimeMetrics`, `SessionEventType::{Decision, ModelCall, ToolCall, BudgetWarning}`, and `RuntimeConfig::{retain_full_prompts, retain_tool_bodies}`. RFC-0002 shipped durable `EventStore` persistence for any Appendix A envelope. RFC-0003 shipped session/run control and `SessionPlane::signal_budget_warning`. No module yet records decisions with F-17 defaults, meters cost, or wires spend into the budget hook. Without this RFC, ModelRouter (0007), Scheduler (0010), workers (0013), and `alloy events` (0015) have no shared metering/decision substrate.

### Scope

| In scope | Detail |
| --- | --- |
| `DecisionLog` trait + concrete impl | Record decision / model_call / tool_call via `RuntimeHandle::append_session` |
| `DecisionRecord` / `DecisionKind` | Normative shapes + serde |
| `CostMeter` / `CostSnapshot` / shared handle | Incremental updates; unknown usage never fabricated |
| Hash helpers | `Digest::sha256` wrappers for prompts/tool bodies |
| Redaction helpers | Secret patterns + path deny list; apply retention flags |
| Query helpers | List/filter/parse decision-related events from `EventStore` |
| Budget integration | `CostMeter` → `BudgetSnapshot` → `SessionPlane::signal_budget_warning` |
| `WorkerMetrics` usage contract | How producers feed meters / model_call payloads (producers themselves → 0013) |
| `ObsError` | Additive observability error type + mapping from existing errors |
| Module `alloy-runtime::obs` | Same crate; no sixth crate; no OTLP |
| Tests | Unit + EventStore integration + budget hook integration |

### Non-goals

- Model routing / provider attribution logic → **RFC-0007** (consumes `DecisionLog` / `CostMeter`).
- Scheduler execution / node_state emission ownership → **RFC-0010**.
- Capability worker implementations that produce `WorkerMetrics` → **RFC-0013**.
- `alloy events` CLI UX / printing → **RFC-0015** (consumes query helpers).
- Calibrated cost bands / marketing savings numbers → **RFC-0016** / V2 §18.
- OTLP export, Observability TUI → **Architecture V2 deferred** (§15.2).
- Redesigning V2, RFC-0001/0002/0003; new crates; new runtime services; parallel `EventStore`; second event log.
- Replacing, wrapping, or forking `EventSink` / `EventStore` / `SessionEventType` / `RuntimeHandle` / `WorkerMetrics` / `BudgetSnapshot`.
- Constraint evaluation including `Constraint::MaxUsd` on goals → **RFC-0010** / **RFC-0015** (this RFC only meters against `BudgetPolicy`).
- Writing or overwriting `.env`.

### Day-1 MVP (normative)

1. **EventStore-backed decision recording** through the installed sink (`RuntimeHandle::append_session`) — no parallel persistence.
2. **Metadata + content hashes by default**; full prompt/tool bodies only when `RuntimeConfig.retain_full_prompts` / `retain_tool_bodies` are true.
3. **`CostMeter` snapshots without OTLP** — process-local counters + session event payloads only.
4. **No `.env` writes**; `example.env` unchanged unless a new key is strictly required (none expected).

---

## 2. Architecture Integration

### Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §15 Observability | Decision records · cost metering · tool call metadata; OTLP optional later — **not** a separate crate in MVP |
| §15.2 / ADR F-17 | Default = metadata + content hashes + redacted decision records; full prompts / file-body tool results **opt-in**; no file bodies by default |
| §3.4 Observable decisions | Routing, context inclusion, tool grant, retry attributable |
| §3.6 Cost-aware execution | Metering APIs always on; numeric savings claims are **not** architecture proof |
| §5.6 Budget exhaustion | Stop non-essential; summarize; ask user — **signaling** via RFC-0003 hook; **accounting** here |
| §18 Cost Model | Budgets + metering APIs + decision-log cost fields; strip numeric differentiators (ADR F-08) |
| Appendix A | `decision` / `model_call` / `tool_call` / `budget_warning` event types — envelopes owned by 0001/0002 |
| Appendix B | `[observability] retain_full_prompts` / `retain_tool_bodies` — already loaded by RFC-0001 `RuntimeConfig` |

**V2 sketch superseded by `main`:** any historical `WorkerMetrics.confidence: f32` (required). **Normative on main:** `confidence: Option<f32>`.

### Relationship to RFC-0001

RFC-0001 is **authoritative** for:

- `WorkerMetrics` / `RuntimeMetrics` field shapes (writers deferred here; shapes already published)
- `SessionEventType`, `SessionEvent`, `NewSessionEvent`, `EventSink`, `RuntimeEvent`
- `RuntimeConfig::{retain_full_prompts, retain_tool_bodies}`
- `RuntimeHandle::{emit, append_session, config, metrics, …}`
- ID types including `Digest::sha256` / `Digest::try_from_hex`
- `BudgetPolicy`, `BudgetSnapshot`, `ModelTier`

This RFC **adds** `obs` APIs that **consume** those types. It MUST NOT redefine `WorkerMetrics` or change `RuntimeConfig` keys.

### Relationship to RFC-0002

RFC-0002 is **authoritative** for:

- `EventStore` / `SqliteEventStore` / `AlloyStorage` / `install_sqlite_event_sink`
- Per-session gapless `EventSeq`, exclusive cursor pagination, handoff, durability
- `StoreError` → `EventSinkError` / `store_to_session` / `store_to_runtime`
- Content-agnostic storage: the store does not strip bodies; **writers** enforce F-17 defaults

This RFC **owns what payloads** DecisionLog appends; it does **not** fork storage.

### Relationship to RFC-0003

RFC-0003 is **authoritative** for:

- Session ownership, event flow, budget **policy attachment**
- `SessionPlane::signal_budget_warning(session, run, snapshot, message) -> Result<EventSeq, SessionError>`
- Per-session locks and “MUST NOT hold session locks across unrelated awaits” rules

This RFC **supplies metering** that invokes the existing hook. It MUST NOT assume control of session/run lifecycle, MUST NOT call Planner/Scheduler, and MUST NOT auto-`cancel` / `request_replan` inside the meter (callers MAY after the hook returns).

**Dependency note:** The RFC index lists 0004 depends on 0001+0002 (RFC-0003 §2). Budget hook integration is an **in-crate** call to the already-merged `SessionPlane` API on `main` — not a new workspace crate edge.

### Already implemented | Added by RFC-0004 | Deferred

| Item | Owner |
| --- | --- |
| `WorkerMetrics`, `RuntimeMetrics` shapes | **0001** |
| `SessionEventType::{Decision, ModelCall, ToolCall, BudgetWarning, Error, …}` | **0001** |
| `RuntimeConfig.retain_full_prompts` / `retain_tool_bodies` | **0001** |
| `RuntimeHandle::append_session` / `emit` / `config` | **0001** |
| `Digest::sha256` | **0001** |
| `BudgetPolicy` / `BudgetSnapshot` / `ModelTier` | **0001** |
| `EventSink` / `EventStore` / SQLite durability | **0002** |
| `install_sqlite_event_sink` / `AlloyStorage` | **0002** |
| `SessionPlane` / `signal_budget_warning` / budget policy attachment | **0003** |
| `DecisionLog` + `DecisionRecord` + `DecisionKind` | **0004** |
| `CostMeter` + `CostSnapshot` + shared handle | **0004** |
| Hash / redaction / retention helpers | **0004** |
| Query helpers over `EventStore` | **0004** |
| `ObsError` + mappings | **0004** |
| `obs` module + crate-root re-exports | **0004** |
| Additive `EventStore::replay_session` `where Self: Sized` | **0004** (storage seam) |
| Budget exhaustion **accounting** + hook invocation helpers | **0004** |
| ModelRouter route/complete attribution | **0007** |
| Scheduler execution / node_state producers | **0010** |
| Worker implementations emitting `WorkerMetrics` | **0013** |
| `alloy events` CLI UX | **0015** |
| Calibrated holdout cost bands | **0016** |
| OTLP export / Observability TUI | **V2 deferred** |

### Dependency boundaries

```text
alloy-cli ──► alloy-runtime
                 ├── obs (0004) ──► RuntimeHandle::append_session (0001)
                 │              ──► EventStore read/query (0002)
                 │              ──► SessionPlane::signal_budget_warning (0003, in-crate)
                 │              ──► RuntimeConfig retention flags (0001)
                 │              ──► WorkerMetrics / BudgetSnapshot / Digest (0001)
                 ├── session (0003)
                 ├── storage (0002)
                 └── runtime / types / events (0001)
```

No new workspace crate. No new OS service. No observability database. No OTLP crate.

---

## 3. Public Rust API

All items live in `alloy-runtime` (edition 2021, Tokio 1.x, `async_trait` on public traits through M1 — same pins as RFC-0001/0002/0003).

**Do not break** existing public signatures. Extend via new types in `obs::`, additive crate-root re-exports, and helpers that call existing APIs.

### 3.1 Module layout & re-exports

```rust
// alloy-runtime/src/lib.rs  — ADDITIVE
pub mod obs;

pub use obs::{
    hash_content, hash_prompt, hash_tool_body,
    list_decision_events, parse_decision_event, parse_model_call_event, parse_tool_call_event,
    reaccumulate_cost_from_events, redact_json_strings, redact_secrets,
    apply_prompt_retention, apply_tool_retention,
    BudgetCheck, CostByTier, CostMeter, CostSnapshot, DecisionPage, SharedCostMeter, TierCost,
    DecisionKind, DecisionLog, DecisionRecord, EventDecisionLog, RecordingDecisionLog,
    ModelCallRecord, ModelUsdSource, ObsError, RetentionPolicy, ToolCallRecord,
    maybe_signal_budget_warning,
};
```

`WorkerMetrics` / `RuntimeMetrics` / `BudgetSnapshot` / `SessionEventType` remain re-exported from their existing modules — **do not** re-export a conflicting `WorkerMetrics` from `obs`.

### 3.1a Additive EventStore seam (authorized)

`EventStore::replay_session` is generic and currently makes the trait **not dyn-compatible** on main. RFC-0004 **authorizes** this additive change to RFC-0002’s trait (same class of seam as RFC-0003’s existence probes):

```rust
async fn replay_session<F>(
    &self,
    session: SessionId,
    on_event: F,
) -> Result<Option<EventSeq>, StoreError>
where
    Self: Sized, // ADDITIVE — enables `&dyn EventStore` for other methods
    F: FnMut(&SessionEvent) -> Result<(), StoreError> + Send;
```

MVP query/reaccumulate helpers MUST page via `list_session_events` (dyn-safe) and MUST NOT require `replay_session` on a trait object. The `Sized` bound is still REQUIRED so future typed helpers can use `&dyn EventStore`.

### 3.2 `ObsError`

```rust
// alloy-runtime/src/obs/error.rs
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// Invalid record / retention / payload construction.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Append through `RuntimeHandle` failed.
    #[error("append: {0}")]
    Append(#[from] RuntimeError),
    /// Budget warning hook / session lookup failed.
    #[error("session: {0}")]
    Session(#[from] SessionError),
    /// EventStore query/read failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Redaction/retention helper failed (malformed input after deny-list).
    #[error("redaction: {0}")]
    Redaction(String),
    /// Internal invariant.
    #[error("internal: {0}")]
    Internal(String),
}
```

**Justification for a dedicated type:** observability call sites must distinguish append failures from budget-hook/`SessionError` and from `StoreError` on query without collapsing everything into `RuntimeError`. Mapping helpers:

| Source | Maps to |
| --- | --- |
| `RuntimeError` from `append_session` | `ObsError::Append` (`#[from]`) |
| `SessionError` from `signal_budget_warning` | `ObsError::Session` (`#[from]`) |
| `StoreError` from `EventStore` reads | `ObsError::Store` (`#[from]`) |
| Empty required fields / unknown `DecisionKind` wire value on parse | `ObsError::Invalid` |
| Deny-list match when body retention was requested but body is unsafe | strip body + keep hash (not an error); only raise `Redaction` on helper misuse |

### 3.3 `DecisionKind`

```rust
// alloy-runtime/src/obs/decision.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    ModelRoute,
    ContextInclusion,
    ToolGrant,
    Retry,
    Gate,
    Budget,
    /// Extension point.
    Custom(String),
}
```

**Serde rules (normative):**

Externally tagged (serde default) + `rename_all = "snake_case"`:

| Variant | JSON |
| --- | --- |
| `ModelRoute` | `"model_route"` |
| `ContextInclusion` | `"context_inclusion"` |
| `ToolGrant` | `"tool_grant"` |
| `Retry` | `"retry"` |
| `Gate` | `"gate"` |
| `Budget` | `"budget"` |
| `Custom("x")` | `{"custom":"x"}` |

Implementers MUST lock these shapes with a unit-test golden JSON suite. Unknown unit strings on `DecisionKind` deserialize MUST fail (surfaced as `ObsError::Invalid` by parse helpers). Outer event payloads MUST NOT use `deny_unknown_fields` (forward-compatible metadata), but `kind` itself MUST be a valid `DecisionKind`.

### 3.4 `DecisionRecord` (in-memory API — not the wire format)

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    pub kind: DecisionKind,
    /// MUST be a JSON **object** or `Null` (normalized to `{}` on record). Other shapes → `ObsError::Invalid`.
    pub metadata: serde_json::Value,
    pub content_hash: Option<Digest>,
    pub prompt_body: Option<String>,
}
```

**Ownership:** callers own construction; `DecisionLog::record` takes `DecisionRecord` by value.

**Wire format:** private `DecisionPayload` (§5.3). Public records MUST NOT derive `Serialize` for event append — hand-build / map through payload structs so envelope fields are not duplicated incorrectly.

### 3.5 `ModelCallRecord` / `ToolCallRecord` (in-memory API)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUsdSource {
    ProviderReported,
    OperatorPriceTable,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ModelCallRecord {
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    pub provider_id: ProviderId,
    pub model_tier: ModelTier,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub confidence: Option<f32>,
    pub error_class: Option<ErrorClass>,
    pub content_hash: Option<Digest>,
    pub prompt_body: Option<String>,
    pub endpoint_id: Option<EndpointId>,
    pub model: Option<String>,
    pub route_event_seq: Option<EventSeq>,
    pub usd_source: Option<ModelUsdSource>,
    pub finish_reason: Option<String>,
    pub provider_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    pub tool_name: String,
    pub tool_server: Option<String>,
    pub latency_ms: Option<u64>,
    pub denied: bool,
    pub content_hash: Option<Digest>,
    pub body: Option<String>,
}
```

`ModelCallRecord` is non-exhaustive after the RFC-0007 amendment. External callers construct it with `ModelCallRecord::new(session, provider_id, model_tier)` and the builder methods (`run`, `node`, `tokens`, `usd`, `endpoint_id`, `model`, `route_event_seq`, `usd_source`, `finish_reason`, and `provider_request_id`) rather than struct literals. Same rule: wire payloads are private structs in §5.4–5.5; `usage_unknown` exists only on the wire.

### 3.6 `DecisionLog` trait

```rust
#[async_trait]
pub trait DecisionLog: Send + Sync {
    /// Append a `SessionEventType::Decision` event after redaction/retention.
    async fn record(&self, rec: DecisionRecord) -> Result<EventSeq, ObsError>;

    /// Append a `SessionEventType::ModelCall` event after redaction/retention.
    async fn record_model_call(&self, rec: ModelCallRecord) -> Result<EventSeq, ObsError>;

    /// Append a `SessionEventType::ToolCall` event after redaction/retention.
    async fn record_tool_call(&self, rec: ToolCallRecord) -> Result<EventSeq, ObsError>;
}
```

### 3.7 `EventDecisionLog` (concrete MVP impl)

```rust
/// DecisionLog backed by `RuntimeHandle::append_session` + retention from config.
pub struct EventDecisionLog {
    handle: RuntimeHandle,
    storage: Arc<AlloyStorage>,
    retention: RetentionPolicy,
}

impl EventDecisionLog {
    #[must_use]
    pub fn new(
        handle: RuntimeHandle,
        storage: Arc<AlloyStorage>,
        retention: RetentionPolicy,
    ) -> Self { /* … */ }

    /// Load retention from `handle.config()` (requires configure).
    pub fn from_handle(
        handle: RuntimeHandle,
        storage: Arc<AlloyStorage>,
    ) -> Result<Self, ObsError> { /* … */ }
}

#[async_trait]
impl DecisionLog for EventDecisionLog { /* §5 */ }
```

**Construction / DI:**

| Context | Construction |
| --- | --- |
| Production | `EventDecisionLog::from_handle(handle.clone(), storage)` after SQLite install |
| Tests | Same with in-memory/SQLite store |
| Injection | `Arc<dyn DecisionLog>` |

`EventDecisionLog` MUST NOT call `install_sqlite_event_sink`. Appends go through `RuntimeHandle`.

**Session existence (normative):** before every successful append, `get_session(session)` MUST return `Some`. Missing → `ObsError::Session(SessionError::NotFound(session))`. Orphan decision events (no session row) are **forbidden**.

### 3.7a `RecordingDecisionLog` (test double)

```rust
pub struct RecordingDecisionLog {
    retention: RetentionPolicy,
    records: Mutex<Vec<DecisionRecord>>,
    model_calls: Mutex<Vec<ModelCallRecord>>,
    tool_calls: Mutex<Vec<ToolCallRecord>>,
    next_seq: AtomicU64,
}

impl RecordingDecisionLog {
    pub fn new(retention: RetentionPolicy) -> Self;
    /// Returns records **post-retention/redaction** (what `EventDecisionLog` would have appended).
    pub fn recorded_decisions(&self) -> Vec<DecisionRecord>;
    // analogous accessors for model/tool (also post-retention)
}
```

No I/O; assigns monotonic fake `EventSeq` starting at 0. Applies `RetentionPolicy` on `record*` the same way as `EventDecisionLog` (minus persistence). For unit tests in 0007/0010/0013.

### 3.8 SessionEventType mappings (pinned)

| API | `SessionEventType` | Notes |
| --- | --- | --- |
| `DecisionLog::record` | `Decision` | Payload §5.3 |
| `DecisionLog::record_model_call` | `ModelCall` | Payload §5.4 |
| `DecisionLog::record_tool_call` | `ToolCall` | Payload §5.5 |
| `maybe_signal_budget_warning` | `BudgetWarning` | Emitted by RFC-0003 hook — **not** by DecisionLog directly |
| Append failure path | — | Return `ObsError`; MUST NOT invent a substitute success event |

Node state transitions remain `SessionEventType::NodeState` and are **owned by RFC-0010 / DAG RFCs**. This RFC MUST NOT emit `NodeState`.

### 3.9 `RetentionPolicy`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub retain_full_prompts: bool,
    pub retain_tool_bodies: bool,
}

impl RetentionPolicy {
    #[must_use]
    pub const fn defaults() -> Self {
        Self { retain_full_prompts: false, retain_tool_bodies: false }
    }
}

impl From<&RuntimeConfig> for RetentionPolicy {
    fn from(c: &RuntimeConfig) -> Self {
        Self {
            retain_full_prompts: c.retain_full_prompts,
            retain_tool_bodies: c.retain_tool_bodies,
        }
    }
}
```

Default when flags are false: metadata + hashes only; bodies stripped before append.

### 3.10 Hash helpers

```rust
/// SHA-256 lowercase hex via `Digest::sha256`.
#[must_use]
pub fn hash_content(bytes: &[u8]) -> Digest {
    Digest::sha256(bytes)
}

#[must_use]
pub fn hash_prompt(prompt: &str) -> Digest {
    Digest::sha256(prompt.as_bytes())
}

#[must_use]
pub fn hash_tool_body(body: &str) -> Digest {
    Digest::sha256(body.as_bytes())
}
```

**Stability:** identical UTF-8 bytes → identical `Digest`. Hash **before** redaction of secrets when the caller wants a hash of the original attributable content; DecisionLog hashing rules are pinned in §5.2.

### 3.11 Redaction helpers

```rust
/// Redact secret-like substrings in `text` (API keys, Bearer tokens, env assignments).
#[must_use]
pub fn redact_secrets(text: &str) -> String { /* §5.6 */ }

/// Apply prompt retention: always return content_hash when body/hash input exists;
/// return body only if `retain_full_prompts` after secret redaction + deny-list check.
pub fn apply_prompt_retention(
    prompt: Option<&str>,
    policy: RetentionPolicy,
) -> Result<(Option<Digest>, Option<String>), ObsError>;

/// Apply tool-body retention analogously using `retain_tool_bodies`.
pub fn apply_tool_retention(
    body: Option<&str>,
    policy: RetentionPolicy,
) -> Result<(Option<Digest>, Option<String>), ObsError>;
```

### 3.12 `CostMeter` / `CostSnapshot` / `SharedCostMeter`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Sum of reported USD amounts. `None` if **no** USD has been reported yet
    /// (including zero events). `Some(0.0)` only after at least one `usd: Some(_)` update
    /// that summed to zero — implementers MUST document via unit tests.
    /// Unknown/missing USD on a given update MUST NOT invent a value; that update
    /// simply does not change `usd_spent` when it was already `None`, and when it was
    /// `Some` leaves it unchanged (partial USD reporting is allowed).
    pub usd_spent: Option<f64>,
    pub model_calls: u64,
    /// Number of model-usage updates where input **or** output tokens were `None`.
    pub unknown_token_events: u64,
    pub by_tier: CostByTier,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostByTier {
    pub premium: TierCost,
    pub standard: TierCost,
    pub economy: TierCost,
    pub local: TierCost,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TierCost {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usd: Option<f64>,
    pub calls: u64,
}

/// Process-local incremental meter. Not shared across tasks by itself.
#[derive(Debug, Default, Clone)]
pub struct CostMeter {
    // private fields matching CostSnapshot accumulators
}

impl CostMeter {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Record model usage. `input`/`output`/`usd` use `None` for unknown — NEVER fabricate.
    /// Non-finite or negative finite `usd` → skip USD update, `tracing::warn`, tokens still apply.
    /// Cumulative meter/tier USD uses finite saturating add (never decreases; never overflows to ±∞).
    pub fn add_model_usage(
        &mut self,
        tier: ModelTier,
        input: Option<u64>,
        output: Option<u64>,
        usd: Option<f64>,
    );

    /// Feed a completed `WorkerMetrics`.
    ///
    /// Normative behaviour:
    /// - Uses `metrics.model_tier_used` for tier buckets.
    /// - Treats `input_tokens` / `output_tokens` as **known** (`Some`) — never bumps
    ///   `unknown_token_events` (producers lacking usage MUST call `add_model_usage` with `None`s).
    /// - Increments `model_calls` / tier `calls` once even when `error_class` is `Some`
    ///   (failed calls still count toward budget ceilings).
    /// - Ignores `confidence`, `duration_ms`, `tool_calls`, `cache_hits`, `provider_id` for metering
    ///   (provider attribution belongs on `ModelCall` events / RFC-0007).
    /// - `usd` non-finite or negative → same skip+warn as `add_model_usage`.
    pub fn add_worker_metrics(&mut self, metrics: &WorkerMetrics, usd: Option<f64>);

    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot;

    /// Map known token totals into `BudgetSnapshot`.
    /// `usd_spent` uses `0.0` when `CostSnapshot.usd_spent` is `None` **only for the
    /// BudgetSnapshot field type** (`BudgetSnapshot.usd_spent: f64` on main). The
    /// unknown-USD condition remains visible via `CostSnapshot.usd_spent.is_none()` and
    /// MUST NOT be presented as a measured zero cost in user-facing savings claims.
    #[must_use]
    pub fn to_budget_snapshot(&self) -> BudgetSnapshot;

    /// Compare against policy ceilings.
    #[must_use]
    pub fn check_budget(&self, policy: &BudgetPolicy) -> BudgetCheck;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    Ok,
    TokensExhausted,
    UsdExhausted,
    TokensAndUsdExhausted,
}

impl BudgetCheck {
    #[must_use]
    pub fn is_exhausted(self) -> bool { !matches!(self, Self::Ok) }
}

/// `Arc<Mutex<CostMeter>>` wrapper for concurrent producers (router/workers/scheduler).
#[derive(Clone, Default)]
pub struct SharedCostMeter {
    inner: Arc<std::sync::Mutex<CostMeter>>,
}

impl SharedCostMeter {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    pub fn add_model_usage(
        &self,
        tier: ModelTier,
        input: Option<u64>,
        output: Option<u64>,
        usd: Option<f64>,
    ) { /* lock; forward */ }

    pub fn add_worker_metrics(&self, metrics: &WorkerMetrics, usd: Option<f64>) { /* … */ }

    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot { /* … */ }

    #[must_use]
    pub fn to_budget_snapshot(&self) -> BudgetSnapshot { /* … */ }

    #[must_use]
    pub fn check_budget(&self, policy: &BudgetPolicy) -> BudgetCheck { /* … */ }

    /// Run a closure under the meter lock (keep critical sections short — §8).
    /// Non-reentrant: calling other `SharedCostMeter` methods inside `f` deadlocks.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut CostMeter) -> R) -> R { /* … */ }
}
```

**Poisoned mutex:** on poison, `SharedCostMeter` MUST recover via `PoisonError::into_inner` (same pattern as existing runtime locks on main) and continue; it MUST NOT panic.

`SharedCostMeter` and `EventDecisionLog` MUST implement `Debug` (manual impl OK for the mutex wrapper).

### 3.13 Budget warning helper

```rust
/// If `meter.check_budget(policy)` is exhausted, invoke
/// `SessionPlane::signal_budget_warning` with `meter.to_budget_snapshot()`.
/// Returns `Ok(None)` when under budget; `Ok(Some(seq))` when a warning was appended.
pub async fn maybe_signal_budget_warning(
    plane: &SessionPlane,
    session: SessionId,
    run: Option<RunId>,
    meter: &SharedCostMeter,
    policy: &BudgetPolicy,
) -> Result<Option<EventSeq>, ObsError>;
```

Message strings (normative prefixes):

| `BudgetCheck` | Message |
| --- | --- |
| `TokensExhausted` | `"budget exhausted: tokens"` |
| `UsdExhausted` | `"budget exhausted: usd"` |
| `TokensAndUsdExhausted` | `"budget exhausted: tokens and usd"` |

Callers (0007/0010) MAY then `request_replan(..., ReplanReason::BudgetPolicy)` or `cancel` — not automatic inside this helper (RFC-0003 §5.6).

### 3.14 Query helpers

```rust
#[derive(Debug, Clone)]
pub struct DecisionPage {
    pub events: Vec<SessionEvent>,
    /// Exclusive resume cursor: pass as `after` on the next call.
    /// `None` means the scan reached the end of the session log (no more events to scan).
    pub next_after: Option<EventSeq>,
}

/// Page matching `Decision` | `ModelCall` | `ToolCall`.
///
/// Uses only `EventStore::list_session_events` (dyn-safe). Ascending `seq` order.
///
/// - `limit == 0` → treated as `1` matching event max (same spirit as `clamp_events_page_limit`).
/// - `limit` is max **matching** events returned (clamped to `MAX_EVENTS_PAGE`).
/// - Internally scans store pages of size `clamp_events_page_limit(MAX_EVENTS_PAGE)` until
///   `events.len() == limit`, a store page returns short/empty, **or** `max_scan_pages` (normative default **16**) store pages have been read — then return with `next_after` set so the caller can resume.
/// - `next_after` is always the `seq` of the last **scanned** store event (matching or not)
///   when more store events may exist; `None` when the store page was short/empty.
pub async fn list_decision_events(
    store: &dyn EventStore,
    session: SessionId,
    after: Option<EventSeq>,
    limit: usize,
) -> Result<DecisionPage, ObsError>;

pub fn parse_decision_event(ev: &SessionEvent) -> Result<DecisionRecord, ObsError>;
pub fn parse_model_call_event(ev: &SessionEvent) -> Result<ModelCallRecord, ObsError>;
pub fn parse_tool_call_event(ev: &SessionEvent) -> Result<ToolCallRecord, ObsError>;
```

`parse_*` MUST require matching `ev.type_`; else `ObsError::Invalid`. Envelope `session_id` / `run_id` win over any payload echo.

`parse_model_call_event`: if wire `usage_unknown` disagrees with token nullness (`usage_unknown != (input is null || output is null)`), return `ObsError::Invalid("usage_unknown inconsistent")`.

`AlloyStorage::events()` returns `Arc<SqliteEventStore>` which coerces to `&dyn EventStore` after the §3.1a seam.

### 3.15 `WorkerMetrics` relationship (usage contract)

`WorkerMetrics` remains defined solely in `types::metrics` (RFC-0001). RFC-0004 MUST NOT redefine it.

| Producer (later RFC) | Contract |
| --- | --- |
| RFC-0013 workers | Populate `WorkerMetrics` on `CapabilityOutput`; MUST NOT duplicate model-call metering or recording for router-owned completions |
| RFC-0007 `TomlModelRouter` | Sole producer of `ModelCall` / `add_model_usage` for LLM `complete` attempts |
| RFC-0010 scheduler | MAY aggregate `SharedCostMeter` per run and call `maybe_signal_budget_warning` |

**RFC-0007 amendment:** `TomlModelRouter` owns both `DecisionLog::record_model_call` and `SharedCostMeter::add_model_usage` for every LLM completion it executes. Workers may report their broader `WorkerMetrics`, but MUST NOT call `add_worker_metrics`, `add_model_usage`, or `record_model_call` for that same routed completion; doing so would double-count usage. This supersedes the earlier worker-producer guidance wherever the completion is router-owned.

**Field rules:**

- `confidence: Option<f32>` — copy through to `ModelCallRecord.confidence`; `None` when unavailable.
- `input_tokens` / `output_tokens` on `WorkerMetrics` are `u64`. When usage is unknown, producers MUST NOT mint a fake `WorkerMetrics` with zeros to mean “unknown”; they MUST call `add_model_usage` with `None`s.
- `provider_id` / `model_tier_used` MUST be the actual provider/tier used (attribution owned by 0007; shape reused here).

### 3.16 `RuntimeMetrics` relationship

`RuntimeMetrics` (host phase/run counters) is **out of scope** for mutation by this RFC. Observability of DecisionLog/CostMeter failures uses `tracing` (§13), not `RuntimeMetrics` fields.

### 3.17 AlloyStorage integration

| Operation | API |
| --- | --- |
| Append decisions | `RuntimeHandle::append_session` (active sink = SQLite after install) |
| Query/replay | `storage.events()` (`Arc<SqliteEventStore>` / `&dyn EventStore` after §3.1a) + `list_decision_events` |
| Budget warning | `SessionPlane::signal_budget_warning` (appends via same handle) |

MUST NOT introduce `ObsStore`, dual-write, or artifact-CAS-as-decision-log for MVP.

---

## 4. Internal Module Design

```text
alloy-runtime/src/obs/
  mod.rs           # re-exports
  error.rs         # ObsError
  decision.rs      # DecisionKind, DecisionRecord, ModelCallRecord, ToolCallRecord, DecisionLog, EventDecisionLog
  cost.rs          # CostMeter, CostSnapshot, SharedCostMeter, BudgetCheck, CostByTier, TierCost
  hash.rs          # hash_content, hash_prompt, hash_tool_body
  redact.rs        # redact_secrets, apply_*_retention, deny lists
  query.rs         # list_decision_events, DecisionPage, parse_*
  budget.rs        # maybe_signal_budget_warning
  recording.rs     # RecordingDecisionLog
```

### Dependency direction

```text
obs → runtime::RuntimeHandle
obs → session::SessionPlane          (budget helper only)
obs → events::{SessionEvent*, NewSessionEvent, SessionEventType}
obs → storage::{EventStore, StoreError}
obs → config::RuntimeConfig          (RetentionPolicy)
obs → types::{ids, budget, metrics, diagnostic::ErrorClass}
obs → error::{RuntimeError, SessionError}
```

`session` / `storage` / `runtime` MUST NOT depend on `obs` (avoid cycles). Later RFCs (0007/0010/0013) depend on `obs` APIs.

### Ownership

| Object | Owner |
| --- | --- |
| `EventDecisionLog` | Caller-held `Arc` / local; clones `RuntimeHandle` |
| `SharedCostMeter` | Typically one per run (created by 0010/0015) or per session — **convention**, not enforced by type |
| Session event durability | RFC-0002 EventStore |
| Budget warning events | RFC-0003 hook |

### Redaction flow

Caller builds record → `EventDecisionLog` applies retention/redaction → builds `NewSessionEvent` payload → `RuntimeHandle::append_session` → EventSink/EventStore.

### Crate boundaries

Still exactly **five** workspace members. No `alloy-obs` crate. No OTLP dependency in `Cargo.toml` for this RFC.

---

## 5. Decision Recording

### 5.1 Recording algorithm (`DecisionLog::record`)

1. If `storage.sessions().get_session(rec.session)` is `None` → `ObsError::Session(NotFound)`.
2. Normalize `metadata`: `Null` → `{}`. If not `Object` after normalize → `ObsError::Invalid("metadata must be object")`. Key `idempotency_key` is reserved for caller dedupe (§5.8); DecisionLog does not interpret it beyond redaction.
3. Enforce size caps (§5.1a). Excess → `ObsError::Invalid`.
4. Compute body retention:
   - Let `raw = rec.prompt_body.as_deref()`.
   - `(hash, body) = apply_prompt_retention(raw, self.retention)?`.
   - If `rec.content_hash` is `Some(h)` and `raw` is `Some` and helper hash ≠ `h`: replace with helper hash and `tracing::warn` **once per record** (`content_hash mismatch; using recomputed`). If `raw` is `None`, keep caller `content_hash`.
5. Build **private** `DecisionPayload` JSON (§5.3) with stripped body when retention denies; `metadata` after `redact_json_strings`.
6. `handle.append_session(NewSessionEvent { session_id, run_id, type_: Decision, payload })`.
7. On success return `EventSeq`. On failure return `ObsError::Append` — **fail closed**.

#### 5.1a Size caps (normative)

| Field | Max |
| --- | --- |
| `metadata` JSON byte length (after normalize, before redaction) | 64 KiB |
| `prompt_body` / tool `body` UTF-8 bytes (pre-redaction) | 256 KiB |

Exceed → `ObsError::Invalid` (do not truncate silently — caller must hash and omit body). Caps are measured **pre-redaction**; `[REDACTED]` substitution may grow the appended payload by a bounded amount — do not re-reject after redaction.

#### 5.1b `record_model_call` / `record_tool_call` order

Same validation order as §5.1:

1. Session existence (`NotFound` / probe `StoreError` → `ObsError::Store`).
2. Size caps on bodies.
3. Non-finite `usd` on model calls → `ObsError::Invalid` (wire must not carry NaN/∞).
4. Retention / redaction / hash-mismatch warn-and-replace (same as §5.1 step 4).
5. Synthesize `usage_unknown` for model calls.
6. Append private payload; fail closed on sink error.

### 5.2 Content hash computation

| Input | Rule |
| --- | --- |
| Prompt/tool UTF-8 text | `Digest::sha256(text.as_bytes())` — hash of **pre-redaction** bytes when hashing attributable content |
| Empty string | Valid hash of empty input |
| No content | `content_hash: None` and no body fields |

Secret redaction for **retained bodies** happens after hashing: hash covers original attributable bytes; retained body is redacted. When retention is off, only the hash (if any) is stored.

### 5.3 `Decision` wire payload (private `DecisionPayload`)

```rust
#[derive(Serialize, Deserialize)]
struct DecisionPayload {
    kind: DecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<NodeId>,
    metadata: serde_json::Value,
    content_hash: Option<Digest>,
    prompt_body: Option<String>,
}
```

Wire JSON example:

```json
{
  "kind": "model_route",
  "node_id": "<uuid>",
  "metadata": { },
  "content_hash": "<64 lowercase hex>",
  "prompt_body": null
}
```

- No `session` / `run` fields on payload (envelope owns them).
- `node_id` omitted or null when `None`.
- Public `DecisionRecord` maps to this via field rename `node` → `node_id`.

### 5.4 `ModelCall` wire payload (private `ModelCallPayload`)

```rust
#[derive(Serialize, Deserialize)]
struct ModelCallPayload {
    node_id: Option<NodeId>,
    provider_id: ProviderId,
    model_tier: ModelTier,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    usage_unknown: bool, // synthesized: input.is_none() || output.is_none()
    usd: Option<f64>,    // never non-finite on wire
    duration_ms: Option<u64>,
    confidence: Option<f32>,
    error_class: Option<ErrorClass>,
    content_hash: Option<Digest>,
    prompt_body: Option<String>,
    #[serde(default)]
    endpoint_id: Option<EndpointId>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    route_event_seq: Option<EventSeq>,
    #[serde(default)]
    usd_source: Option<ModelUsdSource>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    provider_request_id: Option<String>,
}
```

| Field | Rule |
| --- | --- |
| `usage_unknown` | `true` iff `input_tokens.is_none() \|\| output_tokens.is_none()` at append time |
| `usd` | JSON `null` when unknown; MUST NOT write non-finite (reject before append as `Invalid`) |
| `prompt_body` | subject to `retain_full_prompts` + size cap |
| RFC-0007 attribution fields | Nullable and `#[serde(default)]`; pre-amendment events parse with `None` |

### 5.5 `ToolCall` wire payload (private `ToolCallPayload`)

```rust
#[derive(Serialize, Deserialize)]
struct ToolCallPayload {
    node_id: Option<NodeId>,
    tool_name: String,
    tool_server: Option<String>,
    latency_ms: Option<u64>,
    denied: bool,
    content_hash: Option<Digest>,
    body: Option<String>,
}
```

`body` subject to `retain_tool_bodies` + redaction + deny list + size cap.

### 5.6 Secret redaction & deny lists

**Dependency:** MVP MUST implement matchers **hand-rolled** (string scans / simple state machines). **MUST NOT** add a `regex` crate dependency for RFC-0004.

**`redact_secrets` replacement rule:** each match span is replaced with the literal `[REDACTED]` (the matched text including prefixes like `api_key=` is entirely replaced — `api_key=sk-1` → `[REDACTED]`, not `api_key=[REDACTED]`). Case-insensitive for ASCII letters. Leftmost-longest; non-overlapping; scan left to right.

**Token boundary for env-style names:** the secret name must be bounded on the left by start-of-string or a non-alphanumeric/`_` character (so `MY_API_KEY=abc` matches as a whole assignment starting at `API_KEY` **only if** using substring — **normative:** require the name to match as a full identifier token: `[A-Za-z_][A-Za-z0-9_]*` equality against the deny-name set after lowercasing, **or** the identifier ends with `_api_key` / `_secret` / `_token` / `_password`. `MY_API_KEY=abc` → entire `MY_API_KEY=abc` replaced with `[REDACTED]`.

**Bare `sk-` tokens:** `sk-` + 8+ alphanumeric characters is redacted in **both** free text (`redact_secrets`) and JSON string leaves.

**MUST mask at least:**

| Pattern class | Detection (hand-rolled) |
| --- | --- |
| Env-style assignment | name in `{api_key, api-key, secret, token, password, authorization}` (case-insensitive) followed by optional spaces, `=`, optional spaces, then one or more non-whitespace → redact whole assignment span |
| Bearer headers | `authorization:` (ci) + whitespace + `bearer` (ci) + whitespace + non-whitespace token |
| PEM private key blocks | from `-----BEGIN` through `PRIVATE KEY-----` … matching `-----END …-----` |

**JSON key-name deny-list (`redact_json_strings`):** recursively walk `Value`. For each `Object`, if a key’s ASCII-lowercased form equals or contains `api_key` / `api-key` / `secret` / `password` / `token` / `authorization` / `credential`, replace that entry’s **value** with `"[REDACTED]"` (whether string or nested). For every `Value::String` leaf, also apply `redact_secrets`. Bare token strings matching `sk-` + 8+ alphanumerics → `[REDACTED]` (same rule as `redact_secrets`).

**Path deny list:** if retained body contains a path segment equal to `.env` or ends with `/.env`, strip retained body (keep hash), `tracing::warn`. Not an `ObsError`.

Helper misuse → `ObsError::Redaction`.

### 5.7 Prompt / tool retention

| Flag | Behaviour |
| --- | --- |
| `retain_full_prompts=false` (default) | Store `content_hash` when available; `prompt_body` always `null` on wire |
| `retain_full_prompts=true` | Store redacted `prompt_body` unless deny-list strips it |
| `retain_tool_bodies=false` (default) | Store `content_hash` when available; `body` always `null` |
| `retain_tool_bodies=true` | Store redacted `body` unless deny-list strips it |

### 5.8 Idempotency

MVP DecisionLog is **append-only** and **not** automatically idempotent. Duplicate `record*` calls create duplicate events with new `EventSeq` values.

Callers that need dedupe MUST:

1. Include a caller-defined key under `metadata.idempotency_key` (string), and
2. Query via `list_decision_events` / payload inspect before re-append.

DecisionLog MUST NOT silently drop duplicates.

### 5.9 Append failure behaviour

| Failure | Behaviour |
| --- | --- |
| `RuntimeError` / `EventSinkError` from append | `ObsError::Append`; no seq consumed (0002) |
| Session missing | `ObsError::Session(NotFound)` before append |
| Session existence probe `StoreError` | `ObsError::Store` |
| Partial redaction / deny-list strip | Still append metadata+hash; not a failure |
| Missing config on `from_handle` | `ObsError::Append(RuntimeError::InvalidPhase { … })` via `handle.config()?` |

**Phase note (normative vs incorrect draft claim):** `RuntimeHandle::append_session` has **no** phase gate on main — it can succeed in `Draining`. `SessionPlane::signal_budget_warning` **does** require `Running` via `require_mutating_phase`. During drain: decision appends may still succeed; budget signaling returns `SessionError::Invalid` → `ObsError::Session`. Callers MUST treat signaling failure during drain as non-retryable for that process.

MUST NOT emit synthetic `SessionEventType::Error` as a substitute for a failed decision append.

### 5.10 Sequence — decision → redact → append → replay

```mermaid
sequenceDiagram
  participant Prod as Producer (0007/0010/0013)
  participant DL as EventDecisionLog
  participant Red as redact/retention
  participant H as RuntimeHandle
  participant ES as EventSink/EventStore
  participant Q as list_decision_events

  Prod->>DL: record(DecisionRecord)
  DL->>Red: apply_prompt_retention + redact_json_strings
  Red-->>DL: hash + optional body
  DL->>H: append_session(Decision payload)
  H->>ES: append_session
  ES-->>H: EventSeq
  H-->>DL: EventSeq
  DL-->>Prod: Ok(seq)

  Note over Prod,Q: Later / after restart
  Prod->>Q: list_decision_events(store, session, after, limit)
  Q->>ES: list_session_events (paged)
  ES-->>Q: SessionEvent page
  Q-->>Prod: filtered Decision/ModelCall/ToolCall
```

---

## 6. Cost Metering

### 6.1 Incremental updates

`CostMeter::add_model_usage`:

1. `model_calls += 1`; tier `calls += 1`.
2. If `input` is `Some(n)`: saturating-add to `tokens_in` and tier `tokens_in`.
3. If `output` is `Some(n)`: saturating-add to `tokens_out` and tier `tokens_out`.
4. If `input.is_none() || output.is_none()`: `unknown_token_events += 1` (**at most once per call**).
5. If `usd` is `Some(x)`:
   - If `!x.is_finite()`: do **not** update USD; `tracing::warn!("non-finite usd ignored")`.
   - Else if `x < 0.0`: do **not** update USD; `tracing::warn!("negative usd ignored")`.
   - Else apply **finite saturating add** to meter `usd_spent` and the matching tier `usd`:
     - If the field is `None`, set `Some(x)` (already finite and ≥ 0).
     - If the field is `Some(cur)`, let `sum = cur + x`. If `sum.is_finite()`, store `Some(sum)`;
       otherwise store `Some(f64::MAX)` (explicit overflow saturation — totals MUST NOT become ±∞
       or decrease).
   - If `usd` is `None`, leave `usd_spent` unchanged.

Token counters use `u64::saturating_add`. USD totals MUST NOT decrease and MUST NOT leave the
finite non-negative range except via the `f64::MAX` saturation above. Budget compare uses `>=`
on finite values only.

### 6.2 `CostSnapshot` semantics

`snapshot()` returns a deep copy of counters. It is a point-in-time view — not durable by itself. Durability of usage is via `ModelCall` events (and optional decision metadata), not a separate cost table.

### 6.3 Token accounting

Known tokens accumulate. Unknown tokens do not invent zeros into “measured” marketing fields. `unknown_token_events` exists so CLI/eval can show incomplete metering without fabricating usage.

### 6.4 Currency accounting

USD is optional end-to-end. Missing provider USD MUST NOT be replaced with estimates inside RFC-0004. Pricing tables belong to RFC-0007 (if any) and MUST pass `Some(usd)` only when known.

### 6.5 Unknown usage behaviour (normative)

| Situation | Required behaviour |
| --- | --- |
| Provider omits usage | `add_model_usage(..., None, None, None)`; `usage_unknown: true` on event |
| Provider omits USD only | tokens recorded; `usd: null` |
| Overflow | saturate tokens (`u64::saturating_add`); saturate USD to `f64::MAX`; do not panic |
| Negative finite USD | ignore + warn (same class as non-finite); tokens still apply |
| Marketing savings % | **MUST NOT** appear in code, docs strings, or event payloads |

### 6.6 Interaction with `BudgetPolicy`

```text
max_tok = policy.max_tokens_per_run   // u64 — always finite; no is_finite check
max_usd = policy.max_usd_per_run

If max_tok == 0: TokensExhausted is true even at zero spend (immediately exhausted).
TokensExhausted  iff tokens_in.saturating_add(tokens_out) >= max_tok

If !max_usd.is_finite() || max_usd < 0.0: treat as UsdExhausted immediately (fail closed).
Else UsdExhausted iff usd_spent.is_some() && usd_spent.unwrap() >= max_usd
```

`TokensExhausted` MUST use `u64::saturating_add` for the sum (wrapping `+` is forbidden). Threshold
semantics remain `>=`. When `usd_spent` is `None` and `max_usd` is finite and ≥ 0, USD ceiling MUST NOT trigger.

`to_budget_snapshot()`:

```rust
BudgetSnapshot {
    usd_spent: self.snapshot().usd_spent.unwrap_or(0.0),
    tokens_in: ...,
    tokens_out: ...,
}
```

**`Constraint::MaxUsd`:** NOT evaluated by RFC-0004. Owned by goal/scheduler policy in **RFC-0010** / CLI in **RFC-0015**. Listed under Non-goals.

### 6.7 Interaction with `signal_budget_warning`

`maybe_signal_budget_warning`:

1. `check = meter.check_budget(policy)`.
2. If `Ok`, return `Ok(None)` (no await on session plane required).
3. If exhausted, `plane.signal_budget_warning(session, run, meter.to_budget_snapshot(), message).await?` and return `Ok(Some(seq))`.
4. MUST NOT hold the `SharedCostMeter` lock across the `signal_budget_warning` await: snapshot under lock, drop lock, then await.

Repeated calls while still exhausted will append multiple `BudgetWarning` events (same as RFC-0003 hook semantics). Callers that want single-shot warnings MUST gate locally.

### 6.8 Required contracts for later RFCs

| RFC | MUST |
| --- | --- |
| **0007** | `TomlModelRouter` is the sole LLM-completion producer: log every route via `DecisionLog::record(kind=ModelRoute, …)`; on complete, `record_model_call` + `CostMeter` update with provider usage; never hardcode savings |
| **0010** | Own per-run `SharedCostMeter` lifecycle; call `maybe_signal_budget_warning` after usage updates that can cross ceilings; emit `NodeState` itself (not via DecisionLog) |
| **0013** | Fill `WorkerMetrics` honestly; do not duplicate router-owned `ModelCall` / meter updates; `confidence` remains `Option<f32>` |
| **0015** | Use `list_decision_events` / snapshots for `alloy events` display; print budget warnings; no OTLP |
| **0016** | Only publish calibrated cost bands from holdout — MUST NOT read fabricated RFC-0004 estimates |

---

## 7. Persistence Integration

### 7.1 EventStore durability

All decision/model/tool events use the same durability as RFC-0002 session events (WAL / `synchronous` policy from storage open options). RFC-0004 adds **no** weaker path and **no** parallel DB.

### 7.2 Payload JSON

Payloads are `serde_json::Value` objects per §5.3–5.5. Field names are snake_case. Digests are 64-char lowercase hex strings. Enums use existing serde representations from RFC-0001 types.

### 7.3 `StoreError` mapping

| Path | Mapping |
| --- | --- |
| Append via handle | `RuntimeError` → `ObsError::Append` (existing `EventSinkError`/`StoreError` mapping inside handle/storage unchanged) |
| Query helpers | `StoreError` → `ObsError::Store` |
| Budget hook | `SessionError` → `ObsError::Session` (`store_to_session` already applied inside 0003) |

### 7.4 Replay

`EventStore::replay_session` / `list_session_events` return bit-identical `(seq, ts, type, payload)` after restart (RFC-0002). Query helpers filter those events; they MUST NOT rewrite payloads.

### 7.5 Restart recovery

| State | Recovery |
| --- | --- |
| Durable appended decision events | Visible via query helpers after reopen + install |
| In-memory `CostMeter` | **Not** durable — rebuild via `reaccumulate_cost_from_events` |

#### `reaccumulate_cost_from_events` (MVP required)

```rust
pub async fn reaccumulate_cost_from_events(
    store: &dyn EventStore,
    session: SessionId,
    run: Option<RunId>,
) -> Result<CostMeter, ObsError>;
```

Behaviour:

1. Page all session events via `list_session_events` (not `replay_session` on a trait object).
2. For each `ModelCall`, parse payload; on parse error → `ObsError::Invalid` (fail the rebuild).
3. **Run filter:**
   - `run: Some(id)` — include only events whose envelope `run_id == Some(id)`.
   - `run: None` — include **all** `ModelCall` events in the session (every run).
4. Apply `add_model_usage` with parsed `Option` token/usd fields and `model_tier` (skip non-finite / negative usd per §6.1; saturate overflow).
5. Ignore non-`ModelCall` events.
6. Return rebuilt `CostMeter`.

**Invariant:** rebuild is complete only for usage that was recorded via `record_model_call`. Meter-only updates without a durable `ModelCall` are **lossy** on restart. `TomlModelRouter` MUST pair router-owned completion metering with `record_model_call`; workers MUST NOT duplicate that pair. Document this in residual comments / this section — not best-effort silent.

---

## 8. Concurrency

| Rule | Normative behaviour |
| --- | --- |
| `EventDecisionLog` | `Send + Sync`; concurrent `record*` allowed; EventSink serializes appends (0001/0002) |
| `CostMeter` | Single-threaded via `&mut`; not shared bare across tasks |
| `SharedCostMeter` | `std::sync::Mutex` (match existing runtime locks on main); lock only for counter updates |
| Lock across await | MUST NOT hold `SharedCostMeter` lock across `append_session` or `signal_budget_warning` |
| Session locks | `maybe_signal_budget_warning` MUST NOT take session locks itself; the 0003 hook takes its own per-session lock internally |
| Control plane | Observability MUST NOT hold RFC-0003 per-run locks |

Concurrent appenders of decisions are safe. Cost aggregation races are resolved by mutex ordering; lost updates MUST NOT occur under `SharedCostMeter` APIs.

---

## 9. Async Model

- `DecisionLog` methods are `async` via `async_trait` (M1).
- Hash/redaction/retention helpers are **sync** and MUST NOT block on I/O.
- `CostMeter` APIs are **sync**.
- SQLite remains on `spawn_blocking` inside storage (0002). `obs` MUST NOT add nested `spawn_blocking`.
- Prefer: await `RuntimeHandle` / `SessionPlane` / `EventStore` futures only.
- Tokio multi-thread runtime assumed (workspace pin).

---

## 10. Shutdown and Durability

| Event | Guarantee |
| --- | --- |
| Successful `append_session` commit | Decision/model/tool event durable per RFC-0002 |
| Crash before append commit | Event absent; caller receives error if still running |
| Graceful drain/shutdown | `append_session` has **no** phase gate (may still succeed in `Draining`); `signal_budget_warning` fails when not `Running` |
| In-memory `CostMeter` on crash | Lost; rebuild via `reaccumulate_cost_from_events` (ModelCall-backed only) |
| Budget warning commit | Durable via 0003/0002 when phase allows |

Durability of observability data equals EventStore durability. No extra fsync API.

---

## 11. Error Handling

### Recoverable vs fatal

| Class | Examples | Caller action |
| --- | --- | --- |
| Recoverable | `ObsError::Append` (sink/io); `Session` NotFound; `Store` Busy | Retry or surface; do not invent events |
| Invalid input | `ObsError::Invalid` (metadata, caps, usd, usage_unknown) | Fix caller record |
| Drain-phase budget warn | `ObsError::Session(Invalid)` | Non-retryable in this process |

### Append failures

Fail closed for that record. Do not degrade by writing a different event type unless the caller explicitly does so.

### Redaction failures

Deny-list hits strip bodies (warn). True helper misuse → `ObsError::Redaction`.

### Metering failures

`CostMeter` updates do not I/O; they do not fail except mutex poison recovery (non-fatal). Budget helper failures surface as `ObsError::Session` / mapped errors.

---

## 12. Configuration

**No new configuration keys required.**

Reuse:

| Key / field | Source |
| --- | --- |
| `retain_full_prompts` | `RuntimeConfig` ← profile `[observability]` (RFC-0001) |
| `retain_tool_bodies` | same |
| Data dir / storage open | RFC-0001 / RFC-0002 |

**MUST NOT** write or overwrite `.env`.  
**MUST NOT** modify `example.env` for this RFC (no new keys).

Profile defaults remain:

```toml
[observability]
retain_full_prompts = false
retain_tool_bodies = false
```

---

## 13. Observability of Observability

Use existing `tracing` facade (RFC-0001 logging). No OTLP. No marketing metrics.

| Event | Level | Fields |
| --- | --- | --- |
| Decision append failure | `error` | `session_id`, `run_id`, `kind`, `error` |
| Model/tool append failure | `error` | `session_id`, `run_id`, `error` |
| Retention deny-list stripped body | `warn` | `session_id`, `reason` |
| Content hash mismatch replaced | `warn` | `session_id` |
| Budget warning helper invoked | `warn` | `session_id`, `run_id`, `check` (hook also logs) |
| Meter lock poison recovered | `error` | — |

Counters: optional private atomics under `obs` for tests (`records_appended`, `record_failures`) MAY exist but MUST NOT invent savings percentages or OTLP exporters.

---

## 14. Testing Strategy

### Unit tests (`obs` module / `#[cfg(test)]`)

| Test | Asserts |
| --- | --- |
| `hash_prompt_stable` | Same string → same `Digest`; matches `Digest::sha256` |
| `hash_tool_body_stable` | Same as above for tool bodies |
| `prompt_redaction_strips_api_key` | `api_key=sk-1` → `[REDACTED]` entirely |
| `json_key_redaction_masks_secret_values` | `{"api_key":"sk-…"}` value becomes `[REDACTED]` |
| `tool_redaction_strips_env_path` | deny-list prevents retained body when `.env` path present |
| `prompt_deny_list_strips_env_path` | same for retained prompts |
| `retention_default_strips_prompt_body` | `retain_full_prompts=false` → body `None`, hash `Some` |
| `retention_opt_in_keeps_redacted_body` | `true` → body present and redacted |
| `retention_tool_bodies_default_off` | analogous for tools |
| `cost_snapshot_arithmetic` | known token/usd sums; all four tiers distinct |
| `cost_unknown_usage_no_fabricated_tokens` | `None` inputs → `unknown_token_events`, tokens unchanged |
| `cost_unknown_usd_none` | no usd updates → `usd_spent.is_none()` |
| `cost_non_finite_usd_ignored` | NaN/∞ skipped; finite usd still works after |
| `cost_negative_usd_ignored` | finite `usd < 0` skipped + warn; meter/tier totals unchanged; later finite ≥ 0 still applies |
| `cost_usd_overflow_saturates` | large finite adds that would yield ±∞ store `f64::MAX`; totals never decrease |
| `add_worker_metrics_counts_failed_calls` | `error_class: Some(_)` still increments calls; tokens known |
| `budget_check_tokens_and_usd` | thresholds with `>=`; token sum via `saturating_add` |
| `budget_zero_tokens_immediately_exhausted` | `max_tokens_per_run=0` → exhausted at zero |
| `budget_non_finite_usd_ceiling_exhausted` | fail closed |
| `decision_kind_serde_golden` | wire JSON locked |
| `decision_payload_no_session_fields` | wire payload omits session/run |
| `usage_unknown_consistency_parse` | contradicting flag → Invalid |
| `worker_metrics_confidence_option` | `ModelCallRecord` round-trips `confidence: None` |
| `shared_cost_meter_no_lost_updates` | concurrent adds; totals match |
| `shared_cost_meter_poison_recovers` | poisoned mutex → into_inner, continues |
| `metadata_rejects_non_object` | array/string → Invalid |
| `size_cap_rejects_huge_body` | >256 KiB → Invalid |

### Integration tests (`tests/obs_rfc0004.rs`)

| Test | Asserts |
| --- | --- |
| `decision_append_and_list` | record → `DecisionPage` with hash, null body under defaults |
| `model_call_and_tool_call_round_trip` | parse helpers restore records |
| `list_decision_events_cursor` | `next_after` resumes without skipping/duping |
| `replay_after_restart` | close/reopen; same seq/type/payload JSON |
| `opt_in_prompt_retention` | flags true → body present (redacted) |
| `budget_warning_hook_integration` | past policy → `BudgetWarning` |
| `budget_warning_fails_when_draining` | phase drain → `ObsError::Session` |
| `reaccumulate_cost_from_events` | rebuild matches; `run: None` aggregates all runs |
| `reaccumulate_run_filter` | `Some(run)` excludes other runs |
| `session_missing_rejects_record` | no session row → NotFound |
| `append_failure_surfaces_obs_error` | `AlloyStorage::close()` then append → `ObsError` |
| `never_writes_dotenv` | `.env` sentinel untouched |
| `obs_module_not_imported_by_session_storage_runtime` | static/architecture check or module graph comment test |

Harness: reuse RFC-0002/0003 patterns (`AlloyRuntime`, `install_sqlite_event_sink`, `SessionPlane`, temp `data_dir`).

---

## 15. MVP vs Deferred

### MVP (this RFC)

- `DecisionLog` / `EventDecisionLog` / `RecordingDecisionLog`
- `DecisionRecord` / `DecisionKind` / `ModelCallRecord` / `ToolCallRecord` + private wire payloads
- `CostMeter` / `CostSnapshot` / `SharedCostMeter` / `BudgetCheck` / `CostByTier` / `TierCost`
- Hash helpers / redaction helpers / `RetentionPolicy`
- Query helpers (`DecisionPage`) + `reaccumulate_cost_from_events`
- `maybe_signal_budget_warning`
- `ObsError`
- Additive `EventStore::replay_session` `where Self: Sized`
- Tests in §14

### Deferred (do not implement here)

| Item | Owner |
| --- | --- |
| ModelRouter / provider attribution producers | **RFC-0007** |
| Scheduler execution / node_state producers | **RFC-0010** |
| Worker implementations producing `WorkerMetrics` | **RFC-0013** |
| `alloy events` CLI UX | **RFC-0015** |
| Eval-calibrated cost bands / savings publication | **RFC-0016** |
| OTLP export | **V2 deferred** |
| Observability TUI | **V2 deferred** |

Do not invent additional deferred work in this RFC.

---

## 16. Acceptance Criteria

Implementation checklist — all items REQUIRED before merge:

- [ ] Default log = **metadata + content hashes only** (`retain_*=false`)
- [ ] Opt-in full prompts / tool bodies honored via `RuntimeConfig` / `RetentionPolicy`
- [ ] Secret redaction (incl. JSON key-name deny-list) + `.env` path deny-list; hand-rolled (no `regex` crate)
- [ ] Size caps enforced (64 KiB metadata / 256 KiB bodies)
- [ ] `DecisionLog` maps to `SessionEventType::{Decision, ModelCall, ToolCall}` exactly as §3.8; private wire payloads
- [ ] Session existence checked before append (no orphans)
- [ ] Reusable `CostMeter` / `SharedCostMeter` / `CostSnapshot` APIs; non-finite / negative USD ignored; USD saturate to `f64::MAX`; `TokensExhausted` uses `saturating_add`
- [ ] Unknown provider usage NEVER fabricates tokens or USD
- [ ] `EventStore::replay_session` gains `where Self: Sized` (§3.1a); helpers use `&dyn EventStore` + `list_session_events`
- [ ] `DecisionPage` cursor semantics implemented
- [ ] Persistence is **EventStore-only** — no parallel obs DB
- [ ] No OTLP crate / exporter; no `regex` crate added for this RFC
- [ ] No numeric savings claims in code or docs output
- [ ] `maybe_signal_budget_warning` integrates with RFC-0003 hook; drain-phase behavior documented
- [ ] `WorkerMetrics.confidence: Option<f32>` compatibility preserved
- [ ] `reaccumulate_cost_from_events` rebuilds meter; `run: None` = all runs; ModelCall-backed only
- [ ] Unit + integration tests in §14 passing (incl. concurrency, poison, redaction, caps)
- [ ] `cargo fmt -p alloy-runtime -- --check` clean
- [ ] `cargo clippy -p alloy-runtime --all-targets -- -D warnings` clean
- [ ] `session` / `storage` / `runtime` do not depend on `obs`
- [ ] Workspace still **≤5 crates** / exactly five members
- [ ] `.env` never written; `example.env` policy preserved
- [ ] Crate root re-exports updated explicitly (no glob); `#![deny(missing_docs)]` satisfied
- [ ] Series [Definition of Done](./README.md#definition-of-done-merge-gate) satisfied

## Definition of Done

Merge only when the series [Definition of Done](./README.md#definition-of-done-merge-gate) is fully met:

- [ ] Architecture compliance: **PASS** — V2 §15 / ADR F-17 / §5.6 / §18 boundaries hold; no new crate, no new service, no parallel event log
- [ ] RFC acceptance criteria: **100% satisfied** (§16)
- [ ] Unit tests: **passing**
- [ ] Integration tests: **passing** — `tests/obs_rfc0004.rs` (and existing suites still green)
- [ ] Documentation: **complete** — this RFC matches the implementation
- [ ] Public APIs: **reviewed and stable** — signatures match §3
- [ ] Clippy: **clean**
- [ ] Formatting: **clean**
- [ ] No TODO or placeholder implementations left in this RFC’s scope (explicit **Stub** / deferred only)
- [ ] Code review: **approved**

---

## 17. Open Questions

Only genuine unresolved implementation spikes — settled V2/0001/0002/0003 decisions are not reopened.

1. **`SharedCostMeter` lock library:** MVP pins `std::sync::Mutex` for consistency with `RuntimeInner` locks on main. If contention profiling during 0010 dogfood shows need, a follow-up MAY switch to `tokio::sync::Mutex` only if all meter APIs become async — out of scope unless required.
2. **Multi-warning suppression:** MVP allows repeated `BudgetWarning` events while exhausted. If CLI noise is painful, RFC-0015 MAY gate display; a single-shot flag on `SharedCostMeter` is deferred until then.

**Settled (do not reopen):**

- ADR F-17 metadata+hashes default; prompts/bodies opt-in
- No separate OTel/OTLP crate in MVP; no `regex` crate for this RFC
- No numeric savings claims until Eval (V2 §18 / ADR F-08)
- EventStore is the only decision persistence substrate (RFC-0002)
- `WorkerMetrics.confidence` is `Option<f32>` on main
- Budget **signaling** is `SessionPlane::signal_budget_warning` (RFC-0003); metering invokes it and does not own cancel/replan
- RFC index depends-on for 0004 remains 0001+0002; 0003 integration is in-crate
- ≤5 crates; never write `.env`
- `SessionEventType` vocabulary is fixed by RFC-0001 — do not add variants for obs
- Node state events are not owned by this RFC
- Unknown usage MUST NOT fabricate values
- `append_session` has no phase gate; budget warning requires `Running`
- Orphan decision appends forbidden (session row required)
- `Constraint::MaxUsd` not owned here
- Wire payloads are private structs distinct from public records

---

## Estimated implementation effort

**2–4 person-days** (aligned with RFC index).

Suggested split: `obs` module skeleton + `ObsError`/types (0.5d) · DecisionLog + redaction/hash (1d) · CostMeter + budget helper (0.5–1d) · query + reaccumulate (0.5d) · tests/integration (1d).

---

*— arkadianet*
