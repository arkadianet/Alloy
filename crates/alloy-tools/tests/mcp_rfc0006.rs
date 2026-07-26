//! Integration tests for RFC-0006 MCP Host & In-Process Builtins.
//!
//! These exercise the whole `call` pipeline against a
//! [`RecordingSandboxBroker`] and the [`StubPatchApplyBackend`], so they assert
//! host behaviour (ordering, fail-closed authz, result mapping, lifecycle)
//! without needing a real isolation backend on the test host. Backend
//! enforcement itself is covered by the RFC-0005 suite.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::obs::{DecisionLog, ObsError, RecordingDecisionLog, RetentionPolicy};
use alloy_runtime::{
    Digest, ExecAllow, Glob, Grant, PermissionToken, ProfileId, RunId, SessionId, Timestamp,
    ToolCall, ToolError, ToolName, ToolSelector,
};
use alloy_tools::mcp::{
    ApplyPatchArgs, ApplyPatchOutcome, InProcessMcpHost, McpError, McpHostConfig, McpHostPhase,
    McpPlatform, PatchApplyBackend, PatchApplyError, PermissionDenial, StubPatchApplyBackend,
};
use alloy_tools::{
    BackendStatus, ExecClass, OperatorHomes, RecordingSandboxBroker, SandboxBackend, SandboxBroker,
    SandboxCapabilities, SandboxError, SandboxExecRequest, SandboxExecResult, SandboxProfile,
};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use tracing_subscriber::prelude::*;

// --- fixtures ---------------------------------------------------------------

/// Jail + operator homes containing a fake `cargo` on a trusted PATH dir.
struct Fixture {
    _root: TempDir,
    jail: PathBuf,
    homes: OperatorHomes,
    profile: SandboxProfile,
}

impl Fixture {
    fn new() -> Self {
        Self::with_profile(|_| {})
    }

