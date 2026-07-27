//! RFC-0008 acceptance and failure-injection tests against a real git repository.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use alloy_runtime::storage::{EventStore, StorageOpenOptions};
use alloy_runtime::{
    AlloyStorage, ArtifactBlob, ArtifactId, ArtifactMeta, ArtifactPut, ArtifactStore, CheckpointId,
    Digest, EditContext, EditEngine, EditError, EditRequest, ExecAllow, FilePatch, Glob, Grant,
    Hunk, PatchSet, PermissionToken, ProfileId, RunId, SemanticEditOp, StoreError, TxState,
};
use alloy_tools::mcp::{ApplyPatchArgs, PatchApplyBackend, PatchApplyError};
use alloy_tools::{
    trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
    GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBroker,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult, SandboxProfile,
};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    jail: PathBuf,
    native: Arc<NativeSandboxBroker>,
    storage: AlloyStorage,
    session: alloy_runtime::SessionId,
    profile: SandboxProfile,
    homes: OperatorHomes,
}

impl Fixture {
    async fn build() -> Option<Self> {
        Self::build_with_initial_commit(true).await
    }

    /// `initial_commit == false` leaves the repo with an unborn HEAD (AC 22).
    async fn build_with_initial_commit(initial_commit: bool) -> Option<Self> {
        let root = tempfile::tempdir().unwrap();
        let jail = root.path().join("repo");
        std::fs::create_dir_all(&jail).unwrap();
        let jail = jail.canonicalize().unwrap();

        let cargo_home = root.path().join("cargo");
        let rustup_home = root.path().join("rustup");
        std::fs::create_dir_all(cargo_home.join("bin")).unwrap();
        std::fs::create_dir_all(&rustup_home).unwrap();
        let homes = OperatorHomes::new(cargo_home, rustup_home);

        let profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        let native =
            match NativeSandboxBroker::with_operator_homes(profile.clone(), homes.clone()).await {
                Ok(broker) => match check_backend_status(&broker) {
                    BackendStatus::Available { .. } => Arc::new(broker),
                    BackendStatus::Unavailable { reason } => return skip(&reason),
                    BackendStatus::NotApplicable => return skip("not applicable on this platform"),
                },
                Err(SandboxError::BackendUnavailable { message, .. }) => return skip(&message),
                Err(error) => panic!("broker construction failed: {error}"),
            };

        run_git(&native, &jail, &["init"]).await;
        std::fs::write(jail.join("a.txt"), "one\n").unwrap();
        std::fs::write(jail.join("delete.txt"), "bye\n").unwrap();
        if initial_commit {
            run_git(&native, &jail, &["add", "."]).await;
            run_git(
                &native,
                &jail,
                &[
                    "-c",
                    "user.name=alloy",
                    "-c",
                    "user.email=alloy@localhost",
                    "commit",
                    "-m",
                    "init",
                ],
            )
            .await;
        }

        let storage =
            AlloyStorage::open(StorageOpenOptions::for_data_dir(root.path().join("data")))
                .await
                .unwrap();
        Some(Self {
            _root: root,
            jail,
            native,
            storage,
            session: alloy_runtime::SessionId::new(),
            profile,
            homes,
        })
    }

    fn engine(&self) -> Arc<GitEditEngine> {
        self.engine_with(
            self.native.clone() as Arc<dyn SandboxBroker>,
            self.storage.artifacts() as Arc<dyn ArtifactStore>,
            None,
        )
    }

    fn engine_with(
        &self,
        broker: Arc<dyn SandboxBroker>,
        artifacts: Arc<dyn ArtifactStore>,
        max_digest_files: Option<u64>,
    ) -> Arc<GitEditEngine> {
        let policy = PathPolicy::from_profile(&self.profile, Vec::new()).unwrap();
        let mut config = GitEditEngineConfig::new(
            broker,
            policy,
            trusted_exec_path(&self.homes),
            artifacts,
            self.storage.events(),
        );
        if let Some(limit) = max_digest_files {
            config.max_digest_files = limit;
        }
        Arc::new(GitEditEngine::new(config).unwrap())
    }

    fn ctx(&self, perms: PermissionToken) -> EditContext {
        EditContext {
            session_id: Some(self.session),
            run_id: Some(perms.run_id),
            perms,
        }
    }

    async fn close(self) {
        self.storage.close().await.unwrap();
    }
}

