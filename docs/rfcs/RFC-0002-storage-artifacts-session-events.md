# RFC-0002: Storage, Artifacts & Session Event Log

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001 |
| Effort | 4–7 person-days |

## Purpose

Provide the MVP data plane: SQLite-backed session event log, artifact blob store (digests + metadata), and DAG/graph version references. Implements V2 principle 3.3 (“if it isn’t in the session event log or DAG store, it didn’t happen”).

## Scope

### In scope

- SQLite schema for sessions, runs, events (Appendix A), artifacts, optional DAG blob rows (consumed by RFC-0009)
- Append-only `SessionEvent` writer/reader (`seq`, `ts`, `type`, `payload`)
- Artifact Store: store blob by digest, fetch by `ArtifactId`, metadata only in default retention
- Paths under `.alloy/` (or XDG) for DB + artifacts; graph path reserved for RFC-0011
- Decision payload default = metadata + content hashes (bodies opt-in flag stored, not required)

### Out of scope

- `SessionService` / `RunController` orchestration → [RFC-0003](./RFC-0003-session-manager-run-controller.md)
- Rich observability exporters / TUI → [RFC-0004](./RFC-0004-observability-cost-metering.md) + deferred TUI
- ProjectGraph tables beyond foreign keys / `GraphVersion` column → [RFC-0011](./RFC-0011-project-graph.md)
- Postgres, OverlayFS snapshot bundles → deferred (V2 §0.7, §6.3)

## Dependencies

- **RFC-0001** — IDs, event type constants, digests

## Public API

```rust
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, ev: NewSessionEvent) -> Result<EventSeq, StoreError>;
    async fn list(&self, session: SessionId, after: EventSeq) -> Result<Vec<SessionEvent>, StoreError>;
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, bytes: &[u8], meta: ArtifactMeta) -> Result<ArtifactId, StoreError>;
    async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError>;
    async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError>;
}

pub struct SessionEvent {
    pub seq: EventSeq,
    pub ts: Timestamp,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub type_: SessionEventType, // Appendix A enum
    pub payload: serde_json::Value,
}
```

Event `type` enum matches V2 Appendix A exactly.

## Internal architecture

- Module in `alloy-runtime` (e.g. `storage::sqlite`)
- Single writer connection pattern; migrations versioned
- Artifacts on disk content-addressed; SQLite holds index

## Data structures

| Table / store | Key columns |
| --- | --- |
| `sessions` | `id`, `workspace_root`, `profile`, `budget_json`, `created_at` |
| `session_events` | `session_id`, `seq`, `ts`, `run_id`, `type`, `payload_json` |
| `artifacts` | `id`, `digest`, `kind`, `path`, `meta_json` |
| `runs` (thin) | `id`, `session_id`, `goal_json`, `state` |

## State machine

N/A for the store itself. Event append is monotonic (`seq` strictly increasing per session). Session/run lifecycle state belongs to RFC-0003.

## Failure modes

| Failure | Handling |
| --- | --- |
| Disk full / SQLite locked | Structured `StoreError`; fail closed on append |
| Corrupt payload JSON | Reject write; never silently drop seq gaps |
| Missing artifact blob | `NotFound`; callers treat as hard error |
| Migration mismatch | Refuse start until migrated |

## Testing strategy

- Unit: append/list ordering; concurrent append serialization
- Property: seq monotonicity
- Integration: temp-dir SQLite round-trip of Appendix A event types
- Hashing: put/get digest integrity

## Acceptance criteria

- [ ] Append-only event log with Appendix A types
- [ ] Artifact put/get by digest/`ArtifactId`
- [ ] Default payloads support metadata+hash fields (no mandatory full prompts)
- [ ] Storage roots documented; user `.env` never written
- [ ] Other RFCs can depend on traits without knowing SQLite details

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

**4–7 person-days**.

## Future extensions

- Postgres if multi-user daemon appears (V2 §21.2)
- OverlayFS / alloy snapshot bundles as checkpoint backend (V2 §6.3 deferred)
- OTLP export of stored events (RFC-0004 evolution)
