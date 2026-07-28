//! [`ToolHandleToolCaller`] — the `alloy_runtime::ToolCaller` implementation
//! over [`ToolHandle`] (RFC-0010 §3.5).
//!
//! The only file in `alloy-runtime` or `alloy-tools` allowed to name
//! `McpError` in a `match` that must stay exhaustive: [`map_mcp_error`] has
//! no catch-all arm, so a new `McpError` variant breaks the build here
//! rather than silently falling through to a generic mapping.
//!
//! Author: arkadianet

use alloy_runtime::{
    PermissionToken, ToolCall, ToolCaller, ToolCallerError, ToolResult, ToolSelector,
};
use async_trait::async_trait;

use crate::mcp::error::{McpError, PermissionDenial};
use crate::mcp::handle::ToolHandle;

/// Wraps a [`ToolHandle`] as an `alloy_runtime::ToolCaller`, mapping
/// `McpError` to `ToolCallerError` at the boundary so `alloy-runtime` never
/// has to name `McpError` (rule M5).
pub struct ToolHandleToolCaller {
    handle: ToolHandle,
}

impl ToolHandleToolCaller {
    /// Wrap an existing [`ToolHandle`].
    #[must_use]
    pub fn new(handle: ToolHandle) -> Self {
        Self { handle }
    }

    /// Selectors this caller was built with (host wiring assertions / tests).
    #[must_use]
    pub fn selectors(&self) -> &[ToolSelector] {
        self.handle.selectors()
    }
}

