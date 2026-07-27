//! Cross-subsystem integration: RFC-0001 + 0002 + 0004 + 0005 + 0006 together.
//!
//! Deliberately **not** named `*_rfcNNNN.rs`. Every other integration suite in
//! this workspace verifies one RFC against itself, which is also how the system
//! was built — so nothing yet proves the subsystems compose.
//!
//! The specific seam under test: [`InProcessMcpHost::with_decision_log`] accepts
//! any [`DecisionLog`], and [`EventDecisionLog`] writes durably through the
//! runtime's event sink into SQLite. Both halves exist and are well tested in
//! isolation; the RFC-0006 suite only ever passes the in-memory
//! `RecordingDecisionLog`, and `EventDecisionLog` appears only in the RFC-0004
//! suite. Until this file, nothing had ever joined them — a tool call had never
//! been shown to survive as a durable, re-readable event.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_runtime::obs::{parse_tool_call_event, EventDecisionLog, RetentionPolicy};
use alloy_runtime::session::SessionPlane;
use alloy_runtime::storage::{EventStore, StorageOpenOptions};
use alloy_runtime::{
    install_sqlite_event_sink, AlloyRuntime, AlloyStorage, ArtifactId, ArtifactKind, ArtifactStore,
    BudgetPolicy, ConfigPaths, CreateSession, EditContext, EditEngine, ExecAllow, Glob, Grant,
    LanguageId, PermissionToken, ProfileId, RunId, RuntimeConfig, SessionEventType, SessionId,
    ToolCall, ToolName, ToolSelector, TransactionId,
};
use alloy_tools::mcp::{
    InProcessMcpHost, McpError, McpHostConfig, McpPlatform, PermissionDenial, ToolHandle,
};
use alloy_tools::{
    trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
    GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBroker,
    SandboxError, SandboxExecRequest, SandboxProfile,
};
use serde_json::json;
use tempfile::TempDir;

// --- host fixture -----------------------------------------------------------

/// Runtime + SQLite storage + a real sandbox broker + a real MCP host.
struct Stack {
    rt: AlloyRuntime,
    _root: TempDir,
    storage: Arc<AlloyStorage>,
    host: Arc<InProcessMcpHost>,
    engine: Arc<GitEditEngine>,
    broker: Arc<NativeSandboxBroker>,
    jail: PathBuf,
    plane: SessionPlane,
    workspace: PathBuf,
}

