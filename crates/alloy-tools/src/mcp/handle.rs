//! [`ToolHandle`] — capability-facing wrapper over a platform (RFC-0006 §3.8).
//!
//! Author: arkadianet

use std::sync::Arc;

use alloy_runtime::{PermissionToken, ToolCall, ToolResult, ToolSelector, ToolView};

use crate::mcp::error::{McpError, PermissionDenial};
use crate::mcp::platform::McpPlatform;

/// A platform plus the selectors a capability was granted.
///
/// Calls to tools outside the disclosed set are refused before the platform is
/// touched. The registry is immutable after host construction, so there is no
/// stale-cache window.
#[derive(Clone)]
pub struct ToolHandle {
    platform: Arc<dyn McpPlatform>,
    selectors: Vec<ToolSelector>,
}

impl std::fmt::Debug for ToolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandle")
            .field("selectors", &self.selectors)
            .finish_non_exhaustive()
    }
}

impl ToolHandle {
    /// Bind a platform to a fixed selector set.
    #[must_use]
    pub fn new(platform: Arc<dyn McpPlatform>, selectors: Vec<ToolSelector>) -> Self {
        Self {
            platform,
            selectors,
        }
    }

    /// Tools disclosed for these selectors.
    ///
    /// # Errors
    ///
    /// Propagates the platform error (e.g. [`McpError::ShuttingDown`]).
    pub async fn tools(&self) -> Result<Vec<ToolView>, McpError> {
        self.platform.tools_for(&self.selectors).await
    }

    /// Invoke a disclosed tool. Cancellation is by dropping the future.
    ///
    /// # Errors
    ///
    /// [`PermissionDenial::NotDisclosed`] when `call.name` is outside the
    /// disclosed set; otherwise the platform error.
    pub async fn call(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, McpError> {
        let disclosed = self.platform.tools_for(&self.selectors).await?;
        if !disclosed.iter().any(|view| view.name == call.name) {
            return Err(McpError::PermissionDenied(PermissionDenial::NotDisclosed));
        }
        self.platform.call(call, perms).await
    }

    /// Selectors this handle was built with.
    #[must_use]
    pub fn selectors(&self) -> &[ToolSelector] {
        &self.selectors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::recording::RecordingMcpPlatform;
    use alloy_runtime::{ProfileId, RunId, ToolName};
    use serde_json::json;

    fn token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: Vec::new(),
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[tokio::test]
    async fn tool_handle_not_disclosed() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let handle = ToolHandle::new(platform.clone(), vec![ToolSelector::tag("sel.fs")]);
        assert_eq!(handle.selectors().len(), 1);
        assert_eq!(handle.tools().await.unwrap().len(), 1);

        let call = ToolCall::new(ToolName::new("cargo_check").unwrap(), json!({}));
        let err = handle.call(call, token()).await.unwrap_err();
        assert!(matches!(
            err,
            McpError::PermissionDenied(PermissionDenial::NotDisclosed)
        ));
        // Fail closed: the platform never saw the call.
        assert!(platform.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn tool_handle_forwards_disclosed_call() {
        let platform = Arc::new(RecordingMcpPlatform::with_builtin_views());
        let name = ToolName::new("fs_read").unwrap();
        platform.push(Ok(alloy_runtime::ToolResult::ok(
            name.clone(),
            json!({ "text": "hi" }),
            1,
        )));
        let handle = ToolHandle::new(platform.clone(), vec![ToolSelector::name(name.clone())]);
        let result = handle
            .call(ToolCall::new(name, json!({})), token())
            .await
            .unwrap();
        assert!(!result.is_error());
        assert_eq!(platform.recorded_calls().len(), 1);
    }
}