impl std::fmt::Debug for ToolHandleToolCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandleToolCaller")
            .field("selectors", &self.handle.selectors())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolCaller for ToolHandleToolCaller {
    async fn call(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, ToolCallerError> {
        self.handle.call(call, perms).await.map_err(map_mcp_error)
    }
}

/// Exhaustive: no catch-all arm, so a new `McpError` variant breaks the
/// build here rather than silently mapping to `Internal`.
#[must_use]
pub fn map_mcp_error(err: McpError) -> ToolCallerError {
    match err {
        McpError::UnknownTool(name) => ToolCallerError::UnknownTool(name),
        McpError::PermissionDenied(PermissionDenial::NotDisclosed) => {
            ToolCallerError::PermissionDenied("tool not disclosed for handle selectors".into())
        }
        McpError::PermissionDenied(other) => {
            // `Display` is already redacted (RFC-0006 §9.1).
            ToolCallerError::PermissionDenied(other.to_string())
        }
        McpError::TokenExpired => ToolCallerError::TokenExpired,
        McpError::InvalidToken(m) => ToolCallerError::InvalidToken(m),
        McpError::InvalidArguments(m) => ToolCallerError::InvalidArguments(m),
        McpError::Unsupported(m) => ToolCallerError::Unsupported(m),
        McpError::ShuttingDown => ToolCallerError::ShuttingDown,
        McpError::Cancelled => ToolCallerError::Cancelled,
        // TC3: Duration is dropped — the adapter reports the fact, the
        // scheduler owns deadlines (§5.19).
        McpError::Timeout(_) => ToolCallerError::Timeout,
        // Already redacted (`McpError::Sandbox` is only ever constructed via
        // the crate-private `map_sandbox_error`, which redacts paths).
        McpError::Sandbox(e) => ToolCallerError::Sandbox(e.to_string()),
        McpError::Internal(m) => ToolCallerError::Internal(m),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use alloy_runtime::{ProfileId, RunId, ToolName};
    use serde_json::json;

    use super::*;
    use crate::mcp::recording::RecordingMcpPlatform;
    use crate::sandbox::SandboxError;

    fn token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: Vec::new(),
            expires: None,
            run_id: RunId::new(),
        }
    }

    // ---- map_mcp_error: exhaustive over every McpError variant ----

    #[test]
    fn maps_unknown_tool() {
        assert!(matches!(
            map_mcp_error(McpError::UnknownTool("x".into())),
            ToolCallerError::UnknownTool(n) if n == "x"
        ));
    }

    #[test]
    fn maps_permission_denied_not_disclosed_with_fixed_message() {
        let mapped = map_mcp_error(McpError::PermissionDenied(PermissionDenial::NotDisclosed));
        assert!(matches!(
            mapped,
            ToolCallerError::PermissionDenied(ref m) if m == "tool not disclosed for handle selectors"
        ));
    }

    #[test]
    fn maps_permission_denied_other_via_display() {
        let denial = PermissionDenial::ExecNotAllowlisted;
        let expected = denial.to_string();
        let mapped = map_mcp_error(McpError::PermissionDenied(denial));
        assert!(matches!(
            mapped,
            ToolCallerError::PermissionDenied(ref m) if *m == expected
        ));
    }

    #[test]
    fn maps_token_expired() {
        assert!(matches!(
            map_mcp_error(McpError::TokenExpired),
            ToolCallerError::TokenExpired
        ));
    }

    #[test]
    fn maps_invalid_token() {
        assert!(matches!(
            map_mcp_error(McpError::InvalidToken("bad glob".into())),
            ToolCallerError::InvalidToken(m) if m == "bad glob"
        ));
    }

    #[test]
    fn maps_invalid_arguments() {
        assert!(matches!(
            map_mcp_error(McpError::InvalidArguments("bad shape".into())),
            ToolCallerError::InvalidArguments(m) if m == "bad shape"
        ));
    }

    #[test]
    fn maps_unsupported() {
        assert!(matches!(
            map_mcp_error(McpError::Unsupported("mcp server".into())),
            ToolCallerError::Unsupported(m) if m == "mcp server"
        ));
    }

    #[test]
    fn maps_shutting_down() {
        assert!(matches!(
            map_mcp_error(McpError::ShuttingDown),
            ToolCallerError::ShuttingDown
        ));
    }

    #[test]
    fn maps_cancelled() {
        assert!(matches!(
            map_mcp_error(McpError::Cancelled),
            ToolCallerError::Cancelled
        ));
    }

    #[test]
    fn maps_timeout_drops_duration() {
        assert!(matches!(
            map_mcp_error(McpError::Timeout(Duration::from_secs(30))),
            ToolCallerError::Timeout
        ));
    }

    #[test]
    fn maps_sandbox_via_display() {
        let err = SandboxError::UnsupportedOs;
        let expected = err.to_string();
        let mapped = map_mcp_error(McpError::Sandbox(err));
        assert!(matches!(mapped, ToolCallerError::Sandbox(ref m) if *m == expected));
    }

    #[test]
    fn maps_internal() {
        assert!(matches!(
            map_mcp_error(McpError::Internal("bug".into())),
            ToolCallerError::Internal(m) if m == "bug"
        ));
    }

    // ---- ToolHandleToolCaller wiring ----

    #[tokio::test]
    async fn forwards_successful_call() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let name = ToolName::new("fs_read").unwrap();
        platform.push(Ok(ToolResult::ok(name.clone(), json!({"text": "hi"}), 1)));
        let handle = ToolHandle::new(platform.clone(), vec![ToolSelector::name(name.clone())]);
        let caller = ToolHandleToolCaller::new(handle);

        let result = caller
            .call(ToolCall::new(name, json!({})), token())
            .await
            .unwrap();
        assert!(!result.is_error());
        assert_eq!(platform.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn maps_platform_error_through_map_mcp_error() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let name = ToolName::new("fs_read").unwrap();
        platform.push(Err(McpError::TokenExpired));
        let handle = ToolHandle::new(platform, vec![ToolSelector::name(name.clone())]);
        let caller = ToolHandleToolCaller::new(handle);

        let err = caller
            .call(ToolCall::new(name, json!({})), token())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallerError::TokenExpired));
    }

    #[tokio::test]
    async fn not_disclosed_never_reaches_platform() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let name = ToolName::new("cargo_check").unwrap();
        // Selector only discloses fs_read; cargo_check is not disclosed.
        let handle = ToolHandle::new(
            platform.clone(),
            vec![ToolSelector::name(ToolName::new("fs_read").unwrap())],
        );
        let caller = ToolHandleToolCaller::new(handle);

        let err = caller
            .call(ToolCall::new(name, json!({})), token())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolCallerError::PermissionDenied(ref m) if m == "tool not disclosed for handle selectors"
        ));
        assert!(platform.recorded_calls().is_empty());
    }

    #[test]
    fn selectors_are_exposed() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let selectors = vec![ToolSelector::tag("sel.fs")];
        let handle = ToolHandle::new(platform, selectors.clone());
        let caller = ToolHandleToolCaller::new(handle);
        assert_eq!(caller.selectors(), selectors.as_slice());
    }
}
