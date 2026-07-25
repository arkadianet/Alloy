# RFC-0005: Sandbox Broker

| Field | Value |
| --- | --- |
| **Status** | Ready for Implementation |
| **Author** | arkadianet |
| **Architecture** | Alloy Architecture V2 (**frozen**) — do not redesign |
| **Depends on** | [RFC-0001](./RFC-0001-alloy-runtime.md) (merged) |
| **Effort** | 5–8 person-days |
| **Related RFCs** | [0006](./RFC-0006-mcp-host-builtins.md) MCP host (every Exec → broker) · [0008](./RFC-0008-edit-engine.md) sandboxed git/fs where applicable · [0015](./RFC-0015-cli-profiles-config.md) full profile UX · [0016](./RFC-0016-eval-harness-holdout-gates.md) dogfood ban until sandbox+holdout green |
| **Product** | Alloy — AI Engineering Runtime |
| **Supersedes** | Draft outline of this filename (expanded to implementation grade) |

**Mental model (V2 §14 / ADR F-07):** Models and tools are untrusted for filesystem and exec outside the broker. Every `Grant::Exec` runs under a real isolation backend (Landlock on Linux, Seatbelt on macOS, **or** container). Default network deny + dependency quarantine. Fail closed if the selected backend is unavailable — **never** silently bare-exec. Policy is immutable by repo text (V2 §14.5).

**Authority order (highest → lowest):** current `main` source → RFC-0001 → Architecture V2. Never modify an existing public API solely to match an older V2 sketch or this document’s draft outline.

---

## 1. Overview

### Purpose

Ship the MVP **Sandbox Broker** in `alloy-tools`:

1. **`SandboxBroker` trait** + concrete `NativeSandboxBroker` that executes only under an isolation backend.
2. **Profiles** from Appendix B: per-class backends (`check` / `test`), `network = deny`, `quarantine_deps = true`, workspace jail, deny globs for credential paths.
3. **`PathPolicy`** shared with RFC-0006 (`fs_read`) so `.env` / key denial is not re-implemented.
4. **Process lifecycle** — timeout, process-group kill, stdout/stderr caps, env scrubbing, argv allowlist validation against `Grant::Exec` / `ExecAllow` as published on `main`.
5. **Fail-closed construction** — probe backends (including netns / container) at broker build time; refuse operation without a working configured backend.
6. **Residual risk documentation** — `cargo check` still runs `build.rs` / proc-macros inside the jail (V2 R8).

### Problem Statement

RFC-0001 published `Grant`, `ExecAllow`, `Glob`, `HostAllow`, `PermissionToken`. RFC-0006 requires every Exec grant to go through a sandbox. `alloy-tools` is an empty stub (`#![forbid(unsafe_code)]`, no dependencies). Without this RFC there is no choke point preventing bare `std::process` in builtins, no jail, and no enforceable network/credential policy — violating ADR F-07.

### Scope

| In scope | Detail |
| --- | --- |
| `SandboxBroker` trait + `NativeSandboxBroker` | Single async `exec` entry point |
| `ExecClass::{Check, Test}` | Backend selection — **not** argv sniffing |
| Backends MVP | Linux Landlock+user+netns; macOS Seatbelt; Container (docker/podman) |
| `SandboxProfile` load from profile TOML `[sandbox]` | Parsed in `alloy-tools` from `RuntimeConfig.profile_path` |
| `PermissionToken` authorization | Reuse RFC-0001 types exactly; no Grant redesign |
| `PathPolicy` | Deny-globs / jail checks for exec cwd and for 0006/0008 |
| Env scrubbing + hard deny list | Deny-by-default child env |
| Process supervision | Timeout, process group, caps, drop-guard kill |
| `RecordingSandboxBroker` | Full test-double API for 0006/0008/0013 |
| Tests + residual-risk doc + CI workflow | Unit + platform integration + negative suite |
| `profiles/default.toml` `[sandbox]` | In-scope deliverable |
| Clippy `disallowed_methods` | No bare `Command::new` outside sandbox modules |

### Non-goals

- MCP host / tool mediation / `PermissionToken` issuance → **RFC-0006**.
- EditEngine apply / git checkpoint orchestration → **RFC-0008**.
- Full profile UX / CLI doctor → **RFC-0015**.
- Community MCP allowlists, gVisor, multi-tenant hardening → **V2 deferred**.
- Alloy-on-Alloy dogfood → blocked until sandbox + holdout green (**RFC-0016** / ADR F-07).
- Memory/CPU rlimits beyond wall-clock timeout and stdio caps → deferred.
- Redesigning V2 or RFC-0001 permission types; new workspace crates; OTLP.
- Writing or overwriting `.env`.

### Day-1 MVP (normative)

1. Configured **check** backend MUST probe Available at `NativeSandboxBroker::new` or construction fails closed. Unavailable **test** backend defers failure to `exec(Test)`.
2. `network = deny` enforced by user+netns (Landlock path), Seatbelt, or `--network none` (container) — never via Landlock FS rules alone.
3. Default deny globs block `.env` and key material; child env never loads `.env`.
4. Non-zero child exit → `Ok(SandboxExecResult)`; signal death encoded per §3.4; policy denials → `Err`.
5. No bare `std::process::Command` outside sandbox modules in `alloy-tools`.

---

## 2. Architecture Integration

### Relationship to Architecture V2

| V2 reference | Application |
| --- | --- |
| §14.1 Threat model | Prompt injection, malicious tools, build.rs RCE, credential theft, path traversal |
| §14.2 / ADR F-07 | Sandbox before dogfood; Landlock/Seatbelt **or** container on all exec; quarantine default |
| §14.3 Filesystem isolation | Workspace jail; deny `.env` / `*.pem` / ssh keys; never replace `.env` |
| §14.5 Prompt injection | Tool policy immutable by repo text |
| §14.6 Approvals | New dependency requires gate — workspace dep additions listed in §10 |
| §5.4 / crate map | Sandbox lives in `alloy-tools` |
| §5.6 Sandbox denial | Broker returns typed denial; escalation is caller’s |
| Appendix B `[sandbox]` | Normative default profile keys |
| Appendix E | `PermissionToken` / `Grant` — **main wins** |

