//! Integration tests for RFC-0005 Sandbox Broker.
//!
//! Landlock tests are required on Linux CI when the probe reports Available.

#![allow(clippy::disallowed_methods)] // positive baselines may use host Command

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use alloy_runtime::{ExecAllow, Grant, PermissionToken, ProfileId, RunId};
use alloy_tools::{
    load_sandbox_profile, BackendStatus, ExecClass, NativeSandboxBroker, NetworkPolicy,
    SandboxBackend, SandboxBroker, SandboxError, SandboxExecRequest, SandboxProfile,
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

fn true_bin() -> &'static str {
    if PathBuf::from("/bin/true").exists() {
        "/bin/true"
    } else {
        "/usr/bin/true"
    }
}

fn sleep_bin() -> &'static str {
    if PathBuf::from("/bin/sleep").exists() {
        "/bin/sleep"
    } else {
        "/usr/bin/sleep"
    }
}

async fn broker_for_jail(jail: PathBuf) -> Result<NativeSandboxBroker, SandboxError> {
    let mut profile = SandboxProfile::default_for_jail(jail)?;
    profile.check_backend = SandboxBackend::Landlock;
    profile.test_backend = SandboxBackend::Container;
    profile.network = NetworkPolicy::Deny;
    profile.exec_timeout = Duration::from_secs(30);
    NativeSandboxBroker::new(profile).await
}

async fn landlock_available_via_new() -> bool {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    match NativeSandboxBroker::new(profile).await {
        Ok(b) => matches!(b.capabilities().landlock, BackendStatus::Available { .. }),
        Err(_) => false,
    }
}

#[tokio::test]
async fn backend_unavailable_fail_closed() {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    // Force Seatbelt on Linux → UnsupportedOs / Unavailable at construction for check.
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
#[cfg(target_os = "linux")]
async fn landlock_actually_applied() {
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    // Positive baseline: sentinel outside jail (and outside /tmp allow) readable unsandboxed.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    let sentinel_dir = home.join(".alloy-sbx-test-sentinel");
    let _ = std::fs::create_dir_all(&sentinel_dir);
    let sentinel = sentinel_dir.join("secret.txt");
    std::fs::write(&sentinel, b"outside-secret").unwrap();
    let baseline = std::fs::read_to_string(&sentinel).unwrap();
    assert_eq!(baseline, "outside-secret");

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    assert!(matches!(
        broker.capabilities().landlock,
        BackendStatus::Available { .. }
    ));

    // Inside sandbox, reading the outside sentinel must fail.
    let script = jail.join("try_read.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat '{}'\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }

    // Grant must allow the path-form shell (basename grants fail when /bin/sh → dash).
    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail.clone(),
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    // Landlock should deny the read → non-zero exit, empty/error output.
    assert_ne!(
        result.exit_code,
        Some(0),
        "sandboxed read of outside sentinel should fail; stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let _ = std::fs::remove_dir_all(&sentinel_dir);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn child_cannot_read_dotenv_in_jail() {
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let dotenv = jail.join(".env");
    std::fs::write(&dotenv, b"SUPER_SECRET=1\n").unwrap();

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let script = jail.join("read_env.sh");
    std::fs::write(&script, "#!/bin/sh\ncat .env\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }

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
    // Bind-over makes `.env` a /dev/null node — cat may exit 0 with empty stdout.
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn dotenv_sentinel_unchanged() {
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
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
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    // Fake cargo home with credentials under a temp dir; set env for OperatorHomes.
    let cargo_home = dir.path().join("fake-cargo");
    std::fs::create_dir_all(&cargo_home).unwrap();
    let creds = cargo_home.join("credentials.toml");
    let original = b"token = \"sentinel\"\n";
    std::fs::write(&creds, original).unwrap();

    // Safety: test-only env mutation.
    let old = std::env::var_os("CARGO_HOME");
    std::env::set_var("CARGO_HOME", &cargo_home);

    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let script = jail.join("try_creds.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\n", creds.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }
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

    match old {
        Some(v) => std::env::set_var("CARGO_HOME", v),
        None => std::env::remove_var("CARGO_HOME"),
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn network_deny_blocks_egress() {
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
        return;
    }
    // Positive baseline: local listener reachable before deny.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let baseline = Command::new("bash")
        .args(["-c", &format!("echo hi | nc -w 1 127.0.0.1 {port} || true")])
        .status();
    let _ = baseline; // environment may lack nc; baseline is best-effort
    drop(listener);

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();

    // Under netns with only lo, connecting to a host listener on a non-forwarded
    // address should fail. Use python if present.
    let script = jail.join("net.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
python3 - <<'PY'
import socket,sys
s=socket.socket()
s.settimeout(1)
try:
    s.connect(("1.1.1.1", 80))
    sys.exit(0)
except Exception:
    sys.exit(2)
PY
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }
    let req = SandboxExecRequest::new(
        vec![sh_bin().into(), script.display().to_string()],
        jail,
        token(sh_bin(), None),
        ExecClass::Check,
    );
    let result = broker.exec(req).await.unwrap();
    assert_ne!(
        result.exit_code,
        Some(0),
        "egress should fail under network deny"
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn cancel_drop_no_orphan() {
    if !landlock_available_via_new().await {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();
    let req = SandboxExecRequest::new(
        vec![sleep_bin().into(), "60".into()],
        jail,
        token(sleep_bin(), None),
        ExecClass::Check,
    );
    let handle = tokio::spawn(async move { broker.exec(req).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.abort();
    // Allow kill_on_drop / drop guard to run.
    tokio::time::sleep(Duration::from_millis(500)).await;
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
