//! Host-owned permission minting for verify adapters (RFC-0010 §3.7).
//!
//! Adapters MUST NOT invent grants; this is the sole seam that mints a
//! [`PermissionToken`] for `cargo_check`/`cargo_test`.

use std::sync::Arc;

use async_trait::async_trait;

use super::NodeExecRef;
use crate::error::AdapterError;
use crate::storage::SessionRows;
use crate::types::permission::{ExecAllow, Grant, PermissionToken};

/// Which verify tool a permission token is being minted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyClass {
    /// `cargo_check` under `ExecClass::Check`.
    Compile,
    /// `cargo_test` under `ExecClass::Test`.
    Test,
}

/// Host-owned permission minting for verify adapters.
#[async_trait]
pub trait VerifyPermissions: Send + Sync {
    /// Mint a token authorizing `class`'s tool call for the executing node.
    ///
    /// Async because `SessionRows::get_session` is async on `main`. MUST
    /// return [`AdapterError::PermissionDenied`] (not `Internal`) when the
    /// profile catalog has no exec grant for `class`, so a mis-provisioned
    /// profile is reported as a denial rather than a crash.
    async fn token_for(
        &self,
        ctx: &NodeExecRef,
        class: VerifyClass,
    ) -> Result<PermissionToken, AdapterError>;
}

/// Day-1 production [`VerifyPermissions`]: resolves `Session.profile` and
/// mints a `cargo` exec grant from a per-class argv glob configured by host
/// assembly.
///
/// A glob-only struct without session access is **not** sufficient — the
/// minted token's `profile` field MUST come from the session row.
pub struct SessionVerifyPermissions {
    sessions: Arc<dyn SessionRows>,
    compile_args_glob: Option<String>,
    test_args_glob: Option<String>,
}

impl std::fmt::Debug for SessionVerifyPermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionVerifyPermissions")
            .field("compile_args_glob", &self.compile_args_glob)
            .field("test_args_glob", &self.test_args_glob)
            .finish_non_exhaustive()
    }
}

impl SessionVerifyPermissions {
    /// Construct with per-class `cargo` argv globs. Host assembly owns the
    /// glob strings; `None` means "no grant configured for this class",
    /// which `token_for` reports as `PermissionDenied`, not a crash.
    #[must_use]
    pub fn new(
        sessions: Arc<dyn SessionRows>,
        compile_args_glob: Option<String>,
        test_args_glob: Option<String>,
    ) -> Self {
        Self {
            sessions,
            compile_args_glob,
            test_args_glob,
        }
    }
}

#[async_trait]
impl VerifyPermissions for SessionVerifyPermissions {
    async fn token_for(
        &self,
        ctx: &NodeExecRef,
        class: VerifyClass,
    ) -> Result<PermissionToken, AdapterError> {
        let session = self
            .sessions
            .get_session(ctx.session_id)
            .await
            .map_err(|e| AdapterError::Internal(format!("session lookup failed: {e}")))?
            .ok_or_else(|| {
                AdapterError::PermissionDenied(format!("session {} missing", ctx.session_id))
            })?;

        let glob = match class {
            VerifyClass::Compile => &self.compile_args_glob,
            VerifyClass::Test => &self.test_args_glob,
        };
        let Some(glob) = glob else {
            return Err(AdapterError::PermissionDenied(format!(
                "no exec grant configured for {class:?}"
            )));
        };

        Ok(PermissionToken {
            profile: session.profile,
            grants: vec![Grant::Exec(ExecAllow {
                binary: "cargo".into(),
                args_glob: Some(glob.clone()),
            })],
            expires: None, // MVP: no session-level expiry policy yet.
            run_id: ctx.run_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::budget::BudgetPolicy;
    use crate::types::ids::{DagId, NodeId, ProfileId, RunId, SessionId, Timestamp};

    async fn open_store() -> (tempfile::TempDir, AlloyStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        (dir, storage)
    }

    fn exec_ref(session_id: SessionId, run_id: RunId) -> NodeExecRef {
        NodeExecRef {
            session_id,
            run_id,
            dag_id: DagId::new(),
            node_id: NodeId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            attempt: 1,
        }
    }

    #[tokio::test]
    async fn mints_token_from_session_profile_and_class_glob() {
        let (_dir, storage) = open_store().await;
        let session = Session {
            id: SessionId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            profile: ProfileId::new("ci").unwrap(),
            budget: BudgetPolicy::default(),
            language_backends: vec![],
            created_at: Timestamp::now(),
        };
        storage.sessions().upsert_session(&session).await.unwrap();

        let perms = SessionVerifyPermissions::new(
            storage.sessions(),
            Some("check *".into()),
            Some("test *".into()),
        );
        let run_id = RunId::new();
        let token = perms
            .token_for(&exec_ref(session.id, run_id), VerifyClass::Compile)
            .await
            .unwrap();
        assert_eq!(token.profile, session.profile);
        assert_eq!(token.run_id, run_id);
        assert_eq!(token.expires, None);
        assert_eq!(
            token.grants,
            vec![Grant::Exec(ExecAllow {
                binary: "cargo".into(),
                args_glob: Some("check *".into()),
            })]
        );
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn different_class_uses_its_own_glob() {
        let (_dir, storage) = open_store().await;
        let session = Session {
            id: SessionId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            profile: ProfileId::new("ci").unwrap(),
            budget: BudgetPolicy::default(),
            language_backends: vec![],
            created_at: Timestamp::now(),
        };
        storage.sessions().upsert_session(&session).await.unwrap();

        let perms = SessionVerifyPermissions::new(
            storage.sessions(),
            Some("check *".into()),
            Some("test *".into()),
        );
        let token = perms
            .token_for(&exec_ref(session.id, RunId::new()), VerifyClass::Test)
            .await
            .unwrap();
        assert_eq!(
            token.grants,
            vec![Grant::Exec(ExecAllow {
                binary: "cargo".into(),
                args_glob: Some("test *".into()),
            })]
        );
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn missing_session_is_permission_denied_not_internal() {
        let (_dir, storage) = open_store().await;
        let perms = SessionVerifyPermissions::new(storage.sessions(), Some("check *".into()), None);
        let err = perms
            .token_for(
                &exec_ref(SessionId::new(), RunId::new()),
                VerifyClass::Compile,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::PermissionDenied(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn missing_glob_for_class_is_permission_denied() {
        let (_dir, storage) = open_store().await;
        let session = Session {
            id: SessionId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            profile: ProfileId::new("ci").unwrap(),
            budget: BudgetPolicy::default(),
            language_backends: vec![],
            created_at: Timestamp::now(),
        };
        storage.sessions().upsert_session(&session).await.unwrap();

        // No test glob configured.
        let perms = SessionVerifyPermissions::new(storage.sessions(), Some("check *".into()), None);
        let err = perms
            .token_for(&exec_ref(session.id, RunId::new()), VerifyClass::Test)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::PermissionDenied(m) if m.contains("Test")));
        storage.close().await.unwrap();
    }
}