fn skip<T>(reason: &str) -> Option<T> {
    if std::env::var("ALLOY_REQUIRE_LANDLOCK").as_deref() == Ok("1") {
        panic!("ALLOY_REQUIRE_LANDLOCK=1 but sandbox unavailable: {reason}");
    }
    eprintln!("skip edit_failpoints: sandbox unavailable ({reason})");
    None
}

fn check_backend_status(broker: &NativeSandboxBroker) -> BackendStatus {
    let backend = broker.profile().backend_for(ExecClass::Check);
    match backend {
        alloy_tools::SandboxBackend::Landlock => broker.capabilities().landlock.clone(),
        alloy_tools::SandboxBackend::Seatbelt => broker.capabilities().seatbelt.clone(),
        alloy_tools::SandboxBackend::Container => broker.capabilities().container.clone(),
    }
}

fn token(grants: Vec<Grant>) -> PermissionToken {
    PermissionToken {
        profile: ProfileId::new("default").unwrap(),
        grants,
        expires: None,
        run_id: RunId::new(),
    }
}

fn edit_token() -> PermissionToken {
    token(vec![
        Grant::FsWrite(Glob("**".into())),
        Grant::GitWrite,
        Grant::Exec(ExecAllow {
            binary: "git".into(),
            args_glob: None,
        }),
    ])
}

fn git_write_token() -> PermissionToken {
    token(vec![
        Grant::GitWrite,
        Grant::Exec(ExecAllow {
            binary: "git".into(),
            args_glob: None,
        }),
    ])
}

fn setup_git_token() -> PermissionToken {
    token(vec![Grant::Exec(ExecAllow {
        binary: "git".into(),
        args_glob: None,
    })])
}

async fn run_git(broker: &Arc<NativeSandboxBroker>, jail: &Path, args: &[&str]) -> String {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let result = broker
        .exec(SandboxExecRequest::new(
            argv,
            jail.to_path_buf(),
            setup_git_token(),
            ExecClass::Check,
        ))
        .await
        .unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "git {args:?} stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

async fn checkpoint_refs(fx: &Fixture) -> Vec<String> {
    run_git(
        &fx.native,
        &fx.jail,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/alloy/checkpoints",
        ],
    )
    .await
    .lines()
    .map(str::to_string)
    .collect()
}

/// Add `rel` to the index the way a careless `git add .env` would.
///
/// The sandbox binds deny-glob paths away from the child, so a plain `git add`
/// cannot see `.env` at all; `update-index --cacheinfo` records the same index
/// entry without reading the worktree.
async fn track_deny_path(fx: &Fixture, rel: &str) {
    let blob = run_git(&fx.native, &fx.jail, &["hash-object", "-w", "a.txt"]).await;
    let cacheinfo = format!("100644,{},{rel}", blob.trim());
    run_git(
        &fx.native,
        &fx.jail,
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
    )
    .await;
    assert!(
        run_git(&fx.native, &fx.jail, &["ls-files"])
            .await
            .lines()
            .any(|line| line == rel),
        "{rel} must be tracked for this test to mean anything"
    );
}