**V2 sketch superseded by `main`:** sketches using `Read`/`Write`/`McpInvoke`, `ExecAllow { argv0, args_glob: Glob }`, or `HostAllow { hosts: Vec }`. **Normative:** `crates/alloy-runtime/src/types/permission.rs`.

### Relationship to RFC-0001

Authoritative for: `Grant`, `ExecAllow`, `Glob`, `HostAllow`, `PermissionToken`, `RunId`, `ProfileId`, `Timestamp`, `Digest`, `RuntimeConfig.profile_path`.

This RFC consumes those types. It MUST NOT redefine them. `Timestamp` has no `Ord` on main — expiry compares via `perms.expires.as_ref().map(|t| t.0)` against `Timestamp::now().0` (`OffsetDateTime`).

### Already implemented | Added by RFC-0005 | Deferred

| Item | Owner |
| --- | --- |
| Permission types + `RuntimeConfig.profile_path` | **0001** |
| `alloy-tools` stub crate | **0001** workspace |
| Full sandbox broker + backends + path policy | **0005** |
| MCP builtins calling broker | **0006** |
| EditEngine sandboxed git | **0008** |
| Full profile UX | **0015** |
| gVisor / multi-tenant / community MCP | **V2 deferred** |

### Dependency boundaries

```text
alloy-cli ──► alloy-tools ──► alloy-runtime (types + Digest/Timestamp only)
                 └── sandbox (0005); mcp (0006 later)

alloy-runtime MUST NOT depend on alloy-tools.
```

Exactly five workspace crates. No sandbox OS service.

---

## 3. Public Rust API

All new items in `alloy-tools`. Permission types imported from `alloy-runtime`.

### 3.1 Crate root

```rust
//! Alloy tooling: sandbox broker (RFC-0005) and MCP host (RFC-0006).
#![deny(missing_docs)]
// This RFC replaces the stub's `#![forbid(unsafe_code)]` with `deny`, so
// `backend/linux.rs` and `backend/macos.rs` may `#![allow(unsafe_code)]`.
#![deny(unsafe_code)]

pub mod sandbox; // submodule visibility: `pub use` below; other sandbox::* remain pub(crate) unless listed

pub use sandbox::{
    default_deny_globs, load_sandbox_profile,
    BackendStatus, DenialReason, ExecClass, NativeSandboxBroker, NetworkPolicy,
    PathAccess, PathPolicy, RecordingSandboxBroker, SandboxBackend, SandboxBroker,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult, SandboxProfile,
};
```

### 3.2 Existing permission types (normative — do not change)

```rust
// alloy-runtime::types::permission — AUTHORITATIVE

pub struct Glob(pub String);
pub struct ExecAllow { pub binary: String, pub args_glob: Option<String> }
pub struct HostAllow { pub host: String }
pub enum Grant { FsRead(Glob), FsWrite(Glob), Exec(ExecAllow), Network(HostAllow), GitWrite }
pub struct PermissionToken {
    pub profile: ProfileId,
    pub grants: Vec<Grant>,
    pub expires: Option<Timestamp>,
    pub run_id: RunId,
}
```

### 3.3 Enums, TOML DTO, profile

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecClass { Check, Test }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend { Landlock, Seatbelt, Container }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy { Deny, Allow }

/// Wire DTO for `[sandbox]` — NOT the runtime profile.
#[derive(Debug, Clone, Deserialize)]
struct SandboxConfigToml {
    check: String,                         // required
    test: String,                          // required
    #[serde(default = "default_network")]
    network: String,                       // "deny" | "allow"
    #[serde(default = "default_true")]
    quarantine_deps: bool,
    #[serde(default = "default_timeout")]
    exec_timeout_secs: u64,                // default 1800
    #[serde(default = "default_cap")]
    stdout_cap: usize,                     // default 2_097_152
    #[serde(default = "default_cap")]
    stderr_cap: usize,
    #[serde(default)]
    container_image: Option<String>,       // else env / default image
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    pub check_backend: SandboxBackend,
    pub test_backend: SandboxBackend,
    pub network: NetworkPolicy,
    pub quarantine_deps: bool,
    pub fs_jail: PathBuf,           // absolute canonical
    pub deny_globs: Vec<Glob>,
    pub exec_timeout: Duration,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
    /// Container image when any class uses Container (see §5.5).
    pub container_image: String,
}

impl SandboxProfile {
    pub fn default_for_jail(fs_jail: PathBuf) -> Result<Self, SandboxError>;
    #[must_use]
    pub fn backend_for(&self, class: ExecClass) -> SandboxBackend { /* … */ }
}
```

**MVP load rule:** `network = "allow"` → `SandboxError::Invalid` **unconditionally** (host allowlisting deferred). Missing `[sandbox]` table → `Invalid("missing [sandbox] section")` (no silent default in production load). Unknown keys under `[sandbox]`: **ignored** (serde default). Required keys: `check`, `test`.

### 3.4 Request / result / error

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SandboxExecRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Extra env **names** permitted beyond the broker base set (§6).
    /// Values always come from the parent env of those names — no value injection API in MVP
    /// (RFC-0006 may extend additively).
    pub env_allow: Vec<String>,
    /// Authoritative token (includes `run_id`).
    pub perms: PermissionToken,
    pub class: ExecClass,
}

