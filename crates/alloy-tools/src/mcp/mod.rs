//! MCP host and in-process builtins (RFC-0006).
//!
//! The host is Alloy's **sole tool bus**. Builtin and (future) external tools
//! share one schema model, one permission path, one dispatch path, and one
//! result model, so no worker can invent an ad-hoc tool invocation route.
//!
//! # Day-1 shape
//!
//! - Exactly four builtins: `cargo_check`, `cargo_test`, `fs_read`,
//!   `apply_patch`. No `graph_query` (ADR F-04), no raw bash.
//! - Lazy disclosure: [`McpPlatform::tools_for`] with empty selectors discloses
//!   nothing, never the catalogue.
//! - Fail closed: every `call` validates the [`PermissionToken`] before
//!   dispatch, and `InvalidArguments` precedes `PermissionDenied`.
//! - Every exec path runs through the RFC-0005 sandbox broker; `fs_read` goes
//!   through `PathPolicy::authorize`. Builtins never spawn a process directly.
//! - `apply_patch` calls an injected [`PatchApplyBackend`]; MVP ships the
//!   deterministic [`StubPatchApplyBackend`] until RFC-0008 wires EditEngine.
//!
//! # Module map
//!
//! | Module | Owns |
//! | --- | --- |
//! | `error` | [`McpError`], [`PermissionDenial`], the sole sandbox-error mapping |
//! | `platform` | the [`McpPlatform`] trait |
//! | `host` | [`InProcessMcpHost`] lifecycle, admission, timeout, obs |
//! | `registry` | immutable name → handler / view table |
//! | `disclose` | pure filter/sort/cap over `&[ToolView]` |
//! | `authz` | expiry and grant checks over the shared sandbox matcher |
//! | `builtins` | argument validation, argv derivation, dispatch |
//! | `patch` | the [`PatchApplyBackend`] seam and its stub |
//! | `handle` | [`ToolHandle`] |
//! | `recording` | [`RecordingMcpPlatform`] test double |
//! | `metrics` | [`McpMetricsSnapshot`] counters |
//! | `schema` | normative JSON Schemas + committed snapshots |
//!
//! [`PermissionToken`]: alloy_runtime::PermissionToken
//!
//! Author: arkadianet

#![forbid(unsafe_code)]

pub(crate) mod authz;
pub(crate) mod builtins;
pub(crate) mod disclose;
pub(crate) mod error;
pub(crate) mod handle;
pub(crate) mod host;
pub(crate) mod metrics;
pub(crate) mod patch;
pub(crate) mod platform;
pub(crate) mod recording;
pub(crate) mod registry;
pub(crate) mod schema;

pub use builtins::cargo_check::CargoCheckArgs;
pub use builtins::cargo_test::CargoTestArgs;
pub use builtins::fs_read::FsReadArgs;
pub use builtins::{BuiltinToolId, MAX_ARGUMENT_BYTES, MAX_ARG_STRING_BYTES, MAX_FEATURES};
pub use disclose::MAX_TOOLS_PER_DISCLOSURE;
pub use error::{McpError, PermissionDenial};
pub use handle::ToolHandle;
pub use host::{InProcessMcpHost, McpHostConfig, McpHostPhase};
pub use metrics::McpMetricsSnapshot;
pub use patch::{
    ApplyPatchArgs, ApplyPatchOutcome, PatchApplyBackend, PatchApplyError, StubPatchApplyBackend,
};
pub use platform::McpPlatform;
pub use recording::RecordingMcpPlatform;
