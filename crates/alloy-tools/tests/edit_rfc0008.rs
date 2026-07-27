//! Integration tests for RFC-0008 EditEngine against a real git repository.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_runtime::storage::{EventStore, StorageOpenOptions};
use alloy_runtime::{
    AlloyStorage, ArtifactKind, ArtifactStore, EditContext, EditEngine, EditError, EditRequest,
    EventSeq, EventSink, EventSinkError, ExecAllow, FilePatch, Glob, Grant, Hunk, NewSessionEvent,
    PatchSet, PermissionToken, ProfileId, RunId, RuntimeEvent, SessionEventType, SessionId,
    TxState,
};
use alloy_tools::mcp::{ApplyPatchArgs, PatchApplyBackend, PatchApplyError, PermissionDenial};
use alloy_tools::{
    trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
    GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBackend,
    SandboxBroker, SandboxError, SandboxExecRequest, SandboxProfile,
};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    jail: PathBuf,
    broker: Arc<NativeSandboxBroker>,
    storage: AlloyStorage,
    engine: Arc<GitEditEngine>,
    session: SessionId,
    profile: SandboxProfile,
    homes: OperatorHomes,
}

impl Fixture {
    async fn build() -> Option<Self> {
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
        let broker =
            match NativeSandboxBroker::with_operator_homes(profile.clone(), homes.clone()).await {
                Ok(b) => match check_backend_status(&b) {
                    BackendStatus::Available { .. } => Arc::new(b),
                    BackendStatus::Unavailable { reason } => return skip(&reason),
                    BackendStatus::NotApplicable => return skip("not applicable on this platform"),
                },
                Err(SandboxError::BackendUnavailable { message, .. }) => return skip(&message),
                Err(e) => panic!("broker construction failed: {e}"),
            };

        run_git(&broker, &jail, &["init"]).await;
        std::fs::write(jail.join("a.txt"), "one\n").unwrap();
        std::fs::write(jail.join("delete.txt"), "bye\n").unwrap();
        run_git(&broker, &jail, &["add", "."]).await;
        run_git(
            &broker,
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

        let storage =
            AlloyStorage::open(StorageOpenOptions::for_data_dir(root.path().join("data")))
                .await
                .unwrap();
        let policy = PathPolicy::from_profile(&profile, Vec::new()).unwrap();
        let engine = Arc::new(
            GitEditEngine::new(GitEditEngineConfig::new(
                broker.clone() as Arc<dyn SandboxBroker>,
                policy,
                trusted_exec_path(&homes),
                storage.artifacts() as Arc<dyn ArtifactStore>,
                storage.events(),
            ))
            .unwrap(),
        );
        Some(Self {
            _root: root,
            jail,
            broker,
            storage,
            engine,
            session: SessionId::new(),
            profile,
            homes,
        })
    }

    /// A second engine over the same repo with a different event sink.
    fn engine_with_events(&self, events: Arc<dyn EventSink>) -> Arc<GitEditEngine> {
        let policy = PathPolicy::from_profile(&self.profile, Vec::new()).unwrap();
        Arc::new(
            GitEditEngine::new(GitEditEngineConfig::new(
                self.broker.clone() as Arc<dyn SandboxBroker>,
                policy,
                trusted_exec_path(&self.homes),
                self.storage.artifacts() as Arc<dyn ArtifactStore>,
                events,
            ))
            .unwrap(),
        )
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
    eprintln!("skip edit_rfc0008: sandbox unavailable ({reason})");
    None
}

fn check_backend_status(broker: &NativeSandboxBroker) -> BackendStatus {
    let backend = broker.profile().backend_for(ExecClass::Check);
    match backend {
        SandboxBackend::Landlock => broker.capabilities().landlock.clone(),
        SandboxBackend::Seatbelt => broker.capabilities().seatbelt.clone(),
        SandboxBackend::Container => broker.capabilities().container.clone(),
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

fn setup_git_token() -> PermissionToken {
    token(vec![Grant::Exec(ExecAllow {
        binary: "git".into(),
        args_glob: None,
    })])
}

async fn run_git(broker: &Arc<NativeSandboxBroker>, jail: &Path, args: &[&str]) -> String {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|s| (*s).to_string()));
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
        "git {:?} stderr={}",
        args,
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

/// Rejects every append, so a post-commit event failure is observable.
struct FailingEventSink;

#[async_trait]
impl EventSink for FailingEventSink {
    async fn append_runtime(&self, _ev: RuntimeEvent) -> Result<(), EventSinkError> {
        Err(EventSinkError::Io("sink down".into()))
    }

    async fn append_session(&self, _ev: NewSessionEvent) -> Result<EventSeq, EventSinkError> {
        Err(EventSinkError::Io("sink down".into()))
    }
}

fn modify_patch() -> EditRequest {
    EditRequest::TextPatch {
        patch: PatchSet {
            files: vec![FilePatch::Modify {
                path: "a.txt".into(),
                hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["-one".into(), "+two".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        },
    }
}

#[tokio::test]
async fn textpatch_apply_checkpoint_event_and_rollback() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let head_before = run_git(&fx.broker, &fx.jail, &["rev-parse", "HEAD"]).await;
    let tx = fx
        .engine
        .apply(modify_patch(), &fx.ctx(edit_token()))
        .await
        .unwrap();
    assert_eq!(tx.state, TxState::Committed);
    assert_eq!(tx.files_touched, vec!["a.txt"]);
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n"
    );

    let checkpoint = tx.checkpoint_id.unwrap();
    let checkpoint_ref = format!("refs/alloy/checkpoints/{checkpoint}");
    let checkpoint_sha = run_git(&fx.broker, &fx.jail, &["rev-parse", &checkpoint_ref]).await;
    assert_eq!(checkpoint_sha.trim().len(), 40);
    let head_after = run_git(&fx.broker, &fx.jail, &["rev-parse", "HEAD"]).await;
    assert_eq!(head_after, head_before, "EditEngine must not move HEAD");

    let events = fx
        .storage
        .events()
        .list_session_events(fx.session, None, 16)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].type_, SessionEventType::EditApplied);
    assert!(events[0].payload.get("patch_artifact_id").is_some());
    let artifact_id = tx.patch_artifact_id.unwrap();
    let meta = fx.storage.artifacts().meta(artifact_id).await.unwrap();
    assert_eq!(meta.kind, ArtifactKind::Patch);

    fx.engine
        .rollback(tx.id, &fx.ctx(edit_token()))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    fx.engine
        .rollback(tx.id, &fx.ctx(edit_token()))
        .await
        .unwrap();
    fx.close().await;
}

/// After the commit point an `EditApplied` failure MUST NOT roll back and MUST
/// still return `Ok(EditTransaction)` (RFC-0008 §5.1 / Day-1 item 2).
#[tokio::test]
async fn event_failure_after_commit_still_commits() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let engine = fx.engine_with_events(Arc::new(FailingEventSink));

    let tx = engine
        .apply(modify_patch(), &fx.ctx(edit_token()))
        .await
        .expect("a committed edit must not fail because the event sink is down");

    assert_eq!(tx.state, TxState::Committed);
    assert!(tx.post_digest.is_some());
    assert!(tx.patch_artifact_id.is_some());
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "two\n",
        "the mutation must survive: no restore after commit"
    );
    let checkpoint_ref = format!("refs/alloy/checkpoints/{}", tx.checkpoint_id.unwrap());
    let sha = run_git(&fx.broker, &fx.jail, &["rev-parse", &checkpoint_ref]).await;
    assert_eq!(sha.trim().len(), 40);
    assert!(
        fx.storage
            .events()
            .list_session_events(fx.session, None, 16)
            .await
            .unwrap()
            .is_empty(),
        "the failing sink recorded nothing, which is what makes this a real failpoint"
    );
    fx.close().await;
}

