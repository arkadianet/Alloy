//! Integration tests for RFC-0005 Sandbox Broker.
//!
//! Landlock tests are required on Linux CI when `ALLOY_REQUIRE_LANDLOCK=1`.
//! Without that env var, Unavailable hosts skip (local nested-userns) rather
//! than reporting a dishonest green pass for enforcement.
//!
//! Operator homes are injected via `NativeSandboxBroker::with_operator_homes`
//! (dependency injection) — no process-global CARGO_HOME mutation.

#![allow(clippy::disallowed_methods)] // positive baselines may use host Command

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use alloy_runtime::Timestamp;
use alloy_runtime::{ExecAllow, Grant, PermissionToken, ProfileId, RunId};
use alloy_tools::{
    load_sandbox_profile, BackendStatus, ExecClass, NativeSandboxBroker, NetworkPolicy,
    OperatorHomes, SandboxBackend, SandboxBroker, SandboxError, SandboxExecRequest, SandboxProfile,
};
use tempfile::tempdir;

fn token(binary: &str, args_glob: Option<&str>) -> PermissionToken {
    PermissionToken {
        profile: ProfileId::new("default").unwrap(),
        grants: vec![Grant::Exec(ExecAllow {
            binary: binary.into(),
            args_glob: args_glob.map(str::to_string),
        })],
        expires: None,
        run_id: RunId::new(),
    }
}

fn sh_bin() -> &'static str {
    if PathBuf::from("/bin/sh").exists() {
        "/bin/sh"
    } else {
        "/usr/bin/sh"
    }
}

#[cfg(target_os = "linux")]
fn true_bin() -> &'static str {
    if PathBuf::from("/bin/true").exists() {
        "/bin/true"
    } else {
        "/usr/bin/true"
    }
}

#[cfg(target_os = "linux")]
fn sleep_bin() -> &'static str {
    if PathBuf::from("/bin/sleep").exists() {
        "/bin/sleep"
    } else {
        "/usr/bin/sleep"
    }
}

#[cfg(target_os = "linux")]
fn require_landlock() -> bool {
    std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
}

#[cfg(target_os = "macos")]
fn require_seatbelt() -> bool {
    std::env::var_os("ALLOY_REQUIRE_SEATBELT").is_some()
}

/// Returns `true` when Seatbelt is Available. When required by CI, panics if not.
#[cfg(target_os = "macos")]
async fn seatbelt_or_skip() -> bool {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Seatbelt;
    profile.test_backend = SandboxBackend::Seatbelt;
    let (available, detail) = match NativeSandboxBroker::new(profile).await {
        Ok(b) => match &b.capabilities().seatbelt {
            BackendStatus::Available { detail } => (true, detail.clone()),
            BackendStatus::Unavailable { reason } => (false, reason.clone()),
            other => (false, format!("{other:?}")),
        },
        Err(SandboxError::BackendUnavailable { message, .. }) => (false, message),
        Err(SandboxError::UnsupportedOs) => (false, "unsupported OS".into()),
        Err(e) => panic!("unexpected broker error: {e:?}"),
    };
    if available {
        return true;
    }
    if require_seatbelt() {
        panic!("ALLOY_REQUIRE_SEATBELT=1 but Seatbelt is Unavailable: {detail}");
    }
    eprintln!("skip: seatbelt unavailable ({detail}); set ALLOY_REQUIRE_SEATBELT=1 to fail");
    false
}

/// Returns `true` when Landlock is Available. When required by CI, panics if not.
#[cfg(target_os = "linux")]
async fn landlock_or_skip() -> bool {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    let (available, detail) = match NativeSandboxBroker::new(profile).await {
        Ok(b) => match &b.capabilities().landlock {
            BackendStatus::Available { detail } => (true, detail.clone()),
            BackendStatus::Unavailable { reason } => (false, reason.clone()),
            other => (false, format!("{other:?}")),
        },
        Err(e) => (false, format!("NativeSandboxBroker::new: {e}")),
    };
    if available {
        return true;
    }
    if require_landlock() {
        panic!(
            "ALLOY_REQUIRE_LANDLOCK=1 but Landlock is Unavailable: {detail} \
             (need unprivileged userns identity maps + Landlock ABI >= 2; \
             on ubuntu-24.04 set kernel.apparmor_restrict_unprivileged_userns=0)"
        );
    }
    eprintln!("skip: landlock unavailable ({detail}); set ALLOY_REQUIRE_LANDLOCK=1 to fail");
    false
}