impl Stack {
    /// Wire every merged subsystem together over one temp workspace.
    ///
    /// Returns `None` when this host cannot isolate, matching the skip
    /// behaviour of the RFC-0005 suite.
    async fn build() -> Option<Self> {
        let root = tempfile::tempdir().unwrap();
        write_config_fixtures(root.path());

        // --- RFC-0001 runtime + RFC-0002 storage ---
        let cfg = RuntimeConfig::load(ConfigPaths {
            profile: root.path().join("profiles/default.toml"),
            router: root.path().join("router.toml"),
            example_env: root.path().join("example.env"),
            data_dir: Some(root.path().join("data")),
            workspace_root: Some(root.path().to_path_buf()),
        })
        .unwrap();
        let data_dir = cfg.data_dir.clone();
        let mut rt = AlloyRuntime::new();
        rt.configure(cfg).unwrap();
        let handle = rt.start().await.unwrap();
        let storage =
            install_sqlite_event_sink(&handle, Some(StorageOpenOptions::for_data_dir(data_dir)))
                .await
                .unwrap();

        // --- RFC-0005 sandbox, real backend ---
        let jail = root.path().join("jail");
        std::fs::create_dir_all(&jail).unwrap();
        let jail = jail.canonicalize().unwrap();
        let cargo_home = root.path().join("cargo");
        let cargo_bin = cargo_home.join("bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        write_fake_cargo(&cargo_bin.join("cargo"));
        let rustup_home = root.path().join("rustup");
        std::fs::create_dir_all(&rustup_home).unwrap();
        let homes = OperatorHomes::new(cargo_home, rustup_home);

        let profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        // A skip past this point must still close the runtime and storage it
        // already opened, or the process warns about dropping them un-closed.
        let broker = match NativeSandboxBroker::with_operator_homes(profile.clone(), homes.clone())
            .await
        {
            Ok(b) => match check_backend_status(&b) {
                BackendStatus::Available { .. } => b,
                BackendStatus::Unavailable { reason } => {
                    return skip_after_open(rt, &storage, &reason).await
                }
                BackendStatus::NotApplicable => {
                    return skip_after_open(rt, &storage, "not applicable on this platform").await
                }
            },
            Err(SandboxError::BackendUnavailable { message, .. }) => {
                return skip_after_open(rt, &storage, &message).await
            }
            Err(e) => panic!("broker construction failed: {e}"),
        };
        let broker = Arc::new(broker);
        init_git_repo(&broker, &jail).await;

        // --- RFC-0004 durable decision log + RFC-0006 MCP host ---
        let decision_log = Arc::new(EventDecisionLog::new(
            handle.clone(),
            Arc::clone(&storage),
            RetentionPolicy::defaults(),
        ));
        let path_policy = PathPolicy::from_profile(&profile, vec![]).unwrap();
        let engine = Arc::new(
            GitEditEngine::new(GitEditEngineConfig::new(
                broker.clone() as Arc<dyn SandboxBroker>,
                path_policy,
                trusted_exec_path(&homes),
                storage.artifacts() as Arc<dyn ArtifactStore>,
                storage.events(),
            ))
            .unwrap(),
        );
        let patch_backend = Arc::new(EditEnginePatchBackend::new(
            engine.clone() as Arc<dyn EditEngine>
        ));
        let host = InProcessMcpHost::new(
            broker.clone(),
            homes,
            vec![jail.clone()],
            patch_backend,
            McpHostConfig::new(),
        )
        .unwrap()
        .with_decision_log(decision_log);
        let host = Arc::new(host);

        let plane = SessionPlane::new(handle, Arc::clone(&storage));
        let workspace = root.path().to_path_buf();

        Some(Self {
            rt,
            _root: root,
            storage,
            host,
            engine,
            broker: broker.clone(),
            jail,
            plane,
            workspace,
        })
    }

    /// A durable session, created through the RFC-0003 control plane.
    ///
    /// The decision log resolves the session row before writing, so an invented
    /// `SessionId` is silently dropped with only a warning — the audit record
    /// must be attached to a session that genuinely exists.
    async fn new_session(&self) -> SessionId {
        self.plane
            .sessions()
            .create(CreateSession {
                workspace_root: self.workspace.clone(),
                profile: ProfileId::new("default").unwrap(),
                budget: BudgetPolicy::default(),
                language_backends: vec![LanguageId::new("rust").unwrap()],
            })
            .await
            .expect("create session")
    }

    /// Close storage and stop the runtime, as a real host would.
    async fn shutdown(self) {
        self.storage.close().await.ok();
        self.rt.shutdown().await.ok();
    }

    /// Every `ToolCall` event durably recorded for `session`.
    async fn tool_call_events(&self, session: SessionId) -> Vec<alloy_runtime::SessionEvent> {
        self.storage
            .events()
            .list_session_events(session, None, 128)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.type_ == SessionEventType::ToolCall)
            .collect()
    }
}

/// Status of the backend that enforces `ExecClass::Check` on this platform.
fn check_backend_status(broker: &NativeSandboxBroker) -> BackendStatus {
    let caps = broker.capabilities();
    #[cfg(target_os = "linux")]
    {
        caps.landlock.clone()
    }
    #[cfg(target_os = "macos")]
    {
        caps.seatbelt.clone()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        caps.container.clone()
    }
}

/// Skip cleanly once the runtime and storage are already open.
///
/// Composition coverage is effectively Linux-only: on a host without an
/// isolation backend these tests report success while asserting nothing, so CI
/// on such a platform must set `ALLOY_REQUIRE_LANDLOCK=1` to turn the skip into
/// a failure rather than silent green.
async fn skip_after_open(
    rt: AlloyRuntime,
    storage: &Arc<AlloyStorage>,
    reason: &str,
) -> Option<Stack> {
    storage.close().await.ok();
    rt.shutdown().await.ok();
    if std::env::var("ALLOY_REQUIRE_LANDLOCK").as_deref() == Ok("1") {
        panic!("ALLOY_REQUIRE_LANDLOCK=1 but the check backend is unavailable: {reason}");
    }
    eprintln!("skip: sandbox unavailable ({reason}); set ALLOY_REQUIRE_LANDLOCK=1 to fail");
    None
}

