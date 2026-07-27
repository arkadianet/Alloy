//! MCP `apply_patch` adapter for EditEngine (RFC-0008 §8.3).
//!
//! Author: arkadianet

use std::sync::Arc;

use alloy_runtime::{EditContext, EditEngine, EditError, PermissionToken, RunId, SessionId};
use async_trait::async_trait;

use crate::edit::patch_parse::decode_patch_value;
use crate::mcp::{
    ApplyPatchArgs, ApplyPatchOutcome, PatchApplyBackend, PatchApplyError, PermissionDenial,
};

/// MCP adapter: [`PatchApplyBackend`] backed by an [`EditEngine`].
pub struct EditEnginePatchBackend {
    engine: Arc<dyn EditEngine>,
}

impl EditEnginePatchBackend {
    /// Construct an adapter around an edit engine.
    #[must_use]
    pub fn new(engine: Arc<dyn EditEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl PatchApplyBackend for EditEnginePatchBackend {
    async fn apply(
        &self,
        args: ApplyPatchArgs,
        perms: &PermissionToken,
        session: Option<SessionId>,
        run: Option<RunId>,
    ) -> Result<ApplyPatchOutcome, PatchApplyError> {
        let req = decode_patch_value(&args.patch).map_err(map_edit_error)?;
        // RFC-0008 §3.8.5: the call's run wins, otherwise attribute to the run
        // the token was minted for — never leave the edit unattributed.
        let ctx = EditContext {
            session_id: session,
            run_id: Some(run.unwrap_or(perms.run_id)),
            perms: perms.clone(),
        };
        if args.dry_run {
            let validation = self
                .engine
                .validate(req, &ctx)
                .await
                .map_err(map_edit_error)?;
            return Ok(ApplyPatchOutcome {
                dry_run: true,
                files_touched: validation.files_touched.clone(),
                transaction_id: None,
                message: format!("dry_run ok: {} file(s)", validation.files_touched.len()),
            });
        }
        let tx = self.engine.apply(req, &ctx).await.map_err(map_edit_error)?;
        Ok(ApplyPatchOutcome {
            dry_run: false,
            files_touched: tx.files_touched.clone(),
            transaction_id: Some(tx.id),
            message: format!("applied {} file(s)", tx.files_touched.len()),
        })
    }
}

fn map_edit_error(err: EditError) -> PatchApplyError {
    match err {
        EditError::UnsupportedOp { op } => PatchApplyError::Unsupported(op),
        EditError::InvalidRequest(msg) | EditError::InvalidPatch(msg) => {
            PatchApplyError::InvalidPatch(msg)
        }
        EditError::EmptyPatch => PatchApplyError::InvalidPatch("empty patch".into()),
        EditError::PathDenied { .. } => PatchApplyError::PermissionDenied(
            PermissionDenial::PathNotCovered("path denied".into()),
        ),
        EditError::PathNotCovered { path } => {
            PatchApplyError::PermissionDenied(PermissionDenial::PathNotCovered(path))
        }
        EditError::MissingGrant(grant) => {
            PatchApplyError::PermissionDenied(PermissionDenial::MissingGrant(grant))
        }
        EditError::Conflict(msg) => PatchApplyError::Conflict(msg),
        EditError::ContextMismatch { path, detail } => {
            PatchApplyError::Conflict(format!("{path}: {detail}"))
        }
        EditError::OverlappingHunks { path } => {
            PatchApplyError::InvalidPatch(format!("overlapping hunks: {path}"))
        }
        EditError::UntrackedPath { path } => {
            PatchApplyError::Conflict(format!("untracked path in patch: {path}"))
        }
        EditError::TrackedDeniedPath { path } => {
            PatchApplyError::PermissionDenied(PermissionDenial::PathNotCovered(path))
        }
        EditError::CheckpointFailed(msg)
        | EditError::Git(msg)
        | EditError::Io(msg)
        | EditError::Storage(msg) => PatchApplyError::Io(msg),
        EditError::Environment(msg) => PatchApplyError::Unsupported(msg),
        EditError::DigestLimitExceeded(msg) => PatchApplyError::InvalidPatch(msg),
        EditError::RollbackFailed {
            tx,
            checkpoint_id,
            detail,
        } => PatchApplyError::Internal(format!(
            "rollback failed: tx={tx} checkpoint={checkpoint_id}: {detail}"
        )),
        EditError::UnknownTransaction(tx) => {
            PatchApplyError::InvalidPatch(format!("unknown transaction: {tx}"))
        }
        EditError::RollbackNotEligible { tx, state, reason } => PatchApplyError::InvalidPatch(
            format!("transaction not eligible for rollback: {tx}: state={state:?}: {reason}"),
        ),
        EditError::WorkspaceDrifted(tx) => {
            PatchApplyError::Conflict(format!("workspace drifted since transaction: {tx}"))
        }
        EditError::Event(msg) | EditError::Internal(msg) => PatchApplyError::Internal(msg),
        EditError::TokenExpired => PatchApplyError::TokenExpired,
        EditError::Cancelled => PatchApplyError::Io("cancelled".into()),
        EditError::Busy => PatchApplyError::Conflict("edit busy".into()),
        _ => PatchApplyError::Internal("unmapped edit error".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{EditRequest, EditValidation};

    #[test]
    fn maps_permission_and_token_errors() {
        assert!(matches!(
            map_edit_error(EditError::MissingGrant("git_write".into())),
            PatchApplyError::PermissionDenied(PermissionDenial::MissingGrant(ref g)) if g == "git_write"
        ));
        assert!(matches!(
            map_edit_error(EditError::PathDenied {
                path: "/abs".into(),
                reason: "x".into()
            }),
            PatchApplyError::PermissionDenied(PermissionDenial::PathNotCovered(ref p)) if p == "path denied"
        ));
        assert!(matches!(
            map_edit_error(EditError::TokenExpired),
            PatchApplyError::TokenExpired
        ));
    }

    #[test]
    fn maps_conflict_and_invalid_patch() {
        assert!(matches!(
            map_edit_error(EditError::EmptyPatch),
            PatchApplyError::InvalidPatch(ref m) if m == "empty patch"
        ));
        assert!(matches!(
            map_edit_error(EditError::ContextMismatch {
                path: "a.rs".into(),
                detail: "context".into()
            }),
            PatchApplyError::Conflict(ref m) if m.contains("a.rs")
        ));
    }

    struct ValidateOnly;

    #[async_trait]
    impl EditEngine for ValidateOnly {
        async fn validate(
            &self,
            _req: EditRequest,
            _ctx: &EditContext,
        ) -> Result<EditValidation, EditError> {
            Ok(EditValidation {
                files_touched: vec!["a.txt".into()],
            })
        }

        async fn apply(
            &self,
            _req: EditRequest,
            _ctx: &EditContext,
        ) -> Result<alloy_runtime::EditTransaction, EditError> {
            Err(EditError::Internal("should not apply".into()))
        }

        async fn rollback(
            &self,
            _tx: alloy_runtime::TransactionId,
            _ctx: &EditContext,
        ) -> Result<(), EditError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dry_run_uses_validate() {
        let backend = EditEnginePatchBackend::new(Arc::new(ValidateOnly));
        let perms = PermissionToken {
            profile: alloy_runtime::ProfileId::new("default").unwrap(),
            grants: vec![],
            expires: None,
            run_id: RunId::new(),
        };
        let outcome = backend
            .apply(
                ApplyPatchArgs {
                    patch: serde_json::json!({"files":[{"action":"delete","path":"a.txt"}]}),
                    dry_run: true,
                },
                &perms,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.transaction_id, None);
        assert_eq!(outcome.message, "dry_run ok: 1 file(s)");
    }
}