    fn with_profile(tweak: impl FnOnce(&mut SandboxProfile)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let jail = root.path().join("jail");
        std::fs::create_dir_all(&jail).unwrap();
        let jail = jail.canonicalize().unwrap();

        let cargo_home = root.path().join("cargo");
        let cargo_bin = cargo_home.join("bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        write_executable(&cargo_bin.join("cargo"));
        let rustup_home = root.path().join("rustup");
        std::fs::create_dir_all(&rustup_home).unwrap();

        let mut profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        tweak(&mut profile);
        Self {
            _root: root,
            jail,
            homes: OperatorHomes::new(cargo_home, rustup_home),
            profile,
        }
    }

    fn broker(&self) -> Arc<RecordingSandboxBroker> {
        Arc::new(RecordingSandboxBroker::new(self.profile.clone()))
    }

    fn host(&self, broker: Arc<RecordingSandboxBroker>) -> InProcessMcpHost {
        self.host_with(
            broker,
            Arc::new(StubPatchApplyBackend),
            McpHostConfig::new(),
        )
    }

    fn host_with(
        &self,
        broker: Arc<dyn SandboxBroker>,
        patch: Arc<dyn PatchApplyBackend>,
        config: McpHostConfig,
    ) -> InProcessMcpHost {
        InProcessMcpHost::new(broker, self.homes.clone(), Vec::new(), patch, config).unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.jail.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The `cargo` the shared RFC-0005 resolver will pick for these homes.
    ///
    /// Mirrors `trusted_path_dirs` ordering so a path-form `ExecAllow` in a
    /// test names the same binary the matcher resolves.
    fn resolved_cargo(&self) -> PathBuf {
        let mut dirs: Vec<PathBuf> = ["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();
        dirs.push(self.homes.cargo_home.join("bin"));
        dirs.into_iter()
            .map(|d| d.join("cargo"))
            .find(|c| c.is_file())
            .expect("a cargo on the trusted path")
            .canonicalize()
            .unwrap()
    }
}

fn write_executable(path: &Path) {
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
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

fn cargo_grant(args_glob: Option<&str>) -> Grant {
    Grant::Exec(ExecAllow {
        binary: "cargo".into(),
        args_glob: args_glob.map(str::to_string),
    })
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall::new(ToolName::new(name).unwrap(), arguments)
}

fn synthetic(exit_code: Option<i32>, signal: Option<i32>) -> SandboxExecResult {
    SandboxExecResult::synthetic(
        exit_code,
        signal,
        SandboxBackend::Landlock,
        Digest::sha256(b"policy"),
    )
}

/// Patch backend that never resolves; used for cancel / timeout / drain tests.
struct PendingPatchBackend;

#[async_trait]
impl PatchApplyBackend for PendingPatchBackend {
    async fn apply(&self, _args: ApplyPatchArgs) -> Result<ApplyPatchOutcome, PatchApplyError> {
        std::future::pending().await
    }
}

/// Broker whose `exec` future never resolves; Drop of the future sets `dropped`.
struct PendingExecBroker {
    profile: SandboxProfile,
    capabilities: SandboxCapabilities,
    entered: std::sync::atomic::AtomicBool,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

struct PendingExecGuard {
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for PendingExecGuard {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl SandboxBroker for PendingExecBroker {
    async fn exec(&self, _req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _guard = PendingExecGuard {
            dropped: Arc::clone(&self.dropped),
        };
        std::future::pending().await
    }

    fn profile(&self) -> &SandboxProfile {
        &self.profile
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }
}

/// Patch backend returning a fixed successful outcome.
struct OkPatchBackend {
    files_touched: Vec<String>,
}

#[async_trait]
impl PatchApplyBackend for OkPatchBackend {
    async fn apply(&self, args: ApplyPatchArgs) -> Result<ApplyPatchOutcome, PatchApplyError> {
        Ok(ApplyPatchOutcome {
            dry_run: args.dry_run,
            files_touched: self.files_touched.clone(),
            transaction_id: None,
            message: "applied".into(),
        })
    }
}

/// Decision log whose writes always fail.
struct FailingDecisionLog;

#[async_trait]
impl DecisionLog for FailingDecisionLog {
    async fn record(
        &self,
        _rec: alloy_runtime::DecisionRecord,
    ) -> Result<alloy_runtime::types::ids::EventSeq, ObsError> {
        Err(ObsError::Invalid("nope".into()))
    }

    async fn record_model_call(
        &self,
        _rec: alloy_runtime::ModelCallRecord,
    ) -> Result<alloy_runtime::types::ids::EventSeq, ObsError> {
        Err(ObsError::Invalid("nope".into()))
    }

    async fn record_tool_call(
        &self,
        _rec: alloy_runtime::ToolCallRecord,
    ) -> Result<alloy_runtime::types::ids::EventSeq, ObsError> {
        Err(ObsError::Invalid("nope".into()))
    }
}

// --- real sandbox -------------------------------------------------------------

/// End-to-end `cargo_check` through the host and a real Landlock jail.
///
/// Skips when Landlock cannot enforce on this host unless
/// `ALLOY_REQUIRE_LANDLOCK=1` — a dishonest green is worse than a skip.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cargo_check_fixture_sandboxed() {
    use alloy_tools::{NativeSandboxBroker, SandboxBroker};

    if which_cargo().is_none() {
        skip_or_panic("cargo not on PATH");
        return;
    }
    let homes = OperatorHomes::resolve().expect("operator homes");

    let fixtures = copy_fixtures_tree();
    let jail = fixtures.path().canonicalize().unwrap();
    let fixture_root = jail.join("sbx_check");
    assert!(fixture_root.join("Cargo.toml").is_file());

    let mut profile = SandboxProfile::default_for_jail(jail).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    profile.exec_timeout = Duration::from_secs(120);
    let broker = match NativeSandboxBroker::with_operator_homes(profile, homes.clone()).await {
        Ok(broker) => broker,
        Err(err) => {
            skip_or_panic(&format!("landlock broker unavailable: {err}"));
            return;
        }
    };

    let host = InProcessMcpHost::new(
        Arc::new(broker) as Arc<dyn SandboxBroker>,
        homes,
        Vec::new(),
        Arc::new(StubPatchApplyBackend),
        McpHostConfig::new(),
    )
    .unwrap();

    let result = host
        .call(
            call(
                "cargo_check",
                json!({ "workspace_root": fixture_root.display().to_string() }),
            ),
            token(vec![cargo_grant(Some("check*"))]),
        )
        .await
        .unwrap();

    assert!(
        !result.is_error(),
        "sandboxed cargo check failed: {}",
        result.content
    );
    assert_eq!(result.content["exit_code"], 0);
    assert_eq!(result.content["backend"], "landlock");
}

#[cfg(target_os = "linux")]
fn skip_or_panic(reason: &str) {
    assert!(
        std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_none(),
        "ALLOY_REQUIRE_LANDLOCK=1 but: {reason}"
    );
    eprintln!("skip: {reason}; set ALLOY_REQUIRE_LANDLOCK=1 to fail");
}

#[cfg(target_os = "linux")]
fn which_cargo() -> Option<String> {
    [
        std::env::var_os("CARGO").map(PathBuf::from),
        Some(PathBuf::from("/usr/bin/cargo")),
        std::env::var_os("CARGO_HOME").map(|h| PathBuf::from(h).join("bin/cargo")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/cargo")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
    .map(|p| p.display().to_string())
}

/// Copy `tests/fixtures` into a unique tempdir so concurrent cargo-check tests
/// never share a writable jail.
#[cfg(target_os = "linux")]
fn copy_fixtures_tree() -> TempDir {
    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            // Fixtures may carry a `target/` from a prior local run.
            if name.as_os_str() == "target" {
                continue;
            }
            let to = dst.join(&name);
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &to)?;
            } else if ty.is_file() {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    copy_dir_all(&src, dir.path()).expect("copy fixtures");
    dir
}

// --- disclosure --------------------------------------------------------------

#[tokio::test]
async fn tools_for_is_lazy_and_sorted() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());

    assert!(host.tools_for(&[]).await.unwrap().is_empty());

    let views = host
        .tools_for(&[
            ToolSelector::tag("sel.edit"),
            ToolSelector::tag("sel.compiler"),
            ToolSelector::name(ToolName::new("fs_read").unwrap()),
        ])
        .await
        .unwrap();
    let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["apply_patch", "cargo_check", "fs_read"]);
    assert!(views.iter().all(|v| v.builtin));
}

#[tokio::test]
async fn no_bash_registered() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());
    for forbidden in ["bash", "sh", "shell", "raw_exec", "graph_query"] {
        let err = host
            .call(call(forbidden, json!({})), token(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::UnknownTool(ref n) if n == forbidden));
    }
}

// --- pipeline ordering -------------------------------------------------------

#[tokio::test]
async fn invalid_args_before_permission() {
    let fx = Fixture::new();
    let broker = fx.broker();
    let host = fx.host(Arc::clone(&broker));

    // Both malformed (missing workspace_root) and ungranted.
    let err = host
        .call(
            call("cargo_check", json!({ "package": "a" })),
            token(vec![]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::InvalidArguments(ref m) if m.contains("workspace_root")));
    assert!(broker.recorded().is_empty());
}

#[tokio::test]
async fn token_expired_precedes_unknown_tool() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());
    let mut perms = token(vec![]);
    perms.expires = Some(Timestamp::now());
    let err = host.call(call("bash", json!({})), perms).await.unwrap_err();
    assert!(matches!(err, McpError::TokenExpired));
}

