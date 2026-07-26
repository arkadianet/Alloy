# RFC-0007: Model Router & Provider

| Field | Value |
| --- | --- |
| **Status** | Draft |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md) (merged), [RFC-0004](./RFC-0004-observability-cost-metering.md) (merged) |
| **Effort** | 7–9 person-days |
| **Related RFCs** | [0005](./RFC-0005-sandbox-broker.md) sandbox posture / `Grant::Network` · [0006](./RFC-0006-mcp-host-builtins.md) recording-seam pattern · [0010](./RFC-0010-scheduler-runtime-adapters.md) retry consumer · [0012](./RFC-0012-context-engine.md) full `PromptPack` · [0013](./RFC-0013-capability-registry-workers.md) workers · [0016](./RFC-0016-eval-harness-holdout-gates.md) `ScriptedProvider` |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | Draft outline of this filename (expanded to implementation grade) |

**Mental model (V2 §11 / ADR F-20 / BYOM):** Models are plugins behind `ModelRouter`. Core MUST NOT hardcode vendor model IDs. MVP is TOML `capability → tier` plus **one** openai-compatible HTTP provider. Scoring weights are stubbed and unused. This RFC is the **first real LLM call** in the workspace and the **first producer** of the RFC-0004 cost / decision schema.

**Authority order (highest → lowest):** current `main` source → merged RFCs 0001–0006 → Architecture V2 → this draft → roadmaps. Never reshape a merged public API solely to match an older V2 sketch or this document’s prior outline.

---

## 1. Overview

### 1.1 Purpose

Ship the MVP **Model Router & Provider** as a module in `alloy-runtime`:

1. **`ModelRouter` + `ModelProvider` traits** matching Architecture V2 §11.2, with complete supporting types.
2. **`TomlModelRouter`** loading the authoritative `router.toml` schema (`[policy]`, `[[providers]]`, `[[providers.endpoints]]`, `[capability_tiers]`).
3. **One** `OpenAiCompatibleProvider` that performs non-streaming HTTP chat completions using `api_key_env`.
4. **RFC-0004 integration** — every route and completion is decision-logged; every provider usage update feeds `CostMeter` / `SharedCostMeter` without fabricating tokens or USD.
5. **Recording / scripted seam** so RFC-0016 `ScriptedProvider` and in-crate tests implement `ModelProvider` without network.
6. **First network dependency** — justified HTTP client, TLS, feature gate, timeouts, redirect/proxy/reuse policy; retry loops owned by RFC-0010.

### 1.2 Problem Statement

RFC-0001 published `ModelTier`, `ProviderId`, `CapabilityId`, `BudgetSnapshot`, and a provisional router peek that only reads `api_key_env` from a transitional `[provider.*]` map. RFC-0004 published `CostMeter`, `ModelCallRecord`, `DecisionLog`, and the wire `usage_unknown` consistency invariant — **before any provider existed**. Architecture V2 §11 requires a BYOM tier router with no hardcoded model IDs. Without this RFC there is no `ModelRouter`, no HTTP provider, no producer of `ModelCall` events from real usage, and no way for workers or Eval to complete a model call — the control-plane thesis cannot be exercised end-to-end.

### 1.3 Scope

| In scope | Detail |
| --- | --- |
| Traits | `ModelRouter`, `ModelProvider` |
| Concrete router | `TomlModelRouter` |
| Concrete provider | `OpenAiCompatibleProvider` (exactly one kind: `openai_compatible`) |
| Config | Full `router.toml` schema; update `router.toml.example`; document `example.env` keys |
| Tiers | Premium / Standard / Economy / Local via existing `ModelTier` |
| Health | `health()` stub always `Healthy` |
| Observability | Decision-log every route / complete; meter every provider usage outcome |
| Cost → USD | Config price table → optional `usd`; honour V2 §18 / ADR F-08 |
| Recording seam | `RecordingModelProvider` + `ScriptedProvider` contract for RFC-0016 |
| Minimal `PromptPack` | Runtime IR struct; RFC-0012 upgrade path; no collision with `ArtifactKind::PromptPack` |
| Config amendment | Replace provisional RFC-0001 `[provider.*]` peek with file-existence + RFC-0007 ownership |
| Tests | Unit, integration (wiremock), negative, budget, cancel, concurrency, no-hardcoded-IDs |

### 1.4 Non-goals

| Deferred item | Owner |
| --- | --- |
| Multi-factor scoring / multi-provider failover | **ADR F-20** — deferred; weights stubbed |
| Second provider kind or second live provider | Deferred (V2 evolution) |
| Capability worker prompts / `CapabilityContext` | **RFC-0013** |
| Full three-domain `PromptPack` assembly | **RFC-0012** |
| Retry / backoff loops on provider errors | **RFC-0010** (consumes this taxonomy) |
| Streaming chat completions | Deferred — MVP non-streaming (§3.8) |
| Cost marketing numbers / savings claims | **Forbidden** (V2 §18.2 / ADR F-08) |
| OTLP, sixth crate, new OS service, plugin framework | Forbidden |
| Writing or overwriting `.env` | Forbidden |
| Sandbox redesign / moving provider HTTP into the jail | Out of scope — see §2.6 |

### 1.5 Day-1 MVP (normative)

1. `TomlModelRouter::from_paths(...)` MUST load the §7 schema, resolve exactly one `openai_compatible` provider, and fail closed on invalid config or missing/empty `api_key_env`.
2. `route` MUST select tier from `[capability_tiers]` else `[policy].default_tier`, select the first matching endpoint, enforce budget denial **without** tier escalation/downgrade, and record `DecisionKind::ModelRoute` (or `Budget` on denial).
3. `complete` MUST call the selected provider once (no retry loop), map the openai-compatible response into `ModelResponse` + RFC-0004 records, and update `SharedCostMeter` when injected.
4. `OpenAiCompatibleProvider::health` MUST return `Health::Healthy` unconditionally (stub).
5. `[policy].scoring` weights, if present, MUST be parsed and MUST NOT affect routing.
6. Core Rust sources under `alloy-runtime` (excluding tests that assert absence) MUST contain **zero** vendor model-ID string literals and **zero** `match` arms on provider vendor brands for model selection.
7. Alloy MUST NEVER write `.env`. Missing keys fail closed with errors that point at `example.env`.

---

## 2. Architecture Integration

### 2.1 Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §11.1 Requirements | Provider-agnostic; tiers Premium/Standard/Economy/Local; no hardcoded model names |
| §11.2 Architectural interface | `ModelRouter` + `ModelProvider`; capability → tier policy |
| §11.2 MVP | TOML map + one openai-compatible provider; health stub |
| §11.2 Example capability keys | Mixed-case examples are accepted by normalizing to lowercase and rejecting post-normalization duplicates |
| §11.2 Deferred | Multi-factor scoring, residency finesse (ADR F-20) |
| §11.2 Stub | `health()` always Healthy; scoring unused |
| §18.1–18.2 | Budgets + metering APIs normative; cost marketing numbers forbidden |
| §14.2 / ADR F-07 | Sandbox-before-dogfood remains in force for tool exec; this RFC’s host egress is separate (§2.6) |
| §14.4 | Credentials via `api_key_env`; redaction defaults |

### 2.2 Relationship to RFC-0001

Authoritative for: `ModelTier`, `BudgetSnapshot`, `BudgetPolicy`, `TokenBudget`, `ProviderId`, `CapabilityId`, `SessionId` / `RunId` / `NodeId`, `RuntimeConfig` load skeleton, `Grant::Network` / `HostAllow`, five-crate map, `#![forbid(unsafe_code)]` on `alloy-runtime`.

**Amendment (config peek):** The provisional `RuntimeConfig::load` parse of `[provider.<name>] api_key_env` MUST be removed. File existence of `router_path` remains required. Full schema ownership moves to this RFC’s `RouterConfig` (§7). See §7.6.

### 2.3 Relationship to RFC-0004

Authoritative for: `CostMeter`, `SharedCostMeter`, `BudgetCheck`, `CostSnapshot`, `ModelCallRecord`, `DecisionRecord`, `DecisionKind`, `DecisionLog`, `EventDecisionLog`, `RecordingDecisionLog`, `usage_unknown` wire synthesis + query invariant, retention / redaction helpers.

**RFC-0007 is the first real producer.** It MUST call:

| When | Call |
| --- | --- |
| Successful or failed provider contact that yields a completion attempt | `DecisionLog::record_model_call` + `SharedCostMeter::add_model_usage` (when meter injected) |
| Every `route` outcome (success or budget/config denial after admission) | `DecisionLog::record` with `DecisionKind::ModelRoute` or `Budget` |

**RFC-0004 compatibility decision:** the token/meter semantics are reused unchanged, but the first real producer exposes missing attribution fields on the merged `ModelCallRecord`. RFC-0007 therefore REQUIRES the additive RFC-0004 amendment in §5.9.4 before implementation is complete. This amendment is backward-compatible at the private wire-payload level because the added fields are nullable / defaulted on read. The `usage_unknown` invariant (`input.is_none() || output.is_none()`) remains unchanged.

### 2.4 Already implemented | Added by RFC-0007 | Deferred

| Category | Contents |
| --- | --- |
| **Already implemented** | `ModelTier`, `BudgetSnapshot`, `BudgetPolicy`, `ProviderId`, `CapabilityId`, `CostMeter` / `SharedCostMeter` / `BudgetCheck`, `ModelCallRecord` / `DecisionLog` / `DecisionKind::{ModelRoute,Budget}`, redaction helpers, `ArtifactKind::PromptPack` (storage kind only), provisional router file existence check, `Grant::Network` / `HostAllow`, sandbox broker (0005), MCP host (0006) |
| **Added by RFC-0007** | `router` module; traits; `TomlModelRouter`; `OpenAiCompatibleProvider`; minimal `PromptPack` IR; `RouterConfig`; HTTP client dependency behind default-on feature; price→USD derivation; additive `ModelCallRecord` attribution/provenance fields; `RecordingModelProvider`; error taxonomy; `router.toml.example` full schema |
| **Deferred** | Scoring / failover (ADR F-20); second provider; streaming; RFC-0010 retries; RFC-0012 full pack; RFC-0013 workers; RFC-0016 ScriptedProvider body (contract only here) |

### 2.5 Dependency boundaries

```text
alloy-cli / alloy-eval / workers (0013)
        │
        ▼
alloy-runtime::router  ──uses──►  alloy-runtime::obs (DecisionLog, CostMeter)
        │                         alloy-runtime::types (IDs, budgets, ErrorClass)
        │                         alloy-runtime::config (paths only; no provider peek)
        ▼
OpenAiCompatibleProvider ──HTTP──► external openai-compatible API
RecordingModelProvider / ScriptedProvider (0016) ── no network
```

- `alloy-runtime` remains one of ≤5 crates. **No sixth crate.**
- Router MUST NOT depend on `alloy-tools` sandbox for provider HTTP.
- Router MAY depend on `DecisionLog` trait only (same pattern as RFC-0006 MCP).

### 2.6 Sandbox posture and first network egress

RFC-0005 enforces **deny-by-default network** inside sandboxed **tool/exec** children (`Grant::Network(HostAllow)` is the only allow path for those children).

RFC-0007 opens the **first host-process egress** in the workspace: the runtime process itself dials the provider `base_url`.