#[cfg(target_os = "linux")]
async fn broker_for_jail(jail: PathBuf) -> Result<NativeSandboxBroker, SandboxError> {
    let mut profile = SandboxProfile::default_for_jail(jail)?;
    profile.check_backend = SandboxBackend::Landlock;
    profile.test_backend = SandboxBackend::Container;
    profile.network = NetworkPolicy::Deny;
    profile.exec_timeout = Duration::from_secs(30);
    NativeSandboxBroker::new(profile).await
}

#[cfg(target_os = "linux")]
async fn broker_for_jail_with_homes(
    jail: PathBuf,
    homes: OperatorHomes,
) -> Result<NativeSandboxBroker, SandboxError> {
    let mut profile = SandboxProfile::default_for_jail(jail)?;
    profile.check_backend = SandboxBackend::Landlock;
    profile.test_backend = SandboxBackend::Container;
    profile.network = NetworkPolicy::Deny;
    profile.exec_timeout = Duration::from_secs(30);
    NativeSandboxBroker::with_operator_homes(profile, homes).await
}

/// Shared default image tag for container tests (matches profile fallback).
#[cfg(target_os = "linux")]
fn default_container_image() -> String {
    std::env::var("ALLOY_CONTAINER_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/rust:1.97.1-bookworm".into())
}

/// Copy `tests/fixtures` into a unique tempdir so concurrent cargo-check tests
/// do not share a writable jail.
#[cfg(target_os = "linux")]
fn copy_fixtures_tree() -> tempfile::TempDir {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempdir().unwrap();
    copy_dir_all(&src, dir.path()).expect("copy fixtures");
    dir
}