// --- cargo builtins ----------------------------------------------------------

#[tokio::test]
async fn cargo_check_uses_exec_class_check() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Ok(synthetic(Some(0), None)));
    let host = fx.host(Arc::clone(&broker));

    let result = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(Some("check*"))]),
        )
        .await
        .unwrap();

    assert!(!result.is_error());
    assert_eq!(result.content["exit_code"], 0);
    assert_eq!(result.content["backend"], "landlock");
    assert!(result.content["stdout_utf8"].is_string());

    let recorded = broker.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].class, ExecClass::Check);
    assert_eq!(
        recorded[0].argv,
        vec!["cargo", "check", "--message-format", "json"]
    );
    assert!(recorded[0].env_allow.is_empty());
    assert_eq!(recorded[0].cwd, fx.jail);
}

#[tokio::test]
async fn cargo_test_uses_exec_class_test() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Ok(synthetic(Some(0), None)));
    let host = fx.host(Arc::clone(&broker));

    host.call(
        call(
            "cargo_test",
            json!({ "workspace_root": ".", "jobs": 2, "test_name_filter": "foo" }),
        ),
        token(vec![cargo_grant(Some("test*"))]),
    )
    .await
    .unwrap();

    let recorded = broker.recorded();
    assert_eq!(recorded[0].class, ExecClass::Test);
    assert_eq!(
        recorded[0].argv,
        vec!["cargo", "test", "--jobs", "2", "--", "--nocapture", "foo"]
    );
    assert!(recorded[0].env_allow.is_empty());
}

#[tokio::test]
async fn cargo_check_compile_error_is_tool_result() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Ok(
        synthetic(Some(101), None).with_stdio(b"{}".to_vec(), b"error[E0308]".to_vec())
    ));
    let host = fx.host(Arc::clone(&broker));

    let result = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap();

    assert!(result.is_error());
    assert!(matches!(
        result.error(),
        Some(ToolError::ExecutionFailed {
            exit_code: Some(101),
            signal: None,
            ..
        })
    ));
    assert_eq!(result.content["stderr_utf8"], "error[E0308]");
    assert_eq!(host.metrics().calls_tool_error, 1);
}

#[tokio::test]
async fn signal_execution_failed() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Ok(synthetic(None, Some(9))));
    let host = fx.host(Arc::clone(&broker));

    let result = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap();
    assert!(matches!(
        result.error(),
        Some(ToolError::ExecutionFailed {
            exit_code: None,
            signal: Some(9),
            ..
        })
    ));
}