| Path | Network policy |
| --- | --- |
| Sandboxed `cargo_*` / tool children | Unchanged — deny by default; `Grant::Network` / `HostAllow` only |
| `OpenAiCompatibleProvider` HTTP | Host-process egress to configured `base_url` only; not mediated by `SandboxBroker` |
| Dogfood (Alloy-on-Alloy) | Still banned until sandbox + holdout green (ADR F-07 / RFC-0016) |

**Normative:** Implementing RFC-0007 MUST NOT weaken sandbox network deny for tool exec. Operator-configured `base_url` is the sole intended egress target for the provider. Credentials remain env-referenced (`api_key_env`), never written to disk by Alloy.

---

## 3. Public Rust API

New router items live under `alloy_runtime::router` and are re-exported from the crate root where noted in §3.16. Additive cross-cutting items required by RFC-0004 compatibility live with their owning shared surface: `EndpointId` in `types::ids`, and `ModelUsdSource` / added `ModelCallRecord` fields in `obs::decision`. `alloy-runtime` is `#![deny(missing_docs)]`; every public item and public field specified in this section MUST have rustdoc that states ownership and failure semantics at the implementation site.

### 3.1 Reused types (normative — unchanged)

| Type | Source | Notes |
| --- | --- | --- |
| `ModelTier` | `types/budget.rs` | Premium/Standard/Economy/Local; serde `snake_case` |
| `BudgetSnapshot` | `types/budget.rs` | `usd_spent` / `tokens_in` / `tokens_out` — **spent** counters despite V2 field name `budget_remaining` |
| `BudgetPolicy` | `types/budget.rs` | Ceilings for denial |
| `ProviderId`, `CapabilityId` | `types/ids.rs` | Catalog names 1..=128 bytes |
| `SessionId`, `RunId`, `NodeId`, `Digest`, `EventSeq` | `types/ids.rs` | Attribution / hashing |
| `EndpointId` | `types/ids.rs` | Added by RFC-0007; endpoint catalog id shared with RFC-0004 model-call records |
| `ErrorClass` | `types/diagnostic.rs` | `Model`, `Budget`, `Timeout`, `Cancelled`, … |
| `CostMeter`, `SharedCostMeter`, `BudgetCheck` | `obs/cost.rs` | Metering |
| `ModelCallRecord`, `DecisionRecord`, `DecisionKind`, `DecisionLog` | `obs/decision.rs` | Recording |
| `ArtifactKind::PromptPack` | `storage/artifacts.rs` | Storage kind enum **only** — do not alter |

### 3.2 `ComplexityScore`

```rust
/// Serde-stable complexity hint (V2 §11.2). MVP routing MUST ignore this field.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ComplexityScore(pub f32);
```

**Validation:** If present on `RoutingRequest`, values outside `0.0..=1.0` MUST NOT fail routing; they remain ignored. No clamping required in MVP.

### 3.3 `EndpointId`

```rust
/// Catalog id for a model endpoint row in `router.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct EndpointId(String);

impl EndpointId {
    pub fn new(s: impl Into<String>) -> Result<Self, IdError>; // empty or >128 → InvalidName
    pub fn as_str(&self) -> &str;
}
```

Same length rules as `ProviderId` (1..=128). `Deserialize` MUST be handwritten and validating, matching the `name_id!` pattern in `types/ids.rs`; deriving `Deserialize` is forbidden because it bypasses validation. Router config maps `IdError::InvalidName` to `RouterError::Config`.

### 3.4 `ModelEndpoint`

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelEndpoint {
    pub id: EndpointId,
    pub provider: ProviderId,
    /// Human label only — NEVER used as the wire model id.
    pub display_name: String,
    /// Wire model id from TOML `model` — BYOM; not a Rust string literal in core.
    pub model: String,
    pub tiers: Vec<ModelTier>,
    pub supports_tools: bool,
    pub supports_structured_output: bool,
    pub max_context: u32,
    /// USD per 1_000_000 input tokens. `None` → never invent USD for this endpoint.
    pub input_usd_per_mtok: Option<f64>,
    /// USD per 1_000_000 output tokens. `None` → never invent USD for this endpoint.
    pub output_usd_per_mtok: Option<f64>,
}
```

**Ownership:** cloned into `RoutedModel`; cheap enough for MVP. `model` MUST come solely from config.

**V2 extension:** Architecture V2 §11.2 omitted the endpoint wire model id even though BYOM requires an operator-supplied id. RFC-0007 adds `model` to `ModelEndpoint` and to `router.toml` so runtime core does not hardcode vendor IDs.

### 3.5 `Health`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Degraded,
    Unhealthy,
}
```

MVP: providers MUST return `Healthy`. `Degraded` / `Unhealthy` exist for serde stability and future failover (deferred).

### 3.6 `ChatRole` / `ChatMessage` / `Citation` / `PromptPack`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// Opaque source label (path, artifact id, graph node, etc.).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

/// Minimal prompt IR accepted by `ModelRouter::complete`.
///
/// **Not** `ArtifactKind::PromptPack`. That enum variant is a storage classification.
/// Persisting this struct as an artifact MUST use `ArtifactKind::PromptPack` without
/// changing the enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptPack {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub citations: Vec<Citation>,
    /// Reserved for RFC-0012 domain labels / weights. MVP: always absent/ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<serde_json::Value>,
}
```

**Serde stability:** Additive fields with `#[serde(default)]` only. RFC-0012 MUST extend without renaming `messages` / `citations` or changing their element types’ wire names.

**RFC-0012 upgrade path:** Add domain-labelled sections / weights as new fields or by populating `domains`; keep `messages` as the flattenable chat view the router already sends. No breaking change to `ModelRouter::complete` signature.

### 3.7 `CompletionRequest` / `ToolChoice` / `ResponseFormat` / `Usage` / `ModelResponse`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    None,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    /// MVP: empty. Tool schemas arrive with RFC-0013; field reserved serde-stable.
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
    #[serde(default = "tool_choice_none")]
    pub tool_choice: ToolChoice,
    #[serde(default = "response_format_text")]
    pub response_format: ResponseFormat,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
}

fn tool_choice_none() -> ToolChoice { ToolChoice::None }
fn response_format_text() -> ResponseFormat { ResponseFormat::Text }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub text: Option<String>,
    pub structured: Option<serde_json::Value>,
    /// MVP: always empty. Reserved for tool-calling workers (RFC-0013).
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
    pub usage: Usage,
    pub provider_request_id: Option<String>,
    pub finish_reason: Option<String>,
}
```

**Streaming (normative):** MVP MUST NOT stream. `complete` returns one `ModelResponse` after the full HTTP body is received. Future streaming MUST NOT remove or reshape these fields; it MUST add a separate streaming API (e.g. `complete_stream`) or an additive request flag defaulting to off. `ModelResponse` therefore does **not** foreclose streaming.

`finish_reason` is the provider boundary carrier for `choices[0].finish_reason`. `OpenAiCompatibleProvider` MUST redact/truncate it to ≤128 bytes on a UTF-8 boundary before constructing `ModelResponse`. `provider_request_id` MUST be redacted/truncated to ≤256 bytes. `TomlModelRouter` copies both fields into `ModelCallRecord`. `RecordingModelProvider` and RFC-0016 `ScriptedProvider` set them directly on scripted `ModelResponse`s.

### 3.8 `RoutingRequest` / `RoutedModel`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingRequest {
    /// Required for DecisionLog attribution (RFC-0004). Additive vs V2 sketch.
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    pub capability: CapabilityId,
    /// Ignored in MVP; serde-stable.
    pub complexity: Option<ComplexityScore>,
    /// Spent counters (`CostMeter::to_budget_snapshot` shape). Name kept from V2.
    pub budget_remaining: BudgetSnapshot,
    pub requires_tools: bool,
    pub requires_structured_output: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoutedModel {
    pub endpoint: ModelEndpoint,
    pub tier: ModelTier,
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    /// Copied from [`RoutingRequest::requires_structured_output`] for `complete` request shaping.
    pub requires_structured_output: bool,
    /// Event sequence of the route decision, if recording succeeded.
    pub route_event_seq: Option<EventSeq>,
}
```

**Attribution:** `session` is REQUIRED on `RoutingRequest` because RFC-0004 `DecisionLog` requires a session row for durable append, and route decisions MUST be attributable. This is an additive field relative to the V2 sketch (V2 omitted obs attribution that RFC-0004 already merged).

### 3.9 `SecretString`

```rust
/// API key material. NEVER logged, displayed, or included in events.
pub struct SecretString { /* private */ }

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self;
    /// Borrow for Authorization header construction only.
    pub fn expose(&self) -> &str;
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}
```

MUST NOT implement `Display`, `Serialize`, `Clone`, `PartialEq`, or `Eq`. Tests that need to assert key handling MUST inspect redacted `Debug` output or use provider request assertions; they MUST NOT compare secret values through this type.

### 3.10 `RouterError` / `ProviderError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RouterError {
    #[error("config: {0}")]
    Config(String),
    #[error("no endpoint for tier {tier:?} (tools={requires_tools}, structured={requires_structured})")]
    NoEndpoint {
        tier: ModelTier,
        requires_tools: bool,
        requires_structured: bool,
    },
    #[error("budget denied: {0:?}")]
    BudgetDenied(BudgetCheck),
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("cancelled")]
    Cancelled,
    #[error("shutting down")]
    ShuttingDown,
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("auth failed")]
    Auth,
    #[error("rate limited")]
    RateLimit,
    #[error("context length exceeded")]
    ContextLength,
    #[error("timeout")]
    Timeout,
    #[error("malformed response: {0}")]
    MalformedResponse(String),
    #[error("http status {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("internal: {0}")]
    Internal(String),
}
```

**Display / Debug:** `ProviderError` message strings MUST pass through `obs::redact_secrets` before storage in the variant when the source is a provider body or header. `Auth` / `RateLimit` / `ContextLength` / `Timeout` carry **no** body. `HttpStatus.message` and `Transport` / `MalformedResponse` MUST be redacted and truncated to ≤512 bytes. When a construct-time provider validation failure occurs inside `TomlModelRouter::from_paths`, it MUST be mapped to `RouterError::Config`; `RouterError::Provider` is only for `complete`.

Full taxonomy tables: §8.

### 3.11 `ModelProvider` trait

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    async fn complete(
        &self,
        endpoint: &ModelEndpoint,
        req: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError>;

    /// MVP stub: concrete providers MUST return `Health::Healthy`.
    async fn health(&self) -> Health;
}
```

**Send/Sync:** required. Implementations MUST be safe to share via `Arc<dyn ModelProvider>`.

**Async:** `async_trait` (workspace dep already present).

**Visibility:** public trait in `alloy_runtime::router`.

### 3.12 `ModelRouter` trait

```rust
#[async_trait]
pub trait ModelRouter: Send + Sync {
    async fn route(&self, req: RoutingRequest) -> Result<RoutedModel, RouterError>;

    async fn complete(
        &self,
        routed: &RoutedModel,
        prompt: PromptPack,
    ) -> Result<ModelResponse, RouterError>;
}
```

**Contract:** `complete` MUST use `routed.endpoint` / `routed.tier` as selected by a prior `route` (or an equivalent test-constructed `RoutedModel`). Callers MUST NOT mutate `endpoint.model` to bypass TOML.

### 3.13 `TomlModelRouter`

