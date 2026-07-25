//! Sandbox broker (RFC-0005).
//!
//! Every `Grant::Exec` must run through [`SandboxBroker`]. Isolation backends
//! (Landlock / Seatbelt / Container) are selected by [`ExecClass`] and profile
//! TOML — never by argv sniffing. Fail closed if the configured backend is
//! unavailable; never silently bare-exec.
//!
//! # Residual risk
//!
//! `cargo check` still executes `build.rs` and procedural macros **inside** the
//! sandbox. Deny-glob bind-overs apply only to paths present at spawn time.
//! See `docs/security/sandbox-residual-risk.md`.
//!
//! Author: arkadianet

mod backend;
mod broker;
mod env;
mod glob;
mod grant;
mod path;
mod policy_digest;
mod process;
mod profile;
mod recording;
mod types;

pub use broker::NativeSandboxBroker;
pub use glob::default_deny_globs;
pub use path::{PathAccess, PathPolicy};
pub use profile::{load_sandbox_profile, SandboxProfile};
pub use recording::RecordingSandboxBroker;
pub use types::{
    BackendStatus, DenialReason, ExecClass, NetworkPolicy, SandboxBackend, SandboxBroker,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult,
};

#[cfg(any(test, feature = "test-hooks"))]
pub use env::override_operator_homes;
