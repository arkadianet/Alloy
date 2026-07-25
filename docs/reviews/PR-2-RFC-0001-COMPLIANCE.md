# PR #2 — RFC-0001 + Architecture V2 Compliance Gate

| Field | Value |
| --- | --- |
| **PR** | [#2](https://github.com/arkadianet/Alloy/pull/2) — Implement RFC-0001 Alloy Runtime host |
| **Branch** | `cursor/rfc-0001-implement-3f81` |
| **Reviewed against** | Architecture V2 (frozen) · RFC-0001 |
| **Date** | 2026-07-25 |
| **Author** | arkadianet |
| **Overall** | **PASS** |
| **Fix rounds** | 1 (CodeRabbit still-valid findings) |

---

## Overall: PASS

PR #2 ships the RFC-0001 Runtime host within V2 MVP posture (single binary, exactly five crates, `NullScheduler`, stub Session/adapters, no sixth types crate, no `.env` writes). After one CodeRabbit fix round, acceptance criteria and relevant V2 Runtime checks hold; validation green (`cargo test --workspace`, `clippy -D warnings`, `fmt --check`).

---

## RFC-0001 acceptance criteria

| # | Criterion | Result | Evidence |
| --- | --- | --- | --- |
| 1 | Five crates; `cargo build --workspace`; exactly five members | **Pass** | `Cargo.toml` members; `workspace_has_five_members` (toml parse) |
| 2 | Core IDs, budgets, Diagnostic/Failure IR, Grant/PermissionToken, SessionEventType | **Pass** | `crates/alloy-runtime/src/types/*`, `events/mod.rs` |
| 3 | Named catalog IDs are string newtypes; instance IDs are UUID | **Pass** | `types/ids.rs` (`uuid_id!` / `name_id!`) |
| 4 | `AlloyRuntime` create → configure → start → run → drain → shutdown | **Pass** | `runtime/lifecycle.rs`; lifecycle integration tests |
| 5 | Scheduler/Session/RunController/EventSink/Verify*/GateHuman traits; `NullScheduler` + `InMemoryEventSink` default | **Pass** | `RuntimeInner::new` installs both |
| 6 | `run` → `SchedulerUnavailable`; concurrent → `SchedulerBusy` | **Pass** | `null_scheduler_maps_to_scheduler_unavailable`, `single_flight_busy` |
| 7 | Catalog ID / Digest serde rejects invalid values | **Pass** | `types/ids` unit tests |
| 8 | Per-session `EventSeq`; `set_event_sink` does not replace mid-emit | **Pass** | sink tests; `EventSinkBusy` path |
| 9 | `NodeExecContext` non-serde; `NodeExecRef` serde-safe | **Pass** | `adapters/mod.rs` |
| 10 | `shutdown` from `Created`; `run` does not emit `RunAccepted` | **Pass** | `shutdown_from_created`, `run_does_not_emit_run_accepted` |
| 11 | `alloy --help` / `--version`; SIGINT/SIGTERM → drain→shutdown | **Pass** | `cli_smoke`; early-armed signal → cancel → `graceful_shutdown` |
| 12 | `example.env`, `profiles/default.toml`, `router.toml.example`; `.env` never written | **Pass** | artifacts present; config/lifecycle dotenv tests |
| 13 | Module map mirrors V2 names; explicit crate-root re-exports | **Pass** | `lib.rs` modules + `pub use` list |
| 14 | CODEOWNERS (arkadianet) | **Pass** | `CODEOWNERS` |
| 15 | Serde IR round-trips; Timestamp RFC3339; Appendix A wire names | **Pass** | `core_ir_serde_round_trips`, dag serde test |
| 16 | Drop without shutdown does not panic | **Pass** | `drop_without_shutdown_does_not_panic` |
| 17 | No behavioral Session/Scheduler/MCP/Edit beyond stubs | **Pass** | traits + `NullScheduler` / Unavailable adapters only |
| 18 | Downstream can import without a sixth crate | **Pass** | types live in `alloy-runtime` |
| 19 | MSRV/edition/`async_trait` pinned | **Pass** | workspace `edition = "2021"`, `rust-version = "1.85"`, `async-trait` |

---

## Architecture V2 MVP posture (Runtime-relevant)

| Check | Result | Notes |
| --- | --- | --- |
| Single binary (`alloy`) | **Pass** | `alloy-cli` `[[bin]] name = "alloy"` |
| ≤5 crates (~3 months) | **Pass** | Exactly: cli, runtime, tools, index, eval |
| No week-1 18-crate sprawl / no `alloy-daemon` / ACP crate | **Pass** | Absent |
| Types inside `alloy-runtime` (no sixth types crate) | **Pass** | |
| `NullScheduler` / stub Scheduler surface | **Pass** | Default at construct |
| Session/RunController as traits (behavior later) | **Pass** | Stub signatures only |
| Runtime adapters Verify*/GateHuman stubs | **Pass** | Unavailable impls |
| Parallelism defaults = 1 on `BudgetPolicy` | **Pass** | Type defaults; profile skeleton defers parallel fields to RFC-0015 |
| Mental model Runtime → Scheduler → Workers | **Pass** | Host + stub scheduler only |
| Never write `.env` | **Pass** | Loader + tests |

**Intentional deltas (not Failures):**

- `SessionService::events` uses `after: Option<EventSeq>` + `limit` (exclusive cursor, `MAX_EVENTS_PAGE`) — RFC-0001 contract refinement for replay; V2 §5.5 sketch still shows the older two-arg form; behavior remains Session-owned (RFC-0003).
- `WorkerMetrics.confidence: Option<f32>` — unavailable confidence is explicit; V2/RFC sketches historically used bare `f32`; RFC-0001 sketch updated.
- `RuntimeHandle::config() -> Result<Arc<RuntimeConfig>, _>` — fallible pre-configure (review hardening).

---

## CodeRabbit findings

### Inline — Valid → fixed

| Finding | Status | Fix |
| --- | --- | --- |
| CLI loads `router.toml.example` as active router | **Fixed** | `ConfigPaths::for_workspace` defaults to `router.toml`; CLI uses it |
| SIGINT/SIGTERM only after `start` | **Fixed** | Signal arms cancel token before `start`; `start_inner` selects on cancel during `create_dir_all`; post-start waits on cancelled token then drain/shutdown |
| `RuntimeConfig::load` doc omits `ConfigPaths.data_dir` | **Fixed** | Doc lists ALLOY_DATA_DIR → `paths.data_dir` → workspace → XDG |
| `SessionService::events` requires cursor; unbounded page | **Fixed** | `after: Option<EventSeq>`, `limit`, `MAX_EVENTS_PAGE` / `clamp_events_page_limit`; exclusive-cursor docs |
| `confidence: f32` documented optional | **Fixed** | `Option<f32>` |
| `ALLOY_*` overrides documented but unused; example as active router | **Fixed** | `for_workspace` reads `ALLOY_PROFILE` / `ALLOY_ROUTER`; `ALLOY_DATA_DIR` via `resolve_data_dir`; active `router.toml` |
| Profile/router examples include unsupported fields | **Fixed** | Stripped `description` / parallel budget keys from profile; stripped `kind` / `base_url` / `tiers` from `router.toml.example` (no schema expansion) |

### Nitpicks

| Finding | Status | Reason |
| --- | --- | --- |
| `BudgetsSection` discarded — put budgets on `RuntimeConfig` | **Skipped** | RFC-0001 `RuntimeConfig` does not carry budgets; `BudgetPolicy` is session/create IR (RFC-0003). Profile budget parse remains for future RFC-0015 wiring; values intentionally unused today. |
| Bounded retention / drain on `InMemoryEventSink` | **Skipped** | RFC-0001 in-memory buffer until EventStore (RFC-0002); no retention contract in this RFC. |
| Hand-rolled workspace member scan | **Fixed** | `toml::from_str` of `[workspace].members` length == 5 |

---

## Validation (post-fix)

```text
cargo test --workspace          # PASS
cargo clippy --workspace --all-targets -- -D warnings  # PASS
cargo fmt --check               # PASS
```

---

## Open questions / blockers

None. No product decision blocked this gate.