#[tokio::test]
async fn cargo_check_missing_exec_grant() {
    let fx = Fixture::new();
    let broker = fx.broker();
    let host = fx.host(Arc::clone(&broker));

    let err = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![Grant::FsRead(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "exec"
    ));
    assert!(broker.recorded().is_empty());
    assert_eq!(host.metrics().denials, 1);
}

#[tokio::test]
async fn cargo_check_args_not_allowlisted() {
    let fx = Fixture::new();
    let broker = fx.broker();
    let host = fx.host(Arc::clone(&broker));

    let err = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(Some("test*"))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::ArgsNotAllowlisted)
    ));
    assert!(broker.recorded().is_empty());
}

#[tokio::test]
async fn cargo_check_path_form_exec_allow() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Ok(synthetic(Some(0), None)));
    let host = fx.host(Arc::clone(&broker));

    let grant = Grant::Exec(ExecAllow {
        binary: fx.resolved_cargo().display().to_string(),
        args_glob: Some("check*".into()),
    });
    let result = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![grant]),
        )
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(broker.recorded().len(), 1);
}

#[tokio::test]
async fn cargo_check_cwd_outside_jail_denied() {
    let fx = Fixture::new();
    let broker = fx.broker();
    let host = fx.host(Arc::clone(&broker));

    let outside = tempfile::tempdir().unwrap();
    let err = host
        .call(
            call(
                "cargo_check",
                json!({ "workspace_root": outside.path().display().to_string() }),
            ),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::PathNotCovered(_))
    ));
    assert!(broker.recorded().is_empty());
}

#[tokio::test]
async fn backend_unavailable_surfaces() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Err(SandboxError::BackendUnavailable {
        backend: SandboxBackend::Container,
        message: "/home/op/docker.sock missing".into(),
    }));
    let host = fx.host(Arc::clone(&broker));

    let err = host
        .call(
            call("cargo_test", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Sandbox(_)));
    assert!(!err.to_string().contains("/home"));
}

// --- fs_read -----------------------------------------------------------------

#[tokio::test]
async fn fs_read_workspace_file() {
    let fx = Fixture::new();
    fx.write("src/main.rs", b"fn main() {}");
    let host = fx.host(fx.broker());

    let result = host
        .call(
            call("fs_read", json!({ "path": "src/main.rs" })),
            token(vec![Grant::FsRead(Glob("src/**".into()))]),
        )
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(result.content["path"], "src/main.rs");
    assert_eq!(result.content["text"], "fn main() {}");
    assert_eq!(result.content["bytes"], 12);
    assert_eq!(result.content["truncated"], false);
}

