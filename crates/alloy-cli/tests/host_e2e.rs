//! End-to-end tests that run the real `alloy` binary.
//!
//! Every other test in this workspace exercises a subsystem in-process. These
//! start the shipped executable, signal it, and inspect what it left on disk —
//! the only tests that can catch a defect living in the assembly rather than in
//! a module.
//!
//! **Scope note.** `alloy host` today configures the runtime, creates the data
//! directory, and waits for a signal. It does *not* install storage: nothing in
//! `alloy-cli` references `AlloyStorage`, so the SQLite log, artifact CAS, and
//! session plane are unreachable from the binary until RFC-0015 wires the CLI.
//! These tests therefore assert the lifecycle and config-resolution behaviour
//! that genuinely exists, and deliberately assert nothing about schema
//! migration — asserting unbuilt behaviour would be speculation, not coverage.
//!
//! Author: arkadianet

// Signals are the subject of half these tests; the target is Unix-only rather
// than partially compiled.
#![cfg(unix)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Longest a host is given to reach "data dir on disk" before we call it hung.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a host is given to drain after SIGTERM.
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

// --- fixtures ---------------------------------------------------------------

/// A workspace with the two config files `alloy host` requires.
///
/// `router.toml` is deliberately written here rather than copied from the repo:
/// the tracked file is `router.toml.example`, and a fresh clone has no active
/// router at all.
fn workspace() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_profile(dir.path());
    write_router(dir.path());
    std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();
    dir
}

fn write_profile(root: &Path) {
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/default.toml"),
        include_str!("../../../profiles/default.toml"),
    )
    .unwrap();
}

fn write_router(root: &Path) {
    std::fs::write(
        root.join("router.toml"),
        include_str!("../../../router.toml.example"),
    )
    .unwrap();
}

fn alloy_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("alloy")
}

/// Spawn `alloy host --workspace <workspace>`, tee-ing the tracing output to
/// `log` so the test can wait on the readiness line rather than guessing from
/// the filesystem.
///
/// `tracing_subscriber::fmt()` writes to **stdout**, not stderr — capturing the
/// wrong stream yields an empty log and a mystery timeout.
fn spawn_host_logging(workspace: &Path, data_dir: Option<&Path>, log: &Path) -> Child {
    let out = File::create(log).unwrap();
    let mut cmd = Command::new(alloy_bin());
    cmd.arg("host")
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::null());
    match data_dir {
        Some(d) => {
            cmd.env("ALLOY_DATA_DIR", d);
        }
        None => {
            cmd.env_remove("ALLOY_DATA_DIR");
        }
    }
    cmd.spawn().expect("spawn alloy host")
}

/// Spawn without capturing the readiness log.
fn spawn_host(workspace: &Path, data_dir: Option<&Path>) -> Child {
    let mut cmd = Command::new(alloy_bin());
    cmd.arg("host")
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match data_dir {
        Some(d) => {
            cmd.env("ALLOY_DATA_DIR", d);
        }
        None => {
            cmd.env_remove("ALLOY_DATA_DIR");
        }
    }
    cmd.spawn().expect("spawn alloy host")
}

