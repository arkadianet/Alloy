//! The [`McpPlatform`] trait — Alloy's sole tool bus (RFC-0006 §3.4).
//!
//! Author: arkadianet

use alloy_runtime::{
    McpServerSpec, PermissionToken, ServerId, ToolCall, ToolName, ToolResult, ToolSelector,
    ToolView,
};
use async_trait::async_trait;

use crate::mcp::error::McpError;

/// Sole tool bus: builtin and (future) external tools share one schema model,
/// one permission path, one dispatch path, and one result model.
#[async_trait]
pub trait McpPlatform: Send + Sync {
    /// Start an out-of-process MCP server.
    ///
    /// MVP: always [`McpError::Unsupported`] — the out-of-process allowlist is
    /// empty until RFC-0013.
    async fn start_server(&self, spec: McpServerSpec) -> Result<ServerId, McpError>;

    /// Stop an out-of-process MCP server. MVP: always [`McpError::Unsupported`].
    async fn stop_server(&self, id: ServerId) -> Result<(), McpError>;

    /// Lazy disclosure: return only the tools matching `selectors`.
    ///
    /// Empty `selectors` means "disclose nothing", never "disclose the
    /// catalogue" (RFC-0006 §5.4).
    async fn tools_for(&self, selectors: &[ToolSelector]) -> Result<Vec<ToolView>, McpError>;

    /// Whether `name` is in the disclosed set for `selectors`.
    ///
    /// Default calls [`Self::tools_for`]; hosts should override to avoid
    /// cloning full [`ToolView`] schemas on the call hot path.
    async fn discloses(
        &self,
        selectors: &[ToolSelector],
        name: &ToolName,
    ) -> Result<bool, McpError> {
        Ok(self
            .tools_for(selectors)
            .await?
            .iter()
            .any(|view| &view.name == name))
    }

    /// Invoke a tool under `perms` following the RFC-0006 §5.1 pipeline.
    ///
    /// **Cancellation:** callers cancel an in-flight call by **dropping** the
    /// returned future. Drop releases the in-flight permit, drops any nested
    /// `SandboxBroker::exec` future (process-group kill), and writes no
    /// `DecisionLog` record. There is no per-call cancellation token on
    /// `ToolCall` in MVP.
    async fn call(&self, call: ToolCall, perms: PermissionToken) -> Result<ToolResult, McpError>;
}
