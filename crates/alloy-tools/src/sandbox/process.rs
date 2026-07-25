//! Process spawn and supervision — sole `Command::new` seam (RFC-0005 §6.4).
//!
//! # Why `unsafe` lives here
//!
//! The RFC keeps isolation in backends, but `CommandExt::pre_exec` is only
//! reachable where the `Command` is built and a `Command` holds a single hook.
//! So this module owns the one `unsafe` block: `setsid` — which puts the child
//! in a fresh process group so the timeout and drop paths can signal the whole
//! tree — followed by the optional backend hook. Backends hand over a closure
//! instead of spawning themselves.
//!
//! Author: arkadianet

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::sandbox::types::{SandboxError, SandboxExecResult};

/// Grace between SIGTERM and SIGKILL of the process group (RFC-0005 §6.4).
const GROUP_KILL_GRACE: Duration = Duration::from_secs(2);

/// Backend-supplied hook run in the child between `fork` and `execve`.
///
/// `Send` only. `std`/`tokio` demand `Sync` on `pre_exec`, but propagating that
/// bound would force backends to park non-`Sync` isolation state (Landlock's
/// `RulesetCreated`) behind a `Mutex` for no benefit; [`SyncHook`] absorbs it.
#[cfg(unix)]
pub type PreExecHook = Box<dyn FnMut() -> std::io::Result<()> + Send>;

/// Specification for a supervised spawn.
pub struct SpawnSpec {
    /// Canonical absolute executable path (native) or argv0 for container wrapper.
    pub program: PathBuf,
    /// Full argv including original argv0 at index 0.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Scrubbed environment.
    pub env: BTreeMap<OsString, OsString>,
    /// Stdout cap.
    pub stdout_cap: usize,
    /// Stderr cap.
    pub stderr_cap: usize,
    /// Wall-clock timeout.
    pub exec_timeout: Duration,
    /// Optional Unix pre-exec hook (Landlock apply, etc.).
    #[cfg(unix)]
    pub pre_exec: Option<PreExecHook>,
    /// Runs in the parent immediately after `spawn` returns (before wait).
    ///
    /// Used by Seatbelt to close the ready-byte pipe write end once the child
    /// has inherited it — must not run before fork.
    pub after_spawn: Option<Box<dyn FnOnce() + Send>>,
}

/// Outcome before attaching policy metadata.
#[derive(Debug)]
pub struct SupervisedOutcome {
    /// Exit code if exited.
    pub exit_code: Option<i32>,
    /// Signal if signaled.
    pub signal: Option<i32>,
    /// Stdout bytes.
    pub stdout: Vec<u8>,
    /// Stderr bytes.
    pub stderr: Vec<u8>,
    /// Truncation flags.
    pub stdout_truncated: bool,
    /// Truncation flags.
    pub stderr_truncated: bool,
    /// Duration ms.
    pub duration_ms: u64,
}

