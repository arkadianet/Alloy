//! Host-owned permission minting for capability workers (RFC-0013 §11).
//!
//! Direct analogue of the RFC-0010 adapter permission seam. Workers never
//! construct grants or tokens themselves (PM1/PM2); this module is the only
//! place under `capabilities/**` allowed to build a token.
//!
//! **Documented deviation from PM3's letter:** the merged RFC-0008 stack
//! requires more than `FsWrite` on the token a mutating `apply_patch`
//! carries: the §3.8.4 host authorization demands `GitWrite`, and the
//! `GitEditEngine` checkpoint executes `git` through the sandbox **with the
//! caller's token**, demanding `Exec(git)`. PM3's premise ("checkpoint
//! creation is the patch backend's own business") does not hold against the
//! merged implementation, so the `Patch` class mints exactly
//! `FsWrite(glob) + GitWrite + Exec(git)` — never `Network`, never any
//! binary other than `git`, and only from this file (CI grep T11 pins both
//! `Grant::Exec` and `GitWrite` here). The worked trace this enables is the
//! RFC's own Appendix A step 17.

use std::sync::Arc;

use async_trait::async_trait;

use crate::adapters::NodeExecRef;
use crate::error::AdapterError;
use crate::storage::SessionRows;
use crate::types::permission::{Glob, Grant, PermissionToken};

/// Which worker tool a token is being minted for (§11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerToolClass {
    /// `fs_read` under the workspace jail.
    Read,
    /// `apply_patch` write grant.
    Patch,
}

/// Host-owned permission minting for capability workers (PM1).
#[async_trait]
pub trait WorkerPermissions: Send + Sync {
    /// Mint a token authorizing `class`'s tool call for the executing node.
    ///
    /// MUST return [`AdapterError::PermissionDenied`] (not `Internal`) when
    /// the session row is missing or no glob is configured for `class`
    /// (PM4), so a mis-provisioned profile reads as a denial, not a crash.
    async fn token_for(
        &self,
        ctx: &NodeExecRef,
        class: WorkerToolClass,
    ) -> Result<PermissionToken, AdapterError>;
}

/// Day-1 production [`WorkerPermissions`]: resolves `Session.profile` and
/// mints workspace-scoped globs configured by host assembly (RFC-0015 owns
/// the glob strings, Appendix C item 4).
pub struct SessionWorkerPermissions {
    sessions: Arc<dyn SessionRows>,
    read_glob: Option<String>,
    write_glob: Option<String>,
}

impl std::fmt::Debug for SessionWorkerPermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWorkerPermissions")
            .field("read_glob", &self.read_glob)
            .field("write_glob", &self.write_glob)
            .finish_non_exhaustive()
    }
}

impl SessionWorkerPermissions {
    /// Construct with per-class path globs. `None` means "no grant
    /// configured for this class", which `token_for` reports as
    /// `PermissionDenied` (PM4).
    #[must_use]
    pub fn new(
        sessions: Arc<dyn SessionRows>,
        read_glob: Option<String>,
        write_glob: Option<String>,
    ) -> Self {
        Self {
            sessions,
            read_glob,
            write_glob,
        }
    }
}