fn modify_request(old: &str, new: &str) -> EditRequest {
    EditRequest::TextPatch {
        patch: PatchSet {
            files: vec![FilePatch::Modify {
                path: "a.txt".into(),
                hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![format!("-{old}"), format!("+{new}")],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        },
    }
}

fn create_request(path: &str, content: &str) -> EditRequest {
    EditRequest::TextPatch {
        patch: PatchSet {
            files: vec![FilePatch::Create {
                path: path.into(),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![format!("+{content}")],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        },
    }
}

/// Broker decorator that injects a bounded failure into matching argv.
///
/// Matching calls are delegated successfully `successes_before_failure` times;
/// the next `failure_count` matches return the selected error.
struct FailpointBroker {
    inner: Arc<NativeSandboxBroker>,
    argv_substring: String,
    successes_before_failure: usize,
    matching_calls: AtomicUsize,
    failures_remaining: AtomicUsize,
    failure: InjectedFailure,
}

#[derive(Clone, Copy)]
enum InjectedFailure {
    Internal,
    TokenExpired,
}

impl FailpointBroker {
    fn new(
        inner: Arc<NativeSandboxBroker>,
        argv_substring: &str,
        successes_before_failure: usize,
        failure_count: usize,
        failure: InjectedFailure,
    ) -> Self {
        Self {
            inner,
            argv_substring: argv_substring.into(),
            successes_before_failure,
            matching_calls: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(failure_count),
            failure,
        }
    }

    fn take_failure(&self) -> bool {
        self.failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

#[async_trait]
impl SandboxBroker for FailpointBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        if req
            .argv
            .iter()
            .any(|arg| arg.contains(&self.argv_substring))
        {
            let ordinal = self.matching_calls.fetch_add(1, Ordering::SeqCst);
            if ordinal >= self.successes_before_failure && self.take_failure() {
                return Err(match self.failure {
                    InjectedFailure::Internal => {
                        SandboxError::Internal("injected argv failpoint".into())
                    }
                    InjectedFailure::TokenExpired => SandboxError::TokenExpired,
                });
            }
        }
        self.inner.exec(req).await
    }

    fn profile(&self) -> &SandboxProfile {
        self.inner.profile()
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        self.inner.capabilities()
    }
}

/// Broker decorator that reports truncated stdout for one git subcommand.
///
/// The sandbox caps captured stdout, so the engine has to treat a truncated
/// answer as unusable rather than as a smaller result.
struct TruncatingBroker {
    inner: Arc<NativeSandboxBroker>,
    subcommand: String,
}

#[async_trait]
impl SandboxBroker for TruncatingBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        let matched = req.argv.contains(&self.subcommand);
        let mut result = self.inner.exec(req).await?;
        if matched {
            result.stdout_truncated = true;
        }
        Ok(result)
    }

    fn profile(&self) -> &SandboxProfile {
        self.inner.profile()
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        self.inner.capabilities()
    }
}

struct FailingPutStore;

#[async_trait]
impl ArtifactStore for FailingPutStore {
    async fn put(&self, _req: ArtifactPut) -> Result<ArtifactId, StoreError> {
        Err(StoreError::Io("injected artifact put failure".into()))
    }

    async fn get(&self, _id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
        Err(StoreError::NotFound("injected store".into()))
    }

    async fn meta(&self, _id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
        Err(StoreError::NotFound("injected store".into()))
    }

    async fn get_by_digest(&self, _digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
        Ok(None)
    }

    async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
        Err(StoreError::NotFound("injected store".into()))
    }
}

/// Fails the first N puts, then delegates to the real CAS.
struct FailFirstPuts {
    inner: Arc<dyn ArtifactStore>,
    remaining: AtomicUsize,
}

impl FailFirstPuts {
    fn new(inner: Arc<dyn ArtifactStore>, count: usize) -> Self {
        Self {
            inner,
            remaining: AtomicUsize::new(count),
        }
    }

    fn should_fail(&self) -> bool {
        self.remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

#[async_trait]
impl ArtifactStore for FailFirstPuts {
    async fn put(&self, req: ArtifactPut) -> Result<ArtifactId, StoreError> {
        if self.should_fail() {
            return Err(StoreError::Io("injected artifact put failure".into()));
        }
        self.inner.put(req).await
    }

    async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
        self.inner.get(id).await
    }

    async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
        self.inner.meta(id).await
    }

    async fn get_by_digest(&self, digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
        self.inner.get_by_digest(digest).await
    }

    async fn delete(&self, id: ArtifactId) -> Result<(), StoreError> {
        self.inner.delete(id).await
    }
}

#[tokio::test]
async fn artifact_put_failure_restores_and_does_not_commit() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine_with(
        fx.native.clone() as Arc<dyn SandboxBroker>,
        Arc::new(FailingPutStore),
        None,
    );
    let head_before = run_git(&fx.native, &fx.jail, &["rev-parse", "HEAD"]).await;

    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(error, EditError::Storage(_)));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        run_git(&fx.native, &fx.jail, &["rev-parse", "HEAD"]).await,
        head_before
    );
    assert!(
        fx.storage
            .events()
            .list_session_events(fx.session, None, 16)
            .await
            .unwrap()
            .is_empty(),
        "a pre-commit storage failure must not emit EditApplied"
    );
    fx.close().await;
}

