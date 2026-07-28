//! Runtime node adapters (Verify*/GateHuman) — stubs until RFC-0010/0006.

mod capability;
mod diagnostics;
mod gate;
mod perms;
mod tool_caller;
mod verify;

pub use capability::{
    CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityOutcome,
    UnavailableCapabilityExecutor,
};
pub use diagnostics::{diagnostic_fingerprint, parse_rustc_diagnostics};
pub use gate::SessionGateHumanAdapter;
pub use perms::{SessionVerifyPermissions, VerifyClass, VerifyPermissions};
pub use tool_caller::{ToolCaller, ToolCallerError};
pub use verify::{McpVerifyCompileAdapter, McpVerifyTestAdapter};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::AdapterError;
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{ArtifactId, DagId, GateId, NodeId, RunId, SessionId};

/// Compile verification adapter.
#[async_trait]
pub trait VerifyCompileAdapter: Send + Sync {
    /// Run compile/check for a node.
    async fn check(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError>;
}

/// Test verification adapter.
#[async_trait]
pub trait VerifyTestAdapter: Send + Sync {
    /// Run tests for a node.
    async fn test(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError>;
}

/// Human gate adapter.
#[async_trait]
pub trait GateHumanAdapter: Send + Sync {
    /// Wait for approval (RunController::approve resumes).
    async fn wait_approval(
        &self,
        ctx: &NodeExecContext,
        gate: GateId,
    ) -> Result<Approval, AdapterError>;
}

/// Serde-safe node execution identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeExecRef {
    /// Session id.
    pub session_id: SessionId,
    /// Run id.
    pub run_id: RunId,
    /// DAG id.
    pub dag_id: DagId,
    /// Node id.
    pub node_id: NodeId,
    /// Workspace root.
    pub workspace_root: std::path::PathBuf,
    /// 1-based attempt index for this dispatch (RFC-0010 §3.1.1 NX1).
    ///
    /// MUST be `>= 1` whenever a node is dispatched (checkpoint C3). Gate
    /// **wait** contexts (unresolved, no C3 yet) use `0`.
    pub attempt: u32,
}

/// Runtime execution context (not serde; holds cancellation).
#[derive(Debug, Clone)]
pub struct NodeExecContext {
    /// Persistable identity.
    pub meta: NodeExecRef,
    /// Cancellation token.
    pub cancellation: CancellationToken,
}

/// Verify adapter outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyOutcome {
    /// Whether verification succeeded.
    pub ok: bool,
    /// Diagnostics produced.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Optional raw log artifact.
    pub raw_artifact: Option<ArtifactId>,
}

/// Human approval decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Allow ongoing.
    Allow,
    /// Deny.
    Deny,
    /// Allow once.
    AllowOnce,
}

/// Unavailable compile adapter stub.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVerifyCompile;

/// Unavailable test adapter stub.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVerifyTest;

/// Unavailable gate adapter stub.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableGateHuman;

#[async_trait]
impl VerifyCompileAdapter for UnavailableVerifyCompile {
    async fn check(&self, _ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}

#[async_trait]
impl VerifyTestAdapter for UnavailableVerifyTest {
    async fn test(&self, _ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}

#[async_trait]
impl GateHumanAdapter for UnavailableGateHuman {
    async fn wait_approval(
        &self,
        _ctx: &NodeExecContext,
        _gate: GateId,
    ) -> Result<Approval, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}
