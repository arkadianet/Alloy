//! MCP host error taxonomy and the sole `SandboxError` conversion (RFC-0006 §3.3 / §8).
//!
//! Author: arkadianet

use std::time::Duration;

use thiserror::Error;

use crate::sandbox::{DenialReason, SandboxError};

/// Host-level failure returned by [`McpPlatform`](crate::mcp::McpPlatform).
///
/// Tool-level failures (a command that ran and failed) travel inside
/// `Ok(ToolResult)` as `ToolError` instead — see RFC-0006 §8.2.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpError {
    /// Name is not in the immutable registry.
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    /// Fail-closed authorization refusal.
    #[error("permission denied: {0}")]
    PermissionDenied(PermissionDenial),

    /// `PermissionToken.expires` reached (inclusive boundary).
    #[error("permission token expired")]
    TokenExpired,

    /// Token carries an uncompilable grant glob or violates a defensive invariant.
    #[error("invalid permission token: {0}")]
    InvalidToken(String),

    /// Arguments failed the hand-rolled validators or the size caps.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// Requested capability is not part of the MVP (out-of-process servers).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Host is draining or stopped.
    #[error("host shutting down")]
    ShuttingDown,

    /// Host cancel token fired while the call was still polled.
    #[error("cancelled")]
    Cancelled,

    /// Host `call_timeout` elapsed.
    #[error("timeout after {0:?}")]
    Timeout(Duration),

    /// Broker / path-policy error that did not map to a more specific variant.
    ///
    /// Construct **only** via [`map_sandbox_error`]; messages are redacted
    /// there so operator filesystem layout never reaches a model.
    #[error("sandbox: {0}")]
    Sandbox(SandboxError),

    /// Host invariant violation or construction failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// Why an authorization check refused a call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PermissionDenial {
    /// Token carries zero grants of the required kind.
    #[error("missing grant: {0}")]
    MissingGrant(String),
    /// Grants exist but none cover the derived path.
    #[error("grant does not cover path: {0}")]
    PathNotCovered(String),
    /// Binary is not covered by any `ExecAllow`.
    #[error("exec not allowlisted for tool")]
    ExecNotAllowlisted,
    /// Binary matched but every candidate `args_glob` failed.
    #[error("args not allowlisted for tool")]
    ArgsNotAllowlisted,
    /// `ToolHandle` selectors do not disclose the requested tool.
    #[error("tool not disclosed for handle selectors")]
    NotDisclosed,
}

/// Sole `SandboxError` → [`McpError`] conversion (RFC-0006 §8.3).
///
/// Used instead of `?` / `From` so every broker error crosses the MCP boundary
/// through one redaction point.
pub(crate) fn map_sandbox_error(err: SandboxError) -> McpError {
    match err {
        SandboxError::Denied(reason) => McpError::PermissionDenied(map_denial(reason)),
        SandboxError::TokenExpired => McpError::TokenExpired,
        SandboxError::Timeout(d) => McpError::Timeout(d),
        SandboxError::Cancelled => McpError::Cancelled,
        other => McpError::Sandbox(redact_sandbox_error(other)),
    }
}

/// Exhaustive over today's [`DenialReason`] arms on purpose: a future RFC that
/// adds a variant must decide how it surfaces to a model rather than inherit a
/// wildcard.
fn map_denial(reason: DenialReason) -> PermissionDenial {
    match reason {
        DenialReason::MissingExecGrant => PermissionDenial::MissingGrant("exec".into()),
        DenialReason::ExecNotAllowlisted => PermissionDenial::ExecNotAllowlisted,
        DenialReason::ArgsNotAllowlisted => PermissionDenial::ArgsNotAllowlisted,
        DenialReason::PathDenied(_) => PermissionDenial::PathNotCovered("path denied".into()),
        DenialReason::CwdOutsideJail => PermissionDenial::PathNotCovered("cwd outside jail".into()),
        DenialReason::NetworkDenied => PermissionDenial::MissingGrant("network".into()),
        DenialReason::EnvDenied(_) => PermissionDenial::MissingGrant("env".into()),
        DenialReason::QuarantineBlocked(_) => PermissionDenial::MissingGrant("quarantine".into()),
    }
}

/// Rebuild the `SandboxError` variants whose `Display` may embed absolute host
/// paths, replacing pathful strings with fixed tokens (RFC-0006 §9.1).
fn redact_sandbox_error(err: SandboxError) -> SandboxError {
    match err {
        SandboxError::Invalid(_) => SandboxError::Invalid("invalid sandbox request".into()),
        SandboxError::Internal(_) => SandboxError::Internal("internal sandbox error".into()),
        SandboxError::BackendUnavailable { backend, .. } => SandboxError::BackendUnavailable {
            backend,
            message: "backend unavailable".into(),
        },
        SandboxError::BackendCannotEnforce(_) => {
            SandboxError::BackendCannotEnforce("backend cannot enforce policy".into())
        }
        SandboxError::Io(_) => SandboxError::Io(std::io::Error::other("sandbox io error")),
        // Timeout / Cancelled / TokenExpired / UnsupportedOs / Denied are
        // mapped by the caller and carry no host paths.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxBackend;

    #[test]
    fn map_sandbox_error_table() {
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::MissingExecGrant)),
            McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "exec"
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::ExecNotAllowlisted)),
            McpError::PermissionDenied(PermissionDenial::ExecNotAllowlisted)
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::ArgsNotAllowlisted)),
            McpError::PermissionDenied(PermissionDenial::ArgsNotAllowlisted)
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::PathDenied(
                "/home/x".into()
            ))),
            McpError::PermissionDenied(PermissionDenial::PathNotCovered(_))
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::CwdOutsideJail)),
            McpError::PermissionDenied(PermissionDenial::PathNotCovered(_))
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::NetworkDenied)),
            McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "network"
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::EnvDenied("HOME".into()))),
            McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "env"
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Denied(DenialReason::QuarantineBlocked(
                "fetch".into()
            ))),
            McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "quarantine"
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::TokenExpired),
            McpError::TokenExpired
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Timeout(Duration::from_secs(1))),
            McpError::Timeout(_)
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Cancelled),
            McpError::Cancelled
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::UnsupportedOs),
            McpError::Sandbox(SandboxError::UnsupportedOs)
        ));
        assert!(matches!(
            map_sandbox_error(SandboxError::Internal("boom".into())),
            McpError::Sandbox(SandboxError::Internal(_))
        ));
    }

    #[test]
    fn no_abs_paths_in_mcp_errors() {
        let pathful = [
            SandboxError::Invalid("canonicalize /home/op/.cargo/bin/cargo: nope".into()),
            SandboxError::Internal("/home/op/secret leaked".into()),
            SandboxError::BackendUnavailable {
                backend: SandboxBackend::Landlock,
                message: "/home/op/.rustup missing".into(),
            },
            SandboxError::BackendCannotEnforce("/home/op/jail unusable".into()),
            SandboxError::Io(std::io::Error::other("/home/op/x: EACCES")),
            SandboxError::Denied(DenialReason::PathDenied("/home/op/.env".into())),
        ];
        for err in pathful {
            let rendered = map_sandbox_error(err).to_string();
            assert!(
                !rendered.contains("/home"),
                "leaked host path: {rendered:?}"
            );
        }
    }
}