#[tokio::test]
async fn restore_failpoint_after_n_successes_is_failed_dirty_then_reconciles() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let broker = Arc::new(FailpointBroker::new(
        fx.native.clone(),
        "restore",
        1,
        1,
        InjectedFailure::Internal,
    ));
    let artifacts = Arc::new(FailFirstPuts::new(
        fx.storage.artifacts() as Arc<dyn ArtifactStore>,
        2,
    ));
    let engine = fx.engine_with(broker, artifacts, None);

    let first = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(first, EditError::Storage(_)));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );

    let second = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    let checkpoint_id = match second {
        EditError::RollbackFailed { checkpoint_id, .. } => checkpoint_id,
        other => panic!("expected FailedDirty/RollbackFailed, got {other:?}"),
    };
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n"
    );
    let checkpoint_ref = format!("refs/alloy/checkpoints/{checkpoint_id}");
    let retained = run_git(&fx.native, &fx.jail, &["rev-parse", &checkpoint_ref]).await;
    assert_eq!(retained.trim().len(), 40);

    let reconciled = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .expect("the next apply must reconcile the abandoned open transaction");
    assert_eq!(reconciled.state, TxState::Committed);
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn token_expired_mid_restore_is_retained_and_fresh_apply_reconciles() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let broker = Arc::new(FailpointBroker::new(
        fx.native.clone(),
        "restore",
        0,
        1,
        InjectedFailure::TokenExpired,
    ));
    let artifacts = Arc::new(FailFirstPuts::new(
        fx.storage.artifacts() as Arc<dyn ArtifactStore>,
        1,
    ));
    let engine = fx.engine_with(broker, artifacts, None);

    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(error, EditError::TokenExpired));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n"
    );
    let retained_refs = checkpoint_refs(&fx).await;
    assert_eq!(
        retained_refs.len(),
        1,
        "the open checkpoint ref must survive"
    );

    let tx = engine
        .apply(modify_request("one", "fresh"), &fx.ctx(edit_token()))
        .await
        .expect("a fresh token must reconcile before applying");
    assert_eq!(tx.state, TxState::Committed);
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "fresh\n",
        "success proves reconcile restored the expected `one` pre-image first"
    );
    let refs_after = checkpoint_refs(&fx).await;
    assert!(retained_refs.iter().all(|old| refs_after.contains(old)));
    fx.close().await;
}

#[tokio::test]
async fn rollback_rejects_workspace_drift() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let tx = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();
    std::fs::write(fx.jail.join("a.txt"), "drift\n").unwrap();

    let error = engine
        .rollback(tx.id, &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(error, EditError::WorkspaceDrifted(id) if id == tx.id));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "drift\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn rollback_rejects_non_newest_committed_transaction() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let first = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();
    let second = engine
        .apply(modify_request("two", "three"), &fx.ctx(edit_token()))
        .await
        .unwrap();

    let error = engine
        .rollback(first.id, &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EditError::RollbackNotEligible { tx, reason: "not newest", .. } if tx == first.id
    ));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "three\n"
    );

    engine
        .rollback(second.id, &fx.ctx(edit_token()))
        .await
        .unwrap();
    engine
        .rollback(first.id, &fx.ctx(edit_token()))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn orphan_checkpoint_is_not_auto_restored_but_explicit_recovery_works() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let initial_head = run_git(&fx.native, &fx.jail, &["rev-parse", "HEAD"]).await;
    let orphan = CheckpointId::new();
    let orphan_ref = format!("refs/alloy/checkpoints/{orphan}");
    run_git(
        &fx.native,
        &fx.jail,
        &["update-ref", &orphan_ref, initial_head.trim()],
    )
    .await;
    std::fs::write(fx.jail.join("a.txt"), "dirty\n").unwrap();

    let tx = engine
        .apply(modify_request("dirty", "applied"), &fx.ctx(edit_token()))
        .await
        .expect("an orphan ref must not be selected for automatic recovery");
    assert_eq!(tx.state, TxState::Committed);
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "applied\n"
    );

    engine
        .recover_checkpoint(orphan, &fx.ctx(git_write_token()))
        .await
        .expect("operator-selected checkpoint recovery");
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn dotenv_is_denied_and_rollback_preserves_untracked_dotenv() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let denied = engine
        .validate(
            create_request(".env", "SECRET=changed"),
            &fx.ctx(edit_token()),
        )
        .await
        .unwrap_err();
    assert!(matches!(denied, EditError::PathDenied { ref path, .. } if path == ".env"));

    std::fs::write(fx.jail.join(".env"), "SECRET=keep\n").unwrap();
    let tx = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();
    engine.rollback(tx.id, &fx.ctx(edit_token())).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.jail.join(".env")).unwrap(),
        "SECRET=keep\n",
        "rollback may only unlink transaction-owned paths"
    );
    fx.close().await;
}