#[cfg(target_os = "linux")]
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let ty = ent.file_type()?;
        let to = dst.join(ent.file_name());
        if ty.is_dir() {
            copy_dir_all(&ent.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(ent.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn python3_bin() -> Option<PathBuf> {
    // Resolve via this process's PATH; sandbox PATH is scrubbed.
    let out = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn chmod_755(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(path, p).unwrap();
    }
}

#[cfg(target_os = "linux")]
fn process_alive(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
}

#[cfg(target_os = "linux")]
fn python3_ok() -> bool {
    python3_bin().is_some_and(|p| {
        Command::new(&p)
            .arg("-c")
            .arg("1")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[tokio::test]
#[cfg(not(target_os = "macos"))]
async fn backend_unavailable_fail_closed() {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    // Force Seatbelt off-macOS → UnsupportedOs / Unavailable at construction.
    profile.check_backend = SandboxBackend::Seatbelt;
    let err = NativeSandboxBroker::new(profile).await.unwrap_err();
    assert!(
        matches!(
            err,
            SandboxError::UnsupportedOs
                | SandboxError::BackendUnavailable {
                    backend: SandboxBackend::Seatbelt,
                    ..
                }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn backend_unavailable_fail_closed() {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    // Force Landlock on macOS → Unavailable / UnsupportedOs.
    profile.check_backend = SandboxBackend::Landlock;
    let err = NativeSandboxBroker::new(profile).await.unwrap_err();
    assert!(
        matches!(
            err,
            SandboxError::UnsupportedOs
                | SandboxError::BackendUnavailable {
                    backend: SandboxBackend::Landlock,
                    ..
                }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn landlock_actually_applied() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    let outer = tempdir().unwrap();
    let sentinel = outer.path().join("secret.txt");
    std::fs::write(&sentinel, b"outside-secret").unwrap();
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "outside-secret"
    );

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    assert!(matches!(
        broker.capabilities().landlock,
        BackendStatus::Available { .. }
    ));

    let script = jail.join("try_read.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat '{}'\n", sentinel.display()),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail.clone(),
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_ne!(
        result.exit_code,
        Some(0),
        "sandboxed read of outside sentinel should fail; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("outside-secret"),
        "sentinel contents leaked via stdout: {stdout:?}"
    );
}

/// ABI v3+ `truncate(2)` must be handled; otherwise a sandboxed payload can
/// zero operator-writable files outside the jail without open(O_WRONLY).
#[tokio::test]
#[cfg(target_os = "linux")]
async fn landlock_denies_outside_jail_truncate() {
    if !landlock_or_skip().await {
        return;
    }
    if !python3_ok() {
        if require_landlock() {
            panic!("ALLOY_REQUIRE_LANDLOCK=1 but python3 missing for truncate PoC");
        }
        eprintln!("skip: python3 unavailable for truncate PoC");
        return;
    }

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let outer = tempdir().unwrap();
    let sentinel = outer.path().join("victim.txt");
    let original = b"do-not-truncate-me";
    std::fs::write(&sentinel, original).unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let script = jail.join("try_trunc.py");
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/env python3\nimport os\ntry:\n    os.truncate({path:?}, 0)\n    print('TRUNCATE_OK')\nexcept OSError as e:\n    print(f'TRUNCATE_DENIED:{{e.errno}}')\n",
            path = sentinel.display()
        ),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec!["python3".into(), script.display().to_string()],
        jail,
        token("python3", None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        !stdout.contains("TRUNCATE_OK"),
        "outside-jail truncate(2) must be denied: stdout={stdout:?} stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        original,
        "outside-jail sentinel must not be truncated"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn deny_walk_budget_blocks_exec() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    // Blow the 10k entry budget with many sibling files; place `.env` last in
    // name order so a truncated walk would miss it.
    for i in 0..10_050 {
        std::fs::write(jail.join(format!("pad-{i:05}")), b"x").unwrap();
    }
    std::fs::write(jail.join("zzzz.env"), b"nope").unwrap(); // not a deny pattern
    std::fs::write(jail.join(".env"), b"SECRET=1\n").unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let req = SandboxExecRequest::new(
        vec![true_bin().into()],
        jail,
        token(true_bin(), None),
        ExecClass::Check,
    );
    let err = broker.exec(req).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::BackendCannotEnforce(ref m) if m.contains("exceeded")),
        "budget overrun must fail closed, got {err:?}"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn child_cannot_read_dotenv_in_jail() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let dotenv = jail.join(".env");
    std::fs::write(&dotenv, b"SUPER_SECRET=1\n").unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let script = jail.join("read_env.sh");
    std::fs::write(&script, "#!/bin/sh\ncat .env\n").unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail.clone(),
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        !stdout.contains("SUPER_SECRET"),
        "child read .env contents: {stdout:?} exit={:?}",
        result.exit_code
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn dotenv_sentinel_unchanged() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let dotenv = jail.join(".env");
    let original = b"HOST_ENV_SENTINEL=keep\n";
    std::fs::write(&dotenv, original).unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let req = SandboxExecRequest::new(
        vec![true_bin().into()],
        jail.clone(),
        token(true_bin(), None),
        ExecClass::Check,
    );
    let _ = broker.exec(req).await.unwrap();

    let after = std::fs::read(&dotenv).unwrap();
    assert_eq!(after, original, "host .env must remain untouched");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn credentials_sentinel_unchanged() {
    if !landlock_or_skip().await {
        return;
    }

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    let cargo_home = dir.path().join("fake-cargo");
    let rustup_home = dir.path().join("fake-rustup");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::create_dir_all(&rustup_home).unwrap();
    let creds = cargo_home.join("credentials.toml");
    let original = b"token = \"sentinel\"\n";
    std::fs::write(&creds, original).unwrap();

    // Inject operator homes — no process-global CARGO_HOME mutation.
    let broker = broker_for_jail_with_homes(
        jail.clone(),
        OperatorHomes::new(cargo_home.clone(), rustup_home),
    )
    .await
    .unwrap();
    let script = jail.join("try_creds.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\n", creds.display())).unwrap();
    chmod_755(&script);
    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(!stdout.contains("sentinel"), "creds leaked: {stdout}");

    let after = std::fs::read(&creds).unwrap();
    assert_eq!(after, original);
}

/// File RO roots (rustup `settings.toml`) must not receive WriteFile/Truncate.
#[tokio::test]
#[cfg(target_os = "linux")]
async fn landlock_ro_settings_toml_not_writable() {
    if !landlock_or_skip().await {
        return;
    }

    let jail_dir = tempdir().unwrap();
    let jail = jail_dir.path().canonicalize().unwrap();
    // Homes must sit *outside* the jail — otherwise jail RW covers settings.toml.
    let homes_dir = tempdir().unwrap();
    let cargo_home = homes_dir.path().join("fake-cargo");
    let rustup_home = homes_dir.path().join("fake-rustup");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::create_dir_all(&rustup_home).unwrap();
    let settings = rustup_home.join("settings.toml");
    let original = b"default_toolchain = \"stable\"\n";
    std::fs::write(&settings, original).unwrap();

    let broker =
        broker_for_jail_with_homes(jail.clone(), OperatorHomes::new(cargo_home, rustup_home))
            .await
            .unwrap();

    let script = jail.join("try_write_settings.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'pwned' > '{s}' 2>/dev/null && echo WRITE_OK || echo WRITE_DENIED\n",
            s = settings.display()
        ),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        !stdout.contains("WRITE_OK"),
        "settings.toml must stay RO: stdout={stdout:?}"
    );
    assert_eq!(
        std::fs::read(&settings).unwrap(),
        original,
        "settings.toml must not be overwritten"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn network_deny_blocks_egress() {
    if !landlock_or_skip().await {
        return;
    }
    if !python3_ok() {
        if require_landlock() {
            panic!("python3 required for network_deny_blocks_egress under ALLOY_REQUIRE_LANDLOCK");
        }
        eprintln!("skip: python3 unavailable for network deny test");
        return;
    }

    let py = python3_bin().expect("python3_ok ensured a path");
    // Positive baseline: host can reach a local listener (RFC §11).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    let baseline = Command::new(&py)
        .arg("-c")
        .arg(format!(
            "import socket; s=socket.create_connection(('127.0.0.1', {port}), 1); s.close()"
        ))
        .status()
        .expect("python3 baseline");
    assert!(
        baseline.success(),
        "positive baseline: host must reach local listener before deny"
    );
    // Keep `listener` alive through the sandboxed connect: under netns the
    // child's 127.0.0.1 is not the host's, so the connect must still fail.

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();

    let script = jail.join("net.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
'{py}' - <<'PY'
import socket,sys
s=socket.socket()
s.settimeout(1)
try:
    s.connect(("127.0.0.1", {port}))
    sys.exit(0)
except Exception:
    sys.exit(2)
PY
"#,
            py = py.display(),
            port = port
        ),
    )
    .unwrap();
    chmod_755(&script);
    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(2),
        "egress to host listener should fail under network deny; stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    drop(listener);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn cancel_drop_no_orphan() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();

    let pidfile = jail.join("child.pid");
    let script = jail.join("sleep.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec '{}' 60\n",
            pidfile.display(),
            sleep_bin()
        ),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let handle = tokio::spawn(async move { broker.exec(req).await });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Ok(s) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                break pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child never wrote pidfile"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(process_alive(pid), "child {pid} never ran");

    handle.abort();
    let _ = handle.await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} survived abort/drop of the exec future"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn timeout_kills_process_group() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let mut profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    profile.exec_timeout = Duration::from_millis(400);
    let broker = NativeSandboxBroker::new(profile).await.unwrap();

    let pidfile = jail.join("grandchild.pid");
    let script = jail.join("group.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsh -c 'trap \"\" TERM; while :; do sleep 1; done' &\necho $! > '{}'\nexec '{}' 60\n",
            pidfile.display(),
            sleep_bin()
        ),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let err = broker.exec(req).await.unwrap_err();
    assert!(matches!(err, SandboxError::Timeout(_)), "got {err:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let alive = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .is_some_and(process_alive);
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild survived timeout process-group kill"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn token_expired_via_exec() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();

    let mut perms = token(true_bin(), None);
    perms.expires = Some(Timestamp(Timestamp::now().0));

    let req = SandboxExecRequest::new(vec![true_bin().into()], jail, perms, ExecClass::Check);
    let err = broker.exec(req).await.unwrap_err();
    assert!(matches!(err, SandboxError::TokenExpired), "got {err:?}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn child_cannot_umount_dotenv_bind() {
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let dotenv = jail.join(".env");
    std::fs::write(&dotenv, b"SUPER_SECRET=1\n").unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let script = jail.join("umount_env.sh");
    // Attempt to undo the /dev/null bind, then read. CAP_SYS_ADMIN drop must
    // make umount fail so the secret stays hidden. Record whether umount was
    // found and refused (do not swallow missing-command via `|| true`).
    std::fs::write(
        &script,
        r#"#!/bin/sh
if command -v umount >/dev/null 2>&1; then
  if umount .env 2>/dev/null; then
    echo UMOUNT_OK
  else
    echo UMOUNT_DENIED
  fi
elif command -v umount2 >/dev/null 2>&1; then
  if umount2 .env 2>/dev/null; then
    echo UMOUNT_OK
  else
    echo UMOUNT_DENIED
  fi
else
  echo UMOUNT_MISSING
fi
cat .env
"#,
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("UMOUNT_DENIED"),
        "expected umount refusal marker; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("SUPER_SECRET"),
        "umount undid deny bind; leaked: {stdout:?}"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn landlock_cargo_check_fixture() {
    if !landlock_or_skip().await {
        return;
    }
    let cargo = match which_cargo() {
        Some(c) => c,
        None => {
            if require_landlock() {
                panic!("cargo required for landlock_cargo_check_fixture");
            }
            eprintln!("skip: cargo not on PATH");
            return;
        }
    };

    // Jail must cover both the fixture crate and its path dependency.
    // Copy into a unique tempdir so this test does not race container_cargo_check.
    let fixtures_tmp = copy_fixtures_tree();
    let fixtures = fixtures_tmp.path().canonicalize().expect("fixtures canon");
    let fixture_root = fixtures.join("sbx_check");
    assert!(
        fixture_root.join("Cargo.toml").is_file(),
        "missing fixture at {}",
        fixture_root.display()
    );
    let jail = fixtures.clone();
    let cwd = fixture_root.canonicalize().expect("fixture canonicalize");

    let broker = broker_for_jail(jail).await.unwrap();
    let req = SandboxExecRequest::new(
        vec![
            cargo.clone(),
            "check".into(),
            "--offline".into(),
            "--quiet".into(),
        ],
        cwd,
        token(&cargo, Some("check*")),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "sandboxed cargo check --offline failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn landlock_cargo_version_fixture() {
    if !landlock_or_skip().await {
        return;
    }
    let cargo = which_cargo();
    let Some(cargo) = cargo else {
        if require_landlock() {
            panic!("cargo required for landlock_cargo_version_fixture");
        }
        eprintln!("skip: cargo not on PATH");
        return;
    };

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let req = SandboxExecRequest::new(
        vec![cargo.clone(), "--version".into()],
        jail,
        token(&cargo, Some("--version*")),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.to_ascii_lowercase().contains("cargo"),
        "unexpected cargo --version output: {stdout:?}"
    );
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn seatbelt_cargo_check_fixture() {
    if !seatbelt_or_skip().await {
        return;
    }
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let fixture_root = fixtures.join("sbx_check");
    assert!(fixture_root.join("Cargo.toml").is_file());
    let jail = fixtures.canonicalize().unwrap();
    let cwd = fixture_root.canonicalize().unwrap();

    let mut profile = SandboxProfile::default_for_jail(jail).unwrap();
    profile.check_backend = SandboxBackend::Seatbelt;
    profile.test_backend = SandboxBackend::Seatbelt;
    let broker = NativeSandboxBroker::new(profile)
        .await
        .expect("seatbelt available");
    assert!(matches!(
        broker.capabilities().seatbelt,
        BackendStatus::Available { .. }
    ));

    let cargo = which_cargo().expect("cargo on PATH");
    let req = SandboxExecRequest::new(
        vec![
            cargo.clone(),
            "check".into(),
            "--offline".into(),
            "--quiet".into(),
        ],
        cwd,
        token(&cargo, Some("check*")),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "seatbelt cargo check failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn seatbelt_denies_outside_jail_read() {
    if !seatbelt_or_skip().await {
        return;
    }
    let outer = tempdir().unwrap();
    let sentinel = outer.path().join("secret.txt");
    std::fs::write(&sentinel, b"outside-secret").unwrap();

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let mut profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
    profile.check_backend = SandboxBackend::Seatbelt;
    let broker = NativeSandboxBroker::new(profile)
        .await
        .expect("seatbelt available");

    let script = jail.join("try_read.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat '{}'\n", sentinel.display()),
    )
    .unwrap();
    chmod_755(&script);

    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        !stdout.contains("outside-secret"),
        "seatbelt leaked outside-jail read: {stdout:?}"
    );
    assert_ne!(result.exit_code, Some(0));
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn container_cargo_check_fixture() {
    let fixtures_tmp = copy_fixtures_tree();
    let fixtures = fixtures_tmp.path().canonicalize().unwrap();
    let fixture_root = fixtures.join("sbx_check");
    let jail = fixtures.clone();
    let cwd = fixture_root.canonicalize().unwrap();

    let mut profile = SandboxProfile::default_for_jail(jail).unwrap();
    profile.check_backend = SandboxBackend::Container;
    profile.test_backend = SandboxBackend::Container;
    profile.container_image = default_container_image();

    let broker = match NativeSandboxBroker::new(profile).await {
        Ok(b) => b,
        Err(SandboxError::BackendUnavailable { .. }) => {
            eprintln!("skip: container runtime unavailable");
            return;
        }
        Err(e) => panic!("unexpected: {e:?}"),
    };
    if !matches!(
        broker.capabilities().container,
        BackendStatus::Available { .. }
    ) {
        eprintln!("skip: container probe Unavailable");
        return;
    }

    let req = SandboxExecRequest::new(
        vec![
            "cargo".into(),
            "check".into(),
            "--offline".into(),
            "--quiet".into(),
        ],
        cwd,
        token("cargo", Some("check*")),
        ExecClass::Test,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "container cargo check failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn container_backend_probe_status() {
    // Always runs: asserts the container probe returns a typed status rather
    // than panicking. When a runtime is present the optional CI job can go
    // further via container_runtime_smoke.
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    profile.test_backend = SandboxBackend::Container;
    // Landlock may be unavailable → new fails; that's fine for this probe peek.
    match NativeSandboxBroker::new(profile).await {
        Ok(b) => {
            let _ = &b.capabilities().container;
        }
        Err(SandboxError::BackendUnavailable { .. }) | Err(SandboxError::UnsupportedOs) => {}
        Err(e) => panic!("unexpected new error: {e:?}"),
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn container_runtime_smoke() {
    // Optional: only asserts when docker/podman is Available.
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let mut profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
    profile.check_backend = SandboxBackend::Container;
    profile.test_backend = SandboxBackend::Container;
    profile.container_image = default_container_image();

    let broker = match NativeSandboxBroker::new(profile).await {
        Ok(b) => b,
        Err(SandboxError::BackendUnavailable { .. }) => {
            eprintln!("skip: container runtime unavailable");
            return;
        }
        Err(e) => panic!("unexpected: {e:?}"),
    };
    if !matches!(
        broker.capabilities().container,
        BackendStatus::Available { .. }
    ) {
        eprintln!("skip: container probe Unavailable");
        return;
    }

    let req = SandboxExecRequest::new(
        vec!["/bin/true".into()],
        jail,
        token("/bin/true", None),
        ExecClass::Test,
    );
    let result = broker.exec(req).await.unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "container true failed: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn load_sandbox_from_workspace_profile() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let profile_path = root.join("profiles/default.toml");
    let jail = tempdir().unwrap();
    let profile = load_sandbox_profile(&profile_path, jail.path().to_path_buf()).unwrap();
    assert_eq!(profile.network, NetworkPolicy::Deny);
    assert!(profile.quarantine_deps);
}

fn which_cargo() -> Option<String> {
    for p in [
        std::env::var_os("CARGO").map(PathBuf::from),
        Some(PathBuf::from("/usr/bin/cargo")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/cargo")),
    ]
    .into_iter()
    .flatten()
    {
        if p.is_file() {
            return Some(p.display().to_string());
        }
    }
    None
}
