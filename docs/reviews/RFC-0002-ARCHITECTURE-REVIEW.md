# RFC-0002 Architecture Review — Storage, Artifacts & Session Event Log

| Field | Value |
| --- | --- |
| **RFC** | [RFC-0002](../rfcs/RFC-0002-storage-artifacts-session-events.md) |
| **Binding** | Architecture V2 (**frozen**) · RFC-0001 APIs + merged Runtime (**authoritative**) |
| **Reviewer** | Principal Rust Systems / durable storage review gate |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **Rounds** | 2 |
| **Final Verdict** | **APPROVE** |

---

## Round history

| Round | Verdict | Summary |
| --- | --- | --- |
| 1 | **NEEDS REVISION** | Handoff left as Prefer/or + open question; `replay_session` signature conflict; no exact-seq import path; `get_by_digest` multi-id undefined; Conflict→Io mapping wrong; AC missing durability/replay/shutdown binaries; failure modes incomplete |
| 2 | **APPROVE** | Normative single-path `handoff_event_sink` + `import_handoff_snapshot` + restore-on-fail; signatures pinned; failure table + AC tightened; OQ1 closed. No architectural ambiguity blocking implementers |

---

## 1. Required Changes (mandatory only)

*Round 1 findings — all addressed in Round 2 RFC edits. Listed for audit trail.*

| # | Location | Issue | Why violates RFC-0001 / V2 | Required correction | Severity |
| --- | --- | --- | --- | --- | --- |
| R1 | §3.7, §17 OQ1 | Handoff was Prefer / or / and/or; bare drain-then-`set_event_sink` races with emit and contradicts main’s non-empty refusal | RFC-0001 requires **atomic lossless** handoff before Arc swap is visible; day-1 refusal is not a substitute for a defined seam | Pin single path: `drain_for_handoff` + `import_handoff_snapshot` + `RuntimeHandle::handoff_event_sink` under write lock; keep `set_event_sink` unchanged for empty/durable swaps; close OQ1 | **Critical** |
| R2 | §3.4 vs §5 Replay | `replay_session` returned `EventSeq` in API sketch and `Option<EventSeq>` in prose | Implementers cannot compile one contract; empty-session semantics were ambiguous | Pin `Result<Option<EventSeq>, StoreError>`; empty → `None`, zero callbacks; callback `Err` aborts | **Critical** |
| R3 | §3.7 / EventSink | Lossless handoff required preserving seq/ts, but only `append_session` (reallocates seq, stamps `now`) was specified | RFC-0001 lossless handoff forbids silent renumbering | Add `EventStore::import_handoff_snapshot` (exact seq/ts, one tx); forbid `Timestamp::now` / new seq alloc on import | **Critical** |
| R4 | §3.5 Integrity | Same-digest ArtifactId policy said “or return existing” then MVP decision without `get_by_digest` determinism | Ambiguity → divergent impls / flaky callers | Pin: always new `ArtifactId` row; `get_by_digest` → oldest non-deleted (`created_at ASC, id ASC`) | **Important** |
| R5 | §3.1 `From<StoreError>` | Conflict/Corrupt/Migration mapped to `EventSinkError::Io` while Append text said Conflict → Internal | Mis-typed failures break Busy/Io retry policy vs integrity bugs | Map Conflict/Corrupt/Migration/NotFound/Closed → `Internal`; Busy→Busy; Io→Io | **Important** |
| R6 | §16 AC | Missing binary AC for crash durability, deterministic reopen replay, ordered shutdown, failed-handoff restore | Gate would merge without proving V2 explicit-state / 0001 handoff | Add AC bullets (done in Round 2) | **Important** |
| R7 | §6 Failure | Scattered handling; no Detection/Recovery/Outcome matrix for append / artifact / DB / resource / replay / handoff | Review dimension incomplete; engineers invent recovery | Add normative failure-mode table (done) | **Important** |

**Round 2 status:** All required changes applied in `docs/rfcs/RFC-0002-storage-artifacts-session-events.md`. No remaining mandatory blockers.

---

## 2. Recommended Changes (non-blocking)

| # | Location | Improvement | Reason |
| --- | --- | --- | --- |
| N1 | §12 `ALLOY_SQLITE_SYNCHRONOUS=OFF` | Document that `OFF` voids durability AC; tests should run at default `NORMAL` | Addressed in Round-2+ RFC durability note + `SqliteSynchronous` on `StorageOpenOptions` |
| N2 | §7 Artifact put | Optional soft cap / document “MVP loads full `Vec<u8>` in memory” | Sets expectation; OOM remains fail-closed via OS |
| N3 | §3.6 `RunRow.state` | Keep opaque; add one-line cross-link that 0003 must not persist free-form forever without enum | Already in OQ; clarity only |
| N4 | Metrics §13 | Keep counters; avoid expanding to histograms in this RFC | Prefer removing complexity later if unused |

---

## 3. Approved Sections

