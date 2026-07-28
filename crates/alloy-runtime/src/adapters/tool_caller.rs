//! `ToolCaller` — the only tool seam runtime adapters may use (RFC-0010 §3.4).
//!
//! Implemented in `alloy-tools` (`ToolHandleToolCaller`, over `ToolHandle`)
//! and by recording doubles in tests. `alloy-runtime` MUST NOT depend on
//! `alloy-tools` (rule B1) and MUST NOT name `ToolHandle`, `McpError`, or
//! `McpPlatform` anywhere (rule M5) — this trait is the seam that keeps the
//! dependency edge `alloy-tools -> alloy-runtime` one-directional.

use async_trait::async_trait;

use crate::types::permission::PermissionToken;
use crate::types::tools::{ToolCall, ToolResult};

/// The only tool seam runtime adapters may use.
///
/// Cancellation is by dropping the returned future (RFC-0006 §3.8).
#[async_trait]
pub trait ToolCaller: Send + Sync {
    /// Invoke a tool under the given permission token.
    async fn call(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, ToolCallerError>;
}

/// Host-boundary failure, mirroring `McpError` without naming it.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum ToolCallerError {
    /// Name is not in the tool registry.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Fail-closed authorization refusal.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// `PermissionToken.expires` reached.
    #[error("permission token expired")]
    TokenExpired,
    /// Token carries an uncompilable grant glob or violates a defensive
    /// invariant — distinguishable from a policy denial (TC1).
    #[error("invalid permission token: {0}")]
    InvalidToken(String),
    /// Arguments failed validation.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// Requested capability is not part of the MVP (out-of-process servers).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Host is draining or stopped.
    #[error("host shutting down")]
    ShuttingDown,
    /// Cancelled via the host's cancel token.
    #[error("cancelled")]
    Cancelled,
    /// Host deadline elapsed. Never carries a `Duration` (TC3): the adapter
    /// reports the fact, the scheduler owns deadlines (§5.19).
    #[error("timeout")]
    Timeout,
    /// Sandbox/broker failure. Already redacted (TC2: no `SandboxError` or
    /// `PermissionDenial` embedded, both collapse into a string upstream).
    #[error("sandbox: {0}")]
    Sandbox(String),
    /// Host invariant violation or construction failure.
    #[error("internal: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::ProfileId;
    use crate::types::ids::RunId;

    /// Minimal in-memory double, exercised as a behavioral compile/run check
    /// that the trait shape is usable from outside this module (this crate's
    /// convention for pure type/trait scaffolding — see CLAUDE.md process
    /// notes: a compile-and-behave check counts as the test for scaffolding).
    struct EchoCaller;

    #[async_trait]
    impl ToolCaller for EchoCaller {
        async fn call(
            &self,
            call: ToolCall,
            _perms: PermissionToken,
        ) -> Result<ToolResult, ToolCallerError> {
            Ok(ToolResult::ok(call.name, call.arguments, 0))
        }
    }

    fn token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![],
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[tokio::test]
    async fn tool_caller_trait_object_is_callable() {
        let caller: std::sync::Arc<dyn ToolCaller> = std::sync::Arc::new(EchoCaller);
        let call = ToolCall::new(
            crate::types::tools::ToolName::new("cargo_check").unwrap(),
            serde_json::json!({"a": 1}),
        );
        let result = caller.call(call, token()).await.unwrap();
        assert!(!result.is_error());
        assert_eq!(result.content, serde_json::json!({"a": 1}));
    }

    #[test]
    fn tool_caller_error_display_strings_are_stable() {
        assert_eq!(
            ToolCallerError::UnknownTool("x".into()).to_string(),
            "unknown tool: x"
        );
        assert_eq!(
            ToolCallerError::PermissionDenied("nope".into()).to_string(),
            "permission denied: nope"
        );
        assert_eq!(
            ToolCallerError::TokenExpired.to_string(),
            "permission token expired"
        );
        assert_eq!(
            ToolCallerError::InvalidToken("bad glob".into()).to_string(),
            "invalid permission token: bad glob"
        );
        assert_eq!(
            ToolCallerError::InvalidArguments("bad args".into()).to_string(),
            "invalid arguments: bad args"
        );
        assert_eq!(
            ToolCallerError::Unsupported("mcp".into()).to_string(),
            "unsupported: mcp"
        );
        assert_eq!(
            ToolCallerError::ShuttingDown.to_string(),
            "host shutting down"
        );
        assert_eq!(ToolCallerError::Cancelled.to_string(), "cancelled");
        assert_eq!(ToolCallerError::Timeout.to_string(), "timeout");
        assert_eq!(
            ToolCallerError::Sandbox("redacted".into()).to_string(),
            "sandbox: redacted"
        );
        assert_eq!(
            ToolCallerError::Internal("bug".into()).to_string(),
            "internal: bug"
        );
    }
}
