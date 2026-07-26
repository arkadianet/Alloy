//! Alloy tooling: sandbox broker (RFC-0005) and MCP host (RFC-0006).
//!
//! # Crate map
//!
//! - [`sandbox`] — SandboxBroker, PathPolicy, backends (RFC-0005)
//! - [`mcp`] — MCP host, in-process builtins, tool disclosure (RFC-0006)
//!
//! Public surface matches RFC-0005 §3.1 and RFC-0006 §3.1. Internal helpers
//! stay crate-private: `sandbox::grant` / `sandbox::path` are `pub(crate)` so
//! `mcp` reuses one authorization implementation, never a second one.
//!
//! Author: arkadianet

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod mcp;
pub mod sandbox;

pub use sandbox::{
    default_deny_globs, load_sandbox_profile, BackendStatus, DenialReason, ExecClass,
    NativeSandboxBroker, NetworkPolicy, OperatorHomes, PathAccess, PathPolicy,
    RecordingSandboxBroker, SandboxBackend, SandboxBroker, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult, SandboxProfile,
};

pub use mcp::{
    ApplyPatchArgs, ApplyPatchOutcome, BuiltinToolId, CargoCheckArgs, CargoTestArgs, FsReadArgs,
    InProcessMcpHost, McpError, McpHostConfig, McpHostPhase, McpMetricsSnapshot, McpPlatform,
    PatchApplyBackend, PatchApplyError, PermissionDenial, RecordingMcpPlatform,
    StubPatchApplyBackend, ToolHandle, MAX_ARGUMENT_BYTES, MAX_ARG_STRING_BYTES, MAX_FEATURES,
    MAX_TOOLS_PER_DISCLOSURE,
};

// Shared IR named by the MCP trait (also available from `alloy-runtime`).
pub use alloy_runtime::{McpServerSpec, McpTransport, ServerId};