| Section | Assessment |
| --- | --- |
| §1–2 Scope / V2 / 0001 split | Correct: data plane only; no SessionService/Scheduler/MCP/DAG/Postgres/OverlayFS; ≤5 crates; storage inside `alloy-runtime` |
| §3.1–3.3 StoreError, layout, `AlloyStorage` lifecycle façade | Fit for purpose; clear open/migrate/checkpoint/close |
| §3.4 EventStore + EventSink | Matches 0001 exclusive cursor / `MAX_EVENTS_PAGE` / per-session gapless seq (after Round 2 pin) |
| §3.5 ArtifactStore | CAS + index; secrets not stored by defaults; digest via 0001 `Digest` |
| §3.6 Thin SessionRows | Correctly deferred orchestration to 0003 |
| §3.7 Handoff (Round 2) | Normative atomic seam; EventSink trait unchanged; dual-write forbidden |
| §4 Module map | Acyclic crates; install→handle dashed edge documented; rusqlite bundled |
| §5 Event log | Appendix A types; ordering; exclusive cursor; recovery seq cross-check |
| §6 Lifecycle | WAL checkpoint ≠ git checkpoint (V2); migrations additive; reserved dag/graph only |
| §7–10 Artifacts, concurrency, async, shutdown | Single-process; `spawn_blocking`; ordered shutdown when installed |
| §11–13 Errors, config/`example.env`, observability | Never write `.env`; cite `example.env`; no OTLP creep |
| §14–16 Tests, MVP boundary, AC | Binary and testable after Round 2 |
| Schema v1 | Compatible with 0001 `Session` fields + reserved columns |

---

## 4. Final Verdict

**APPROVE**

Implementation may begin. Engineers can implement without guessing on handoff, replay, import, digests, or durability acceptance.

---

## Review dimensions (Round 2)

### 1. API Quality (vs RFC-0001)

| Check | Result |
| --- | --- |
| `EventSink` / `SessionEvent` / `NewSessionEvent` unchanged | Pass |
| Exclusive cursor + `clamp_events_page_limit` / `MAX_EVENTS_PAGE` | Pass |
| Ownership: store assigns seq; host emit/append unchanged | Pass |
| Async `async_trait` + `spawn_blocking` for rusqlite | Pass |
| No leakage of SQLite types into public EventSink | Pass |
| Additive `handoff_event_sink` (does not break `set_event_sink`) | Pass |

### 2. Responsibility Boundaries

| Component | Owns | Does not own |
| --- | --- | --- |
| Event Log | Append/list/replay/seq | Session orchestration (0003) |
| Artifact Store | CAS + meta index | Prompt retention policy enforcement (0004 writers) |
| Migration / Checkpoint | Schema version, WAL flush | Git EditEngine checkpoints |
| Recovery | Reopen, seq rebuild, orphan warn | Run resume UX (0003) |
| Runtime / Scheduler / MCP | Unchanged except additive handoff helper | Not expanded |

### 3. Module and Crate Boundaries

- Still **≤5** workspace crates; no `alloy-storage` crate.
- Modules under `alloy-runtime::storage/*`; traits for backends.
- Install→`RuntimeHandle` edge explicit and narrow.

### 4. Failure Mode Completeness

Normative Detection / Recovery / Outcome table covers **append, artifact, DB, resource, replay, handoff**. Pass.

### 5. Acceptance Criteria Quality

Binary checks for durability, deterministic replay, artifact round-trip, cursor, shutdown, handoff lossless + fail-restore, `.env` never written. Pass.

### 6. Overengineering

Reserved `dag_blobs` / `graph/` are additive schema/path only (allowed for 0009/0011). No alt engines, no Postgres, no GC product, no multi-process SQLite. Acceptable; prefer not growing further.

### 7. Underengineering

Round 1 gaps closed. Remaining spikes (conn pool, `RunRow.state`) correctly deferred. Soft max blob size remains recommended-only.

### 8. Testability

Unit / integration / recovery / migration / concurrency / handoff / config sentinel / contract `Arc<dyn EventSink>` — sufficient for merge gate.

### 9. Future Maintainability

Single EventSink slot, additive migrations, thin SessionRows, explicit MVP/deferred table — low thrash risk if 0003/0009 stay consumers-only.

---

## RFC-0002 Specific Checklist

| Item | Result |
| --- | --- |
| Event Log (per-session gapless, exclusive cursor, Appendix A types) | Pass |
| Persistence lifecycle transitions (open→migrate→ready→checkpoint→close→reopen) | Pass |
| Artifact Store (CAS + digest + soft-delete MVP) | Pass |
| Config / `example.env` (keys documented; never write `.env`) | Pass |
| MVP boundary (no 0003/0004/0009/0011 behavior; no sixth crate) | Pass |

---

## Binding compliance

| Binding | Result |
| --- | --- |
| V2 frozen (no redesign, no alt storage architecture) | Pass |
| RFC-0001 APIs authoritative | Pass |
| No code / no V2 / no RFC-0001 doc edits in this review loop | Pass (RFC-0002 + `example.env` docs only) |
| Prefer removing complexity; ambiguity → tighten RFC | Pass |

---

— arkadianet
