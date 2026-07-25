//! Public sandbox types, errors, and the [`SandboxBroker`] trait (RFC-0005 §3).

use std::path::PathBuf;
use std::time::Duration;

use alloy_runtime::{Digest, PermissionToken};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sandbox::profile::SandboxProfile;

/// Which exec class selects the profile backend (not argv sniffing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecClass {
    /// Light verification path (typically Landlock/Seatbelt).
    Check,
    /// Heavier path (typically container).
    Test,
}

/// Isolation backend selected by profile TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// Linux Landlock + user/mount/net namespaces.
    Landlock,
    /// macOS Seatbelt via `sandbox-exec`.
    Seatbelt,
    /// Docker/Podman container.
    Container,
}

/// Network egress policy. MVP rejects `Allow` at profile load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// No egress (default).
    Deny,
    /// Deferred — load rejects this in MVP.
    Allow,
}

/// Request to execute under the sandbox.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SandboxExecRequest {
    /// Argv including binary name/path at index 0.
    pub argv: Vec<String>,
    /// Working directory (must canonicalize inside the jail).
    pub cwd: PathBuf,
    /// Extra env **names** permitted beyond the broker base set.
    ///
    /// Values always come from the parent process environment — no value
    /// injection API in MVP. Each name must pass
    /// [`crate::sandbox::validate_env_allow_name`].
    pub env_allow: Vec<String>,
    /// Authoritative permission token (includes `run_id`).
    pub perms: PermissionToken,
    /// Selects check vs test backend.
    pub class: ExecClass,
}

impl SandboxExecRequest {
    /// Build a request with an empty `env_allow` list.
    #[must_use]
    pub fn new(argv: Vec<String>, cwd: PathBuf, perms: PermissionToken, class: ExecClass) -> Self {
        Self {
            argv,
            cwd,
            env_allow: Vec::new(),
            perms,
            class,
        }
    }

    /// Attach additional allowed environment variable names.
    #[must_use]
    pub fn with_env_allow(mut self, names: Vec<String>) -> Self {
        self.env_allow = names;
        self
    }
}

/// Result of a sandboxed execution.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SandboxExecResult {
    /// `Some(code)` if the child exited; `None` if killed by signal.
    pub exit_code: Option<i32>,
    /// `Some(signo)` if terminated by signal; `None` otherwise.
    pub signal: Option<i32>,
    /// Captured stdout (may be truncated).
    pub stdout: Vec<u8>,
    /// Captured stderr (may be truncated).
    pub stderr: Vec<u8>,
    /// Whether stdout exceeded the profile cap.
    pub stdout_truncated: bool,
    /// Whether stderr exceeded the profile cap.
    pub stderr_truncated: bool,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Backend that enforced isolation.
    pub backend: SandboxBackend,
    /// Digest over portable policy JSON (excludes absolute `fs_jail`).
    pub policy_digest: Digest,
}

impl SandboxExecResult {
    /// Construct a synthetic result for tests / [`crate::RecordingSandboxBroker`].
    #[must_use]
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

    /// Attach stdio bytes (test helper).
    #[must_use]
    pub fn with_stdio(mut self, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        self.stdout = stdout;
        self.stderr = stderr;
        self
    }
}

/// Sandbox broker errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// Configured backend is not available on this host.
    #[error("backend unavailable: {backend:?}: {message}")]
    BackendUnavailable {
        /// Backend that failed the probe or vanished at exec time.
        backend: SandboxBackend,
        /// Operator-facing guidance.
        message: String,
    },
    /// Backend is present but cannot enforce the requested policy.
    #[error("backend cannot enforce policy: {0}")]
    BackendCannotEnforce(String),
    /// Host OS does not support the configured backend.
    #[error("unsupported host OS for configured backend")]
    UnsupportedOs,
    /// Policy denial.
    #[error("permission denied: {0}")]
    Denied(DenialReason),
    /// Malformed request or profile.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// `PermissionToken` expiry reached (inclusive).
    #[error("permission token expired")]
    TokenExpired,
    /// Wall-clock timeout; process group was killed.
    #[error("exec timed out after {0:?}")]
    Timeout(Duration),
    /// Explicit cancel token (reserved for RFC-0006; unreachable in MVP).
    #[error("cancelled")]
    Cancelled,
    /// I/O failure during setup or supervision.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

/// Why an exec was denied.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DenialReason {
    /// No `Grant::Exec` on the token.
    #[error("missing exec grant")]
    MissingExecGrant,
    /// Binary not covered by any Exec allow.
    #[error("exec not allowlisted")]
    ExecNotAllowlisted,
    /// Binary matched but args failed every matching allow.
    #[error("args not allowlisted")]
    ArgsNotAllowlisted,
    /// Path failed jail / deny-glob / RO-root checks.
    #[error("path denied: {0}")]
    PathDenied(String),
    /// Cwd outside the workspace jail.
    #[error("cwd outside jail")]
    CwdOutsideJail,
    /// Network egress denied by profile.
    #[error("network denied")]
    NetworkDenied,
    /// Environment variable hard-denied.
    #[error("env var denied: {0}")]
    EnvDenied(String),
    /// Quarantine blocked a cargo network subcommand.
    #[error("quarantine blocked command: {0}")]
    QuarantineBlocked(String),
}

/// Probe status for a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    /// Backend can enforce policy.
    Available {
        /// Short diagnostic detail.
        detail: String,
    },
    /// Backend missing or unenforceable.
    Unavailable {
        /// Operator-facing reason.
        reason: String,
    },
    /// Not relevant on this OS (e.g. Seatbelt on Linux).
    NotApplicable,
}

/// Cached probe results from broker construction.
#[derive(Debug, Clone)]
pub struct SandboxCapabilities {
    /// Linux Landlock status.
    pub landlock: BackendStatus,
    /// macOS Seatbelt status.
    pub seatbelt: BackendStatus,
    /// Container runtime status.
    pub container: BackendStatus,
}

/// Sole public exec entry point for sandboxed processes.
#[async_trait]
pub trait SandboxBroker: Send + Sync {
    /// Execute `req` under the profile backend for `req.class`.
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError>;

    /// Immutable profile used by this broker.
    fn profile(&self) -> &SandboxProfile;

    /// Cached backend capabilities.
    fn capabilities(&self) -> &SandboxCapabilities;
}
