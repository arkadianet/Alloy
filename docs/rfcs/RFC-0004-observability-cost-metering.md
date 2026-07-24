# RFC-0004: Observability & Cost Metering

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001, RFC-0002 |
| Effort | 2–4 person-days |

## Purpose

Record attributable decisions (routing, context inclusion, tool grants, retries) and always-on cost metering APIs. Default retention = metadata + content hashes; full prompts / tool bodies opt-in per session (V2 §15, ADR F-17).

## Scope

### In scope

- Decision record writer (append via session events `decision` / `model_call` / `tool_call`)
- Cost counters: tokens in/out, estimated USD fields when provider reports usage
- Hash helpers for prompt/tool payloads
- Query helpers for `alloy events` (CLI wires in RFC-0015)
- Profile flags: `retain_full_prompts`, `retain_tool_bodies` (Appendix B)

### Out of scope

- Observability TUI → deferred (V2 §15)
- Separate OTel crate / mandatory OTLP → deferred
- Numeric marketing cost claims → forbidden until Eval calibrates (V2 §18; [RFC-0016](./RFC-0016-eval-harness-holdout-gates.md))
- Product routing policy → [RFC-0007](./RFC-0007-model-router-provider.md)

## Dependencies

- **RFC-0001** — IDs, `WorkerMetrics` field shapes
- **RFC-0002** — event append storage

## Public API

```rust
#[async_trait]
pub trait DecisionLog: Send + Sync {
    async fn record(&self, rec: DecisionRecord) -> Result<(), ObsError>;
}

pub struct DecisionRecord {
    pub session: SessionId,
    pub run: Option<RunId>,
    pub node: Option<NodeId>,
    pub kind: DecisionKind, // route | context | tool_grant | retry | gate | …
    pub metadata: serde_json::Value,
    pub content_hash: Option<Digest>,
    pub prompt_body: Option<String>, // only if retain_full_prompts
}

pub struct CostMeter {
    // incremental APIs
    pub fn add_model_usage(&mut self, tier: ModelTier, input: u64, output: u64, usd: Option<f64>);
    pub fn snapshot(&self) -> CostSnapshot;
}

pub struct WorkerMetrics {
    pub model_tier_used: ModelTier,
    pub provider_id: ProviderId,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tool_calls: u32,
    pub cache_hits: u32,
    pub duration_ms: u64,
    pub confidence: f32,
    pub error_class: Option<ErrorClass>,
}
```

## Internal architecture

Thin module in `alloy-runtime::obs` wrapping `EventStore`. Redaction applied before append when flags false.

## Data structures

Cost snapshot per run/capability; decision records as event payloads. No separate observability database in MVP.

## State machine

N/A — append-only telemetry. Node state transitions are owned by the DAG (RFC-0009) and mirrored as `node_state` events here.

## Failure modes

| Failure | Handling |
| --- | --- |
| Meter overflow / missing usage | Record tokens as unknown; never invent marketing savings |
| Redaction bug leaking `.env` / secrets | Deny-list paths; redact `api_key` patterns |
| Obs write failure | Fail closed for auditable runs or degrade with explicit `error` event |

## Testing strategy

- Unit: hash stability; redaction strips bodies when flags false
- Unit: cost snapshot arithmetic
- Integration: decision events readable after a fake model_call

## Acceptance criteria

- [ ] Default log = metadata + hashes only
- [ ] Opt-in full prompts/bodies honored per session profile
- [ ] Cost metering APIs always available to router/workers
- [ ] No separate OTel crate in MVP
- [ ] No numeric savings claims in code/docs output

## Definition of Done

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

## Estimated implementation effort

**2–4 person-days**.

## Future extensions

- OTLP export; rich TUI reading same log (V2 §15)
- Calibrated cost bands published only from holdout runs (V2 §18)