#[tokio::test]
async fn missing_exec_git_and_run_id_mismatch_are_rejected() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let no_exec = token(vec![Grant::FsWrite(Glob("**".into())), Grant::GitWrite]);
    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(no_exec))
        .await
        .unwrap_err();
    assert!(matches!(error, EditError::MissingGrant(ref grant) if grant.starts_with("exec")));

    let perms = edit_token();
    let mismatched = EditContext {
        session_id: Some(fx.session),
        run_id: Some(RunId::new()),
        perms,
    };
    let error = engine
        .apply(modify_request("one", "two"), &mismatched)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EditError::InvalidRequest(ref message) if message == "run_id mismatch"
    ));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn untracked_and_gitignored_modify_are_rejected() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    for (path, content) in [("untracked.txt", "plain\n"), ("ignored.txt", "ignored\n")] {
        if path == "ignored.txt" {
            std::fs::write(fx.jail.join(".git/info/exclude"), "ignored.txt\n").unwrap();
        }
        std::fs::write(fx.jail.join(path), content).unwrap();
        let request = EditRequest::TextPatch {
            patch: PatchSet {
                files: vec![FilePatch::Modify {
                    path: path.into(),
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 1,
                        lines: vec![format!("-{}", content.trim_end()), "+changed".into()],
                        eof_newline: true,
                        old_eof_no_newline: false,
                    }],
                }],
            },
        };
        let error = engine
            .apply(request, &fx.ctx(edit_token()))
            .await
            .unwrap_err();
        assert!(matches!(error, EditError::UntrackedPath { path: rejected } if rejected == path));
        assert_eq!(
            std::fs::read_to_string(fx.jail.join(path)).unwrap(),
            content
        );
    }
    fx.close().await;
}

#[tokio::test]
async fn index_lock_and_merge_in_progress_are_rejected_before_checkpoint() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let index_lock = fx.jail.join(".git/index.lock");
    std::fs::write(&index_lock, "").unwrap();
    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(error, EditError::Git(ref message) if message == "index.lock present"));
    std::fs::remove_file(index_lock).unwrap();

    let merge_head = fx.jail.join(".git/MERGE_HEAD");
    std::fs::write(&merge_head, "0000000000000000000000000000000000000000\n").unwrap();
    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(error, EditError::Conflict(ref message) if message.contains("repo state")));
    assert!(checkpoint_refs(&fx).await.is_empty());
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn digest_file_limit_and_target_patch_path_are_rejected() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let limited = fx.engine_with(
        fx.native.clone() as Arc<dyn SandboxBroker>,
        fx.storage.artifacts() as Arc<dyn ArtifactStore>,
        Some(1),
    );
    let error = limited
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EditError::DigestLimitExceeded(ref dimension) if dimension == "file count"
    ));
    assert!(checkpoint_refs(&fx).await.is_empty());

    let error = fx
        .engine()
        .validate(
            create_request("target/generated.txt", "generated"),
            &fx.ctx(edit_token()),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EditError::InvalidPatch(ref message) if message == "path excluded from digest"
    ));
    fx.close().await;
}

#[tokio::test]
async fn semantic_ops_are_rejected_through_edit_engine_backend() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let backend = EditEnginePatchBackend::new(engine as Arc<dyn EditEngine>);
    let error = backend
        .apply(
            ApplyPatchArgs {
                patch: json!({
                    "kind": "semantic_ops",
                    "ops": [{
                        "op": "rename_type",
                        "from_path": "crate::Old",
                        "to_name": "New",
                        "update_references": true
                    }]
                }),
                dry_run: false,
            },
            &edit_token(),
            Some(fx.session),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PatchApplyError::Unsupported(ref op) if op == "rename_type"
    ));

    let direct = fx
        .engine()
        .apply(
            EditRequest::SemanticOps {
                ops: vec![SemanticEditOp::ReplaceBody {
                    item_path: "crate::f".into(),
                    new_body: "{}".into(),
                }],
            },
            &fx.ctx(edit_token()),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        direct,
        EditError::UnsupportedOp { ref op } if op == "replace_body"
    ));
    fx.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn modify_preserves_executable_mode() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fx) = Fixture::build().await else {
        return;
    };
    let mut permissions = std::fs::metadata(fx.jail.join("a.txt"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(fx.jail.join("a.txt"), permissions).unwrap();
    let engine = fx.engine();

    engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();

    assert_eq!(
        std::fs::metadata(fx.jail.join("a.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    fx.close().await;
}

/// AC 44: a truncated `ls-files` would under-report the tracked set, so the
/// engine must fail closed with the operator-facing cap hint before mutating.
#[tokio::test]
async fn truncated_ls_files_stdout_is_environment_error_before_mutate() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine_with(
        Arc::new(TruncatingBroker {
            inner: fx.native.clone(),
            subcommand: "ls-files".into(),
        }),
        fx.storage.artifacts() as Arc<dyn ArtifactStore>,
        None,
    );

    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        EditError::Environment(ref message)
            if message == "git stdout truncated; raise sandbox stdout_cap"
    ));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(checkpoint_refs(&fx).await.is_empty());
    fx.close().await;
}

/// AC 44: truncated `diff --name-only` (merge-conflict probe) must likewise fail closed.
#[tokio::test]
async fn truncated_diff_stdout_is_environment_error_before_mutate() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine_with(
        Arc::new(TruncatingBroker {
            inner: fx.native.clone(),
            subcommand: "diff".into(),
        }),
        fx.storage.artifacts() as Arc<dyn ArtifactStore>,
        None,
    );

    let error = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        EditError::Environment(ref message)
            if message == "git stdout truncated; raise sandbox stdout_cap"
    ));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(checkpoint_refs(&fx).await.is_empty());
    fx.close().await;
}

