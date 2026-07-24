# RFC-0015: CLI, Profiles & Config

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0003, RFC-0004, RFC-0010, RFC-0013 |
| Effort | 4–6 person-days |

## Purpose

Ship the `alloy` binary user surface: goals, approvals, config. Owns user I/O only—not planning logic (V2 §5.2). Milestone path: `alloy run "fix E0502 in crate X"` with decision log under sandbox.

## Scope

### In scope

- `alloy-cli` binary: `run`, `events`, `approve`, `cancel`, `resume` (names may match clap design; semantics per Session/RunController)
- Load `profiles/default.toml` (Appendix B), `router.toml`, `example.env` documentation
- TTY approval prompts for GateHuman
- Profile flags: default | autonomous | readonly
- Never overwrite `.env`; document `example.env` only
- Budget warnings printed from observability snapshots

### Out of scope

- Optional TUI → deferred
- alloyd / ACP → deferred
- Eval CLI gates → [RFC-0016](./RFC-0016-eval-harness-holdout-gates.md) (`alloy-eval` binary or subcommand OK)
- Planning/DAG internals → RFCs 0009–0010

## Dependencies

- **RFC-0003** — session/run APIs
- **RFC-0004** — events display
- **RFC-0010** — running DAG
- **RFC-0013** — workers behind scheduler

## Public API

CLI surface (V2 §1.4 / §5.5 facade):

```text
alloy run "<goal>" [--profile default|autonomous|readonly] [--workspace PATH]
alloy events [--session ID] [--after SEQ]
alloy approve --run ID --gate ID --decision allow|deny
alloy cancel --run ID
alloy resume --session ID
```

Config files:

- `example.env` — `ALLOY_API_KEY=` etc.
- `router.toml` / `router.toml.example`
- `profiles/default.toml` — gates, sandbox, budgets, context weights, observability flags

## Internal architecture

Thin clap (or equivalent) front-end constructing `CreateSession` / calling traits. No business logic in CLI beyond I/O and config parse.

## Data structures

Parsed `ProfileConfig` matching Appendix B fields.

## State machine

N/A beyond driving Session/RunController states (see RFC-0003). CLI blocks on WaitingApproval when interactive.

## Failure modes

| Failure | Handling |
| --- | --- |
| Missing config / API key | Clear error; point to `example.env` (do not create `.env` silently with secrets) |
| Non-interactive + gate required | Exit nonzero with gate id |
| Sandbox backend missing | Fail before run (fail closed) |

## Testing strategy

- CLI parse unit tests
- Integration: scripted run against fixture with ScriptedProvider (eval)
- Snapshot help text
- Ensure no `.env` write in tests

## Acceptance criteria

- [ ] Engineer can run goal → compile-gated path under sandbox (with deps)
- [ ] Approvals and events inspectable from CLI
- [ ] Profiles match Appendix B defaults
- [ ] `.env` never replaced; `example.env` documented
- [ ] CLI contains no planner/scheduler business logic

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

**4–6 person-days**.

## Future extensions

- Optional TUI reading same event log
- `alloyd` / ACP only if measured need (V2 §5.3, §21.2)