#[async_trait]
impl WorkerPermissions for SessionWorkerPermissions {
    /// Tokens are minted per call, never cached (PM5/CW3).
    async fn token_for(
        &self,
        ctx: &NodeExecRef,
        class: WorkerToolClass,
    ) -> Result<PermissionToken, AdapterError> {
        let session = self
            .sessions
            .get_session(ctx.session_id)
            .await
            .map_err(|e| AdapterError::Internal(format!("session lookup failed: {e}")))?
            .ok_or_else(|| {
                AdapterError::PermissionDenied(format!("session {} missing", ctx.session_id))
            })?;

        let grants = match class {
            WorkerToolClass::Read => {
                let Some(glob) = &self.read_glob else {
                    return Err(AdapterError::PermissionDenied(
                        "no read glob configured for workers".into(),
                    ));
                };
                vec![Grant::FsRead(Glob(glob.clone()))]
            }
            WorkerToolClass::Patch => {
                let Some(glob) = &self.write_glob else {
                    return Err(AdapterError::PermissionDenied(
                        "no write glob configured for workers".into(),
                    ));
                };
                // GitWrite + Exec(git): required by the merged RFC-0008 host
                // check and git-backed checkpoint respectively (see module
                // docs for the PM3 deviation record). Never Network, never a
                // binary other than git (SEC8 intent preserved).
                vec![
                    Grant::FsWrite(Glob(glob.clone())),
                    Grant::GitWrite,
                    Grant::Exec(crate::types::permission::ExecAllow {
                        binary: "git".into(),
                        args_glob: None,
                    }),
                ]
            }
        };

        Ok(PermissionToken {
            profile: session.profile,
            grants,
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

    async fn seed_session(storage: &AlloyStorage) -> SessionId {
        let session = Session {
            id: SessionId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            profile: ProfileId::new("ci").unwrap(),
            budget: BudgetPolicy::default(),
            language_backends: vec![],
            created_at: Timestamp::now(),
        };
        storage.sessions().upsert_session(&session).await.unwrap();
        session.id
    }

    #[tokio::test]
    async fn session_worker_permissions_mints_only_fs_grants() {
        // PM3/SEC8 as adjusted for the merged RFC-0008 stack: Read mints
        // exactly one FsRead; Patch mints FsWrite + GitWrite + Exec(git);
        // Network never appears and no non-git binary is ever granted.
        let (_dir, storage) = open_store().await;
        let sid = seed_session(&storage).await;
        let perms =
            SessionWorkerPermissions::new(storage.sessions(), Some("**".into()), Some("**".into()));
        let run = RunId::new();

        let read = perms
            .token_for(&exec_ref(sid, run), WorkerToolClass::Read)
            .await
            .unwrap();
        assert_eq!(read.grants, vec![Grant::FsRead(Glob("**".into()))]);
        assert_eq!(read.run_id, run);

        let patch = perms
            .token_for(&exec_ref(sid, run), WorkerToolClass::Patch)
            .await
            .unwrap();
        assert_eq!(
            patch.grants,
            vec![
                Grant::FsWrite(Glob("**".into())),
                Grant::GitWrite,
                Grant::Exec(crate::types::permission::ExecAllow {
                    binary: "git".into(),
                    args_glob: None,
                }),
            ]
        );
        // SEC8 as a positive allowlist: nothing beyond fs grants and the
        // patch path's git checkpoint grants is ever minted — in particular
        // no network grant and no non-git binary.
        for token in [&read, &patch] {
            assert!(
                token.grants.iter().all(|g| match g {
                    Grant::FsRead(_) | Grant::FsWrite(_) | Grant::GitWrite => true,
                    Grant::Exec(allow) => allow.binary == "git",
                    _ => false,
                }),
                "workers must only receive fs/patch grants: {:?}",
                token.grants
            );
        }
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn session_worker_permissions_missing_session_is_permission_denied() {
        // PM4.
        let (_dir, storage) = open_store().await;
        let perms = SessionWorkerPermissions::new(storage.sessions(), Some("**".into()), None);
        let err = perms
            .token_for(
                &exec_ref(SessionId::new(), RunId::new()),
                WorkerToolClass::Read,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::PermissionDenied(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn session_worker_permissions_missing_glob_is_permission_denied() {
        // PM4: an unconfigured glob for the class is a denial, not Internal.
        let (_dir, storage) = open_store().await;
        let sid = seed_session(&storage).await;
        let perms = SessionWorkerPermissions::new(storage.sessions(), Some("**".into()), None);
        let err = perms
            .token_for(&exec_ref(sid, RunId::new()), WorkerToolClass::Patch)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::PermissionDenied(_)));
        storage.close().await.unwrap();
    }
}