#[tokio::test]
async fn fs_read_dotenv_denied_integration() {
    let fx = Fixture::new();
    fx.write(".env", b"SECRET=1");
    let host = fx.host(fx.broker());

    let err = host
        .call(
            call("fs_read", json!({ "path": ".env" })),
            token(vec![Grant::FsRead(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::PathNotCovered(_))
    ));
    assert!(!err.to_string().contains("SECRET"));
}

#[tokio::test]
async fn fs_read_rejects_outside_jail() {
    let fx = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secrets.txt");
    std::fs::write(&target, b"nope").unwrap();
    let host = fx.host(fx.broker());

    let err = host
        .call(
            call("fs_read", json!({ "path": target.display().to_string() })),
            token(vec![Grant::FsRead(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::PathNotCovered(_))
    ));
}

#[tokio::test]
async fn fs_read_requires_fs_read_grant() {
    let fx = Fixture::new();
    fx.write("README.md", b"hi");
    let host = fx.host(fx.broker());

    let err = host
        .call(
            call("fs_read", json!({ "path": "README.md" })),
            token(vec![Grant::GitWrite]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "fs_read"
    ));

    let err = host
        .call(
            call("fs_read", json!({ "path": "README.md" })),
            token(vec![Grant::FsRead(Glob("src/**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::PathNotCovered(ref p)) if p == "README.md"
    ));
}

#[tokio::test]
async fn fs_read_max_bytes_truncates() {
    let fx = Fixture::new();
    fx.write("big.txt", &b"a".repeat(100));
    let host = fx.host(fx.broker());

    let result = host
        .call(
            call("fs_read", json!({ "path": "big.txt", "max_bytes": 10 })),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap();
    assert_eq!(result.content["truncated"], true);
    assert_eq!(result.content["bytes"], 10);
    assert_eq!(result.content["text"], "aaaaaaaaaa");
}

#[tokio::test]
async fn fs_read_cap_splits_leading_multibyte() {
    let fx = Fixture::new();
    fx.write("uni.txt", "é-rest".as_bytes());
    let host = fx.host(fx.broker());

    let result = host
        .call(
            call("fs_read", json!({ "path": "uni.txt", "max_bytes": 1 })),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(result.content["text"], "");
    assert_eq!(result.content["truncated"], true);
}

#[tokio::test]
async fn fs_read_interior_invalid_utf8() {
    let fx = Fixture::new();
    fx.write("bad.txt", b"ok\xFFbad");
    let host = fx.host(fx.broker());

    let result = host
        .call(
            call("fs_read", json!({ "path": "bad.txt" })),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap();
    assert!(result.is_error());
    assert!(matches!(
        result.error(),
        Some(ToolError::Permanent { code, .. }) if code == "not_utf8"
    ));
    assert!(result.content.get("text").is_none());
}

#[tokio::test]
async fn fs_read_max_bytes_over_hard_max() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());
    let err = host
        .call(
            call(
                "fs_read",
                json!({ "path": "a.txt", "max_bytes": 2_000_000 }),
            ),
            token(vec![Grant::FsRead(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::InvalidArguments(ref m) if m.contains("max_bytes")));
}

#[tokio::test]
async fn fs_read_not_found_code() {
    let fx = Fixture::new();
    // Authorize resolves a missing leaf via its parent, so the grant check
    // passes and the open surfaces `not_found` as a tool-level error.
    let host = fx.host(fx.broker());
    let result = host
        .call(
            call("fs_read", json!({ "path": "missing.txt" })),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap();
    assert!(result.is_error());
    assert!(matches!(
        result.error(),
        Some(ToolError::Permanent { code, .. }) if code == "not_found"
    ));
}

#[tokio::test]
async fn fs_read_directory_is_not_a_file() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.jail.join("src")).unwrap();
    let host = fx.host(fx.broker());
    let result = host
        .call(
            call("fs_read", json!({ "path": "src" })),
            token(vec![Grant::FsRead(Glob("src".into()))]),
        )
        .await
        .unwrap();
    assert!(matches!(
        result.error(),
        Some(ToolError::Permanent { code, .. }) if code == "not_a_file"
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn fs_read_opens_canonical_path() {
    let fx = Fixture::new();
    fx.write("real/target.txt", b"canonical");
    std::os::unix::fs::symlink(fx.jail.join("real/target.txt"), fx.jail.join("link.txt")).unwrap();
    let host = fx.host(fx.broker());

    // The grant must cover the *canonical* jail-relative path, not the link.
    let err = host
        .call(
            call("fs_read", json!({ "path": "link.txt" })),
            token(vec![Grant::FsRead(Glob("link.txt".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::PathNotCovered(ref p)) if p == "real/target.txt"
    ));

    let result = host
        .call(
            call("fs_read", json!({ "path": "link.txt" })),
            token(vec![Grant::FsRead(Glob("real/**".into()))]),
        )
        .await
        .unwrap();
    assert_eq!(result.content["path"], "real/target.txt");
    assert_eq!(result.content["text"], "canonical");
}

#[tokio::test]
async fn permission_fail_closed_no_exec() {
    let fx = Fixture::new();
    let broker = fx.broker();
    fx.write("src/main.rs", b"fn main() {}");
    let host = fx.host(Arc::clone(&broker));

    for (tool, args) in [
        ("cargo_check", json!({ "workspace_root": "." })),
        ("cargo_test", json!({ "workspace_root": "." })),
        ("fs_read", json!({ "path": "src/main.rs" })),
        ("apply_patch", json!({ "patch": "diff" })),
    ] {
        let err = host
            .call(call(tool, args), token(vec![]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::PermissionDenied(_)),
            "{tool} produced {err:?}"
        );
    }
    assert!(broker.recorded().is_empty());
}

// --- apply_patch --------------------------------------------------------------

#[tokio::test]
async fn apply_patch_stub_is_deterministic() {
    let fx = Fixture::new();
    let broker = fx.broker();
    let host = fx.host(Arc::clone(&broker));
    let perms = token(vec![Grant::FsWrite(Glob("**".into()))]);

    for dry_run in [false, true] {
        let result = host
            .call(
                call(
                    "apply_patch",
                    json!({ "patch": "diff", "dry_run": dry_run }),
                )
                .with_call_id("c1"),
                perms.clone(),
            )
            .await
            .unwrap();
        assert!(result.is_error());
        assert_eq!(result.call_id.as_deref(), Some("c1"));
        assert_eq!(
            result.content,
            json!({ "code": "edit_engine_unwired", "dry_run": dry_run })
        );
        assert_eq!(
            result.error(),
            Some(&ToolError::Permanent {
                code: "edit_engine_unwired".into(),
                message: "edit_engine_unwired: apply_patch requires RFC-0008 EditEngine".into(),
            })
        );
    }
    // No exec, no writes.
    assert!(broker.recorded().is_empty());
}

#[tokio::test]
async fn stub_never_writes() {
    let fx = Fixture::new();
    fx.write("src/main.rs", b"fn main() {}");
    let before = std::fs::read(fx.jail.join("src/main.rs")).unwrap();
    let host = fx.host(fx.broker());

    host.call(
        call("apply_patch", json!({ "patch": "--- a\n+++ b\n" })),
        token(vec![Grant::FsWrite(Glob("**".into()))]),
    )
    .await
    .unwrap();

    assert_eq!(
        Digest::sha256(&before),
        Digest::sha256(&std::fs::read(fx.jail.join("src/main.rs")).unwrap())
    );
}

#[tokio::test]
async fn apply_patch_requires_fs_write() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());
    let err = host
        .call(
            call("apply_patch", json!({ "patch": "diff" })),
            token(vec![Grant::FsRead(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "fs_write"
    ));
}

#[tokio::test]
async fn apply_patch_success_and_unsafe_output() {
    let fx = Fixture::new();
    let perms = token(vec![Grant::FsWrite(Glob("**".into()))]);

    let ok_host = fx.host_with(
        fx.broker(),
        Arc::new(OkPatchBackend {
            files_touched: vec!["src/main.rs".into()],
        }),
        McpHostConfig::new(),
    );
    let result = ok_host
        .call(
            call("apply_patch", json!({ "patch": "diff" })),
            perms.clone(),
        )
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(result.content["files_touched"][0], "src/main.rs");
    assert_eq!(ok_host.metrics().calls_ok, 1);

    let bad_host = fx.host_with(
        fx.broker(),
        Arc::new(OkPatchBackend {
            files_touched: vec!["/etc/passwd".into()],
        }),
        McpHostConfig::new(),
    );
    let result = bad_host
        .call(call("apply_patch", json!({ "patch": "diff" })), perms)
        .await
        .unwrap();
    assert!(result.is_error());
    assert_eq!(result.content, json!({ "code": "unsafe_backend_output" }));
}

// --- lifecycle ----------------------------------------------------------------

#[tokio::test]
async fn drain_grace_then_cancel() {
    let fx = Fixture::new();
    let host = Arc::new(fx.host_with(
        fx.broker(),
        Arc::new(PendingPatchBackend),
        McpHostConfig::new(),
    ));

    let caller = Arc::clone(&host);
    let inflight = tokio::spawn(async move {
        caller
            .call(
                call("apply_patch", json!({ "patch": "diff" })),
                token(vec![Grant::FsWrite(Glob("**".into()))]),
            )
            .await
    });

    // Wait until the call is admitted and parked on the pending backend.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.metrics().in_flight != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "in-flight call was never admitted"
        );
        tokio::task::yield_now().await;
    }

    host.drain(Duration::from_millis(50)).await.unwrap();
    assert_eq!(host.phase(), McpHostPhase::Stopped);

    let outcome = tokio::time::timeout(Duration::from_secs(5), inflight)
        .await
        .expect("in-flight call must unwind")
        .unwrap();
    assert!(matches!(outcome, Err(McpError::Cancelled)));
}

#[tokio::test]
async fn tools_for_during_drain() {
    let fx = Fixture::new();
    let host = fx.host(fx.broker());
    host.drain(Duration::from_millis(10)).await.unwrap();
    assert!(matches!(
        host.tools_for(&[ToolSelector::tag("sel.fs")]).await,
        Err(McpError::ShuttingDown)
    ));
}

/// `call_timeout` covers builtins that never touch the sandbox.
///
/// The clock is paused and the deadline is already elapsed, while the read is
/// large enough to need many chunked round trips — so the timeout wins without
/// depending on wall-clock timing.
#[tokio::test(start_paused = true)]
async fn host_timeout_fs_read() {
    // A zero `exec_timeout` is what lets the host accept a zero `call_timeout`.
    let fx = Fixture::with_profile(|p| p.exec_timeout = Duration::ZERO);
    fx.write("big.txt", &b"a".repeat(2 * 1024 * 1024));
    let host = fx.host_with(
        fx.broker(),
        Arc::new(StubPatchApplyBackend),
        McpHostConfig::new().with_call_timeout(Duration::ZERO),
    );

    let err = host
        .call(
            call(
                "fs_read",
                json!({ "path": "big.txt", "max_bytes": 1_048_576 }),
            ),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Timeout(d) if d == Duration::ZERO));
    assert_eq!(host.metrics().calls_mcp_error, 1);
}

/// The same wrapper bounds the patch backend, with no filesystem race at all.
#[tokio::test(start_paused = true)]
async fn host_timeout_apply_patch() {
    let fx = Fixture::with_profile(|p| p.exec_timeout = Duration::from_millis(10));
    let host = fx.host_with(
        fx.broker(),
        Arc::new(PendingPatchBackend),
        McpHostConfig::new().with_call_timeout(Duration::from_millis(10)),
    );

    let err = host
        .call(
            call("apply_patch", json!({ "patch": "diff" })),
            token(vec![Grant::FsWrite(Glob("**".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Timeout(d) if d == Duration::from_millis(10)));
}

#[tokio::test]
async fn cancel_by_drop_no_orphan() {
    let fx = Fixture::new();
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let broker = Arc::new(PendingExecBroker {
        profile: fx.profile.clone(),
        capabilities: SandboxCapabilities {
            landlock: BackendStatus::Available {
                detail: "pending".into(),
            },
            seatbelt: BackendStatus::Available {
                detail: "pending".into(),
            },
            container: BackendStatus::Available {
                detail: "pending".into(),
            },
        },
        entered: std::sync::atomic::AtomicBool::new(false),
        dropped: Arc::clone(&dropped),
    });
    let host = fx
        .host_with(
            Arc::clone(&broker) as Arc<dyn SandboxBroker>,
            Arc::new(StubPatchApplyBackend),
            McpHostConfig::new(),
        )
        .with_decision_log(log.clone());

    let mut pending = Box::pin(host.call(
        call("cargo_check", json!({ "workspace_root": "." })).with_attribution(
            Some(SessionId::new()),
            None,
            None,
        ),
        token(vec![cargo_grant(None)]),
    ));
    // Drive until the broker exec future is polled, then drop the call.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !broker.entered.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "exec future was never entered"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err(),
            "call completed before exec was cancelled"
        );
    }
    drop(pending);

    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "dropping the call must drop the nested SandboxBroker::exec future"
    );
    assert_eq!(host.metrics().in_flight, 0);
    assert_eq!(host.metrics().calls_mcp_error, 0);
    assert!(log.recorded_tool_calls().is_empty());
    assert_eq!(host.phase(), McpHostPhase::Running);
}

#[tokio::test]
async fn concurrent_calls_semaphore() {
    let fx = Fixture::new();
    fx.write("a.txt", b"a");
    fx.write("b.txt", b"b");
    let host = Arc::new(fx.host_with(
        fx.broker(),
        Arc::new(StubPatchApplyBackend),
        McpHostConfig::new().with_max_in_flight(1),
    ));

    let perms = token(vec![Grant::FsRead(Glob("*.txt".into()))]);
    let mut tasks = Vec::new();
    for name in ["a.txt", "b.txt"] {
        let host = Arc::clone(&host);
        let perms = perms.clone();
        tasks.push(tokio::spawn(async move {
            host.call(call("fs_read", json!({ "path": name })), perms)
                .await
        }));
    }
    for task in tasks {
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("no deadlock")
            .unwrap()
            .unwrap();
        assert!(!result.is_error());
    }
    assert_eq!(host.metrics().calls_ok, 2);
    assert_eq!(host.metrics().in_flight, 0);
}

#[tokio::test]
async fn metrics_snapshot_counts() {
    let fx = Fixture::new();
    fx.write("a.txt", b"a");
    let broker = fx.broker();
    broker.push(Ok(synthetic(Some(1), None)));
    let host = fx.host(Arc::clone(&broker));

    host.call(
        call("fs_read", json!({ "path": "a.txt" })),
        token(vec![Grant::FsRead(Glob("*.txt".into()))]),
    )
    .await
    .unwrap();
    host.call(
        call("cargo_check", json!({ "workspace_root": "." })),
        token(vec![cargo_grant(None)]),
    )
    .await
    .unwrap();
    let _ = host
        .call(call("fs_read", json!({ "path": "a.txt" })), token(vec![]))
        .await;
    let _ = host.call(call("bash", json!({})), token(vec![])).await;

    let metrics = host.metrics();
    assert_eq!(metrics.calls_ok, 1);
    assert_eq!(metrics.calls_tool_error, 1);
    assert_eq!(metrics.calls_mcp_error, 2);
    assert_eq!(metrics.denials, 1);
    assert_eq!(metrics.in_flight, 0);
}

// --- observability --------------------------------------------------------------

#[tokio::test]
async fn decision_log_contract() {
    let fx = Fixture::new();
    fx.write("a.txt", b"a");
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let host = fx.host(fx.broker()).with_decision_log(log.clone());

    let session = SessionId::new();
    let run = RunId::new();
    host.call(
        call("fs_read", json!({ "path": "a.txt" })).with_attribution(
            Some(session),
            Some(run),
            None,
        ),
        token(vec![Grant::FsRead(Glob("*.txt".into()))]),
    )
    .await
    .unwrap();

    // No session attribution → skipped entirely.
    host.call(
        call("fs_read", json!({ "path": "a.txt" })),
        token(vec![Grant::FsRead(Glob("*.txt".into()))]),
    )
    .await
    .unwrap();

    // Permission denial → denied = true.
    let _ = host
        .call(
            call("fs_read", json!({ "path": "a.txt" })).with_attribution(Some(session), None, None),
            token(vec![]),
        )
        .await;

    // Unknown tool is not a grant denial.
    let _ = host
        .call(
            call("bash", json!({})).with_attribution(Some(session), None, None),
            token(vec![]),
        )
        .await;

    let records = log.recorded_tool_calls();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].session, session);
    assert_eq!(records[0].run, Some(run));
    assert_eq!(records[0].tool_name, "fs_read");
    assert_eq!(records[0].tool_server.as_deref(), Some("alloy.builtins"));
    assert!(records[0].latency_ms.is_some());
    assert!(!records[0].denied);
    assert!(records[0].content_hash.is_none());
    assert!(records[0].body.is_none());
    assert!(records[1].denied);
    assert!(!records[2].denied);
}

#[tokio::test]
async fn denied_flag_on_quarantine() {
    let fx = Fixture::new();
    let broker = fx.broker();
    broker.push(Err(SandboxError::Denied(
        alloy_tools::DenialReason::QuarantineBlocked("fetch".into()),
    )));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let host = fx.host(Arc::clone(&broker)).with_decision_log(log.clone());

    let err = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })).with_attribution(
                Some(SessionId::new()),
                None,
                None,
            ),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        McpError::PermissionDenied(PermissionDenial::MissingGrant(ref k)) if k == "quarantine"
    ));
    assert!(log.recorded_tool_calls()[0].denied);
}

/// Captures `mcp permission denied` warn targets so §9.1 coverage is asserted
/// for both prepare-time and broker-mapped denials.
struct DenyWarnCapture {
    msgs: Arc<std::sync::Mutex<Vec<String>>>,
}

impl<S> tracing_subscriber::Layer<S> for DenyWarnCapture
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut MessageVisitor(&mut message));
        if message.contains("mcp permission denied") {
            self.msgs.lock().unwrap().push(message);
        }
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

#[tokio::test]
async fn permission_deny_warns_prepare_and_broker() {
    let msgs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(DenyWarnCapture {
        msgs: Arc::clone(&msgs),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let fx = Fixture::new();
    fx.write(".env", b"SECRET=1");

    // Prepare-time: PathPolicy deny on `.env` before any open.
    let host = fx.host(fx.broker());
    let prep_err = host
        .call(
            call("fs_read", json!({ "path": ".env" })),
            token(vec![Grant::FsRead(Glob(".env".into()))]),
        )
        .await
        .unwrap_err();
    assert!(matches!(prep_err, McpError::PermissionDenied(_)));

    // Broker-mapped: quarantine denial after dispatch starts exec.
    let broker = fx.broker();
    broker.push(Err(SandboxError::Denied(
        alloy_tools::DenialReason::QuarantineBlocked("fetch".into()),
    )));
    let host = fx.host(Arc::clone(&broker));
    let broker_err = host
        .call(
            call("cargo_check", json!({ "workspace_root": "." })),
            token(vec![cargo_grant(None)]),
        )
        .await
        .unwrap_err();
    assert!(matches!(broker_err, McpError::PermissionDenied(_)));

    let captured = msgs.lock().unwrap().clone();
    assert!(
        captured.len() >= 2,
        "expected prepare-time and broker-mapped deny warns, got {captured:?}"
    );
}

#[tokio::test]
async fn obs_failure_does_not_fail_call() {
    let fx = Fixture::new();
    fx.write("a.txt", b"a");
    let host = fx
        .host(fx.broker())
        .with_decision_log(Arc::new(FailingDecisionLog));

    let result = host
        .call(
            call("fs_read", json!({ "path": "a.txt" })).with_attribution(
                Some(SessionId::new()),
                None,
                None,
            ),
            token(vec![Grant::FsRead(Glob("*.txt".into()))]),
        )
        .await
        .unwrap();
    assert!(!result.is_error());
}