/// Spawn and supervise a child according to RFC-0005 lifecycle rules.
pub async fn spawn_supervised(mut spec: SpawnSpec) -> Result<SupervisedOutcome, SandboxError> {
    let start = Instant::now();

    if spec.argv.is_empty() {
        return Err(SandboxError::Invalid("empty argv".into()));
    }

    let mut cmd = Command::new(&spec.program);
    // argv[0] for the child should be original; remaining args follow.
    #[cfg(unix)]
    cmd.arg0(std::ffi::OsStr::new(&spec.argv[0]));
    if spec.argv.len() > 1 {
        cmd.args(&spec.argv[1..]);
    }
    cmd.current_dir(&spec.cwd);
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Belt and suspenders for the direct child; ChildGuard kills the group.
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use rustix::process::setsid;

        // A Command keeps one hook, so chain setsid and the backend's isolate.
        // SyncHook lets the backend hook stay `Send`-only.
        let mut hook = SyncHook(spec.pre_exec.take());
        // SAFETY: runs in the forked child before execve; setsid and the backend
        // hook are async-signal-safe syscall wrappers and allocate nothing.
        unsafe {
            cmd.pre_exec(move || {
                setsid().map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                if let Some(hook) = hook.get_mut().as_mut() {
                    hook()?;
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(SandboxError::Io)?;
    if let Some(hook) = spec.after_spawn.take() {
        hook();
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut guard = ChildGuard::new(child);

    let stdout_cap = spec.stdout_cap;
    let stderr_cap = spec.stderr_cap;
    let out_task = tokio::spawn(async move {
        match stdout {
            Some(r) => drain_capped(r, stdout_cap).await,
            None => Ok((Vec::new(), false)),
        }
    });
    let err_task = tokio::spawn(async move {
        match stderr {
            Some(r) => drain_capped(r, stderr_cap).await,
            None => Ok((Vec::new(), false)),
        }
    });

    let status = match guard.wait_for(spec.exec_timeout).await {
        Some(Ok(status)) => {
            // Reaped: Drop must not signal a pid the kernel may have recycled.
            guard.disarm();
            status
        }
        Some(Err(e)) => {
            guard.kill_group().await;
            return Err(SandboxError::Io(e));
        }
        None => {
            guard.kill_group().await;
            // Pipes are closed once the group is gone, so these joins finish.
            let _ = out_task.await;
            let _ = err_task.await;
            return Err(SandboxError::Timeout(spec.exec_timeout));
        }
    };

    let (stdout, stdout_truncated) = out_task
        .await
        .map_err(|e| SandboxError::Internal(format!("stdout join: {e}")))?
        .map_err(SandboxError::Io)?;
    let (stderr, stderr_truncated) = err_task
        .await
        .map_err(|e| SandboxError::Internal(format!("stderr join: {e}")))?
        .map_err(SandboxError::Io)?;

    let (exit_code, signal) = encode_status(status);
    Ok(SupervisedOutcome {
        exit_code,
        signal,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// Carries a `Send`-only value through an API that asks for `Sync`.
///
/// `pre_exec` requires `Sync` conservatively. We never alias the value: it is
/// owned by exactly one `Command`, moved into it, and the closure inside runs
/// only in the forked child, which has a single thread.
#[cfg(unix)]
struct SyncHook<T>(T);

#[cfg(unix)]
impl<T> SyncHook<T> {
    /// Access through the wrapper: a closure that touched `.0` directly would
    /// capture the field alone (edition 2021) and lose the `Sync` assertion.
    fn get_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

// SAFETY: no reference to the inner value is ever observed from a second
// thread — see the type docs.
#[cfg(unix)]
unsafe impl<T: Send> Sync for SyncHook<T> {}

/// Process group of a supervised child; `setsid` makes pgid == child pid.
#[derive(Clone, Copy)]
struct Group {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
}

#[cfg(unix)]
impl Group {
    fn of(child: &Child) -> Self {
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        Self { pgid }
    }

    /// Best-effort group signal; `ESRCH` is expected once the group is empty.
    fn signal(self, sig: rustix::process::Signal) {
        if let Some(pgid) = self.pgid {
            let _ = rustix::process::kill_process_group(pgid, sig);
        }
    }

    fn term(self) {
        self.signal(rustix::process::Signal::Term);
    }

    fn kill(self) {
        self.signal(rustix::process::Signal::Kill);
    }
}

#[cfg(not(unix))]
impl Group {
    fn of(_child: &Child) -> Self {
        Self {}
    }

    fn term(self) {}

    fn kill(self) {}
}

/// Owns the child until it is reaped, killing the whole group otherwise.
///
/// RFC-0005 §6.4 requires the same kill path on timeout *and* on drop of the
/// `exec` future: SIGTERM the group, wait up to [`GROUP_KILL_GRACE`], SIGKILL,
/// reap — no orphans either way. `Drop` cannot await, so it signals inline and
/// hands the child to a detached escalation task.
struct ChildGuard {
    /// `None` once the child has been reaped or handed off.
    child: Option<Child>,
    group: Group,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        let group = Group::of(&child);
        Self {
            child: Some(child),
            group,
        }
    }

    /// Wait for the direct child; `None` if `limit` elapsed first.
    async fn wait_for(&mut self, limit: Duration) -> Option<std::io::Result<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Some(Err(std::io::Error::other("child already reaped")));
        };
        timeout(limit, child.wait()).await.ok()
    }

    /// Give up ownership after a successful `wait`, so `Drop` stays quiet.
    fn disarm(&mut self) {
        self.child = None;
    }

    /// Run the kill path to completion (timeout / spawn error path).
    async fn kill_group(&mut self) {
        if let Some(child) = self.child.take() {
            kill_group_and_reap(child, self.group).await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let group = self.group;

        // Signal now: the escalation below may never get scheduled.
        group.term();

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { kill_group_and_reap(child, group).await });
            }
            Err(_) => {
                // Dropped off-runtime: nothing will poll a future, so kill the
                // direct child here and escalate on a plain thread.
                std::thread::spawn(move || {
                    std::thread::sleep(GROUP_KILL_GRACE);
                    group.kill();
                });
                let _ = child.start_kill();
                drop(child);
            }
        }
    }
}

/// SIGTERM the group, wait up to [`GROUP_KILL_GRACE`], SIGKILL, then reap.
async fn kill_group_and_reap(mut child: Child, group: Group) {
    group.term();
    #[cfg(not(unix))]
    let _ = child.start_kill();

    // Returns as soon as the leader exits — never an unconditional 2s sleep.
    let graceful = timeout(GROUP_KILL_GRACE, child.wait()).await.is_ok();

    // Sweep the group even on a graceful exit: a leader that honoured SIGTERM
    // can still leave children behind, and orphans are forbidden (§6.4).
    group.kill();

    if !graceful {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

async fn drain_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        if buf.len() < cap {
            let room = cap - buf.len();
            let take = n.min(room);
            buf.extend_from_slice(&tmp[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
            // Continue reading and discard to avoid deadlock.
        }
    }
    Ok((buf, truncated))
}

fn encode_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (None, Some(sig));
        }
    }
    (status.code(), None)
}

#[cfg(all(test, unix))]
mod signal_status_tests {
    use super::encode_status;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn signal_status_encoding() {
        // Construct a signaled status without spawning a host binary (macOS
        // has /usr/bin/true, not /bin/true).
        let signaled = ExitStatus::from_raw(9); // SIGKILL wait-status
        let (code, sig) = encode_status(signaled);
        assert_eq!(code, None);
        assert_eq!(sig, Some(9));

        // Exited-0 half: prefer a path that exists on this host.
        let true_bin = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|p| std::path::Path::new(p).is_file())
            .expect("true binary");
        let out = std::process::Command::new(true_bin).status().unwrap();
        let (code, sig) = encode_status(out);
        assert_eq!(code, Some(0));
        assert_eq!(sig, None);
    }
}

/// Attach policy fields to a supervised outcome.
#[must_use]
pub fn into_exec_result(
    outcome: SupervisedOutcome,
    backend: crate::sandbox::types::SandboxBackend,
    policy_digest: alloy_runtime::Digest,
) -> SandboxExecResult {
    SandboxExecResult {
        exit_code: outcome.exit_code,
        signal: outcome.signal,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        stdout_truncated: outcome.stdout_truncated,
        stderr_truncated: outcome.stderr_truncated,
        duration_ms: outcome.duration_ms,
        backend,
        policy_digest,
    }
}

/// Helper used by container backend to run an outer runtime command.
pub async fn spawn_runtime_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    stdout_cap: usize,
    stderr_cap: usize,
    exec_timeout: Duration,
) -> Result<SupervisedOutcome, SandboxError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.display().to_string());
    argv.extend(args.iter().cloned());
    spawn_supervised(SpawnSpec {
        program: program.to_path_buf(),
        argv,
        cwd: cwd.to_path_buf(),
        env: env.clone(),
        stdout_cap,
        stderr_cap,
        exec_timeout,
        #[cfg(unix)]
        pre_exec: None,
        after_spawn: None,
    })
    .await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `/bin/sh` exists on every supported host; skip rather than fail if not.
    fn shell() -> Option<PathBuf> {
        let sh = PathBuf::from("/bin/sh");
        sh.exists().then_some(sh)
    }

    /// Spec running `script` under `sh -c`.
    ///
    /// Deliberately not a temp script file: a file written moments earlier may
    /// still be open for write elsewhere in the process, and exec'ing it then
    /// fails with `ETXTBSY`.
    fn sh_spec(sh: &Path, script: &str, exec_timeout: Duration, cap: usize) -> SpawnSpec {
        SpawnSpec {
            program: sh.to_path_buf(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: PathBuf::from("/"),
            env: BTreeMap::from([(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))]),
            stdout_cap: cap,
            stderr_cap: cap,
            exec_timeout,
            pre_exec: None,
            after_spawn: None,
        }
    }

    /// Alive means present and not a zombie: a victim reparented to a pid 1
    /// that does not reap lingers as a zombie, which is not "still running".
    fn process_alive(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            // `comm` may contain ')', so state is the field after the last one.
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        }
        #[cfg(not(target_os = "linux"))]
        {
            rustix::process::Pid::from_raw(pid)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        }
    }

    fn read_pid(path: &Path) -> Option<i32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    #[tokio::test]
    async fn timeout_kills_child() {
        let Some(sh) = shell() else { return };
        let err = spawn_supervised(sh_spec(&sh, "sleep 30", Duration::from_millis(200), 1024))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout(_)));
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let Some(sh) = shell() else { return };
        let out = spawn_supervised(sh_spec(
            &sh,
            "dd if=/dev/zero bs=1 count=5000 2>/dev/null",
            Duration::from_secs(10),
            100,
        ))
        .await
        .unwrap();
        assert!(out.stdout_truncated);
        assert!(out.stdout.len() <= 100);
        assert_eq!(out.exit_code, Some(0));
    }

    /// Dropping the future must take the whole group down. The grandchild traps
    /// SIGTERM, so it dies only if the guard escalates to SIGKILL on the group.
    #[tokio::test]
    async fn dropping_exec_kills_process_group() {
        let Some(sh) = shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let script = format!(
            "sh -c 'trap \"\" TERM; while :; do sleep 1; done' & echo $! > {} ; sleep 30",
            pidfile.display()
        );
        let mut fut = Box::pin(spawn_supervised(sh_spec(
            &sh,
            &script,
            Duration::from_secs(30),
            1024,
        )));

        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild = loop {
            tokio::select! {
                res = &mut fut => panic!("exec finished before the grandchild started: {res:?}"),
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            if let Some(pid) = read_pid(&pidfile) {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild never reported its pid"
            );
        };
        assert!(
            process_alive(grandchild),
            "grandchild {grandchild} never ran"
        );

        drop(fut);

        let deadline = Instant::now() + GROUP_KILL_GRACE + Duration::from_secs(5);
        while process_alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived drop of the exec future"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
