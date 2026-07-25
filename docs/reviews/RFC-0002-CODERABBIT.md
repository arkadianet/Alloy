# RFC-0002 CodeRabbit verification

| Field | Value |
| --- | --- |
| **PR** | Draft #3 (`docs/rfc-0002-storage`) |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **Scope** | Docs-only verification of CodeRabbit findings against branch content |
| **Architecture review** | **APPROVE** retained (normative tightenings only; no V2 redesign) |

Findings verified against `docs/rfcs/RFC-0002-storage-artifacts-session-events.md` and `docs/reviews/RFC-0002-ARCHITECTURE-REVIEW.md` on branch `docs/rfc-0002-storage`.

---

## Fixed

| Finding | Location | Fix |
| --- | --- | --- |
| Machine-specific Canvas path | `RFC-0002-ARCHITECTURE-REVIEW.md` L10 | Removed `Canvas` field (no portable checked-in target) |
| Overview heading hierarchy | RFC L18 (`###` → skip `##`) | Promoted numbered sections `### N.` → `## N.` and subsections `####` → `###` (matches RFC-0001) |
| `SqliteEventSink` vs public name | RFC ownership table | Renamed to `SqliteEventStore` |
| Missing typed synchronous on open options | RFC §3.2 | Added `SqliteSynchronous` + `StorageOpenOptions.synchronous` (default `Normal`); wired through open/checkpoint prose + env build note |
| Handoff verify without durable rollback | RFC §3.7 | Import+verify in one transaction (or cleanup before memory restore); failure table + AC updated |
| `replay_session` vs cancellation requirement | RFC §9 / failure table | Removed `RuntimeHandle::cancellation` requirement from `EventStore`; pinned signature unchanged; cancel = drop/abort future |
| `metrics()` + snapshot type missing | RFC §3.3 / §13 / re-exports | Added `AlloyStorage::metrics()`, `StorageMetricsSnapshot`, crate-root re-exports |
| `close(self)` vs `Arc<AlloyStorage>` | RFC §3.3 | Changed to `close(&self)` idempotent barrier (`Closed` after first success) |
| `set_event_sink` vs non-empty handoff | RFC stubs § / §3.7 table | Clarified installer uses `handoff_event_sink`; bare `set_event_sink` empty/durable-only |
| Artifact put missing parent-dir fsync | RFC §6 / §7 write path | Normative: fsync CAS parent dir after rename; durability + mermaid updated |

---

## Skipped

None — every listed finding was still valid against branch content.

---

— arkadianet