```rust
pub struct TomlModelRouter { /* private fields — see §4 */ }

impl TomlModelRouter {
    /// Load + validate config; resolve API key; build HTTP client + provider.
    /// Fail closed on invalid TOML, unknown kinds, missing key, empty providers.
    #[cfg(feature = "http-provider")]
    pub fn from_paths(
        router_path: &Path,
        budget_policy: BudgetPolicy,
        example_env_hint: &Path,
    ) -> Result<Self, RouterError>;

    /// Test/injection constructor (no file I/O).
    pub fn from_parts(parts: TomlModelRouterParts) -> Result<Self, RouterError>;

    pub fn with_decision_log(self, log: Arc<dyn DecisionLog>) -> Self;
    pub fn with_cost_meter(self, meter: SharedCostMeter) -> Self;

    pub fn metrics(&self) -> RouterMetricsSnapshot;

    /// Begin drain: reject new route/complete with `ShuttingDown`.
    pub async fn shutdown(&self) -> RouterShutdownReport;
}
```

```rust
pub struct TomlModelRouterParts {
    pub config: RouterConfig,
    pub provider: Arc<dyn ModelProvider>,
    pub budget_policy: BudgetPolicy,
    pub decision_log: Option<Arc<dyn DecisionLog>>,
    pub cost_meter: Option<SharedCostMeter>,
    pub shutdown_token: Option<tokio_util::sync::CancellationToken>,
}
```

**Construction ownership:** `from_paths` owns parsing + building `OpenAiCompatibleProvider`. `from_parts` is the injection point for `RecordingModelProvider` / RFC-0016 scripts.

`from_paths`, `OpenAiCompatibleProvider`, `OpenAiCompatibleSpec`, and `http_client` are gated behind `http-provider`. `from_parts`, traits, config DTOs, `RecordingModelProvider`, and all shared types compile without default features.

**Budget policy:** injected at construction from `RuntimeConfig::budget_policy` (§7.6). Used by `route` denial (§5.4). Not read from `router.toml`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterShutdownReport {
    pub cancelled_in_flight: bool,
    pub remaining_in_flight: usize,
}
```

### 3.14 `OpenAiCompatibleProvider`

```rust
pub struct OpenAiCompatibleProvider { /* private */ }

impl OpenAiCompatibleProvider {
    pub fn new(spec: OpenAiCompatibleSpec, client: reqwest::Client) -> Result<Self, ProviderError>;
}

pub struct OpenAiCompatibleSpec {
    pub id: ProviderId,
    pub base_url: String,       // no trailing slash required; normalized at construct
    pub api_key: SecretString,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}
```

Implements `ModelProvider`. Performs `POST {base_url}/chat/completions`.

`OpenAiCompatibleProvider::new` re-validates `base_url` and returns `ProviderError` when used directly. `TomlModelRouter::from_paths` MUST catch any construct-time `ProviderError` from this constructor and return `RouterError::Config(redacted_message)` instead; `RouterError::Provider` is reserved for `complete` failures.

### 3.15 `RecordingModelProvider` (test double)

```rust
/// FIFO scripted outcomes + recorded invocations. No network.
pub struct RecordingModelProvider {
    // private
}

impl RecordingModelProvider {
    pub fn new(id: ProviderId) -> Self;
    pub fn push(&self, outcome: Result<ModelResponse, ProviderError>);
    pub fn recorded(&self) -> Vec<(ModelEndpoint, CompletionRequest)>;
}
```

Implements `ModelProvider`:
- `complete` — record args, then pop FIFO (exhausted → `ProviderError::Internal("recording exhausted")`).
- `health` — always `Healthy`.
- `id` — constructor id.

**RFC-0016 contract:** `ScriptedProvider` in `alloy-eval` MUST implement the same `ModelProvider` trait. It MAY use a `HashMap` keyed by a deterministic fingerprint of `CompletionRequest` instead of FIFO; it MUST NOT perform HTTP; it MUST return `Health::Healthy`. The trait surface in this RFC is the sole coupling.

### 3.16 Crate-root re-exports

`alloy_runtime` MUST re-export at least:

`ModelRouter`, `ModelProvider`, `TomlModelRouter`, `OpenAiCompatibleProvider` (when `http-provider` is enabled), `RecordingModelProvider`, `RoutingRequest`, `RoutedModel`, `ModelEndpoint`, `EndpointId`, `CompletionRequest`, `ModelResponse`, `Usage`, `PromptPack`, `ChatMessage`, `ChatRole`, `Citation`, `ComplexityScore`, `Health`, `ToolChoice`, `ResponseFormat`, `RouterError`, `ProviderError`, `RouterConfig`, `RouterMetricsSnapshot`, `RouterShutdownReport`, `ModelUsdSource`.

### 3.17 Additive RFC-0004 `ModelCallRecord` amendment

RFC-0007 MUST extend `ModelCallRecord` and its private wire payload additively:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUsdSource {
    ProviderReported,
    OperatorPriceTable,
}

pub struct ModelCallRecord {
    // existing RFC-0004 fields unchanged...
    pub endpoint_id: Option<EndpointId>,
    pub model: Option<String>,
    pub route_event_seq: Option<EventSeq>,
    pub usd_source: Option<ModelUsdSource>,
    pub finish_reason: Option<String>,
    pub provider_request_id: Option<String>,
}
```

**Compatibility:** existing event payloads without these fields MUST parse with `None`. `record_model_call` MUST write them when provided. `reaccumulate_cost_from_events` MUST ignore these fields for arithmetic and preserve RFC-0004 cost semantics.

**Source impact:** `ModelCallRecord` is an existing public struct and is not `#[non_exhaustive]` on `main`; adding fields is source-breaking for in-workspace struct literals. The implementation PR MUST update all current literals/tests and MUST consider adding `#[non_exhaustive]` or constructor helpers in the same amendment so later additive fields do not repeat this cost.

**Producer rules in RFC-0007:**

- `endpoint_id`: `Some(routed.endpoint.id.clone())`.
- `model`: `Some(routed.endpoint.model.clone())`; value is operator config, never a core constant.
- `route_event_seq`: `RoutedModel.route_event_seq` from the successful route decision append, or `None` if route decision logging failed.
- `usd_source`: `Some(ModelUsdSource::OperatorPriceTable)` iff `usd.is_some()` from §5.8; `None` when `usd` is `None`. Openai-compatible providers do not report dollars in MVP, so `ProviderReported` is reserved for future provider kinds and MUST NOT be used by this provider.
- `finish_reason`: provider `choices[0].finish_reason` when it is a string, redacted/truncated to ≤128 bytes on a UTF-8 boundary; `None` otherwise.
- `provider_request_id`: `ModelResponse.provider_request_id` redacted/truncated to ≤256 bytes on a UTF-8 boundary.

This amendment is REQUIRED because route metadata alone is not a durable join: `EventSeq` ordering is not a reliable route→call correlation under concurrent calls, `node` is optional, and BYOM audit must answer which endpoint/model actually ran.

Implementation MUST update merged RFC-0004 text where it lists `ModelCallRecord`, private model-call payload fields, parser behaviour, and later-RFC contracts so the merged RFC and code do not diverge. It MUST update RFC-0001 / config documentation for `RuntimeConfig::budget_policy` (§7.6) in the same implementation series.

RFC-0007 also requires two small RFC-0004-adjacent clarifications:

1. `obs` MUST expose a crate-visible body limit seam, e.g. `pub(crate) use redact::BODY_MAX_BYTES as MODEL_PROMPT_BODY_MAX_BYTES;`, because sibling module `router` cannot name private `obs::redact::BODY_MAX_BYTES` directly.
2. `BudgetCheck` does not need `Serialize` for RFC-0007; route `Budget` decision metadata MUST write the explicit strings in §5.10 instead.

### 3.18 `check_budget_snapshot` (crate-private fallback helper)

```rust
/// Apply budget arithmetic to a spent snapshot without mutating a meter.
pub(crate) fn check_budget_snapshot(spent: &BudgetSnapshot, policy: &BudgetPolicy) -> BudgetCheck;
```

Lives in `alloy_runtime::router::select`. It is a meter-less fallback only; it MUST NOT be re-exported from the crate root. When a `SharedCostMeter` is injected, `route` MUST use `meter.check_budget(&self.budget_policy)` instead (§5.4).

**Snapshot arithmetic:**

```text
tokens_exhausted iff spent.tokens_in.saturating_add(spent.tokens_out) >= policy.max_tokens_per_run
  (including max_tokens_per_run == 0 ⇒ immediately exhausted)

usd_exhausted iff
  !policy.max_usd_per_run.is_finite() || policy.max_usd_per_run < 0.0
  OR spent.usd_spent >= policy.max_usd_per_run
```

**Known divergence from `CostMeter::check_budget`:** `BudgetSnapshot.usd_spent` is an `f64`, so the fallback treats `0.0` as known. A live `CostMeter` with `usd_spent == None` and `max_usd_per_run == 0.0` does **not** trigger USD exhaustion under RFC-0004; the fallback with `BudgetSnapshot { usd_spent: 0.0, ... }` does. This is accepted only for the meter-less fallback. Unit tests MUST pin the difference.

### 3.19 Visibility & construction summary

| Item | Visibility | Construction |
| --- | --- | --- |
| Traits | `pub` | N/A |
| `TomlModelRouter` | `pub` | `from_paths` (`http-provider`) / `from_parts` |
| `OpenAiCompatibleProvider` | `pub` behind `http-provider` | `new` |
| `RecordingModelProvider` | `pub` | `new` |
| `SecretString` | `pub` | `new` |
| Wire DTOs (HTTP serde) | `pub(crate)` | private to provider |
| Price math | `pub(crate)` fn | pure |

---

## 4. Internal Module Design

### 4.1 Hierarchy

```text
crates/alloy-runtime/src/router/
  mod.rs           # re-exports, module docs
  error.rs         # RouterError, ProviderError, boundary maps
  types.rs         # PromptPack, messages, endpoints, requests/responses, Health
  traits.rs        # ModelRouter, ModelProvider
  config.rs        # RouterConfig parse + validate
  select.rs        # pure tier/endpoint selection + budget check
  price.rs         # tokens → Option<usd>
  meter_bridge.rs  # ModelCallRecord build + CostMeter updates
  decision_bridge.rs # DecisionRecord metadata builders
  http_client.rs   # reqwest Client builder (timeouts, TLS, redirects)
  openai.rs        # OpenAiCompatibleProvider + wire types
  toml_router.rs   # TomlModelRouter lifecycle + route/complete
  recording.rs     # RecordingModelProvider
  metrics.rs       # RouterMetricsSnapshot
  secret.rs        # SecretString
```

### 4.2 Responsibilities

| Module | Responsibility | MUST NOT |
| --- | --- | --- |
| `select` | capability→tier, endpoint filter, budget denial | HTTP, logging secrets |
| `price` | pure USD derivation | invent prices; marketing strings |
| `openai` | HTTP complete + response map | retry loops; match on vendor model IDs |
| `toml_router` | orchestration, obs, lifecycle | hardcode model strings |
| `config` | TOML schema | write `.env` |
| `recording` | test double | network |
| `http_client` | shared client policy | per-call client rebuild |

### 4.3 Dependency direction