/// AC 36: `git add .env` after a committed edit makes a deny-glob path tracked,
/// and a whole-tree restore would rewrite it — so rollback must refuse instead.
#[tokio::test]
async fn deny_glob_path_tracked_after_commit_blocks_rollback() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let tx = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();

    std::fs::write(fx.jail.join(".env"), "SECRET=keep\n").unwrap();
    track_deny_path(&fx, ".env").await;

    let error = engine
        .rollback(tx.id, &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(error, EditError::TrackedDeniedPath { ref path } if path == ".env"));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n",
        "the refused rollback must leave the committed edit in place"
    );
    assert_eq!(
        std::fs::read_to_string(fx.jail.join(".env")).unwrap(),
        "SECRET=keep\n"
    );
    fx.close().await;
}

/// AC 22: an unborn HEAD has no tree for `stash create` to checkpoint against,
/// which is a permanent environment problem the operator has to fix.
#[tokio::test]
async fn unborn_head_is_environment_error() {
    let Some(fx) = Fixture::build_with_initial_commit(false).await else {
        return;
    };

    let error = fx
        .engine()
        .apply(create_request("new.txt", "hello"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        EditError::Environment(ref message)
            if message == "empty repository: make initial commit"
    ));
    assert!(!fx.jail.join("new.txt").exists());
    fx.close().await;
}

/// AC 45: a `.git` gitfile means a linked worktree, whose checkpoint refs would
/// live in a repository the engine never probed.
#[tokio::test]
async fn gitfile_instead_of_git_dir_is_environment_error() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    std::fs::rename(fx.jail.join(".git"), fx.jail.join("real.git")).unwrap();
    std::fs::write(fx.jail.join(".git"), "gitdir: real.git\n").unwrap();

    let error = fx
        .engine()
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        EditError::Environment(ref message) if message == "linked worktree not supported"
    ));
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}

/// AC 15: `session_id: None` has nowhere to send `EditApplied`, which must skip
/// the event rather than fail the apply.
#[tokio::test]
async fn apply_without_session_id_commits_and_emits_no_event() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let perms = edit_token();
    let sessionless = EditContext {
        session_id: None,
        run_id: Some(perms.run_id),
        perms,
    };

    let tx = engine
        .apply(modify_request("one", "two"), &sessionless)
        .await
        .unwrap();

    assert_eq!(tx.state, TxState::Committed);
    assert!(tx.patch_artifact_id.is_some());
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n"
    );
    assert!(
        fx.storage
            .events()
            .list_session_events(fx.session, None, 16)
            .await
            .unwrap()
            .is_empty(),
        "a sessionless apply must not emit EditApplied"
    );

    // The same engine still emits the event once a session is attached, so the
    // empty log above is the missing session and not a broken event path.
    engine
        .apply(modify_request("two", "three"), &fx.ctx(edit_token()))
        .await
        .unwrap();
    let events = fx
        .storage
        .events()
        .list_session_events(fx.session, None, 16)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    fx.close().await;
}

#[tokio::test]
async fn rollback_succeeds_without_fswrite_grant() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine();
    let tx = engine
        .apply(modify_request("one", "two"), &fx.ctx(edit_token()))
        .await
        .unwrap();

    engine
        .rollback(tx.id, &fx.ctx(git_write_token()))
        .await
        .expect("Appendix A permits rollback with GitWrite + Exec(git)");

    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.close().await;
}
