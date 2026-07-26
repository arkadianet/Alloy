//! `apply_patch` injection seam and the deterministic MVP stub (RFC-0006 §3.7).
//!
//! RFC-0008 replaces [`StubPatchApplyBackend`] by injecting a different
//! `Arc<dyn PatchApplyBackend>`; the host needs no code change.
//!
//! Author: arkadianet

use alloy_runtime::TransactionId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// `apply_patch` arguments (schema: RFC-0006 §5.3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchArgs {
    /// Unified diff string or opaque TextPatch object. Not interpreted here.
    pub patch: serde_json::Value,
    /// Validate only; never mutate the workspace.
    #[serde(default)]
    pub dry_run: bool,
}

/// Backend result for a patch application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchOutcome {
    /// Echo of the requested mode.
    pub dry_run: bool,
    /// Jail-relative paths only (`/`-separated, no leading `/`, no `..`).
    /// The host re-validates before forwarding (RFC-0006 §5.9).
    pub files_touched: Vec<String>,
    /// Checkpoint / transaction id when the backend created one.
    pub transaction_id: Option<TransactionId>,
    /// Operator- and model-safe summary. Never raw patch bodies or absolute paths.
    pub message: String,
}

/// Exact stub refusal message (RFC-0006 §3.7.1) — the host matches it verbatim.
pub(crate) const EDIT_ENGINE_UNWIRED_MESSAGE: &str =
    "edit_engine_unwired: apply_patch requires RFC-0008 EditEngine";

/// Stable code for the stub refusal.
pub(crate) const EDIT_ENGINE_UNWIRED_CODE: &str = "edit_engine_unwired";

/// Applies patches on behalf of the `apply_patch` builtin.
///
/// The host enforces permissions **before** calling this trait and sanitizes
/// every success and error mapping afterwards (RFC-0006 §5.9).
#[async_trait]
pub trait PatchApplyBackend: Send + Sync {
    /// Apply (or dry-run) the patch.
    async fn apply(&self, args: ApplyPatchArgs) -> Result<ApplyPatchOutcome, PatchApplyError>;
}

/// Backend failure taxonomy mapped to `ToolError` by the host (RFC-0006 §8.4).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PatchApplyError {
    /// Capability not wired (MVP stub, or a backend refusing a patch dialect).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Patch body failed backend validation.
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
    /// Patch did not apply cleanly against the working tree.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(String),
    /// Backend invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

/// MVP backend: refuses every patch deterministically until RFC-0008 lands.
///
/// No files are read or written, no git operations occur, and no sandbox exec
/// happens — including for `dry_run: true` and empty patches.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubPatchApplyBackend;

#[async_trait]
impl PatchApplyBackend for StubPatchApplyBackend {
    async fn apply(&self, args: ApplyPatchArgs) -> Result<ApplyPatchOutcome, PatchApplyError> {
        let _ = args;
        Err(PatchApplyError::Unsupported(
            EDIT_ENGINE_UNWIRED_MESSAGE.into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn stub_refuses_every_input() {
        for args in [
            ApplyPatchArgs {
                patch: json!(""),
                dry_run: false,
            },
            ApplyPatchArgs {
                patch: json!({}),
                dry_run: true,
            },
        ] {
            let err = StubPatchApplyBackend.apply(args).await.unwrap_err();
            assert!(
                matches!(err, PatchApplyError::Unsupported(ref m) if m == EDIT_ENGINE_UNWIRED_MESSAGE)
            );
        }
    }
}