```text
toml_router → select, price, meter_bridge, decision_bridge, traits, error, metrics
openai → http_client, secret, types, error
config → types, error
recording → traits, types, error
select / price → types only (pure)
```

No cycles. `openai` MUST NOT import `toml_router`.

`config` MUST parse into private `Deserialize` mirror structs that match TOML spellings (`*_ms`, string `kind`, raw capability keys), then validate/map into public `RouterConfig` / `RouterPolicy` / `ProviderConfig`. Public config structs need not derive `Deserialize`.

### 4.4 Injection points

| Seam | Type | Consumer |
| --- | --- | --- |
| Decision log | `Option<Arc<dyn DecisionLog>>` | tests / runtime host |
| Cost meter | `Option<SharedCostMeter>` | RFC-0010 run lifecycle |
| Provider | `Arc<dyn ModelProvider>` | Recording / Scripted / OpenAI |
| Budget policy | `BudgetPolicy` | profile (RFC-0001 / 0015) |

---

## 5. Execution Algorithm

### 5.1 Pipeline overview

```mermaid
flowchart TD
  A[route: RoutingRequest] --> B{ShuttingDown?}
  B -->|yes| Z1[Err ShuttingDown]
  B -->|no| C[Resolve tier]
  C --> D[Select endpoint]
  D --> E{Endpoint found?}
  E -->|no| Z2[Err NoEndpoint + ModelRoute decision]
  E -->|yes| F[meter.check_budget or snapshot fallback]
  F --> G{BudgetCheck::Ok?}
  G -->|no| Z3[Err BudgetDenied + Budget decision]
  G -->|yes| H[Ok RoutedModel + ModelRoute decision]
  H --> I[complete: PromptPack]
  I --> J{ShuttingDown?}
  J -->|yes| Z4[Err ShuttingDown]
  J -->|no| K[Build CompletionRequest]
  K --> L[provider.complete]
  L --> M{Result}
  M -->|Ok| N[Map usage + USD]
  N --> O[add_model_usage + record_model_call]
  O --> P[Ok ModelResponse]
  M -->|Err| Q[Map ProviderError]
  Q --> R[add_model_usage unknown-or-partial + record_model_call with error_class]
  R --> S[Err RouterError::Provider]
```

### 5.2 Tier resolution

```text
INPUT: capability, config.capability_tiers, config.policy.default_tier
canonical = capability.as_str().to_ascii_lowercase()
IF canonical is a key in capability_tiers:
  tier = map[canonical]
  tier_source = "capability_map"
ELSE:
  tier = policy.default_tier
  tier_source = "default"
OUTPUT: (tier, tier_source)
```

**Canonical lookup:** capability map keys in `router.toml` are normalized with `to_ascii_lowercase()` at load. This preserves compatibility with Architecture V2’s example (`Repair`, `Edit`, `Review`, `Planning`) while aligning with `CapabilityId` rustdoc examples (`repair`, `edit`). `RouterConfig` MUST reject two keys that collide after normalization (for example `Repair` and `repair`). `route` normalizes `CapabilityId::as_str()` to lowercase before lookup.

**Unknown capability** does **not** error in MVP — it uses `default_tier`. Fallback to default MUST increment `routes_default_tier`, MUST record `tier_source = "default"`, and MUST include `capability_mapped = false` in route metadata so silent misrouting is observable. A future strict mode may add a new `RouterError` variant because `RouterError` is `#[non_exhaustive]`; RFC-0007 does not reserve an unused variant.

`[policy].scoring` is parsed into `ScoringWeights` (all fields optional `f64`) and MUST NOT be read by `select`.

### 5.3 Endpoint selection

Among `config` endpoints where:

1. `endpoint.provider == selected_provider.id` (MVP: the sole provider),
2. `endpoint.tiers` contains resolved `tier`,
3. if `requires_tools` then `endpoint.supports_tools`,
4. if `requires_structured_output` then `endpoint.supports_structured_output`,

select the **first** matching endpoint in TOML declaration order.

If none match → `Err(RouterError::NoEndpoint { … })`.

**No scoring. No random tie-break. No failover to another tier.**

### 5.4 Budget enforcement point

**Denial occurs in `route`, not `complete`.**

1. If `self.cost_meter` is `Some(meter)`, compute `check = meter.check_budget(&self.budget_policy)`.
2. If no meter is injected, compute `check = check_budget_snapshot(&req.budget_remaining, &self.budget_policy)`.
3. If `check.is_exhausted()`:
   - Record `DecisionKind::Budget` with metadata (§9.2).
   - Return `Err(RouterError::BudgetDenied(check))`.
4. **MUST NOT** escalate or downgrade tier to satisfy budget in MVP.
5. `complete` MUST NOT re-check `BudgetPolicy`. Run-level post-call warnings remain RFC-0010 (`maybe_signal_budget_warning`).

**Caller observation:** `Err(BudgetDenied(_))` — no provider HTTP occurs.

**Concurrent overshoot:** RFC-0007 bounds admission with `max_in_flight` (§6.3) but does not reserve budget per prompt. If N calls route concurrently before their completions update the meter, all N can pass the same budget check. This bounded overshoot is accepted for MVP and MUST be documented in `Budget` decision metadata as `in_flight_at_route`; RFC-0010 owns stricter per-node serialization / reservation.

### 5.5 `complete` request construction

```text
CompletionRequest {
  messages: prompt.messages.clone(),
  tools: [],                                      // MVP
  tool_choice: ToolChoice::None,                  // MVP
  response_format:
      if routed.requires_structured_output { JsonObject } else { Text },
  temperature: None,                              // provider/API default
  max_output_tokens: None,
}
```

`RoutedModel.requires_structured_output` is copied from the routing request at `route` time (§3.8).

Citations / domains are **not** sent on the wire in MVP; they exist for hashing / RFC-0012.

### 5.6 Provider HTTP call (openai-compatible)

```text
POST {base_url}/chat/completions
Headers:
  Authorization: Bearer {api_key.expose()}
  Content-Type: application/json
  Accept: application/json
Body:
  {
    "model": endpoint.model,          // from TOML only
    "messages": [ {"role","content"}, ... ],
    "stream": false,
    optional "temperature",
    optional "max_tokens" <- max_output_tokens,
    optional "response_format": {"type":"json_object"} when JsonObject
  }
```

**No `tools` in MVP body** even if `supports_tools` is true (workers land in RFC-0013). Feature flags still filter endpoints at route time so future callers do not select incapable endpoints.

### 5.7 Response mapping → `ModelResponse` + usage

| OpenAI-compatible JSON | Alloy field |
| --- | --- |
| Any HTTP `2xx` status with valid response shape | success path; `200` is not special |
| `choices[0].message.content` string | `text: Some(...)` |
| `choices[0].message.content` null/absent | `text: None` |
| `choices[0].message.content` array of content parts | concatenate string `text` fields in order with no separator; ignore non-text parts |
| structured request + content parses as JSON object | `structured: Some(value)` and `text: Some(original_content)` |
| structured request + content is not a JSON object | `structured: None` and `text: Some(original_content)` |
| `usage.prompt_tokens` number | `usage.input_tokens: Some(n)` |
| `usage.completion_tokens` number | `usage.output_tokens: Some(n)` |
| `usage` absent, null, malformed, fractional, negative, or out of `u64` range | corresponding `Option` = `None` (**never fabricate 0**); do not fail an otherwise valid completion |
| `id` string | `provider_request_id: Some(...)` |
| tool_calls array | ignored → `tool_calls: []` in MVP |
| `choices[0].finish_reason` string | `ModelResponse.finish_reason: Some(redacted_truncated_reason)` |
| `choices[0].finish_reason == "length"` | return `Ok(ModelResponse)`; router copies `ModelResponse.finish_reason` into `ModelCallRecord.finish_reason`; RFC-0013 decides whether to retry/shrink |
| `choices[0].message.refusal` string and no content | `Ok(ModelResponse)` with `text: None`, `structured: None`, `finish_reason: Some("refusal")`; refusal text is not retained in MVP. If provider also supplies a finish reason, `refusal` wins so refusal is observable. |

Missing `choices`, empty `choices`, non-object root, any `2xx` response with top-level `error` object, missing `choices[0].message`, or content array with no text parts → `ProviderError::MalformedResponse`. If the request did not ask for structured output, `structured` MUST be `None` even if `content` happens to parse as JSON.

### 5.8 USD derivation

```text
fn derive_usd(endpoint: &ModelEndpoint, usage: &Usage) -> Option<f64> {
  let (Some(inp), Some(out)) = (usage.input_tokens, usage.output_tokens) else { return None };
  let (Some(pin), Some(pout)) = (endpoint.input_usd_per_mtok, endpoint.output_usd_per_mtok) else {
    return None; // price table incomplete — do not invent
  };
  if !pin.is_finite() || !pout.is_finite() || pin < 0.0 || pout < 0.0 { return None; }
  let usd = (inp as f64 / 1_000_000.0) * pin + (out as f64 / 1_000_000.0) * pout;
  if usd.is_finite() && usd >= 0.0 { Some(usd) } else { None }
}
```

**V2 §18 / ADR F-08:** Derived USD is an **operator-price-table accounting estimate**, not a provider-reported amount and not a marketing claim. Code, docs, and events MUST NOT assert savings percentages or comparative cost bands. Eval (RFC-0016) is the only place calibrated claims may later appear.

**Cached/reasoning token limitation:** MVP ignores provider-specific `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens`. Therefore `usd_source = OperatorPriceTable` records a coarse estimate over total prompt/completion tokens. The estimate MUST NOT be described as exact. More detailed token class pricing is future work (§12.2).

### 5.9 Cost schema reconciliation (highest priority)

#### 5.9.1 `ModelCallRecord` field mapping

| `ModelCallRecord` field | Source on success | Source on provider error after HTTP attempt |
| --- | --- | --- |
| `session` | `routed.session` | same |
| `run` | `routed.run` | same |
| `node` | `routed.node` | same |
| `provider_id` | `provider.id()` | same |
| `model_tier` | `routed.tier` | same |
| `input_tokens` | `usage.input_tokens` | `None` unless error body included parseable usage (MVP: always `None` on error) |
| `output_tokens` | `usage.output_tokens` | `None` (MVP on error) |
| `usd` | `derive_usd(...)` | `None` |
| `duration_ms` | `Some(elapsed_ms)` | `Some(elapsed_ms)` |
| `confidence` | `None` | `None` |
| `error_class` | `None` | mapped (§8.4) |
| `content_hash` | `Some(hash_prompt(canonical_prompt_string))` | same if prompt available |
| `prompt_body` | `Some(canonical_prompt_string)` only when UTF-8 byte length ≤ `obs::MODEL_PROMPT_BODY_MAX_BYTES`; else `None` | same |
| `endpoint_id` | `Some(routed.endpoint.id.clone())` | same |
| `model` | `Some(routed.endpoint.model.clone())` | same |
| `route_event_seq` | `routed.route_event_seq` | same |
| `usd_source` | `Some(OperatorPriceTable)` iff `usd.is_some()` | `None` |
| `finish_reason` | `response.finish_reason.clone()` | `None` |
| `provider_request_id` | `response.provider_request_id.clone()` | `None` |

**Canonical prompt string for hashing:** UTF-8 JSON array of `{role, content}` messages in order (serde_json compact), not Display debug. Citations excluded from hash in MVP. The same canonical string MUST be used for `hash_prompt` and, when small enough, `prompt_body`; this prevents `prepare_model_call::resolve_hash` from warning about a mismatched caller hash.