fn write_config_fixtures(root: &Path) {
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/default.toml"),
        include_str!("../../../profiles/default.toml"),
    )
    .unwrap();
    std::fs::write(
        root.join("router.toml"),
        include_str!("../../../router.toml.example"),
    )
    .unwrap();
    std::fs::write(root.join("example.env"), "ALLOY_API_KEY=\n").unwrap();
}

/// Name of the file the stand-in `cargo` touches when it actually runs.
const RAN_MARKER: &str = "cargo-ran";

/// A stand-in `cargo` on the trusted PATH. The binary being fake is fine — the
/// subject under test is the plumbing from tool call to durable event, not
/// cargo. It leaves a marker in its working directory so a test can distinguish
/// "denied before exec" from "executed, then reported as denied".
fn write_fake_cargo(path: &Path) {
    std::fs::write(
        path,
        format!("#!/bin/sh\ntouch ./{RAN_MARKER} 2>/dev/null\nexit 0\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
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

fn git_token() -> PermissionToken {
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
            git_token(),
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

async fn init_git_repo(broker: &Arc<NativeSandboxBroker>, jail: &Path) {
    run_git(broker, jail, &["init"]).await;
    std::fs::write(jail.join("edit.txt"), "before\n").unwrap();
    run_git(broker, jail, &["add", "."]).await;
    run_git(
        broker,
        jail,
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

fn exec_token(jail: &Path) -> PermissionToken {
    token(vec![
        Grant::Exec(ExecAllow {
            binary: "cargo".into(),
            args_glob: None,
        }),
        Grant::FsRead(Glob(format!("{}/**", jail.display()))),
    ])
}

/// Attach session attribution; `ToolCall` exposes the field directly.
fn session_call(name: &str, session: SessionId, arguments: serde_json::Value) -> ToolCall {
    let mut c = ToolCall::new(ToolName::new(name).unwrap(), arguments);
    c.session = Some(session);
    c
}

/// `cargo_check` requires `workspace_root`; argument validation runs before
/// authorization, so a denial test must still pass a schema-valid payload.
fn cargo_check_call(session: SessionId) -> ToolCall {
    session_call("cargo_check", session, json!({ "workspace_root": "." }))
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

// --- the tests --------------------------------------------------------------

/// A successful tool call must survive as a durable, re-readable event.
///
/// This crosses five RFCs: the MCP host authorizes and dispatches (0006), the
/// sandbox executes (0005), the decision log records (0004), the event sink
/// persists to SQLite (0002), all under the runtime host (0001).
#[tokio::test]
async fn tool_call_is_durably_recorded_across_the_whole_stack() {
    let Some(stack) = Stack::build().await else {
        return;
    };
    let session = stack.new_session().await;

    let result = stack
        .host
        .call(cargo_check_call(session), exec_token(&stack.jail))
        .await
        .expect("cargo_check should dispatch through the sandbox");
    assert!(!result.is_error(), "fake cargo exits 0: {result:?}");

    let events = stack.tool_call_events(session).await;
    assert_eq!(
        events.len(),
        1,
        "exactly one durable ToolCall event should exist, got {events:?}"
    );

    // The event must round-trip through the RFC-0004 reader, not merely exist.
    let record = parse_tool_call_event(&events[0]).expect("ToolCall event must be parseable");
    assert_eq!(record.session, session);
    assert_eq!(record.tool_name, "cargo_check");
    assert!(!record.denied, "a successful call is not a denial");
    assert!(
        stack.jail.join(RAN_MARKER).exists(),
        "the stand-in cargo must really execute, otherwise the denial test's \
         absence-of-marker assertion would pass vacuously"
    );

    stack.shutdown().await;
}

/// A denied call must also be durably recorded. Fail-closed is only auditable
/// if the denial reaches storage, which no test has previously checked.
#[tokio::test]
async fn denied_tool_call_is_durably_recorded_and_does_not_execute() {
    let Some(stack) = Stack::build().await else {
        return;
    };
    let session = stack.new_session().await;

    // A token with no Exec grant: authorization must fail closed.
    let outcome = stack
        .host
        .call(cargo_check_call(session), token(vec![]))
        .await;
    assert!(
        outcome.is_err() || outcome.as_ref().unwrap().is_error(),
        "a call with no Exec grant must not succeed: {outcome:?}"
    );

    let events = stack.tool_call_events(session).await;
    assert_eq!(
        events.len(),
        1,
        "the denial must be durably recorded for audit, got {events:?}"
    );
    let record = parse_tool_call_event(&events[0]).expect("denial event must be parseable");
    assert!(record.denied, "the recorded event must be marked denied");
    assert!(
        !stack.jail.join(RAN_MARKER).exists(),
        "fail-closed means the binary never ran, not that it ran and was then reported denied"
    );

    stack.shutdown().await;
}

/// Disclosure must gate *dispatch*, not merely listing.
///
/// The selector gate lives on [`ToolHandle::call`], not on the platform — so a
/// test that only inspects `tools_for` proves nothing about callability. This
/// drives a real call through a handle scoped to `fs_read` and requires
/// `cargo_check` to be refused with [`PermissionDenial::NotDisclosed`], even
/// though the token carries a valid Exec grant.
#[tokio::test]
async fn undisclosed_tool_is_not_callable_through_a_scoped_handle() {
    let Some(stack) = Stack::build().await else {
        return;
    };
    let session = stack.new_session().await;

    let handle = ToolHandle::new(
        Arc::clone(&stack.host) as Arc<dyn McpPlatform>,
        vec![ToolSelector::name(ToolName::new("fs_read").unwrap())],
    );

    let disclosed = handle.tools().await.unwrap();
    assert_eq!(disclosed.len(), 1, "handle should disclose exactly fs_read");
    assert_eq!(disclosed[0].name.as_str(), "fs_read");

    // Valid Exec grant, but the tool is outside the handle's selector set.
    let err = handle
        .call(cargo_check_call(session), exec_token(&stack.jail))
        .await
        .expect_err("an undisclosed tool must not dispatch");
    assert!(
        matches!(
            err,
            McpError::PermissionDenied(PermissionDenial::NotDisclosed)
        ),
        "expected NotDisclosed, got {err:?}"
    );
    assert!(
        !stack.jail.join(RAN_MARKER).exists(),
        "an undisclosed tool must not reach the sandbox"
    );

    stack.shutdown().await;
}

/// RFC-0008 reference constructor: MCP apply_patch uses EditEnginePatchBackend,
/// mutates a real git workspace, records EditApplied durably, and can roll back.
#[tokio::test]
async fn apply_patch_edit_engine_cross_subsystem_records_edit_applied() {
    let Some(stack) = Stack::build().await else {
        return;
    };
    let session = stack.new_session().await;
    let run = RunId::new();
    let perms = PermissionToken {
        run_id: run,
        ..edit_token()
    };
    let head_before = run_git(&stack.broker, &stack.jail, &["rev-parse", "HEAD"]).await;
    let diff = "--- a/edit.txt\n+++ b/edit.txt\n@@ -1,1 +1,1 @@\n-before\n+after\n";
    let result = stack
        .host
        .call(
            session_call(
                "apply_patch",
                session,
                json!({ "patch": diff, "dry_run": false }),
            ),
            perms.clone(),
        )
        .await
        .expect("apply_patch dispatch");
    assert!(!result.is_error(), "apply_patch should succeed: {result:?}");
    assert_eq!(
        std::fs::read_to_string(stack.jail.join("edit.txt")).unwrap(),
        "after\n"
    );
    let tx_id: TransactionId =
        serde_json::from_value(result.content["transaction_id"].clone()).unwrap();
    let refs = run_git(
        &stack.broker,
        &stack.jail,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/alloy/checkpoints",
        ],
    )
    .await;
    assert!(!refs.trim().is_empty(), "checkpoint ref should exist");
    let head_after = run_git(&stack.broker, &stack.jail, &["rev-parse", "HEAD"]).await;
    assert_eq!(head_after, head_before);

    let events = stack
        .storage
        .events()
        .list_session_events(session, None, 128)
        .await
        .unwrap();
    let edit_event = events
        .iter()
        .find(|e| e.type_ == SessionEventType::EditApplied)
        .expect("EditApplied event");
    assert_eq!(edit_event.run_id, Some(run));
    let artifact_id: ArtifactId =
        serde_json::from_value(edit_event.payload["patch_artifact_id"].clone()).unwrap();
    let meta = stack.storage.artifacts().meta(artifact_id).await.unwrap();
    assert_eq!(meta.kind, ArtifactKind::Patch);

    stack
        .engine
        .rollback(
            tx_id,
            &EditContext {
                session_id: Some(session),
                run_id: Some(run),
                perms,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(stack.jail.join("edit.txt")).unwrap(),
        "before\n"
    );

    stack.shutdown().await;
}