#[tokio::test]
async fn dry_run_does_not_mutate_or_checkpoint() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let validation = fx
        .engine
        .validate(
            modify_patch(),
            &fx.ctx(token(vec![Grant::FsWrite(Glob("**".into()))])),
        )
        .await
        .unwrap();
    assert_eq!(validation.files_touched, vec!["a.txt"]);
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("a.txt")).unwrap(),
        "one\n"
    );
    let refs = run_git(
        &fx.broker,
        &fx.jail,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/alloy/checkpoints",
        ],
    )
    .await;
    assert!(refs.trim().is_empty());
    fx.close().await;
}

#[tokio::test]
async fn create_and_delete_then_rollback_unlinks_create() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let req = EditRequest::TextPatch {
        patch: PatchSet {
            files: vec![
                FilePatch::Create {
                    path: "nested/new.txt".into(),
                    hunks: vec![Hunk {
                        old_start: 0,
                        old_lines: 0,
                        new_start: 1,
                        new_lines: 1,
                        lines: vec!["+hello".into()],
                        eof_newline: true,
                        old_eof_no_newline: false,
                    }],
                },
                FilePatch::Delete {
                    path: "delete.txt".into(),
                },
            ],
        },
    };
    let tx = fx.engine.apply(req, &fx.ctx(edit_token())).await.unwrap();
    assert!(fx.jail.join("nested/new.txt").is_file());
    assert!(!fx.jail.join("delete.txt").exists());
    fx.engine
        .rollback(tx.id, &fx.ctx(edit_token()))
        .await
        .unwrap();
    assert!(!fx.jail.join("nested/new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fx.jail.join("delete.txt")).unwrap(),
        "bye\n"
    );
    fx.close().await;
}

#[tokio::test]
async fn backend_maps_missing_git_write_and_untracked_modify() {
    let Some(fx) = Fixture::build().await else {
        return;
    };
    let backend = EditEnginePatchBackend::new(fx.engine.clone() as Arc<dyn EditEngine>);
    let err = backend
        .apply(
            ApplyPatchArgs {
                patch: json!({"files":[{"action":"delete","path":"delete.txt"}]}),
                dry_run: false,
            },
            &token(vec![Grant::FsWrite(Glob("**".into()))]),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PatchApplyError::PermissionDenied(PermissionDenial::MissingGrant(ref g)) if g == "git_write"
    ));

    std::fs::write(fx.jail.join("untracked.txt"), "old\n").unwrap();
    let req = EditRequest::TextPatch {
        patch: PatchSet {
            files: vec![FilePatch::Modify {
                path: "untracked.txt".into(),
                hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["-old".into(), "+new".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        },
    };
    let err = fx
        .engine
        .apply(req, &fx.ctx(edit_token()))
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::UntrackedPath { ref path } if path == "untracked.txt"));
    fx.close().await;
}
