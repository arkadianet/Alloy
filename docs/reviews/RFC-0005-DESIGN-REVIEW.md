# RFC-0005 Design Review — Sandbox Broker

| Field | Value |
| --- | --- |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **RFC** | [RFC-0005](../rfcs/RFC-0005-sandbox-broker.md) (Ready for Implementation) |
| **Status** | Approved for implementation |

Authority order: `main` source → RFC-0005 → RFC-0001 → Architecture V2.

---

## 1. RFC Summary

RFC-0005 ships the MVP **Sandbox Broker** inside `alloy-tools` so every `Grant::Exec` runs under a real isolation backend — Landlock (+ user/mount/net namespaces) on Linux, Seatbelt on macOS, or a container runtime — with fail-closed construction, default `network=deny`, `quarantine_deps=true`, workspace jail, and deny-globs for credential paths (`.env`, keys, SSH/AWS material).

The broker is a request/response choke point: validate `PermissionToken` expiry and `ExecAllow`, authorize cwd via `PathPolicy`, scrub child env, optionally rewrite cargo argv for offline quarantine, isolate via the selected backend, then supervise the child (timeout, process-group kill, stdio caps). Non-zero child exits are `Ok`; policy denials and backend failures are `Err`. Policy is immutable by repo/prompt text. Residual risk (`build.rs` / proc-macros still execute inside the jail) is documented; Alloy-on-Alloy dogfood stays banned until sandbox + holdout gates are green.

---

## 2. Existing Architecture

| Item | Location / role |
| --- | --- |
| Workspace crates | `alloy-cli`, `alloy-runtime`, `alloy-tools` (stub), `alloy-index`, `alloy-eval` — five crates max |
| Shared types | `alloy-runtime`: `Grant`, `ExecAllow`, `Glob`, `HostAllow`, `PermissionToken`, `RunId`, `ProfileId`, `Timestamp`, `Digest` |
| Config | `RuntimeConfig.profile_path`; `profiles/default.toml` already has `[sandbox]` |
| Env hygiene | `example.env` only; never write `.env` |
| Consumers (later) | RFC-0006 MCP builtins call `SandboxBroker`; RFC-0008 may reuse `PathPolicy` |
| Dependency edge | `alloy-tools → alloy-runtime` only; runtime must not depend on tools |

Reusable: permission types, `Digest::sha256`, `Timestamp::now()`, tracing, thiserror, tokio, toml/serde.

---

## 3. Implementation Plan

1. **Types / errors / trait** — `SandboxBackend`, `ExecClass`, `NetworkPolicy`, request/result, `SandboxError` / `DenialReason`, `SandboxBroker`, capabilities.
2. **Profile load** — parse `[sandbox]` from profile TOML; reject missing section and `network=allow`; `default_for_jail`.
3. **PathPolicy + globs** — compile deny-globs; canonicalize + jail membership; symlink escape deny; RO-root write reject.
4. **Grant + env** — `exec_allow_matches`, trusted-root PATH resolve, `validate_env_allow_name`, scrub + hard/substring deny, quarantine rewrite.
5. **Process** — sole `Command::new` seam; setsid; concurrent stdio drain with caps; SIGTERM→2s→SIGKILL; drop-guard kill.
6. **Backends** — probe at `new`; Linux Landlock+userns+netns+deny binds; macOS sandbox-exec+SBPL; container docker/podman.
7. **Broker** — `NativeSandboxBroker` pipeline; `RecordingSandboxBroker` FIFO double.
8. **Tests / CI / docs** — unit suite from §11; Linux Landlock integration required; residual-risk doc; `sandbox.yml`; clippy `disallowed_methods`.

**Data flow:** MCP/caller → `exec(req)` → expiry/grant → cwd policy → env/quarantine → backend isolate → supervise → `Ok(result)` / `Err`.

**Error flow:** denials → `Denied(*)`; probe/runtime missing → `BackendUnavailable`; FS-only Landlock under Deny → `BackendCannotEnforce`; never bare exec.

---

## 4. Module Layout

```text
crates/alloy-tools/src/
  lib.rs                 # crate root re-exports
  sandbox/
    mod.rs               # module docs + re-exports
    types.rs             # enums, request/result, errors, trait, capabilities
    profile.rs           # TOML DTO + SandboxProfile + load
    glob.rs              # deny-glob compile/match helpers
    path.rs              # PathPolicy / PathAccess
    grant.rs             # ExecAllow matching + binary resolution
    env.rs               # scrub, hard deny, validate_env_allow_name, quarantine
    process.rs           # spawn + supervise (Command seam)
    policy_digest.rs     # Digest over portable policy JSON
    broker.rs            # NativeSandboxBroker
    recording.rs         # RecordingSandboxBroker
    backend/
      mod.rs             # Backend trait + dispatch
      probe.rs           # capability probes
      linux.rs           # Landlock + userns + netns (unsafe allow)
      macos.rs           # Seatbelt trampoline (unsafe allow)
      macos/alloy-check.sb.template
      container.rs       # docker/podman
```

Plus: `docs/security/sandbox-residual-risk.md`, `.github/workflows/sandbox.yml`, `clippy.toml`, `example.env` comments only.

---

## 5. Risk Assessment

| Risk | Mitigation |
| --- | --- |
| Landlock / userns unavailable | Probe at `new`; fail closed for `check`; clear `BackendUnavailable` |
| macOS Seatbelt differences / deprecation | Generated SBPL + ready-byte; residual-risk docs deprecation |
| Container vs native argv/env | Separate resolution + env composition tables per RFC §5.3 / §5.5 |
| CI without docker / blocked userns | Landlock job required when available; container job optional; open Q1 CI overlay if needed |
| Path traversal / symlink escape | Canonicalize; deny out-of-jail symlink targets |
| Env leakage / `.env` | Never parse `.env`; hard + substring deny; deny-glob bind-overs |
| TOCTOU | Canonicalize before auth; deny binds in same mount ns as Landlock |
| Silent bare exec | Clippy ban on `Command::new` outside process/backend; no fallback path |
| Credential theft via cargo home | RO allowlisted subtrees only; `/dev/null` over credentials; per-exec HOME |
| Mount bind leak to host | `MS_REC\|MS_PRIVATE` before binds; sentinel unchanged tests |
| Concurrent exec collisions | Unique per-exec `exec_dir` |

---

## 6. Compliance Checklist

- [x] Lives in `alloy-tools` only (not a sixth crate / not obs / not MCP host)
- [x] Reuses RFC-0001 permission types; no Grant redesign
- [x] No RFC-0006/0008/0010/0016 behavior beyond stubs (`RecordingSandboxBroker`, `PathPolicy` export, `Cancelled` reserved)
- [x] ADR F-07: fail closed, network deny, quarantine, jail, deny credentials, never write `.env`
- [x] Residual build.rs / proc-macro risk documented
- [x] Architecture V2 crate map and Appendix B defaults respected

---

*— arkadianet*
