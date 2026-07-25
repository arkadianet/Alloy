//! Process spawn and supervision — sole `Command::new` seam (RFC-0005 §6.4).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::sandbox::types::{SandboxError, SandboxExecResult};

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
    pub pre_exec: Option<Box<dyn FnMut() -> std::io::Result<()> + Send + Sync>>,
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

    let mut cmd = Command::new(&spec.program);
    // argv[0] for the child should be original; remaining args follow.
    if spec.argv.is_empty() {
        return Err(SandboxError::Invalid("empty argv".into()));
    }
    cmd.arg0(OsStr::new(&spec.argv[0]));
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
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use rustix::process::setsid;
        // Single pre_exec: setsid then optional backend isolate (Command keeps one hook).
        let mut backend_hook = spec.pre_exec.take();
        // SAFETY: pre_exec runs in the child after fork; only async-signal-safe work.
        unsafe {
            cmd.pre_exec(move || {
                setsid().map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                if let Some(ref mut hook) = backend_hook {
                    hook()?;
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(SandboxError::Io)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

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

    let wait_fut = child.wait();
    let timed = timeout(spec.exec_timeout, wait_fut).await;

    let status = match timed {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(SandboxError::Io(e));
        }
        Err(_elapsed) => {
            kill_process_group(&mut child).await;
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

#[cfg(unix)]
async fn kill_process_group(child: &mut tokio::process::Child) {
    if let Some(id) = child.id() {
        // Negative pgid: we called setsid so pgid == pid.
        let pid = rustix::process::Pid::from_raw(id as i32);
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::Term);
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::Kill);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn kill_process_group(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn encode_status(status: std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (None, Some(sig));
        }
    }
    (status.code(), None)
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
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signal_status_encoding_and_timeout() {
        // /bin/sleep should exist on Unix CI.
        let program = PathBuf::from("/bin/sleep");
        if !program.exists() {
            return;
        }
        let err = spawn_supervised(SpawnSpec {
            program,
            argv: vec!["sleep".into(), "30".into()],
            cwd: PathBuf::from("/"),
            env: BTreeMap::new(),
            stdout_cap: 1024,
            stderr_cap: 1024,
            exec_timeout: Duration::from_millis(200),
            pre_exec: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout(_)));
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("spam.sh");
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::PermissionsExt;
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "dd if=/dev/zero bs=1 count=5000 2>/dev/null").unwrap();
            drop(f);
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let out = spawn_supervised(SpawnSpec {
            program: script.clone(),
            argv: vec![script.display().to_string()],
            cwd: PathBuf::from("/"),
            env: BTreeMap::from([(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))]),
            stdout_cap: 100,
            stderr_cap: 100,
            exec_timeout: Duration::from_secs(10),
            pre_exec: None,
        })
        .await
        .unwrap();
        assert!(out.stdout_truncated);
        assert!(out.stdout.len() <= 100);
        assert_eq!(out.exit_code, Some(0));
    }
}