**Body-size rule:** `prepare_model_call` checks body size before retention. The router MUST compute `content_hash` itself and MUST pass `prompt_body: None` whenever `canonical_prompt_string.as_bytes().len() > obs::MODEL_PROMPT_BODY_MAX_BYTES`. Oversize stripping MUST increment `model_call_prompt_body_oversize` and log a warning without changing the model-call return value. This preserves durable cost records for large prompts.

#### 5.9.2 `usage_unknown` consistency

RFC-0004 synthesizes wire `usage_unknown = input_tokens.is_none() \|\| output_tokens.is_none()` in `model_to_payload`. Query rejects inconsistent payloads.

**RFC-0007 MUST:**

- Pass `Option` token fields through honestly.
- NEVER set a parallel `usage_unknown` on the in-memory record (field does not exist on `ModelCallRecord`).
- NEVER write zeros to mean “unknown”.
- When the provider omits `usage` entirely → both token fields `None` → wire `usage_unknown: true` (consistent).
- When only one of prompt/completion tokens is present → the missing one stays `None` → `usage_unknown: true` (consistent).

#### 5.9.3 `CostMeter` entry point

| Who | When | Call |
| --- | --- | --- |
| `TomlModelRouter::complete` | After provider returns `Ok`, including when usage is absent | `meter.add_model_usage(tier, input, output, usd)` |
| `TomlModelRouter::complete` | After provider returns any `ProviderError` (provider errors occur after construction and represent an attempted or attemptable provider call) | `meter.add_model_usage(tier, None, None, None)` |
| `route` budget denial | — | **MUST NOT** call `add_model_usage` |

If `cost_meter` is `None`, skip metering (tests without meter). Decision log remains independent.

**Cancellation-safe ordering:** after the provider returns `Ok` or a recordable `Err`, the router MUST build the `ModelCallRecord` fields, update `SharedCostMeter` synchronously (no `.await` before `add_model_usage`), then await `record_model_call`. Host-level cancellation (§6.4) MUST NOT be selected during this meter+record critical section. If the caller drops the future during the async append, Rust cancellation may still prevent the durable record; this is recorded only when the append returns an error. `obs_record_errors` MUST count returned obs failures for both route decisions and model-call records. This mirrors RFC-0006’s obs-failure-does-not-fail-call rule while keeping the synchronous meter update first.

#### 5.9.4 RFC-0004 compatibility analysis and amendment

RFC-0004 is vindicated for:

| Surface | Decision |
| --- | --- |
| `input_tokens` / `output_tokens: Option<u64>` | Keep unchanged; openai-compatible usage maps naturally to `Some`, omitted/malformed usage maps to `None` |
| `usage_unknown` wire invariant | Keep unchanged; synthesized from token nullness; RFC-0007 never writes it directly |
| `CostMeter::add_model_usage` | Keep unchanged; router uses it for known and unknown usage |
| Optional `usd` | Keep unchanged; RFC-0007 passes `Some` only when operator price table + tokens produce finite non-negative USD |

RFC-0004 is insufficient for BYOM audit without additive fields:

| Gap | Amendment | Rationale |
| --- | --- | --- |
| Which endpoint/model ran | Add nullable `endpoint_id`, `model` | `provider_id` + tier is not enough when one provider fronts multiple operator-configured models |
| Route→call correlation | Add nullable `route_event_seq` | Event order is not a join key under concurrency; `node` is optional |
| USD provenance | Add nullable `usd_source` | Distinguish operator-price-table estimate from future provider-reported dollars |
| Provider truncation / length signal | Add nullable `finish_reason` | Workers and RFC-0010 need to know whether a successful completion ended early |
| Provider-side request correlation | Add nullable `provider_request_id` | Operators need to join Alloy records to provider dashboards/support |

This is an explicit RFC-0004 amendment under the playbook’s stable-public-API rule. It is backward-compatible because old event payloads parse with `None` for the added fields, and cost reaccumulation ignores them. Implementers MUST NOT ship RFC-0007 by smuggling these values only into route decision metadata.

### 5.10 Decision logging — route

Always attempt when `decision_log` is `Some` (session id is present on `RoutingRequest`). A present `SessionId` does not guarantee a stored session row: `EventDecisionLog` returns `ObsError::Session(SessionError::NotFound)` when the row is absent. Unit/integration tests that do not create a durable session MUST inject `RecordingDecisionLog`.

| Outcome | `DecisionKind` | Metadata keys (object) |
| --- | --- | --- |
| Endpoint selected | `ModelRoute` | `capability`, `capability_mapped`, `tier`, `tier_source`, `endpoint_id`, `provider_id`, `model` (wire id from config), `requires_tools`, `requires_structured_output`, `in_flight_at_route` |
| No endpoint | `ModelRoute` | same without endpoint/model; `error`: `"no_endpoint"` |
| Budget denied | `Budget` | `capability`, `capability_mapped`, `tier`, `budget_check`, `tokens_in`, `tokens_out`, `usd_spent`, `budget_source` (`"meter"` or `"snapshot"`), `in_flight_at_route` |

`prompt_body: None`, `content_hash: None` for route decisions.

`budget_check` MUST be one of the strings: `ok`, `tokens_exhausted`, `usd_exhausted`, `tokens_and_usd_exhausted`. The router MUST map explicitly from `BudgetCheck` and MUST NOT rely on `BudgetCheck: Serialize`.

On successful route-decision append, the returned `EventSeq` MUST be copied into `RoutedModel.route_event_seq`. Obs errors → `tracing::warn`, increment `obs_record_errors`, set `route_event_seq = None`, and MUST NOT change `route`/`complete` return value (RFC-0006 pattern).

### 5.11 Failure handling summary

| Failure | Route/Complete | Record | Meter |
| --- | --- | --- | --- |
| Config / missing API key at construct | N/A — construct fails with `RouterError::Config` | no | no |
| No endpoint | `Err(NoEndpoint)` | ModelRoute | no |
| Budget | `Err(BudgetDenied)` | Budget | no |
| Provider Auth | `Err(Provider(Auth))` | ModelCall + error_class Model | unknown usage |
| Rate limit | `Err(Provider(RateLimit))` | ModelCall + Model | unknown |
| Context length | `Err(Provider(ContextLength))` | ModelCall + Model | unknown |
| Timeout | `Err(Provider(Timeout))` | ModelCall + Timeout | unknown |
| Malformed | `Err(Provider(Malformed…))` | ModelCall + Model | unknown |
| Host cancellation before provider returns | `Err(Cancelled)` | no ModelCall | no |
| Caller drops before provider result | no return value | no ModelCall | no |
| Caller drops during async `record_model_call` after provider result | no return value | append may be lost | meter may already be updated per §5.9.3 |
| Shutting down | `Err(ShuttingDown)` | no | no |

### 5.12 Sequence — successful complete

```mermaid
sequenceDiagram
  participant C as Caller
  participant R as TomlModelRouter
  participant S as select/price
  participant P as ModelProvider
  participant M as SharedCostMeter
  participant D as DecisionLog

  C->>R: route(req)
  R->>S: tier + endpoint + budget
  R->>D: record(ModelRoute)
  R-->>C: Ok(RoutedModel)
  C->>R: complete(routed, prompt)
  R->>P: complete(endpoint, req)
  P-->>R: Ok(ModelResponse)
  R->>S: derive_usd
  R->>M: add_model_usage
  R->>D: record_model_call
  R-->>C: Ok(ModelResponse)
```

---

## 6. Lifecycle & Concurrency

### 6.1 Router state machine

```mermaid
stateDiagram-v2
  [*] --> Ready: from_paths / from_parts Ok
  Ready --> Ready: route / complete
  Ready --> Draining: shutdown() CAS winner
  Draining --> Stopped: in_flight == 0 or grace elapsed
  Stopped --> [*]
```

| State | `route` / `complete` |
| --- | --- |
| Ready | admitted after semaphore permit + in-flight increment + phase recheck |
| Draining | `Err(ShuttingDown)` for new calls; in-flight may finish until grace |
| Stopped | `Err(ShuttingDown)` |

Implementation MUST use an `AtomicU8` phase (`Ready = 0`, `Draining = 1`, `Stopped = 2`) with `SeqCst` ordering, an `AtomicUsize` `in_flight`, a `tokio::sync::Notify` for drain wakeups, and a `tokio::sync::Semaphore` sized by `[policy].max_in_flight`. Admission MUST acquire a permit, increment `in_flight`, then re-check phase. If the recheck observes `Draining` or `Stopped`, the admission guard MUST be dropped immediately, decrement `in_flight`, notify drain waiters when it reaches zero, release the permit, and return `ShuttingDown`. Increment-before-recheck is REQUIRED so shutdown cannot observe `in_flight == 0` while a call is between phase check and admission.

### 6.2 Construction

1. Parse + validate `RouterConfig`.
2. Resolve `api_key_env` via `std::env::var` — unset or empty → `RouterError::Config` with the env key name and hint path to `example.env`. **Never invent a key. Never write `.env`.**
3. Build shared `reqwest::Client` (§10).
4. Construct `OpenAiCompatibleProvider`.
5. Enter `Ready` with `in_flight = 0`, semaphore permits = `max_in_flight`, and a router-owned `CancellationToken` (injected in tests or created at construction).

### 6.3 Concurrent completions

- `TomlModelRouter` is `Arc`-shareable (`Send + Sync`).
- Multiple concurrent `complete` calls MUST be allowed up to `policy.max_in_flight`.
- Each admitted `route` / `complete` call increments/decrements an `AtomicUsize` in_flight counter and notifies drain when the counter reaches zero.
- Provider client connection pool provides reuse (§10).
- `SharedCostMeter` already serializes updates via `Mutex` (RFC-0004).
- DecisionLog append concurrency is owned by the session event log.
- If all permits are in use, a new caller awaits a permit unless shutdown begins first. Admission waiting MUST register a shutdown notification before the phase pre-check, then `select!` on semaphore permit vs. shutdown notification. `shutdown()` MUST also close the semaphore or otherwise wake permit waiters so pending admission returns `ShuttingDown` promptly.

### 6.4 Cancellation

| Mechanism | Behaviour |
| --- | --- |
| Drop of `complete` / `route` future | In-flight HTTP aborted (reqwest cancel-on-drop); in_flight dec; **no** DecisionLog / meter update |
| Host-level cancellation token fires before provider returns | `Err(Cancelled)`; no ModelCall unless the provider attempt already returned and entered the meter+record critical section |
| Shutdown grace expires | router calls `shutdown_token.cancel()`; still-polled in-flight provider calls observe `Err(Cancelled)` |

No per-call `CancellationToken` field is added to V2 trait signatures. The cancellation token is router-owned / injected through `TomlModelRouterParts`, matching the RFC-0006 host-level pattern.

### 6.5 Timeouts

| Timeout | Default | Source |
| --- | --- | --- |
| Connect | 10s | `[policy].connect_timeout_ms` |
| Request (total) | 120s | `[policy].request_timeout_ms` |

On expiry → `ProviderError::Timeout` (retryable classification for RFC-0010; **no retry here**).

### 6.6 Drain / shutdown