impl SandboxExecRequest {
    pub fn new(
        argv: Vec<String>,
        cwd: PathBuf,
        perms: PermissionToken,
        class: ExecClass,
    ) -> Self {
        Self { argv, cwd, env_allow: Vec::new(), perms, class }
    }
    #[must_use]
    pub fn with_env_allow(mut self, names: Vec<String>) -> Self {
        self.env_allow = names;
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SandboxExecResult {
    /// `Some(code)` if exited; `None` if killed by signal (see `signal`).
    pub exit_code: Option<i32>,
    /// `Some(signo)` if terminated by signal; `None` otherwise.
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    pub backend: SandboxBackend,
    /// Digest over canonical policy JSON (§9) — globs sorted; no absolute `fs_jail`.
    pub policy_digest: Digest,
}

impl SandboxExecResult {
    /// Construct for tests / `RecordingSandboxBroker` scripts (outside crate needs this
    /// because the struct is `#[non_exhaustive]`).
    pub fn synthetic(
        exit_code: Option<i32>,
        signal: Option<i32>,
        backend: SandboxBackend,
        policy_digest: Digest,
    ) -> Self {
        Self {
            exit_code,
            signal,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 0,
            backend,
            policy_digest,
        }
    }
    #[must_use]
    pub fn with_stdio(mut self, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        self.stdout = stdout;
        self.stderr = stderr;
        self
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    #[error("backend unavailable: {backend:?}: {message}")]
    BackendUnavailable { backend: SandboxBackend, message: String },
    #[error("backend cannot enforce policy: {0}")]
    BackendCannotEnforce(String),
    #[error("unsupported host OS for configured backend")]
    UnsupportedOs,
    #[error("permission denied: {0}")]
    Denied(DenialReason),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("permission token expired")]
    TokenExpired,
    #[error("exec timed out after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DenialReason {
    #[error("missing exec grant")]
    MissingExecGrant,
    #[error("exec not allowlisted")]
    ExecNotAllowlisted,
    #[error("args not allowlisted")]
    ArgsNotAllowlisted,
    #[error("path denied: {0}")]
    PathDenied(String),
    #[error("cwd outside jail")]
    CwdOutsideJail,
    #[error("network denied")]
    NetworkDenied,
    #[error("env var denied: {0}")]
    EnvDenied(String),
    #[error("quarantine blocked command: {0}")]
    QuarantineBlocked(String),
}
```

**Exit / signal contract:**

| Outcome | Return |
| --- | --- |
| Child exits (any code, including cargo compile failure) | `Ok` with `exit_code: Some(code)`, `signal: None` |
| Child killed by signal (OOM, SIGSEGV, …) **not** caused by broker timeout/cancel | `Ok` with `exit_code: None`, `signal: Some(signo)` |
| Broker timeout kill | `Err(Timeout)` — after process-group kill completes |
| Explicit cancel token fired | `Err(Cancelled)` — after kill completes |
| `exec` future **dropped** | Drop guard kills process group; **no** `Cancelled` value is returned (future is gone) |
| Policy denial | `Err(Denied(_))` |
| Backend missing / cannot enforce | `Err(BackendUnavailable \| BackendCannotEnforce)` |

### 3.5 `SandboxBroker` trait

```rust
#[async_trait]
pub trait SandboxBroker: Send + Sync {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError>;
    fn profile(&self) -> &SandboxProfile;
    fn capabilities(&self) -> &SandboxCapabilities;
}

#[derive(Debug, Clone)]
pub struct SandboxCapabilities {
    pub landlock: BackendStatus,
    pub seatbelt: BackendStatus,
    pub container: BackendStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    Available { detail: String },
    Unavailable { reason: String },
    NotApplicable,
}
```

### 3.6 `PathPolicy`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess { Read, Write }

#[derive(Clone)]
pub struct PathPolicy {
    jail: PathBuf,     // canonical absolute
    deny: GlobSet,     // compiled; case-insensitive on macOS
    // RO roots from jail path set — Write access rejected here
    read_only_roots: Vec<PathBuf>,
}

impl PathPolicy {
    pub fn from_profile(profile: &SandboxProfile, read_only_roots: Vec<PathBuf>)
        -> Result<Self, SandboxError>;

    /// Canonicalize + jail membership + deny-glob + RO-root write check.
    pub fn authorize(&self, path: &Path, access: PathAccess) -> Result<PathBuf, SandboxError>;

    /// Cwd must canonicalize inside jail (membership); deny-glob applies.
    pub fn authorize_cwd(&self, cwd: &Path) -> Result<PathBuf, SandboxError>;
}
```

**`PathAccess` semantics:** `Read` allows RO roots; `Write` on a path under any `read_only_roots` entry → `Denied(PathDenied)` (except `.package-cache` and `registry/src` carve-outs).

**Deny-glob matching (normative):**

1. Canonicalize `path` (if final component missing: canonicalize parent, join name). Symlink targets that leave the jail → deny.
2. If canonical path is outside `jail` → `PathDenied` / `CwdOutsideJail`.
3. Render **jail-relative** path with `/` separators (no leading `/`).
4. Match against compiled `GlobSet` built with `GlobBuilder::literal_separator(true)` plus macOS `case_insensitive(true)`:
   - Pattern with `/`: add as jail-relative; also add `**/`+pattern when pattern does not already start with `**/`.
   - Pattern without `/`: add `pattern` and `**/`+pattern.
5. Match → deny.

**Default deny globs (deduped; basename-anywhere covers nested `.env`):**

```text
.env
.env.*
*.pem
*.key
id_rsa
id_rsa.*
id_ed25519
id_ed25519.*
.ssh/**
**/.ssh/**
.aws/**
**/.aws/**
.netrc
```

```rust
#[must_use]
pub fn default_deny_globs() -> Vec<Glob> { /* literals above as Glob(...) */ }
```

**MVP exec path checks:** call `PathPolicy::authorize_cwd` for `req.cwd`. Absolute path arguments are **not** scanned heuristically. Callers encode path restrictions via `args_glob` and backend jail + deny bind-overs. RFC-0006 `fs_read` MUST call `PathPolicy::authorize(..., Read)`.

### 3.7 Construction & `RecordingSandboxBroker`

```rust
impl NativeSandboxBroker {
    /// Probe backends. Fail closed if `check_backend` is Unavailable/unenforceable.
    /// If only `test_backend` is Unavailable, construction **succeeds** and
    /// `exec(ExecClass::Test)` returns `BackendUnavailable` (so `cargo check` path
    /// works on hosts without docker when profile has `test = "container"`).
    pub async fn new(profile: SandboxProfile) -> Result<Self, SandboxError>;
}

pub fn load_sandbox_profile(
    profile_toml: &Path,
    fs_jail: PathBuf,
) -> Result<SandboxProfile, SandboxError>;

/// FIFO canned responses for tests.
pub struct RecordingSandboxBroker {
    profile: SandboxProfile,
    capabilities: SandboxCapabilities,
    scripts: Mutex<VecDeque<Result<SandboxExecResult, SandboxError>>>,
    recorded: Mutex<Vec<SandboxExecRequest>>,
}

impl RecordingSandboxBroker {
    pub fn new(profile: SandboxProfile) -> Self;
    /// Push a canned outcome (FIFO).
    pub fn push(&self, outcome: Result<SandboxExecResult, SandboxError>);
    /// Recorded requests in order.
    pub fn recorded(&self) -> Vec<SandboxExecRequest>;
    /// Default capabilities: all `Available { detail: "recording" }` for configured OS slots.
    pub fn with_capabilities(self, caps: SandboxCapabilities) -> Self;
}

#[async_trait]
impl SandboxBroker for RecordingSandboxBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        // push req to recorded; pop front script or Internal("recording exhausted")
    }
    fn profile(&self) -> &SandboxProfile { &self.profile }
    fn capabilities(&self) -> &SandboxCapabilities { &self.capabilities }
}
```

---

## 4. Internal Module Design

```text
crates/alloy-tools/src/sandbox/
  mod.rs, types.rs, profile.rs, glob.rs, path.rs, grant.rs, env.rs
  process.rs, policy_digest.rs, broker.rs, recording.rs
  backend/{mod.rs, linux.rs, macos.rs, container.rs, probe.rs}
```

`alloy-tools → alloy-runtime` only. No storage/session/obs dependency. Audit via `tracing`; RFC-0006 emits tool events.

---

## 5. Execution Algorithm

### 5.1 Pipeline

```mermaid
sequenceDiagram
  participant MCP as MCP builtin
  participant B as NativeSandboxBroker
  participant G as grant/path/env
  participant BE as Backend
  participant P as process

  MCP->>B: exec(req)
  B->>G: expiry + Exec grant match (pre-rewrite argv)
  B->>G: cwd inside jail + deny globs
  B->>G: scrub env; apply quarantine rewrite
  B->>BE: isolate (Landlock+userns+netns | Seatbelt | container)
  B->>P: spawn resolved binary; supervise
  P-->>B: status + capped stdio
  B-->>MCP: Ok(result) or Err
```

### 5.2 Token & grant validation

1. Expiry: if `expires` is `Some(t)` and `Timestamp::now().0 > t.0` → `TokenExpired`.
2. Collect all `Grant::Exec` allows. If none → `Denied(MissingExecGrant)`.
3. For each allow, test `exec_allow_matches` (§5.3) against **caller argv before quarantine rewrite**. First match wins. If some Exec grants exist but none match binary → `ExecNotAllowlisted`. If a binary matches but every matching binary fails args → `ArgsNotAllowlisted`.
4. Profile `network = Deny` wins over any `Grant::Network` (MVP: Allow profiles rejected at load).
5. `FsRead`/`FsWrite`/`GitWrite` are not interpreted by `exec` beyond cwd jail checks.

### 5.3 Binary resolution & `args_glob`

**Resolve executable (native backends only — Landlock / Seatbelt):**

1. Reject empty argv, NUL bytes, `argv.len() > 256`, total argv bytes `> 64 KiB` → `Invalid`.
2. Reject `argv[0]` containing `..` path segments → `Invalid`.
3. Reject `ExecAllow.binary` containing `..` → `Invalid`.
4. Let `PATH` = scrubbed base PATH (§6). **No implicit `.` entry.**
5. **PATH hit selection** (do not canonicalize yet):
   - If `argv[0]` is absolute: `chosen = argv[0]`.
   - Else if `argv[0]` contains `/` (e.g. `./tool`): `chosen = cwd.join(argv[0])`.
   - Else: search `PATH` directories in order; first existing file with execute bit → `chosen`. If none → `Invalid("binary not found")`.
6. Let `path_hit_basename = basename(chosen)` **before** any canonicalize. This string is the **sole** basename authority for:
   - basename-form `ExecAllow.binary` matching, and
   - quarantine cargo detection (§5.6).
7. Grant match:
   - Basename grant (`allow.binary` has no `/`): `path_hit_basename == allow.binary`.
   - Path grant (`allow.binary` contains `/`): `canonicalize(chosen)? == canonicalize(allow.binary resolved against cwd)?`.
8. **Spawn (native):** `execve(canonicalize(chosen)?, argv_for_child, env)` where `argv_for_child[0] =` the caller’s **original** `argv[0]` string (preserves rustup/`busybox` argv0-dispatch). Do **not** replace `argv[0]` with the canonical path.
9. If canonicalize changes the basename relative to `path_hit_basename`, still keep original argv0; do not re-run grant matching on the post-canonicalize basename.

**Container backend resolution (normative — distinct from native):**

- Grant matching for **basename-form** `argv[0]` uses `path_hit_basename = basename(argv[0])` **without** requiring the binary to exist on the host.
- Path-form / absolute `argv[0]` MUST still exist on the host and match grants as above; the container command then runs that path only if it is under a bind-mounted identical host path — MVP builtins MUST use basename-form `cargo` / tool names.
- Container command line is `req.argv` **verbatim** (argv0 preserved naturally by the runtime). Host-resolved toolchain binaries are **not** passed as the container command (avoids glibc mismatch).
- Host `CARGO_HOME`/`RUSTUP_HOME` binds are **registry/cache only**, not the executed toolchain. The image supplies `rustc`/`cargo` matching `rust-toolchain.toml`.

**`args_glob` dialect:**

| Rule | Value |
| --- | --- |
| Subject string | `argv[1..]` joined with **ASCII space** ` ` (U+0020), not NUL |
| Builder | `GlobBuilder::new(pat).literal_separator(true).case_insensitive(false).backslash_escape(true).build()` |
| Anchoring | globset full-match (entire subject) |
| `None` args_glob | any args allowed (after length caps) |
| `Some("")` | `ArgsNotAllowlisted` |

**Normative examples** (`argv = ["cargo", ...]` already matched binary):

| `args_glob` | argv[1..] | Match? |
| --- | --- | --- |
| `check` | `["check"]` | yes |
| `check` | `["check", "--workspace"]` | **no** (full-match requires exact subject) |
| `check*` | `["check", "--workspace"]` | **yes** (subject `"check --workspace"`; with `literal_separator(true)` only `/` is special, so `*` matches spaces) |
| `check*` | `["test"]` | no |
| `check --workspace` | `["check", "--workspace"]` | yes |
| `+nightly check*` | `["+nightly", "check"]` | yes |

Implementers MUST encode these examples as unit tests.

### 5.4 Backend selection

`backend = profile.backend_for(class)`. If capabilities say Unavailable → `BackendUnavailable`. OS mismatch → `UnsupportedOs`. No silent Container substitution. If exec-time the runtime vanishes → map to `BackendUnavailable` (not bare `Io`) when detection is clear.

### 5.5 Backend enforcement

#### Jail path set (Landlock / Seatbelt) — complete

| Path | Access |
| --- | --- |
| `fs_jail` (canonical) | RW |
| `/usr`, `/lib`, `/lib64`, `/lib32`, `/bin`, `/sbin`, `/etc` | RO (loader + system) |
| Resolved rustup/cargo **allowlisted** subtrees (§ homes) | RO (+ `.package-cache` RW) |
| Toolchain sysroot (native only; container uses image toolchain) | RO |
| Broker tmpdir | RW |
| `/dev/null`, `/dev/urandom`, `/dev/zero` | as needed |
| `/proc` | RO |
| `/tmp` | RW only for broker-created subdirectory; prefer tmp under jail |

**Child `HOME` and cargo/rustup homes (normative):**

1. Before rewriting `HOME`, resolve operator homes:
   - `op_home = parent HOME` (required; if unset → `Invalid("HOME unset")` on Unix).
   - `cargo_home = parent CARGO_HOME` if set, else `op_home.join(".cargo")`.
   - `rustup_home = parent RUSTUP_HOME` if set, else `op_home.join(".rustup")`.
2. **Native backends:** child env sets `CARGO_HOME`/`RUSTUP_HOME` to those absolute paths explicitly (even if parent unset them).
3. Child `HOME` = `fs_jail.join(".alloy-sbx-home")` (created by broker) — **never** `op_home` (all backends).
4. **Allowlisted RO mounts only** (do **not** mount whole `cargo_home` / `rustup_home` — blocks `credentials.toml` / credential theft):
   - `cargo_home/registry` RO
   - `cargo_home/git` RO
   - `cargo_home/bin` RO (**native** only)
   - `rustup_home/toolchains` RO (**native** only)
   - `rustup_home/settings.toml` RO if present (**native** only)
5. **Package-cache write:** RW grant to `cargo_home.join(".package-cache")` only. Offline unpack may also need RW `cargo_home/registry/src` — grant RW to that subtree in MVP; residual-risk notes index cache caveats.
6. **Credential defence in depth:** bind `/dev/null` (file) over `cargo_home/credentials.toml` and `cargo_home/credentials` when they exist, on all backends.
7. `PathPolicy` `read_only_roots` includes the allowlisted RO subtrees; `.package-cache` and `registry/src` are RW.

#### Container env composition (normative — distinct from native scrub)

| Variable | Container child |
| --- | --- |
| `PATH` | **Not forwarded** — image default PATH resolves `req.argv[0]` |
| `RUSTUP_HOME` | **Not forwarded** — image `/usr/local/rustup` (or image default) |
| `RUSTUP_TOOLCHAIN` | **Not forwarded** — image toolchain / `rust-toolchain.toml` in jail |
| `TMPDIR` | Set to `fs_jail.join(".alloy-sbx-tmp")` (created); host `TMPDIR` not forwarded |
| `CARGO_HOME` | Host `cargo_home` absolute path (identical bind for registry/git/cache) |
| `HOME` | `fs_jail/.alloy-sbx-home` |
| `CARGO_NET_OFFLINE` | Forced `true` when quarantine |
| `USER`, `LANG`, `LC_ALL`, `TERM` | Forwarded if present (only these names from §6.2; not overridden by the rows above) |

Native scrub (§6) still applies to Landlock/Seatbelt. Container builds env via this table, then writes `--env-file`.

#### Deny-glob enforcement inside the jail (all backends)

`PathPolicy` alone does not stop a child that opens `fs_jail/.env` after spawn. MVP **MUST** apply:

| Backend | Mechanism (normative) |
| --- | --- |
| Landlock | Parent precomputes bind list (`CString` pairs); child `pre_exec` only mounts. Files → `/dev/null`; directories → empty RO dir/tmpfs bind; other node types → `Internal`. Walk bounded max 10_000 entries. |
| Seatbelt | SBPL `deny file-read*` / `deny file-write*` for each deny-glob expansion (Seatbelt subsection). |
| Container | Files: `-v /dev/null:<abs>:ro`. Directories: bind empty RO dir. Also bind-over `cargo_home/credentials*` when present. |

AC7 / `child_cannot_read_dotenv_in_jail` applies to **all** backends via this table.

#### Landlock (Linux) — ABI floor **2** (FS)

Composition: prepare ruleset FD + uid/gid map buffers + deny bind `CString` pairs **in the parent** before spawn; `pre_exec` only applies them (no allocation/formatting in the multi-threaded child).

1. `unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)`.
2. Write pre-formatted identity `uid_map` / `setgroups=deny` / `gid_map`. If identity+netns fails at **probe** time → Landlock `Unavailable` → `BackendUnavailable` on `new` when check uses Landlock. `BackendCannotEnforce` is reserved for an exec-time attempt to run Landlock **without** netns under `network=Deny` (must not happen if probe is honest).
3. **`mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)`** so subsequent bind-overs **cannot** propagate to the host mount peer group (required — without this, `/dev/null` over `.env` / `credentials.toml` can persist on the host after the sandbox exits).
4. Bind deny matches using parent-prepared `CString` pairs (files → `/dev/null`; dirs → empty RO tmpfs/dir).
5. Loopback up via `SIOCSIFFLAGS` on `lo` (ioctl); no `/bin/ip` dependency.
6. `landlock::restrict_self` on parent-created ruleset with **`CompatLevel::HardRequirement`**, ABI ≥ 2 (sets `PR_SET_NO_NEW_PRIVS`). Empty/best-effort apply → failure — **never** bare-exec.
7. Landlock RO/RW paths from the jail table.

**Probe at `new`:** throwaway child exercises userns+netns+landlock; failure → Landlock `Unavailable` with reason.

**Network:** FS-only Landlock under Deny → `BackendCannotEnforce`.

**Host-untouched assertion:** after sandbox tests, host `credentials.toml` / `.env` must still be the original file (not a lingering `/dev/null` bind) — covered by `dotenv_sentinel_unchanged` and `credentials_sentinel_unchanged`.

#### Seatbelt (macOS)

- Invoke `/usr/bin/sandbox-exec -f <sbpl_path> -- <resolved_file> <args…>` with child argv0 = original `argv[0]` (via trampoline if needed).
- **SBPL source (normative):** broker **generates a tempfile** from shipped template `sandbox/backend/macos/alloy-check.sb.template`, substituting jail/tmp/`CARGO_HOME`/`RUSTUP_HOME`, then appends `deny file-read* file-write*` subpath clauses for each matched deny path. Do not ship a single static SBPL as the sole artifact.
- **Profile apply failure:** trampoline writes one ready-byte on a pipe after `sandbox_init`/exec handoff succeeds; if the supervised process exits before the ready-byte with sandbox-exec diagnostics → `BackendCannotEnforce`, **not** `Ok(exit_code)`.
- Apple deprecates `sandbox-exec` — residual risk §5.7.

#### Container

| Knob | Normative MVP |
| --- | --- |
| Runtime | `ALLOY_CONTAINER_RUNTIME` if set; else probe `docker` then `podman` |
| Image | Default **`docker.io/library/rust:1.97.1-bookworm`** (matches workspace `rust-version` / `rust-toolchain.toml`). Profile field / `ALLOY_CONTAINER_IMAGE` override. Precedence: env > profile TOML `container_image` > default. |
| Command | `req.argv` verbatim as container command (see §5.3 Container resolution) |
| Mounts | Bind-mount `fs_jail` at **identical absolute host path**; cwd unchanged |
| Deny files | `-v /dev/null:<file>:ro` for matched **files**; for matched **directories** bind an empty RO tmpfs/dir (not `/dev/null` — would `ENOTDIR`) |
| Cargo/rustup cache | Bind RO only allowlisted subtrees (§5.5 homes); **not** host toolchain `bin` as the executed binary |
| User | `--user <uid>:<gid>` |
| Network | `--network none` when Deny |
| Identity | `--cidfile <broker_tmp>/cid` (required); timeout: `kill --signal TERM` then after 2s `kill` (SIGKILL); missing container on second kill = success |
| Lifecycle | `--rm`; `--init` |
| Env | Env composed per the container env composition table, written to `broker_tmp/envfile` mode 0600 via `--env-file`; reject values containing `\n` or names starting with `#` as `Invalid`; delete after run |
| Workdir | `--workdir <canonical cwd>` |

**Container status mapping (normative):**

| Runtime/child outcome | Broker result |
| --- | --- |
| Docker/podman failure to start (`125`) | `Err(BackendUnavailable { … })` |
| Container conflict / usage (`126`) | `Err(Internal)` |
| Command not found in image (`127`) | `Ok { exit_code: Some(127), signal: None }` |
| Exit code `n` where `0 ≤ n ≤ 124` | `Ok { exit_code: Some(n), signal: None }` |
| Exit `128+n` with `n ∈ 1..=127` | `Ok { exit_code: None, signal: Some(n) }` |
| Broker-initiated kill on timeout | `Err(Timeout)` after `runtime kill` |

### 5.6 `quarantine_deps`

When `true`:

1. Force child env `CARGO_NET_OFFLINE=true` (**overrides** any inherited/base value).
2. Argv rewrite **after** grant match on original argv:
   - Detect cargo: `path_hit_basename == "cargo"` (§5.3 — **not** `basename(canonical)`).
   - Let `sub` = first argv element after optional `+<toolchain>` that does not start with `-`.
   - If no subcommand (`sub == None`): allow with `CARGO_NET_OFFLINE` only (no `--offline` insert).
   - If `sub ∈ {fetch, update, install, publish, search}` → `Denied(QuarantineBlocked(sub))` before spawn.
   - If `sub ∈ {check, test, build, clippy, tree, metadata}` and `--offline` not already present: insert `--offline` immediately after `sub`.
   - Any other cargo subcommand under quarantine: **allow** with `CARGO_NET_OFFLINE` only (no insert); document in residual-risk.
3. Tracing: log quarantine decision at `info` (`blocked=<sub>` or `offline_inserted=true`). Result struct does not carry argv.

### 5.7 Residual risk doc

Create `docs/security/sandbox-residual-risk.md` covering: build.rs/proc-macro RCE inside jail; quarantine limits; `sandbox-exec` deprecation; dogfood ban until M1 holdout green; Landlock ABI/netns host requirements.

---

## 6. Environment & Process Lifecycle

### 6.1 Hard-denied env (never; `env_allow` cannot override)

Exact names: `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`, `RUSTC_WRAPPER`, `RUSTFLAGS`, `CARGO`, `CARGO_BUILD_RUSTC`, `SSH_AUTH_SOCK`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`.

Substring deny (ASCII case-insensitive **substring** on the env **name**, no regex crate required): `api_key`, `api-key`, `secret`, `password`, `token` — with false-positive acceptance documented (`CARGO_REGISTRY_TOKEN` denied under quarantine anyway).

### 6.2 Base allowed from parent (if set)

`PATH`, `USER`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`.  
`HOME` → rewritten to sandbox home (§5.5).  
`CARGO_NET_OFFLINE` → forced when quarantine.

### 6.3 Scrub algorithm

Empty map → insert base → insert `env_allow` names from parent if not hard-denied → never parse `.env` files → never log values.

### 6.4 Supervision

| Concern | Rule |
| --- | --- |
| Stdin | `/dev/null` |
| Stdio | Concurrent drain; caps; **after cap, continue reading and discard**; set truncation flags; truncation ≠ error; never stop reading (would deadlock child → false Timeout) |
| Process group | `setsid` / new group; timeout: SIGTERM group → wait 2s → SIGKILL; container: runtime `kill` |
| Drop of `exec` future | Drop guard runs same kill path; no return value |
| Cancel token (optional `CancellationToken` field deferred to 0006) | MVP: drop-to-kill only; `Cancelled` reserved for future explicit token API additive to request — **not** required in MVP request struct |
| Orphans | Forbidden after return or drop |
| Parallelism | Broker allows concurrent execs; `max_parallel_cargo=1` enforced by scheduler (V2 Appendix B / ADR F-16), not broker |
| Rlimits | Out of MVP (wall clock + stdio only) |

**MVP cancel note:** Without an explicit token on `SandboxExecRequest`, `Err(Cancelled)` is unreachable in MVP; keep the variant for 0006 additive extension. Drop-guard kill still REQUIRED.

### 6.5–6.7 Concurrency / async / shutdown

`Send+Sync`; concurrent `exec` OK; probes cached at `new` with exec-time remap to `BackendUnavailable` when runtime disappears; `spawn_blocking` only for pre_exec setup if needed; dropping `Arc` broker does not kill other clones’ in-flight children.

---

## 7. Configuration

| Concern | Rule |
| --- | --- |
| Parse `[sandbox]` | `load_sandbox_profile` in alloy-tools |
| Path | `RuntimeConfig.profile_path` |
| `RuntimeConfig` fields | **No** sandbox fields added |
| Missing section | Error |
| `network=allow` | Always `Invalid` in MVP |
| `ALLOY_CONTAINER_RUNTIME` / `ALLOY_CONTAINER_IMAGE` | Process env; never write `.env` |
| Test override | Feature `test-hooks` (never in release) may force backend; **not** `cfg(test)` alone |

`profiles/default.toml` MUST contain Appendix B `[sandbox]` (already added). Commented optional in `example.env`:

```bash
# ALLOY_CONTAINER_RUNTIME=docker
# ALLOY_CONTAINER_IMAGE=docker.io/library/rust:1.97.1-bookworm
```

---

## 8. Error Handling

| Variant | Caller action |
| --- | --- |
| `Denied(*)` | Fail tool / escalate |
| `BackendUnavailable` / `BackendCannotEnforce` | Operator fixes host/profile — never degrade to bare exec |
| `TokenExpired` / `Timeout` / `Invalid` | Surface / retry policy |
| `Cancelled` | Abort (post-MVP token) |
| `Io` / `Internal` | Fail tool call |

---

## 9. Observability

| Event | Level | Fields |
| --- | --- | --- |
| exec start | info | `run_id`, `class`, `backend`, `argv0_basename`, `argc` |
| denied | warn | `run_id`, `reason` |
| timeout | warn | `run_id` |
| probe | info | per-backend status |
| truncate | debug | stream |

No full argv, no env values, no OTLP, no SessionEvent emission, no savings metrics.

**`policy_digest`:** `Digest::sha256` of canonical JSON object with **sorted keys**:
`check_backend`, `test_backend`, `network`, `quarantine_deps`, `deny_globs` (sorted strings), `exec_timeout_secs`, `stdout_cap`, `stderr_cap`, `container_image`.  
**Exclude** absolute `fs_jail` (not portable across machines).

---

## 10. Crate Dependencies & `unsafe`

| Dep | Version floor / notes |
| --- | --- |
| `alloy-runtime` | path |
| `async-trait` | workspace |
| `thiserror` | workspace |
| `tokio` | workspace + `process` |
| `tracing` | workspace |
| `serde` / `serde_json` / `toml` | workspace |
| `globset` | `0.4` |
| `rustix` | `0.38` — `unshare`, `setsid`, `kill`, procfs uid_map helpers preferred over raw `libc` where possible |
| `libc` | `0.2` — only if rustix lacks a needed call |
| `which` | `6` — PATH search |
| `landlock` | `0.4` (Linux target) — must support hard-requirement / ABI check ≥ 2 |
| `tempfile` | dev |

```toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4"
```

`unsafe`: `deny` at crate root; `allow` only in `backend/linux.rs` and `backend/macos.rs` with per-block comments. New deps are human-gated per V2 §14.6 (this RFC is the gate record).

---

## 11. Testing Strategy

### Unit

| Test | Asserts |
| --- | --- |
| `deny_globs_env_and_keys` | matcher on jail-relative paths |
| `path_policy_symlink_escape` / `dotdot` | Denied |
| `path_policy_write_rejects_ro_root` | Write to CARGO_HOME → Denied |
| `exec_allow_examples_table` | §5.3 args_glob examples |
| `binary_resolution_path_basename_shim` | absolute / `./rel` / PATH hit; basename vs path grant; rustup shim keeps argv0; `..` rejected |
| `env_scrub_strips_ld_preload` | hard deny wins |
| `env_substring_denies_registry_token` | `CARGO_REGISTRY_TOKEN` denied by substring rule |
| `child_home_is_sandbox_home` | unit assert HOME rewrite (integration confirms) |
| `token_expired_compares_offsetdatetime` | expiry |
| `quarantine_rewrites_and_blocks_fetch` | `--offline` insert; `fetch` denied |
| `profile_missing_section_errors` | Invalid |
| `network_allow_rejected` | Invalid |
| `policy_digest_stable_and_jail_excluded` | digest |
| `recording_broker_fifo` | push/pop/exhausted |
| `signal_status_encoding` | mock wait status → `signal: Some` |

### Integration

| Test | Asserts |
| --- | --- |
| `landlock_cargo_check_fixture` | Linux |
| `seatbelt_cargo_check_fixture` | macOS |
| `container_cargo_check_fixture` | runtime present |
| `child_cannot_read_dotenv_in_jail` | place `.env` in jail; sandboxed read fails |
| `child_cannot_read_cargo_credentials` | sentinel `credentials.toml` unreadable inside sandbox |
| `netns_probe_marks_unavailable` | probe-time netns failure → Landlock Unavailable / `BackendUnavailable` |
| `network_deny_blocks_egress` | connect fails |
| `timeout_kills_process_group` | grandchild dead |
| `cancel_drop_no_orphan` | drop future → no child |
| `output_cap_truncates` | flags |
| `dotenv_sentinel_unchanged` | host `.env` bytes unchanged |
| `credentials_sentinel_unchanged` | host `credentials.toml` bytes unchanged after sandbox |
| `backend_unavailable_fail_closed` | no bare exec |
| `landlock_actually_applied` | child `open(/etc/shadow)` or out-of-jail path fails under Landlock (proves non-skip) |

### CI deliverable (yes)

Add `.github/workflows/sandbox.yml`:

- `ubuntu-latest`: `cargo test -p alloy-tools -- --nocapture` with Landlock tests **required** (job fails if Landlock tests all ignored).
- Optional macOS job: Seatbelt tests.
- Container job: only if docker available.

Skip policy: individual tests may `ignore` with reason when probe says Unavailable — but the Linux workflow MUST run at least one test that asserts Landlock enforcement (`landlock_actually_applied`) or fail the job.

### Clippy

`clippy.toml` disallows `std::process::Command::new` in `alloy-tools` except `sandbox::process` / `sandbox::backend`.

---

## 12. MVP vs Deferred

**MVP:** broker, three backends as specified, PathPolicy, recording double, tests, CI workflow, residual-risk doc, profile `[sandbox]`, clippy seam.

**Deferred:** MCP builtins (0006), EditEngine (0008), stdio→artifacts, `network=allow`, gVisor, community MCP, memory/CPU rlimits, explicit cancel token on request.

---

## 13. Acceptance Criteria

| # | Criterion | Proof |
| --- | --- | --- |
| 1 | No bare `Command::new` outside sandbox modules | `cargo clippy -p alloy-tools -- -D warnings` + clippy.toml |
| 2 | Permission types match main | compile against `alloy-runtime` types; no local Grant enum |
| 3 | `ExecClass` selects backend; no argv sniffing; §5.3 binary/args rules | `binary_resolution_path_basename_shim` + `exec_allow_examples_table` |
| 4 | Non-zero exit `Ok`; denial `Err`; signal `Ok{signal}` | `signal_status_encoding` + integration cargo fail |
| 5 | `network=deny` via netns/Seatbelt/`--network none`; FS-only Landlock impossible | `netns_failure_is_cannot_enforce` + `network_deny_blocks_egress` |
| 6 | Quarantine forces offline + blocks fetch/update/install | `quarantine_rewrites_and_blocks_fetch` |
| 7 | Deny globs + in-sandbox `.env` unreadable; cargo credentials unreadable; host `.env` / credentials untouched | `child_cannot_read_dotenv_in_jail` + `child_cannot_read_cargo_credentials` + `dotenv_sentinel_unchanged` + `credentials_sentinel_unchanged` |
| 8 | `PathPolicy` exported for 0006 | public API + path policy tests |
| 9 | Unavailable backend fail closed | `backend_unavailable_fail_closed` |
| 10 | Timeout/drop kill process group | `timeout_kills_process_group` + `cancel_drop_no_orphan` |
| 11 | Stdio caps | `output_cap_truncates` |
| 12 | `[sandbox]` in default profile | file content |
| 13 | Residual risk doc | `docs/security/sandbox-residual-risk.md` exists |
| 14 | Dogfood ban referenced | residual-risk + this RFC |
| 15 | `RecordingSandboxBroker` API complete | `recording_broker_fifo` |
| 16 | CI workflow exists; Landlock proven | `.github/workflows/sandbox.yml` + `landlock_actually_applied` |
| 17 | fmt/clippy clean; 5 crates; no `.env` writes | commands |
| 18 | Series DoD | below |

## Definition of Done

- [ ] Architecture compliance: **PASS**
- [ ] RFC acceptance criteria: **100% satisfied** (§13)
- [ ] Unit tests: **passing**
- [ ] Integration tests: **passing** on CI Landlock job
- [ ] Documentation: **complete**
- [ ] Public APIs: **reviewed and stable**
- [ ] Clippy: **clean**
- [ ] Formatting: **clean**
- [ ] No TODO/placeholders in scope
- [ ] Code review: **approved**

---

## 14. Open Questions

1. **Userns identity-map + netns on CI:** if ubuntu-latest blocks unprivileged userns, CI Landlock job uses Container for `check` via a CI-only profile overlay — production default remains Appendix B. Confirm on first green CI run.
2. **rustup shim basename rule:** §5.3 basename grant rule is pinned; if dogfood finds false denies, adjust with a test before widening.

**Settled (do not reopen):** ADR F-07; fail closed; main permission types; `network=deny` + quarantine defaults; never write `.env`; residual build.rs risk; broker in `alloy-tools`; nonzero exit is `Ok`; `ExecClass` explicit; Allow network rejected in MVP; ≤5 crates; container bind-mount at identical host path; Landlock ABI ≥ 2 with HardRequirement; args_glob space-joined full match with `literal_separator(true)`.

---

## Estimated implementation effort

**5–8 person-days** (platform variance dominates).

Suggested split: types/profile/path/glob (1d) · grant/env (0.5d) · process (1d) · Linux (1.5–2d) · macOS (1d) · container (1d) · tests/CI/docs/clippy (1–1.5d).

---

*— arkadianet*
