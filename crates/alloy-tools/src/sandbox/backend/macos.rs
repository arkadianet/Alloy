//! macOS Seatbelt backend via `sandbox-exec` (RFC-0005 §5.5).
//!
//! Isolation is applied by `sandbox-exec`. A small bash trampoline then:
//! 1. Writes one ready-byte on an inherited pipe fd (profile apply succeeded).
//! 2. `exec -a`s the real program so rustup/busybox argv0 dispatch is preserved.
//!
//! Arguments are passed as distinct argv slots to the trampoline — never
//! re-joined into an unquoted `bash -c` string (that would bypass `args_glob`).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::process::{spawn_supervised, SpawnSpec, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxBackend, SandboxError};

/// Seatbelt isolation backend.
pub struct MacosSeatbeltBackend;

const TEMPLATE: &str = include_str!("macos/alloy-check.sb.template");

/// Ready-byte wait after spawn (profile apply / trampoline handoff).
const READY_WAIT: Duration = Duration::from_secs(2);

impl MacosSeatbeltBackend {
    /// Execute under `/usr/bin/sandbox-exec` with argv0 preserved.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        if matches!(profile.network, NetworkPolicy::Allow) {
            return Err(SandboxError::Invalid(
                "network=allow unsupported in MVP".into(),
            ));
        }

        let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            return Err(SandboxError::BackendUnavailable {
                backend: SandboxBackend::Seatbelt,
                message: "/usr/bin/sandbox-exec not found".into(),
            });
        }

        // Policy + trampoline live outside the jail: the SBPL grants write to
        // the whole jail, so an in-jail path would be mutable by the child
        // (policy mutable by workspace text — V2 §14.5).
        let outside = broker_owned_dir()?;
        let sbpl = outside.join("alloy-check.sb");
        let body = render_sbpl(profile, &ctx, &outside)?;
        write_mode_0600(&sbpl, body.as_bytes())?;

        let trampoline = outside.join("trampoline.sh");
        write_trampoline(&trampoline)?;

        let (ready_r, ready_w) = pipe_pair()?;
        let ready_fd = ready_w.as_raw_fd();
        // Child must inherit the write end; clear FD_CLOEXEC before fork.
        clear_cloexec(ready_fd)?;

        let argv0 = ctx
            .argv
            .first()
            .cloned()
            .unwrap_or_else(|| ctx.program.display().to_string());

        // sandbox-exec -- trampoline <ready_fd> <argv0> <program> -- <args…>
        let mut spawn_argv = vec![
            "sandbox-exec".into(),
            "-f".into(),
            sbpl.display().to_string(),
            "--".into(),
            trampoline.display().to_string(),
            ready_fd.to_string(),
            argv0,
            ctx.program.display().to_string(),
            "--".into(),
        ];
        if ctx.argv.len() > 1 {
            spawn_argv.extend(ctx.argv[1..].iter().cloned());
        }

        let ready_w_fd = ready_w.into_raw_fd();
        let after_spawn = Box::new(move || {
            // Parent closes its write end only after fork so the child still
            // inherits it; otherwise the ready-byte pipe is dead on arrival.
            unsafe {
                libc::close(ready_w_fd);
            }
        });

        let mut ready_r = ready_r;
        let ready_task =
            tokio::task::spawn_blocking(move || read_ready_byte(&mut ready_r, READY_WAIT));

        let outcome = spawn_supervised(SpawnSpec {
            program: sandbox_exec,
            argv: spawn_argv,
            cwd: ctx.cwd,
            env: ctx.env,
            stdout_cap: profile.stdout_cap,
            stderr_cap: profile.stderr_cap,
            exec_timeout: profile.exec_timeout,
            pre_exec: None,
            after_spawn: Some(after_spawn),
        })
        .await;

        let got_ready = ready_task.await.unwrap_or(false);

        match outcome {
            Ok(out) if got_ready => Ok(out),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                Err(SandboxError::BackendCannotEnforce(format!(
                    "sandbox-exec/trampoline exited before ready-byte \
                     (exit={:?} signal={:?}): {stderr}",
                    out.exit_code, out.signal
                )))
            }
            Err(e) => Err(e),
        }
    }
}