```text
shutdown():
  winner = compare_exchange(Ready, Draining)
  if phase was Stopped: return RouterShutdownReport { cancelled_in_flight: false, remaining_in_flight: 0 }
  if another shutdown already set Draining:
    follow it by waiting for Stopped with timeout = shutdown_grace_ms + min(1000ms, shutdown_grace_ms)
    return RouterShutdownReport from shared AtomicBool cancelled_in_flight + current in_flight
  notify waiters so pending admission returns ShuttingDown
  enable-then-check drain wait:
    if in_flight == 0: set Stopped and notify
    else wait on Notify up to shutdown_grace_ms
  if in_flight > 0 after grace:
    shutdown_token.cancel()
    wait one bounded post-cancel grace of min(1000ms, shutdown_grace_ms)
  set Stopped, notify all, return report with remaining_in_flight
```

`shutdown` is idempotent. Concurrent callers MUST NOT race a Stopped→Draining transition. The first caller performs cancellation and stores `cancelled_in_flight` in an `AtomicBool`; followers observe the final `Stopped` phase or the bounded follower timeout and report the stored flag plus the current `in_flight`. If `remaining_in_flight > 0`, shutdown MUST log `warn` and return that count in `RouterShutdownReport`.

### 6.7 Connection reuse

One `reqwest::Client` per `OpenAiCompatibleProvider` instance, created at construction, cloned cheaply for requests. MUST NOT build a new client per `complete`.

### 6.8 Sequence — shutdown with in-flight

```mermaid
sequenceDiagram
  participant O as Owner
  participant R as TomlModelRouter
  participant P as Provider HTTP

  par in-flight
    R->>P: complete
  and shutdown
    O->>R: shutdown()
    Note over R: Draining — new route/complete → ShuttingDown
  end
  P-->>R: Ok or cancel
  Note over R: Stopped
```

---

## 7. Configuration

### 7.1 Authoritative `router.toml` schema

```toml
# router.toml — Author: arkadianet
[policy]
default_tier = "standard"          # ModelTier; REQUIRED
connect_timeout_ms = 10000         # u64; default 10000
request_timeout_ms = 120000        # u64; default 120000
shutdown_grace_ms = 5000           # u64; default 5000
max_in_flight = 32                 # u32; default 32; MUST be > 0

# Stubbed unused (ADR F-20). MAY be omitted. If present, parsed and ignored by select.
[policy.scoring]
# complexity_weight = 0.0
# budget_weight = 0.0
# latency_weight = 0.0

[[providers]]
id = "openai-compatible-main"      # ProviderId; REQUIRED
kind = "openai_compatible"         # ONLY supported kind in MVP
base_url = "https://api.example.com/v1"  # REQUIRED; https or loopback http
api_key_env = "ALLOY_API_KEY"      # REQUIRED for openai_compatible

[[providers.endpoints]]
id = "team-workhorse"              # EndpointId; REQUIRED
display_name = "Workhorse"         # REQUIRED
model = "REPLACE_ME"               # REQUIRED wire model id (BYOM — operator sets)
tiers = ["standard"]               # non-empty Vec<ModelTier>; REQUIRED
supports_tools = true              # bool; default false
supports_structured_output = true  # bool; default false
max_context = 200000               # u32; REQUIRED; MUST be > 0
# Optional f64 >= 0 finite. Omit both to keep ModelCallRecord.usd = None.
# A literal 0.0 means measured/declared free, not unknown.
# input_usd_per_mtok = 0.0
# output_usd_per_mtok = 0.0

[capability_tiers]
repair = "standard"
edit = "standard"
review = "economy"
planning = "standard"
```

### 7.2 Validation rules

| Rule | Failure |
| --- | --- |
| File missing / unreadable / invalid TOML | `RouterError::Config` |
| `default_tier` missing / unknown string | Config |
| `[[providers]]` empty | Config |
| MVP: providers.len() != 1 | Config (`"MVP allows exactly one provider"`) |
| `kind != "openai_compatible"` | Config |
| `id` / endpoint `id` empty or >128 | Config |
| Duplicate provider or endpoint ids | Config |
| `base_url` empty | Config |
| `base_url` scheme is neither `https` nor `http` | Config |
| `base_url` uses `http` and host is not loopback (`localhost`, `127.0.0.0/8`, `::1`) | Config |
| `api_key_env` empty | Config |
| Env var unset/empty at construct | Config with `example.env` hint |
| No endpoints under the provider | Config |
| Endpoint `tiers` empty | Config |
| Endpoint `display_name` empty or >256 UTF-8 bytes | Config |
| Endpoint `model` empty or >512 UTF-8 bytes | Config |
| `max_context == 0` | Config |
| Negative / non-finite price fields | Config |
| `capability_tiers` value not a valid `ModelTier` | Config |
| Timeouts == 0 | Config |
| `max_in_flight == 0` | Config |
| capability keys collide after ASCII-lowercase normalization | Config |

**Trailing slash:** `base_url` is normalized by trimming one trailing `/` at construct.

`max_context` is advisory in MVP. RFC-0007 validates it is present and non-zero but does not enforce a tokenizer-based pre-flight check; context overflow is surfaced through `ProviderError::ContextLength`.

**Scheme validation:** both `RouterConfig` validation and `OpenAiCompatibleProvider::new` MUST enforce the same rule. Public HTTPS is required; loopback HTTP is allowed for local openai-compatible servers and CI wiremock. Non-loopback plaintext HTTP is forbidden.

**URL parser:** validation MUST use the `url` crate (`url::Url`) added in §10.2, available regardless of `http-provider`. Hand-rolled parsing is forbidden. The loopback predicate is:

1. Parse with `Url::parse`; parse failure → Config.
2. Require scheme `https` or `http`.
3. For `https`, accept any syntactically valid host.
4. For `http`, accept only:
   - `host_str().eq_ignore_ascii_case("localhost")`;
   - an IPv4 host parsed by `Url` whose `std::net::Ipv4Addr::is_loopback()` is true;
   - an IPv6 host parsed by `Url` whose `std::net::Ipv6Addr::is_loopback()` is true.
5. Reject userinfo for all provider URLs (`username` non-empty or `password` present) so `http://127.0.0.1@evil.example.com` cannot confuse review/logs.
6. Reject hostless URLs.

`localhost.evil.com`, non-canonical integer IPv4 spellings, and IPv4-mapped IPv6 addresses are accepted only if `url::Url` exposes them as loopback IP addresses through the standard `IpAddr` parser; otherwise they are rejected.

