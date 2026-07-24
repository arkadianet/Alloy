# RFC-0005: Sandbox Broker

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001 |
| Effort | 5–8 person-days |

## Purpose

Enforce sandbox-before-dogfood (V2 §14.2 / ADR F-07): every Exec grant runs under Landlock/Seatbelt **or** container; quarantine network/deps by default. Models and tools are untrusted for FS/exec outside the broker.

## Scope

### In scope

- `SandboxBroker` trait + MVP native profile (Landlock on Linux; Seatbelt on macOS) **or** documented container fallback
- Profiles: `check`, `test`, `network=deny`, `quarantine_deps=true` (Appendix B)
- Workspace jail; deny `.env`, `*.pem`, ssh keys—**never replace user’s `.env`**
- Exec allowlist integration points for MCP (cargo/test only by default)
- Document residual risk: `cargo check` still runs `build.rs` / proc-macros inside sandbox

### Out of scope

- MCP host tool mediation → [RFC-0006](./RFC-0006-mcp-host-builtins.md)
- Community MCP allowlists fleet → deferred
- gVisor / multi-tenant hardening → deferred (V2 §14.2)
- Alloy-on-Alloy dogfood → blocked until M1 holdout green ([RFC-0016](./RFC-0016-eval-harness-holdout-gates.md))

## Dependencies

- **RFC-0001** — `Grant`, paths, profile IDs

## Public API

```rust
#[async_trait]
pub trait SandboxBroker: Send + Sync {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError>;
    fn profile(&self) -> &SandboxProfile;
}

pub struct SandboxProfile {
    pub check_backend: SandboxBackend, // Landlock | Seatbelt | Container
    pub test_backend: SandboxBackend,
    pub network: NetworkPolicy, // Deny default
    pub quarantine_deps: bool,
    pub fs_jail: PathBuf,
    pub deny_globs: Vec<Glob>, // .env, *.pem, …
}

pub struct SandboxExecRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env_allow: Vec<String>, // never pass secrets not in allow
    pub grant: Grant, // Exec(...)
    pub run_id: RunId,
}
```

Lives in `alloy-tools`.

## Internal architecture

Broker selected by profile TOML. Fail closed if backend unavailable on host (clear error; do not silently run bare exec).

## Data structures

Sandbox result: exit code, stdout/stderr caps, denial reason enum (`PathDenied`, `NetworkDenied`, `ExecNotAllowlisted`).

## State machine

N/A — request/response isolation. Security policy is immutable by repo text (prompt-injection principle, V2 §14.5).

## Failure modes

| Failure | Handling (V2 §5.6 / §14) |
| --- | --- |
| Sandbox denial | Escalate approval or fail task |
| Backend unsupported on OS | Error with install/container guidance; no bare fallback in default profile |
| Path traversal / `.env` read | Deny; log decision metadata |
| build.rs RCE residual | Documented; quarantine_deps mitigates network exfil |

## Testing strategy

- Unit: deny-glob matching for `.env` / keys
- Integration: landlock/seatbelt/container smoke on CI-capable runner
- Negative: attempted read outside jail fails
- Never require live user `.env` in fixtures

## Acceptance criteria

- [ ] All Exec paths go through `SandboxBroker` (no bare `std::process` in builtins)
- [ ] Default network deny + quarantine_deps
- [ ] `.env` / key material denied; `example.env` only for docs
- [ ] Milestone-1 gate prerequisite documented for dogfood ban
- [ ] Residual build-script risk documented

## Estimated implementation effort

**5–8 person-days** (platform variance dominates).

## Future extensions

- gVisor / harder isolation; community MCP only after allowlists (V2 §14)
- Tighten profiles without changing MCP call path
