//! macOS Seatbelt backend via `sandbox-exec` (RFC-0005 §5.5).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::process::{spawn_supervised, SpawnSpec, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{SandboxBackend, SandboxError};

/// Seatbelt isolation backend.
pub struct MacosSeatbeltBackend;

const TEMPLATE: &str = include_str!("macos/alloy-check.sb.template");

impl MacosSeatbeltBackend {
    /// Execute under `/usr/bin/sandbox-exec` with argv0 preserved.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            return Err(SandboxError::BackendUnavailable {
                backend: SandboxBackend::Seatbelt,
                message: "/usr/bin/sandbox-exec not found".into(),
            });
        }

        let sbpl = ctx.exec_dir.join("alloy-check.sb");
        let body = render_sbpl(profile, &ctx)?;
        std::fs::write(&sbpl, body).map_err(SandboxError::Io)?;

        // Ready-byte pipe: trampoline writes one byte after sandbox-exec handoff.
        // Parent waits briefly; if the child exits before the byte with sandbox
        // diagnostics → BackendCannotEnforce.
        let (ready_r, ready_w) = pipe_pair()?;

        let mut argv = vec![
            "sandbox-exec".into(),
            "-f".into(),
            sbpl.display().to_string(),
            "--".into(),
            // Spawn the resolved binary; argv0 for the *inner* process is set via
            // SpawnSpec.arg0 below so rustup/busybox dispatch is preserved.
            ctx.program.display().to_string(),
        ];
        if ctx.argv.len() > 1 {
            argv.extend(ctx.argv[1..].iter().cloned());
        }

        // Encode original argv0 into the supervised argv[0] slot for Command::arg0.
        let mut env = ctx.env;
        // Pass write-end FD number to a tiny shell trampoline? RFC wants ready-byte
        // after sandbox_init. Without a custom trampoline binary we approximate:
        // parent races a short read on the pipe while the child runs; sandbox-exec
        // itself does not write the byte. For MVP we keep the pipe open in the
        // parent and treat early exit + stderr diagnostics as CannotEnforce.
        // Close write end in parent so read unblocks on child exit.
        drop(ready_w);

        let mut spawn_argv = argv;
        // Ensure argv[0] for the sandboxed program is the caller's original argv0.
        if !ctx.argv.is_empty() {
            // sandbox-exec -- <program> <args>; Command.arg0 applies to sandbox-exec.
            // To preserve inner argv0 we invoke via `exec -a` shell form when needed.
            if ctx.argv[0] != ctx.program.display().to_string() {
                let inner_args = if ctx.argv.len() > 1 {
                    ctx.argv[1..].join(" ")
                } else {
                    String::new()
                };
                spawn_argv = vec![
                    "sandbox-exec".into(),
                    "-f".into(),
                    sbpl.display().to_string(),
                    "--".into(),
                    "/bin/bash".into(),
                    "-c".into(),
                    format!(
                        "exec -a {} {} {}",
                        shell_single_quote(&ctx.argv[0]),
                        shell_single_quote(&ctx.program.display().to_string()),
                        inner_args
                    ),
                ];
            }
        }

        let outcome = spawn_supervised(SpawnSpec {
            program: sandbox_exec,
            argv: spawn_argv,
            cwd: ctx.cwd,
            env,
            stdout_cap: profile.stdout_cap,
            stderr_cap: profile.stderr_cap,
            exec_timeout: profile.exec_timeout,
            pre_exec: None,
        })
        .await?;

        // Drain ready pipe (may be EOF if child never wrote).
        let mut ready_r = ready_r;
        let mut buf = [0u8; 1];
        let _ = ready_r.read(&mut buf);

        if outcome.exit_code == Some(1) {
            let stderr = String::from_utf8_lossy(&outcome.stderr);
            if stderr.contains("sandbox-exec")
                || stderr.contains("sandbox_init")
                || stderr.contains("deny")
            {
                return Err(SandboxError::BackendCannotEnforce(format!(
                    "sandbox-exec profile apply failed: {stderr}"
                )));
            }
        }
        Ok(outcome)
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

fn render_sbpl(profile: &SandboxProfile, ctx: &IsolateContext) -> Result<String, SandboxError> {
    let cargo_registry = ctx.cargo_home.join("registry");
    let cargo_git = ctx.cargo_home.join("git");
    let cargo_bin = ctx.cargo_home.join("bin");
    let rustup_toolchains = ctx.rustup_home.join("toolchains");
    let rustup_settings = ctx.rustup_home.join("settings.toml");

    let mut deny_clauses = String::new();
    for path in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home)
        .into_iter()
        .chain(ctx.deny_paths.iter().cloned())
    {
        let lit = sbpl_literal(&path);
        deny_clauses.push_str(&format!("(deny file-read* file-write* (subpath {lit}))\n"));
    }

    let mut body = TEMPLATE
        .replace("{{JAIL}}", &sbpl_literal(&profile.fs_jail))
        .replace("{{TMP}}", &sbpl_literal(&ctx.exec_dir.join("tmp")))
        .replace("{{HOME}}", &sbpl_literal(&ctx.exec_dir.join("home")))
        .replace("{{CARGO_REGISTRY}}", &sbpl_literal(&cargo_registry))
        .replace("{{CARGO_GIT}}", &sbpl_literal(&cargo_git))
        .replace("{{CARGO_BIN}}", &sbpl_literal(&cargo_bin))
        .replace("{{RUSTUP_TOOLCHAINS}}", &sbpl_literal(&rustup_toolchains))
        .replace("{{RUSTUP_SETTINGS}}", &sbpl_literal(&rustup_settings));
    body.push('\n');
    body.push_str(&deny_clauses);
    let _ = Path::new; // keep Path in scope for clarity
    Ok(body)
}

fn sbpl_literal(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\"', "\\\""))
}

/// Probe Seatbelt by applying a minimal profile (not just statting the binary).
pub fn probe_seatbelt_sync() -> Result<String, String> {
    let p = Path::new("/usr/bin/sandbox-exec");
    if !p.is_file() {
        return Err("/usr/bin/sandbox-exec missing".into());
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let sb = dir.path().join("probe.sb");
    let body = "(version 1)\n(allow default)\n";
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
}
