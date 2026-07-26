//! FIFO recording test-double for [`McpPlatform`] (RFC-0006 §3.9).
//!
//! Downstream RFCs that need real sandbox behaviour should wrap
//! [`InProcessMcpHost`](crate::mcp::InProcessMcpHost) with a
//! [`RecordingSandboxBroker`](crate::sandbox::RecordingSandboxBroker) instead.
//!
//! Author: arkadianet

use std::collections::VecDeque;
use std::sync::Mutex;

use alloy_runtime::{
    McpServerSpec, PermissionToken, ServerId, ToolCall, ToolName, ToolResult, ToolSelector,
    ToolView,
};
use async_trait::async_trait;
use serde_json::json;

use crate::mcp::builtins::BuiltinToolId;
use crate::mcp::disclose::{disclose, discloses_name};
use crate::mcp::error::McpError;
use crate::mcp::platform::McpPlatform;

/// Canned `call` outcomes plus a record of every invocation.
pub struct RecordingMcpPlatform {
    scripts: Mutex<VecDeque<Result<ToolResult, McpError>>>,
    recorded: Mutex<Vec<(ToolCall, PermissionToken)>>,
    views: Vec<ToolView>,
}

impl std::fmt::Debug for RecordingMcpPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingMcpPlatform")
            .field("views", &self.views.len())
            .finish_non_exhaustive()
    }
}

impl RecordingMcpPlatform {
    /// Empty script queue; `tools_for` discloses `views` through the §5.4 helper.
    #[must_use]
    pub fn new(views: Vec<ToolView>) -> Self {
        Self {
            scripts: Mutex::new(VecDeque::new()),
            recorded: Mutex::new(Vec::new()),
            views,
        }
    }

    /// The four builtin views with empty schemas and canonical tags.
    #[must_use]
    pub fn with_builtin_views() -> Self {
        let views = BuiltinToolId::ALL
            .iter()
            .map(|id| {
                ToolView::new(
                    id.name(),
                    "recording",
                    json!({}),
                    id.tags().iter().map(|t| (*t).to_string()).collect(),
                    true,
                )
            })
            .collect();
        Self::new(views)
    }

    /// Push a canned `call` outcome (FIFO).
    pub fn push(&self, outcome: Result<ToolResult, McpError>) {
        Self::lock(&self.scripts).push_back(outcome);
    }

    /// Every recorded call, in order.
    #[must_use]
    pub fn recorded_calls(&self) -> Vec<(ToolCall, PermissionToken)> {
        Self::lock(&self.recorded).clone()
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|poisoned| {
            tracing::error!("recording mcp platform mutex poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

#[async_trait]
impl McpPlatform for RecordingMcpPlatform {
    async fn start_server(&self, _spec: McpServerSpec) -> Result<ServerId, McpError> {
        Err(McpError::Unsupported("recording: start_server".into()))
    }

    async fn stop_server(&self, _id: ServerId) -> Result<(), McpError> {
        Err(McpError::Unsupported("recording: stop_server".into()))
    }

    async fn tools_for(&self, selectors: &[ToolSelector]) -> Result<Vec<ToolView>, McpError> {
        Ok(disclose(&self.views, selectors).0)
    }

    async fn discloses(
        &self,
        selectors: &[ToolSelector],
        name: &ToolName,
    ) -> Result<bool, McpError> {
        Ok(discloses_name(&self.views, selectors, name))
    }

    async fn call(&self, call: ToolCall, perms: PermissionToken) -> Result<ToolResult, McpError> {
        // Recorded before the script is consulted so an exhausted queue still
        // proves what the caller attempted.
        Self::lock(&self.recorded).push((call, perms));
        Self::lock(&self.scripts)
            .pop_front()
            .unwrap_or_else(|| Err(McpError::Internal("recording exhausted".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{ProfileId, RunId, ToolName};

    fn token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: Vec::new(),
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[tokio::test]
    async fn recording_platform_fifo() {
        let platform = RecordingMcpPlatform::with_builtin_views();
        let name = ToolName::new("fs_read").unwrap();
        platform.push(Ok(ToolResult::ok(name.clone(), json!({}), 1)));
        platform.push(Err(McpError::Cancelled));

        let call = ToolCall::new(name, json!({}));
        assert!(platform
            .call(call.clone(), token())
            .await
            .unwrap()
            .is_error()
            .eq(&false));
        assert!(matches!(
            platform.call(call.clone(), token()).await,
            Err(McpError::Cancelled)
        ));
        assert!(matches!(
            platform.call(call, token()).await,
            Err(McpError::Internal(ref m)) if m.contains("exhausted")
        ));
        assert_eq!(platform.recorded_calls().len(), 3);
    }

    #[tokio::test]
    async fn recording_platform_discloses_lazily() {
        let platform = RecordingMcpPlatform::with_builtin_views();
        assert!(platform.tools_for(&[]).await.unwrap().is_empty());
        let views = platform
            .tools_for(&[ToolSelector::tag("sel.edit")])
            .await
            .unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name.as_str(), "apply_patch");
        assert!(matches!(
            platform.stop_server(ServerId::new()).await,
            Err(McpError::Unsupported(_))
        ));
    }
}
