//! Alloy tooling: sandbox broker (RFC-0005), MCP host (RFC-0006), and
//! EditEngine (RFC-0008).
//!
//! # Crate map
//!
//! - [`sandbox`] — SandboxBroker, PathPolicy, backends (RFC-0005)
//! - [`mcp`] — MCP host, in-process builtins, tool disclosure (RFC-0006)
//! - [`edit`] — GitEditEngine + EditEnginePatchBackend (RFC-0008)
//! - `authz` — transport-neutral FsRead/FsWrite grant-glob matching (crate-private)
//! - `redact` — absolute-path redaction for operator/model-visible strings (crate-private)
//!
//! Public surface matches RFC-0005 §3.1, RFC-0006 §3.1, and RFC-0008 §3.
//! Internal helpers stay crate-private: `sandbox::grant` / `sandbox::path` are
//! `pub(crate)` so `mcp` and `edit` reuse one authorization implementation.
//!
//! Author: arkadianet

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub(crate) mod authz;
pub mod edit;
pub mod mcp;
pub(crate) mod redact;
pub mod sandbox;

pub use sandbox::{
    default_deny_globs, load_sandbox_profile, trusted_exec_path, BackendStatus, DenialReason,
    ExecClass, NativeSandboxBroker, NetworkPolicy, OperatorHomes, PathAccess, PathPolicy,
    RecordingSandboxBroker, SandboxBackend, SandboxBroker, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult, SandboxProfile,
};

pub use edit::{EditEnginePatchBackend, GitEditEngine, GitEditEngineConfig};

pub use mcp::{
    ApplyPatchArgs, ApplyPatchOutcome, BuiltinToolId, CargoCheckArgs, CargoTestArgs, FsReadArgs,
    InProcessMcpHost, McpError, McpHostConfig, McpHostPhase, McpMetricsSnapshot, McpPlatform,
    PatchApplyBackend, PatchApplyError, PermissionDenial, RecordingMcpPlatform,
    StubPatchApplyBackend, ToolHandle, MAX_ARGUMENT_BYTES, MAX_ARG_STRING_BYTES, MAX_FEATURES,
    MAX_TOOLS_PER_DISCLOSURE,
};

// Shared IR named by the MCP trait (also available from `alloy-runtime`).
pub use alloy_runtime::{McpServerSpec, McpTransport, ServerId};