### 7.3 `RouterConfig` Rust shape

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct RouterConfig {
    pub policy: RouterPolicy,
    pub providers: Vec<ProviderConfig>,
    pub capability_tiers: BTreeMap<String, ModelTier>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouterPolicy {
    pub default_tier: ModelTier,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_grace: Duration,
    pub max_in_flight: u32,
    pub scoring: ScoringWeights, // stub
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScoringWeights {
    pub complexity_weight: Option<f64>,
    pub budget_weight: Option<f64>,
    pub latency_weight: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key_env: String,
    pub endpoints: Vec<EndpointConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenaiCompatible,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    pub id: EndpointId,
    pub display_name: String,
    pub model: String,
    pub tiers: Vec<ModelTier>,
    pub supports_tools: bool,
    pub supports_structured_output: bool,
    pub max_context: u32,
    pub input_usd_per_mtok: Option<f64>,
    pub output_usd_per_mtok: Option<f64>,
}
```

```rust
impl RouterConfig {
    /// Load, parse, normalize, and validate `router.toml`.
    /// Does not resolve API keys and is available without `http-provider`.
    pub fn load(path: &Path) -> Result<Self, RouterError>;

    /// Parse, normalize, and validate TOML from a string (tests / embedded configs).
    /// `source_name` is used only in redacted error messages.
    pub fn from_str(source_name: &str, toml: &str) -> Result<Self, RouterError>;
}
```

`RouterConfig::load` / `from_str` own TOML parse, capability-key normalization, duplicate detection, URL scheme/loopback/userinfo validation, endpoint validation, and timeout/max-in-flight validation. They are ungated so `--no-default-features` still verifies router config semantics. `TomlModelRouter::from_paths` calls `RouterConfig::load`, then resolves `api_key_env` and builds the HTTP provider behind `http-provider`.

`EndpointConfig` converts to `ModelEndpoint` by copying every same-named field and setting `ModelEndpoint.provider = ProviderConfig.id` from the owning provider. There is no endpoint-level provider override in TOML.

### 7.4 `router.toml.example` deliverable

Verify and parse-test the shipped `router.toml.example` against §7.1 (using placeholder `model = "REPLACE_ME"` and example host). Comments MUST state: copy to `router.toml`; set `model` and `base_url` for your provider; set `ALLOY_API_KEY` in process env / personal `.env` (Alloy never writes `.env`).

### 7.5 `example.env` keys

Existing key remains authoritative:

```bash
# Provider key name referenced by router.toml api_key_env
ALLOY_API_KEY=
```

**No new required env keys** for MVP timeouts (TOML owns them). Optional comment block MAY document that `ALLOY_API_KEY` must match `api_key_env`. **MUST NOT create or modify `.env`.**

### 7.6 Amendment to `RuntimeConfig::load` (RFC-0001 provisional peek)

| Before (main) | After (this RFC) |
| --- | --- |
| Parse `[provider.*]` map; warn if `api_key_env` unset | Require `router_path` is a file; **do not** parse provider tables |
| Tests write `[provider.default]` | Tests write minimal valid §7 schema **or** only assert file existence |
| Profile budgets parsed and discarded | `RuntimeConfig` exposes `pub budget_policy: BudgetPolicy` built from `[budgets]` |

Full router validation + key resolution moves to `TomlModelRouter::from_paths` / `RouterConfig::load`. Profile budget parsing stays in `RuntimeConfig::load` and MUST populate `RuntimeConfig::budget_policy`; callers MUST pass that policy to `TomlModelRouter::from_paths` or `from_parts`. `[budgets].max_usd_per_run` and `[budgets].max_tokens_per_run` override those two fields; `max_parallel_nodes`, `max_parallel_cargo`, and `max_parallel_edits` remain from `BudgetPolicy::default()` until RFC-0015 owns full profile UX. This is an **explicit amendment** to the provisional RFC-0001 config peek, required because that peek’s schema conflicts with V2 §11.2 `[[providers]]` and cannot express endpoints/tiers. Implementations MUST NOT silently fall back to `BudgetPolicy::default()` when a profile budget was loaded.

---

## 8. Error Handling

### 8.1 `RouterError` variant table

| Variant | Producer | Meaning | Retryable? | Persist decision? | Caller visibility |
| --- | --- | --- | --- | --- | --- |
| `Config` | load/validate | bad TOML / invariants | no | n/a (construct) | yes |
| `NoEndpoint` | select | no matching endpoint | no | ModelRoute | yes |
| `BudgetDenied` | route §5.4 | ceilings exhausted | no | Budget | yes |
| `Provider` | complete | wrapped provider failure | see §8.2 | ModelCall | yes |
| `Cancelled` | host-level cancellation token | cancelled before provider result | no | no | yes |
| `ShuttingDown` | lifecycle | drain/stop | no | no | yes |
| `Internal` | invariant | bug | no | optional | yes |

### 8.2 `ProviderError` variant table

| Variant | Producer | Meaning | Retryable? (for RFC-0010) | Persist ModelCall? | Caller visibility |
| --- | --- | --- | --- | --- | --- |
| `Auth` | HTTP 401/403 | bad/missing key rejected by server | no | yes | yes (no body) |
| `RateLimit` | HTTP 429 | rate limited | **yes** | yes | yes |
| `ContextLength` | HTTP 400 with context-length signals | prompt too large | no | yes | yes |
| `Timeout` | client timeout | connect/request deadline | **yes** | yes | yes |
| `MalformedResponse` | JSON/shape | unusable body | no | yes | yes (redacted) |
| `HttpStatus` | other 4xx/5xx | mapped status | 5xx **yes**; other 4xx no | yes | yes |
| `Transport` | DNS/TLS/connect reset | I/O | **yes** | yes | yes |
| `Internal` | provider bug | invariant | no | optional | yes |

**Retry boundary:** RFC-0007 MUST classify retryability as above and MUST NOT implement a retry loop, backoff, or automatic re-route. RFC-0010 owns retries.

### 8.3 HTTP status → `ProviderError` mapping

| Condition | Variant |
| --- | --- |
| 401, 403 | `Auth` |
| 429 | `RateLimit` |
| 400 and body matches context-length heuristics (§8.3.1) | `ContextLength` |
| Client timeout / `reqwest::Error::is_timeout` | `Timeout` |
| Other status | `HttpStatus { status, message }` |
| JSON parse / missing choices | `MalformedResponse` |
| Non-HTTP I/O | `Transport` |

#### 8.3.1 Context-length heuristics

Case-insensitive body substring match on any of:

`context_length_exceeded`, `context length`, `maximum context`, `maximum tokens exceeded`, `too many tokens`, `prompt is too long`

Only applied on HTTP 400. Prefer structured `error.code == "context_length_exceeded"` when present.

### 8.4 `ProviderError` → `ErrorClass` (ModelCall)

| ProviderError | `ErrorClass` |
| --- | --- |
| `Timeout` | `Timeout` |
| `Auth`, `RateLimit`, `ContextLength`, `MalformedResponse`, `HttpStatus`, `Transport`, `Internal` | `Model` |

`BudgetDenied` → DecisionKind::Budget only (not ModelCall); callers MAY map to `ErrorClass::Budget` at scheduler layer (RFC-0010).

### 8.5 Boundary conversion to `RuntimeError`

```rust
impl From<RouterError> for RuntimeError {
    fn from(e: RouterError) -> Self {
        match e {
            RouterError::Config(s) => RuntimeError::Config(s),
            other => RuntimeError::Internal(other.to_string()),
        }
    }
}
```

Host code that surfaces router failures to CLI MAY pattern-match `RouterError` directly instead of collapsing to `Internal`. Additive `RuntimeError::Router(RouterError)` is **optional** and, if introduced, MUST be `#[non_exhaustive]`-compatible; prefer not expanding `RuntimeError` unless a caller on the critical path needs it in this RFC’s implementation PR. Default: map as above without amending the enum unless required by compile integration.

### 8.6 Recovery semantics

| Failure | Recovery |
| --- | --- |
| `Config` (including missing API key env) | Operator fixes TOML / exports env; never auto-write `.env` |
| `BudgetDenied` | Caller ends run or raises budget (RFC-0010/0015) — router does not downgrade |
| `NoEndpoint` | Fix TOML endpoints / capability tiers |
| `RateLimit` / `Timeout` / `Transport` / 5xx | RFC-0010 may retry |
| `Auth` | Fix API key |
| `ContextLength` | Caller shrinks PromptPack (RFC-0012) — no auto-retry |
| `MalformedResponse` | Treat as provider defect; no retry by default |

---

## 9. Observability

### 9.1 Tracing spans (REQUIRED)

| Span / event | Level | Fields |
| --- | --- | --- |
| `alloy.router.route` | info span | `session`, `run?`, `capability`, `tier`, `endpoint_id?` |
| `alloy.router.complete` | info span | `session`, `provider_id`, `endpoint_id`, `tier`, `duration_ms` |
| `alloy.router.provider_http` | debug span | `provider_id`, `endpoint_id`, `status?` |
| budget deny | warn | `budget_check`, `capability` |
| obs record failure | warn | `err` |
| missing api key at construct | error | `env_key`, `hint` (path only) |
| prompt body oversize | warn | `session`, `bytes`, `limit` |

**MUST NOT** log: API key values, `Authorization` headers, raw `.env` contents, full prompt bodies at info (debug MAY log lengths only).

### 9.2 Decision / model-call payloads

Honour RFC-0004 retention defaults (metadata + hashes; bodies stripped unless opt-in). Route metadata MUST NOT include secrets. Model call `prompt_body` subject to `RetentionPolicy`.

Metadata values that are strings MUST be safe; prefer enums/bools/numbers for structured fields. `model` in metadata is the **configured wire id** (operator BYOM), not a hardcoded core constant.

### 9.3 Metrics — `RouterMetricsSnapshot`

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterMetricsSnapshot {
    pub routes_ok: u64,
    pub routes_budget_denied: u64,
    pub routes_no_endpoint: u64,
    pub routes_default_tier: u64,
    pub completes_ok: u64,
    pub completes_err: u64,
    pub in_flight: usize,
    pub obs_record_errors: u64,
    pub model_call_prompt_body_oversize: u64,
}
```

Incremented on returned results (not on future drops).

---

## 10. Crate Dependencies & `unsafe`

### 10.1 HTTP client choice (normative)

| Decision | Choice | Justification |
| --- | --- | --- |
| Crate | `reqwest` pinned exactly to `=0.13.4` | Async, Tokio-native, current registry version at RFC expansion |
| Cargo feature | `http-provider` default-on in `alloy-runtime`; traits/recording compile without it | Consumers that only need `ModelProvider` (RFC-0016 scripts) can opt out of HTTP/TLS |
| Default features | **off** | Avoid accidental native-tls/default changes |
| Enabled reqwest features | `json`, `rustls`, `system-proxy`, `http2`, `charset`, `stream` | Exact feature set from `reqwest = =0.13.4`; `stream` is for capped response reads, not model streaming; implementation PR MUST verify with `cargo tree -e features` |
| TLS crypto provider | AWS-LC via `reqwest` 0.13.4 `rustls` feature (`__rustls-aws-lc-rs`) | First TLS dependency names its crypto backend; this is **not** a pure-Rust-only TLS stack |
| Root store | platform verifier via `rustls-platform-verifier` included by `reqwest` 0.13.4 `rustls` feature | Supports corporate/private CA BYOM gateways better than bundled webpki-only roots |
| Certificate bypass | `danger_accept_invalid_certs(false)` and no config knob | Fail closed |
| JSON | `json` feature | serde_json bodies |
| Redirects | `redirect::Policy::none()` | Fail closed — API should not redirect with Bearer tokens |
| Proxy | explicit: honour standard system / environment HTTPS proxy support through enabled `system-proxy`; no proxy credentials logged | Operator standard; Bearer requests still use no redirects |
| Connect timeout | policy / 10s | §6.5 |
| Request timeout | policy / 120s | §6.5 |
| Connection reuse | single `Client` | §6.7 |
| HTTP/2 | enabled explicitly by reqwest `http2` feature | allowed |
| Cookies | disabled | not required |

Workspace `Cargo.toml` MUST add `reqwest` under `[workspace.dependencies]`; `alloy-runtime` depends on it only through default-on `http-provider`. `OpenAiCompatibleProvider`, `OpenAiCompatibleSpec`, and `http_client` are `#[cfg(feature = "http-provider")]`; traits, `PromptPack`, `ModelEndpoint`, `TomlModelRouter::from_parts`, and `RecordingModelProvider` remain available without HTTP.

This feature set intentionally recreates reqwest 0.13.4's useful defaults (`rustls`, `charset`, `http2`, `system-proxy`) while keeping `default-features = false` so native-tls is not enabled accidentally, then adds `json` and `stream` for request/response bodies and capped reads.

**Version bump procedure:** the implementation PR pins `=0.13.4` to make the first TLS dependency review reproducible. A later patch-level reqwest bump is allowed without a new architecture RFC if it updates this table, verifies feature expansion with `cargo tree -e features`, and reruns the wiremock/no-default-features suite.

**Dev-dependency:** `wiremock` (or `httpmock`) for CI HTTP tests without network egress.

### 10.2 Other new dependencies

| Dep | Reason |
| --- | --- |
| `reqwest` | First network client |
| `url = "2.5.8"` | URL parse/loopback validation independent of `http-provider` |
| `wiremock` (dev) | Offline HTTP |

No other new runtime deps required beyond `reqwest` and `url` (`async-trait`, `serde`, `tokio`, `thiserror`, `tracing`, `toml` already present).

### 10.2.1 HTTP body and header safety

- Authorization header MUST be built as a `HeaderValue` and marked `set_sensitive(true)` before insertion.
- Success and error bodies MUST be read with an explicit cap of 1 MiB. Bodies larger than the cap return `ProviderError::MalformedResponse("response body too large")` or `HttpStatus` with a truncated redacted message for non-2xx.
- Error messages stored in `ProviderError` MUST be redacted and truncated to ≤512 bytes on a UTF-8 character boundary; byte slicing that can panic on multibyte input is forbidden.
- Request/response debug logging MUST NOT include headers or raw bodies.

### 10.3 `unsafe`

`alloy-runtime` remains `#![forbid(unsafe_code)]`. This RFC adds **zero** `unsafe`.

### 10.4 Retry policy ownership

**RFC-0010** consumes §8 retryability. RFC-0007 MUST NOT sleep-and-retry inside `OpenAiCompatibleProvider::complete` or `TomlModelRouter::complete`.

---

## 11. Testing Strategy

### 11.1 Unit

| Test | Asserts |
| --- | --- |
| `toml_parse_v2_example` | §7.1 example parses |
| `toml_rejects_non_loopback_http_base_url` | `http://example.com` fails |
| `toml_accepts_loopback_http_base_url` | `http://127.0.0.1` / `http://localhost` accepted for local CI/provider use |
| `toml_rejects_base_url_userinfo` | `https://user@example.com` rejected |
| `toml_rejects_two_providers` | MVP single provider |
| `toml_rejects_duplicate_provider_endpoint_ids` | duplicate ids rejected |
| `toml_rejects_empty_model` | Config error |
| `toml_rejects_zero_max_in_flight` | `max_in_flight = 0` rejected |
| `tier_from_capability_map` | `repair`→standard |
| `tier_normalizes_capability_id` | `CapabilityId::new("Repair")` and `"repair"` both resolve through normalized map |
| `toml_rejects_capability_key_collision` | `Repair` + `repair` rejected |
| `tier_default_when_unknown_capability` | default_tier used |
| `endpoint_first_match_order` | declaration order |
| `endpoint_filters_tools_structured` | feature flags |
| `budget_denied_no_escalation` | Premium/any tier denied; no downgrade |
| `route_uses_meter_before_snapshot` | injected meter is budget source |
| `check_budget_snapshot_zero_usd_diff` | pins fallback difference from live meter when `max_usd_per_run == 0.0` |
| `derive_usd_known` | tokens+prices → finite usd |
| `derive_usd_missing_price_is_none` | no invention |
| `derive_usd_missing_tokens_is_none` | no invention |
| `usage_omitted_tokens_none` | mapping |
| `malformed_usage_keeps_completion` | invalid usage degrades to `None` tokens |
| `oversize_prompt_body_hash_only` | large prompt records hash and no body; ModelCall still appended |
| `secret_debug_redacted` | Debug has no key material |
| `authorization_header_sensitive` | HeaderValue marked sensitive |
| `prompt_pack_serde_roundtrip` | messages/citations stable |
| `scoring_weights_ignored` | changing weights does not change route |
| `recording_provider_fifo` | Scripted seam |
| `no_hardcoded_vendor_model_ids` | §11.6 |

### 11.2 Integration (no live network)

| Test | Asserts |
| --- | --- |
| `openai_complete_wiremock_ok` | 200 JSON → ModelResponse + meter + ModelCall |
| `openai_auth_401` | → `Auth` |
| `openai_429` | → `RateLimit` |
| `openai_context_length` | → `ContextLength` |
| `openai_timeout` | short timeout → `Timeout` |
| `openai_malformed` | → `MalformedResponse` |
| `openai_200_error_object` | 200 with top-level error → `MalformedResponse` |
| `openai_finish_reason_length` | completion OK; `ModelCallRecord.finish_reason` populated |
| `route_then_complete_decision_log` | ModelRoute + ModelCall recorded |
| `model_call_has_endpoint_model_route_seq` | added RFC-0004 fields populated |
| `usage_unknown_roundtrip_query` | append + `parse_model_call_event` OK when tokens None |

### 11.3 Negative / budget / cancel / concurrency

| Test | Asserts |
| --- | --- |
| `missing_api_key_fail_closed` | construct err; no `.env` write |
| `budget_denial_no_http` | wiremock received 0 requests |
| `drop_complete_no_obs` | drop → no ModelCall |
| `host_cancel_returns_cancelled` | router cancellation token produces reachable `Cancelled` |
| `shutdown_rejects_new` | ShuttingDown |
| `shutdown_idempotent_concurrent` | concurrent shutdowns return final report |
| `max_in_flight_bounds_admission` | calls above limit await or reject on drain |
| `concurrent_completes` | N parallel OK with recording/wiremock |

### 11.4 `ScriptedProvider` contract test (shared with RFC-0016)

| Test | Location | Asserts |
| --- | --- | --- |
| `model_provider_object_safe_arc` | `alloy-runtime` | `Arc<dyn ModelProvider>` compiles with `RecordingModelProvider` |
| `scripted_provider_implements_trait` | `alloy-eval` (0016) | ScriptedProvider implements `ModelProvider` |

RFC-0007 ships the runtime half; RFC-0016 adds the eval half against the same trait.

### 11.5 Error mapping table tests

One test per §8.3 status mapping row.

### 11.6 No vendor model IDs in core

Normative automated test:

1. Walk `crates/alloy-runtime/src/router/**/*.rs` excluding `recording.rs`. For each file, scan only the prefix before the first line containing `#[cfg(test)]`.
2. Fail if lowercase file contents contain any deny-list substring of known vendor model id patterns (e.g. `gpt-4`, `gpt-3.5`, `claude-3`, `claude-opus`, `gemini-`, `o1-`, `o3-`) as Rust string literals. Use plain case-insensitive substring search; do not add a regex dependency for this.
3. Review checklist (not mechanically tested): no `match` on provider id/kind may select vendor model ids; `openai_compatible` is a protocol kind only.

Operator `model` values appear only in TOML examples/tests as config data, not as Rust literals used for branching in core logic.

### 11.7 CI network policy

Provider HTTP tests MUST use `wiremock` on localhost. CI MUST NOT require `ALLOY_API_KEY` or egress to public model APIs for RFC-0007 tests.

HTTP provider and wiremock tests MUST be `#[cfg(feature = "http-provider")]` gated. CI MUST also run `cargo test -p alloy-runtime --no-default-features` (or an equivalent feature-gate compile check) proving traits, shared DTOs, `RouterConfig`, `TomlModelRouter::from_parts`, and `RecordingModelProvider` compile without `reqwest` / TLS. In a resolver-2 workspace build, any package enabling `alloy-runtime` default features will still build `http-provider`; the no-default check must target `alloy-runtime` directly or an explicitly no-default dependent.

---

## 12. MVP vs Deferred

### 12.1 Implemented by RFC-0007

Traits, `TomlModelRouter`, one `openai_compatible` provider, full TOML schema, health stub, scoring stub ignored, decision + cost integration, additive RFC-0004 model-call attribution fields, USD from operator price table, recording seam, error taxonomy, bounded HTTP client behind `http-provider`, tests, example updates, config/budget amendment.

### 12.2 Deferred (reference only — no design)

| Item | Owner |
| --- | --- |
| Retry / backoff / multi-attempt | **RFC-0010** |
| Full PromptPack domains / assembly | **RFC-0012** |
| Workers calling router with tools | **RFC-0013** |
| `ScriptedProvider` body + holdout gates | **RFC-0016** |
| Multi-provider failover / scoring | ADR F-20 / V2 evolution |
| Streaming API | Future RFC |
| Second provider kind | Future RFC |
| Cost marketing bands | Forbidden until Eval calibrates |

---

## 13. Acceptance Criteria

Every criterion is independently testable.

| # | Criterion | Test / proof |
| --- | --- | --- |
| 1 | `ModelRouter` / `ModelProvider` signatures match §3 | compile + docs |
| 2 | Exactly one `openai_compatible` provider works against wiremock | `openai_complete_wiremock_ok` |
| 3 | `router.toml.example` matches §7 schema | parse test |
| 4 | `example.env` documents `ALLOY_API_KEY`; Alloy never writes `.env` | existing sentinel pattern + construct test |
| 5 | No hardcoded vendor model IDs in router core | §11.6 |
| 6 | No `match` on vendor brands for model selection | §11.6 |
| 7 | Route records `DecisionKind::ModelRoute` | integration |
| 8 | Complete records `ModelCall` with honest Option tokens | integration |
| 9 | Omitted/malformed provider usage → token fields None; query invariant holds | `usage_unknown_roundtrip_query` / `malformed_usage_keeps_completion` |
| 10 | `add_model_usage` called on complete outcomes when meter injected | unit/integration |
| 11 | USD only when prices + tokens known; else None; `usd_source` set only for derived USD | price + ModelCall tests |
| 12 | No cost marketing strings in router module | grep / review |
| 13 | Budget denial at `route`; meter takes precedence over snapshot; no tier escalation | `route_uses_meter_before_snapshot`, `budget_denied_no_escalation` |
| 14 | Scoring weights unused | `scoring_weights_ignored` |
| 15 | `health()` always Healthy | unit |
| 16 | MVP non-streaming (`stream: false`) | wiremock request assert |
| 17 | Missing/empty API key fail closed | `missing_api_key_fail_closed` |
| 18 | Secrets redacted in Debug / errors / events; Authorization header sensitive | secret + redact + header tests |
| 19 | `RecordingModelProvider` satisfies `ModelProvider` | contract test |
| 20 | Drop cancel performs no obs write; host cancellation returns `Cancelled` | `drop_complete_no_obs`, `host_cancel_returns_cancelled` |
| 21 | `RuntimeConfig::load` no longer parses `[provider.*]` and exposes profile `budget_policy` | config unit tests updated |
| 22 | `#![forbid(unsafe_code)]` preserved; reqwest version/features/TLS backend justified | crate attrs + Cargo.toml |
| 23 | Retry loop absent | code review + no sleep-retry in openai.rs |
| 24 | Host egress documented vs sandbox Network grants | §2.6 present |
| 25 | `ModelCallRecord` additive endpoint/model/route/usd_source/finish_reason/provider_request_id amendment implemented | schema + parse/reaccumulate tests |
| 26 | Large prompts preserve durable ModelCall by hash-only body handling | `oversize_prompt_body_hash_only` |
| 27 | `max_in_flight` bounds concurrent provider calls | semaphore test |
| 28 | `--no-default-features` keeps non-HTTP router surfaces compiling | CI feature-gate check |
| 29 | RFC-0004 and RFC-0001 merged text updated for additive amendments | docs diff |

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

## 15. Open Questions

Only genuine unresolved items. Settled decisions are not reopened.

1. **Optional `RuntimeError::Router` variant:** Prefer mapping via `From` to `Config`/`Internal` (§8.5). Introduce an explicit variant only if a critical-path caller in the same implementation PR requires exhaustive matching — track as a micro-amendment in the impl PR description if needed.

---

## 16. Estimated Implementation Effort

### 16.1 Implementation slices

| Slice | Work | Effort |
| --- | --- | --- |
| A | Types, errors, `PromptPack`, traits, module skeleton, additive ModelCall fields | 0.75–1 pd |
| B | `RouterConfig` parse/validate + example.toml + config/budget amendment | 1–1.25 pd |
| C | `select` + meter-first budget check + price math + unit tests | 0.75–1 pd |
| D | `http-provider` feature, pinned `reqwest`, TLS/proxy/root-store policy, `OpenAiCompatibleProvider`, wiremock suite | 2–2.5 pd |
| E | `TomlModelRouter` route/complete + cancellation-safe obs/meter bridges + RFC-0006-grade lifecycle | 1.75–2.25 pd |
| F | `RecordingModelProvider` + no-hardcoded-ID test + docs polish | 0.75–1 pd |

### 16.2 Expected effort

**7–9 person-days** total (matches index).

### 16.3 Dependencies / sequencing

1. Merged RFC-0001 + RFC-0004 on `main` (satisfied).
2. Implement A→F in order; D and C may overlap after B.
3. RFC-0016 skeleton may start as soon as traits in slice A land.
4. RFC-0010 / 0013 consume errors and router after merge.

### 16.4 Risk notes

| Risk | Mitigation |
| --- | --- |
| First HTTP dep increases build surface | pinned reqwest; default-features off; named TLS backend/root store; CI must have a C toolchain for AWS-LC; wiremock offline |
| Cost schema mismatch with RFC-0004 | explicit additive amendment; §5.9 mapping; query/reaccumulate tests |
| Operators leave `model = "REPLACE_ME"` | Config allows it (BYOM); live calls fail at provider — document clearly |

---

## Appendix A — `RoutedModel` field list

See §3.8 for the normative `RoutedModel` struct (includes `requires_structured_output`).

## Appendix B — Secret handling checklist (normative)

| Surface | Rule |
| --- | --- |
| Events / Decision metadata | MUST NOT contain API key; sensitive JSON keys redacted by RFC-0004 helpers |
| Tracing | MUST NOT log key or Authorization |
| `Debug` | `SecretString` → `SecretString([REDACTED])` |
| HTTP header | `Authorization` MUST use `HeaderValue::set_sensitive(true)` |
| Provider body | Read cap 1 MiB; error strings redacted and UTF-8-boundary truncated |
| Error messages | `redact_secrets` + truncate |
| `.env` | NEVER create or modify |
| Unset `api_key_env` | Fail closed; hint `example.env` |

## Appendix C — Compatibility matrix for RFC-0016

| Requirement on `ScriptedProvider` | Specified by |
| --- | --- |
| Implements `ModelProvider` | §3.11 |
| `health` → `Healthy` | §3.11 / §1.5 |
| No HTTP | §3.15 / §12 |
| Deterministic `complete` | RFC-0016 |
| Works behind `TomlModelRouter::from_parts` | §3.13 |
