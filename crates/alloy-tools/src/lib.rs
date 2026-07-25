//! Alloy tooling: sandbox broker (RFC-0005) and MCP host (RFC-0006).
//!
//! # Crate map
//!
//! - [`sandbox`] — SandboxBroker, PathPolicy, backends (RFC-0005)
//! - MCP host surface lands in RFC-0006 (not implemented here)
//!
//! Public surface matches RFC-0005 §3.1. Internal helpers stay crate-private.
//!
//! Author: arkadianet

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod sandbox;

pub use sandbox::{
    default_deny_globs, load_sandbox_profile, BackendStatus, DenialReason, ExecClass,
    NativeSandboxBroker, NetworkPolicy, PathAccess, PathPolicy, RecordingSandboxBroker,
    SandboxBackend, SandboxBroker, SandboxCapabilities, SandboxError, SandboxExecRequest,
    SandboxExecResult, SandboxProfile,
};