fn write_trampoline(path: &Path) -> Result<(), SandboxError> {
    // Pure bash: no eval of joined args. argv layout:
    //   $0=trampoline $1=ready_fd $2=argv0 $3=program $4=-- $5..=args
    const BODY: &str = r#"#!/bin/bash
set -euo pipefail
ready_fd="$1"
argv0="$2"
program="$3"
shift 3
if [[ "${1:-}" != "--" ]]; then
  echo "alloy trampoline: missing -- separator" >&2
  exit 74
fi
shift
printf 'x' >&"$ready_fd" || exit 75
exec -a "$argv0" "$program" "$@"
"#;
    write_mode_0600(path, BODY.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(SandboxError::Io)?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms).map_err(SandboxError::Io)?;
    }
    Ok(())
}

fn broker_owned_dir() -> Result<PathBuf, SandboxError> {
    let dir = std::env::temp_dir()
        .join("alloy-sbx-seatbelt")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).map_err(SandboxError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dir)
            .map_err(SandboxError::Io)?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dir, perms).map_err(SandboxError::Io)?;
    }
    Ok(dir)
}

fn write_mode_0600(path: &Path, bytes: &[u8]) -> Result<(), SandboxError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path).map_err(SandboxError::Io)?;
    f.write_all(bytes).map_err(SandboxError::Io)?;
    Ok(())
}

fn clear_cloexec(fd: i32) -> Result<(), SandboxError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if rc != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn read_ready_byte(ready_r: &mut std::fs::File, wait: Duration) -> bool {
    let fd = ready_r.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
    if rc <= 0 {
        return false;
    }
    let mut buf = [0u8; 1];
    matches!(ready_r.read(&mut buf), Ok(1) if buf[0] == b'x')
}

fn pipe_pair() -> Result<(std::fs::File, std::fs::File), SandboxError> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    let r = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let w = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((r, w))
}

fn render_sbpl(
    profile: &SandboxProfile,
    ctx: &IsolateContext,
    broker_dir: &Path,
) -> Result<String, SandboxError> {
    let mut deny_clauses = String::new();
    for path in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home)
        .into_iter()
        .chain(ctx.deny_paths.iter().cloned())
    {
        let matcher = if path.is_file() {
            format!("(literal {})", sbpl_literal(&path))
        } else {
            format!("(subpath {})", sbpl_literal(&path))
        };
        deny_clauses.push_str(&format!("(deny file-read* file-write* {matcher})\n"));
    }

    // Match Linux: grant every allowlisted RO root from IsolateContext
    // (config.toml, .package-cache, toolchains, …).
    let mut ro_clauses = String::new();
    for p in &ctx.read_only_roots {
        if !p.exists() {
            continue;
        }
        if p.is_file() {
            ro_clauses.push_str(&format!(
                "(allow file-read* (literal {}))\n",
                sbpl_literal(p)
            ));
        } else {
            ro_clauses.push_str(&format!(
                "(allow file-read* (subpath {}))\n",
                sbpl_literal(p)
            ));
        }
    }

    let mut body = TEMPLATE
        .replace("{{JAIL}}", &sbpl_literal(&profile.fs_jail))
        .replace("{{TMP}}", &sbpl_literal(&ctx.exec_dir.join("tmp")))
        .replace("{{HOME}}", &sbpl_literal(&ctx.exec_dir.join("home")))
        .replace("{{BROKER_DIR}}", &sbpl_literal(broker_dir));
    body.push('\n');
    body.push_str(&ro_clauses);
    body.push_str(&deny_clauses);
    Ok(body)
}

fn sbpl_literal(path: &Path) -> String {
    let s = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('\"', "\\\"");
    format!("\"{s}\"")
}

/// Probe Seatbelt by applying a minimal profile (not just statting the binary).
pub fn probe_seatbelt_sync() -> Result<String, String> {
    let p = Path::new("/usr/bin/sandbox-exec");
    if !p.is_file() {
        return Err("/usr/bin/sandbox-exec missing".into());
    }
    // Broker-owned dir outside any jail — do not depend on the `tempfile` crate
    // (dev-only) from library code.
    let dir = std::env::temp_dir().join(format!("alloy-sbx-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let sb = dir.join("probe.sb");
    let body = "(version 1)\n(allow default)\n";
    let result = (|| {
        std::fs::write(&sb, body).map_err(|e| e.to_string())?;
        let out = std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-f", &sb.display().to_string(), "--", "/usr/bin/true"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok("sandbox-exec profile apply ok".into())
        } else {
            Err(format!(
                "sandbox-exec probe failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