/// Block until the host logs that it is running.
///
/// Waiting on a filesystem side effect is not a readiness signal: on a restart
/// the data dir already exists, so it is satisfied instantly — before the
/// process has installed its signal handlers.
fn wait_until_running(child: &mut Child, log: &Path) {
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        if let Ok(text) = std::fs::read_to_string(log) {
            if text.contains("runtime running") {
                return;
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("host exited early with {status:?}; log:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("timed out waiting for the host to report running");
}

/// Block until `path` exists, or panic with the child's output.
fn wait_for_path(child: &mut Child, path: &Path) {
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "host exited early with {status}; expected {}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("timed out waiting for {}", path.display());
}

/// SIGTERM via `rustix` so these tests stay free of `unsafe`, like the crates
/// they exercise. `Child::kill` sends SIGKILL and would bypass the drain path
/// that is the whole point of the test.
fn send_sigterm(child: &Child) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("live child pid");
    rustix::process::kill_process(pid, rustix::process::Signal::Term).expect("send SIGTERM");
}

/// SIGTERM the host and require a clean, timely exit.
fn terminate_and_expect_clean_exit(mut child: Child) {
    send_sigterm(&child);
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "host should drain and exit 0 on SIGTERM, got {status:?}"
            );
            return;
        }
        if start.elapsed() > EXIT_TIMEOUT {
            let _ = child.kill();
            panic!("host ignored SIGTERM for {EXIT_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// --- startup / shutdown -----------------------------------------------------

/// A fresh host resolves its data directory and creates it under the workspace.
#[test]
fn host_creates_data_dir_under_workspace() {
    let ws = workspace();
    let data = ws.path().join(".alloy");

    let mut child = spawn_host(ws.path(), None);
    wait_for_path(&mut child, &data);
    assert!(data.is_dir(), "data dir should be a directory");

    terminate_and_expect_clean_exit(child);
}

/// SIGTERM drains rather than aborting. This is the production stop path.
#[test]
fn host_drains_and_exits_zero_on_sigterm() {
    let ws = workspace();
    let data = ws.path().join(".alloy");
    let mut child = spawn_host(ws.path(), None);
    wait_for_path(&mut child, &data);
    terminate_and_expect_clean_exit(child);
}

/// A second run over an existing data dir must start cleanly rather than
/// tripping over its predecessor's state.
///
/// Readiness is taken from the log, not the filesystem: `.alloy` already exists
/// on the second run, so a path check would return before the host has armed
/// its signal handlers and this would silently become a startup-race test.
#[test]
fn host_restart_over_existing_data_dir_is_clean() {
    let ws = workspace();
    let first_log = ws.path().join("first.log");
    let second_log = ws.path().join("second.log");

    let mut first = spawn_host_logging(ws.path(), None, &first_log);
    wait_until_running(&mut first, &first_log);
    terminate_and_expect_clean_exit(first);

    let mut second = spawn_host_logging(ws.path(), None, &second_log);
    wait_until_running(&mut second, &second_log);
    terminate_and_expect_clean_exit(second);

    assert!(
        ws.path().join(".alloy").is_dir(),
        "the data dir must survive a restart cycle"
    );
}

// --- config resolution ------------------------------------------------------

/// Regression: a relative `--workspace` used to be joined twice, yielding
/// `ws/ws/profiles/default.toml`. Absolute paths hid it, so only an end-to-end
/// run with a relative argument can catch a recurrence.
#[test]
fn relative_workspace_root_resolves_without_duplication() {
    let parent = tempfile::tempdir().unwrap();
    let name = "ws";
    let ws = parent.path().join(name);
    std::fs::create_dir_all(&ws).unwrap();
    write_profile(&ws);
    write_router(&ws);
    std::fs::write(ws.join("example.env"), "ALLOY_API_KEY=\n").unwrap();

    let data = ws.join(".alloy");
    let mut child = Command::new(alloy_bin())
        .arg("host")
        .arg("--workspace")
        .arg(name) // relative, resolved against cwd below
        .current_dir(parent.path())
        .env_remove("ALLOY_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&mut child, &data);
    assert!(
        !ws.join(name).exists(),
        "a doubled path ({}) means the workspace root was joined twice",
        ws.join(name).display()
    );
    terminate_and_expect_clean_exit(child);
}

/// `ALLOY_DATA_DIR` outranks `<workspace>/.alloy`.
///
/// The override points at a path that does not exist yet, so the assertion is
/// positive — the host must *create* that directory. Asserting only that
/// `<workspace>/.alloy` is absent would also pass if the override were ignored
/// and no data dir were created at all.
#[test]
fn alloy_data_dir_env_overrides_workspace_dir() {
    let ws = workspace();
    let parent = tempfile::tempdir().unwrap();
    let override_dir = parent.path().join("custom-data");
    assert!(!override_dir.exists(), "override must start absent");

    let mut child = spawn_host(ws.path(), Some(&override_dir));
    wait_for_path(&mut child, &override_dir);
    assert!(override_dir.is_dir(), "the override path must be created");
    assert!(
        !ws.path().join(".alloy").exists(),
        "workspace .alloy must not be created when ALLOY_DATA_DIR is set"
    );
    terminate_and_expect_clean_exit(child);
}

/// Run to completion with a deadline, returning `(status, stderr)`.
///
/// `Command::output()` waits forever. These paths fail in config load today, but
/// a regression that reached the host loop would hang CI instead of failing it.
fn run_bounded(workspace: &Path, log: &Path) -> (std::process::ExitStatus, String) {
    let err = File::create(log).unwrap();
    let mut child = Command::new(alloy_bin())
        .arg("host")
        .arg("--workspace")
        .arg(workspace)
        .env_remove("ALLOY_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .unwrap();

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, std::fs::read_to_string(log).unwrap_or_default());
        }
        if start.elapsed() > EXIT_TIMEOUT {
            let _ = child.kill();
            panic!("a config failure must exit, not start the host loop");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// --- config failure modes ---------------------------------------------------

/// Missing `router.toml` must fail closed and tell the operator the fix.
#[test]
fn missing_router_toml_exits_nonzero_and_names_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    write_profile(dir.path());
    std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();

    let (status, stderr) = run_bounded(dir.path(), &dir.path().join("err.log"));
    assert!(!status.success(), "must not start without a router");
    assert!(
        stderr.contains("router.toml.example"),
        "error should name the remedy, got: {stderr}"
    );
}

/// Missing profile TOML must fail closed.
#[test]
fn missing_profile_toml_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    write_router(dir.path());
    std::fs::write(dir.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();

    let (status, stderr) = run_bounded(dir.path(), &dir.path().join("err.log"));
    assert!(!status.success(), "must not start without a profile");
    assert!(
        stderr.contains("profile"),
        "error should name the missing profile, got: {stderr}"
    );
}

/// Alloy never creates or overwrites `.env` — a sentinel must survive a run.
#[test]
fn host_never_writes_dotenv() {
    let ws = workspace();
    let dotenv = ws.path().join(".env");
    std::fs::write(&dotenv, "SENTINEL=1\n").unwrap();

    let data = ws.path().join(".alloy");
    let mut child = spawn_host(ws.path(), None);
    wait_for_path(&mut child, &data);
    terminate_and_expect_clean_exit(child);

    assert_eq!(
        std::fs::read_to_string(&dotenv).unwrap(),
        "SENTINEL=1\n",
        "the host must never rewrite .env"
    );
}

// --- RFC-0015 §12.2 lifecycle additions -------------------------------------

/// SEC1 — a sentinel `.env` is byte-identical after running every
/// subcommand (process level; extends the merged loader regression).
#[test]
fn no_dotenv_written_by_any_subcommand() {
    let dir = workspace();
    let dotenv = dir.path().join(".env");
    let sentinel = "SENTINEL=1\n# do not touch\n";
    std::fs::write(&dotenv, sentinel).unwrap();

    let bogus = "00000000-0000-4000-8000-000000000000";
    let argsets: Vec<Vec<&str>> = vec![
        vec!["run", "goal", "--dry-run"],
        vec!["events", "--session", bogus],
        vec![
            "approve",
            "--run",
            bogus,
            "--gate",
            bogus,
            "--decision",
            "allow",
        ],
        vec!["cancel", "--run", bogus],
        vec!["resume", "--session", bogus],
        vec!["index", "--stats"],
    ];
    for args in argsets {
        // Exit codes vary (bogus ids); the property under test is the file.
        let _ = Command::new(env!("CARGO_BIN_EXE_alloy"))
            .args(&args)
            .current_dir(dir.path())
            .env_remove("ALLOY_API_KEY")
            .env_remove("ALLOY_DATA_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&dotenv).unwrap(),
            sentinel,
            "a subcommand touched .env: {args:?}"
        );
    }
}

/// SEC2 — a `.env` file setting the API key does **not** satisfy the
/// router: credentials come from the process environment only.
#[test]
fn no_dotenv_read() {
    let dir = workspace();
    std::fs::write(dir.path().join(".env"), "ALLOY_API_KEY=from-dotenv\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_alloy"))
        .args(["run", "fix it"])
        .current_dir(dir.path())
        .env_remove("ALLOY_API_KEY")
        .env_remove("ALLOY_DATA_DIR")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "expected EX_CONFIG");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ALLOY_API_KEY"),
        "error must name the variable: {stderr}"
    );
}
