//! Integration tests for RFC-0005 Sandbox Broker.
//!
//! Landlock tests are required on Linux CI when `ALLOY_REQUIRE_LANDLOCK=1`.
//! Without that env var, Unavailable hosts skip (local nested-userns) rather
//! than reporting a dishonest green pass for enforcement.

#![allow(clippy::disallowed_methods)] // positive baselines may use host Command

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use alloy_runtime::{ExecAllow, Grant, PermissionToken, ProfileId, RunId, Timestamp};
use alloy_tools::{
    load_sandbox_profile, BackendStatus, ExecClass, NativeSandboxBroker, NetworkPolicy,
    SandboxBackend, SandboxBroker, SandboxError, SandboxExecRequest, SandboxProfile,
};
use tempfile::tempdir;

/// Serializes tests that mutate process environment (`CARGO_HOME`, …).
static ENV_LOCK: Mutex<()> = Mutex::new(());

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

fn require_landlock() -> bool {
    std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
}

/// Returns `true` when Landlock is Available. When required by CI, panics if not.
async fn landlock_or_skip() -> bool {
    let dir = tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    let available = match NativeSandboxBroker::new(profile).await {
        Ok(b) => matches!(b.capabilities().landlock, BackendStatus::Available { .. }),
        Err(_) => false,
    };
    if available {
        return true;
    }
    if require_landlock() {
        panic!(
            "ALLOY_REQUIRE_LANDLOCK=1 but Landlock is Unavailable \
             (need unprivileged userns identity maps + Landlock ABI >= 2)"
        );
    }
    eprintln!("skip: landlock unavailable (set ALLOY_REQUIRE_LANDLOCK=1 to fail)");
    false
}

async fn broker_for_jail(jail: PathBuf) -> Result<NativeSandboxBroker, SandboxError> {
    let mut profile = SandboxProfile::default_for_jail(jail)?;
    profile.check_backend = SandboxBackend::Landlock;
    profile.test_backend = SandboxBackend::Container;
    profile.network = NetworkPolicy::Deny;
    profile.exec_timeout = Duration::from_secs(30);
    NativeSandboxBroker::new(profile).await
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

fn process_alive(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
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
    if !landlock_or_skip().await {
        return;
    }
    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    // Sentinel outside the jail and outside Landlock RO roots (sibling tempdir).
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
    // Bind-over makes `.env` a /dev/null node — cat may exit 0 with empty stdout.
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
#[allow(clippy::await_holding_lock)] // serializes CARGO_HOME mutation across awaits
async fn credentials_sentinel_unchanged() {
    if !landlock_or_skip().await {
        return;
    }
    let _env = ENV_LOCK.lock().unwrap();

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();

    let cargo_home = dir.path().join("fake-cargo");
    std::fs::create_dir_all(&cargo_home).unwrap();
    let creds = cargo_home.join("credentials.toml");
    let original = b"token = \"sentinel\"\n";
    std::fs::write(&creds, original).unwrap();

    // Safety: test-only env mutation, serialized by ENV_LOCK.
    let old = std::env::var_os("CARGO_HOME");
    std::env::set_var("CARGO_HOME", &cargo_home);

    let broker = broker_for_jail(jail.clone()).await.unwrap();
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

    match old {
        Some(v) => std::env::set_var("CARGO_HOME", v),
        None => std::env::remove_var("CARGO_HOME"),
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn network_deny_blocks_egress() {
    if !landlock_or_skip().await {
        return;
    }

    let dir = tempdir().unwrap();
    let jail = dir.path().canonicalize().unwrap();
    let broker = broker_for_jail(jail.clone()).await.unwrap();

    // Under netns with only lo, connecting to a public address must fail.
    // Prefer python3 (present on ubuntu-latest); skip honestly if missing.
    if std::process::Command::new("python3")
        .arg("-c")
        .arg("1")
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        if require_landlock() {
            panic!("python3 required for network_deny_blocks_egress under ALLOY_REQUIRE_LANDLOCK");
        }
        eprintln!("skip: python3 unavailable for network deny test");
        return;
    }

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
        "egress should fail under network deny; stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
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

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
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

    // Grandchild must be gone after timeout kill of the process group.
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
    // Inclusive boundary: now == expires → TokenExpired.
    perms.expires = Some(Timestamp(Timestamp::now().0));

    let req = SandboxExecRequest::new(vec![true_bin().into()], jail, perms, ExecClass::Check);
    let err = broker.exec(req).await.unwrap_err();
    assert!(matches!(err, SandboxError::TokenExpired), "got {err:?}");
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
