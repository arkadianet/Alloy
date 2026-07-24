# RFC-0016: Eval Harness & Holdout Gates

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001, RFC-0007 |
| Effort | 5–8 person-days |

## Purpose

Eval gates milestones from week 1: fixtures, `ScriptedProvider`, recorded cargo JSON, holdout local-diagnostic compile success. Falsify control-plane thesis early—without numeric cost marketing until calibrated (V2 §17, ADR F-19/F-25).

## Scope

### In scope

- Crate `alloy-eval`: fixture manifests, offline thesis tests
- `ScriptedProvider: ModelProvider` for deterministic completions
- Recorded `cargo check --message-format=json` fixtures
- `EvalMetrics` struct (V2 §17.2)
- Holdout set for local-diagnostic / E0502-class repairs (P0)
- Milestone exit gate helpers (pass/fail thresholds configurable; M1 bar = compile success under sandbox path when full stack present)
- Permitted corpora only (license hygiene, R17)

### Out of scope

- Production routing policy → RFC-0007
- Large multi-crate feature suites / public leaderboard → deferred
- Lifetime-heavy fixtures as P0 → stretch after RA ops
- Alloy-on-Alloy dogfood → banned until sandbox + holdout green

**Skeleton timing:** Implement ScriptedProvider + one fixture as soon as RFC-0007 traits exist—do not wait for full CLI. Full vertical-slice gates need RFCs 0005–0015.

## Dependencies

- **RFC-0001** — shared metrics/IR types
- **RFC-0007** — `ModelProvider` trait

## Public API

From V2 §17.2:

```rust
pub struct EvalMetrics {
    pub success_rate: f64,
    pub compile_success_rate: f64,
    pub token_efficiency: f64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub cost_usd_p50: f64,
    pub retries_mean: f64,
    pub human_interventions: f64,
    pub unsafe_introduced_rate: f64,
}

// ScriptedProvider implements ModelProvider from RFC-0007
```

Fixture layout under `fixtures/` (or `alloy-eval/fixtures/`).

## Internal architecture

Eval driver can invoke runtime in-process or CLI. Scripted responses keyed by capability/node fingerprint. Cargo outputs replayed for VerifyCompile without network.

## Data structures

Fixture manifest: workspace snapshot, expected diagnostic codes, scripted model turns, success criteria (`compile_clean`, `no_new_unsafe`).

## State machine

N/A — batch runner. Each fixture is an independent run outcome enum `{ Pass, Fail, Error }`.

## Failure modes

| Failure | Handling |
| --- | --- |
| Eval overfitting (R15) | Holdout set never used for prompt tuning in-tree |
| Fixture license issues | Reject corpus; permitted only |
| Control plane loses to naive baseline on holdout | Stop—*control plane* failed (V2 falsification target) |
| Text-diff-only failure | Does **not** alone falsify graph/IR research priority |

## Testing strategy

- Self-test: ScriptedProvider returns scripted bytes
- Golden: at least one local-diagnostic fixture fails compile before repair script and passes after
- CI job runs `alloy-eval` offline (no real provider keys)

## Acceptance criteria

- [ ] Fixtures + ScriptedProvider exist from early milestone
- [ ] EvalMetrics reported (success, compile, cost, retries, …)
- [ ] Holdout local-diagnostic gate defined for M1
- [ ] No cost marketing claims emitted by harness
- [ ] Dogfood ban documented until sandbox+holdout green
- [ ] Offline CI runnable without `.env` secrets

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

**5–8 person-days** (skeleton ~2 pd; full gates with stack ~remainder).

## Future extensions

- Expand corpus; calibrate cost claims; gate each phase exit
- Lifetime fixtures after RA-assisted ops
- Public leaderboard only post-calibration (deferred)
