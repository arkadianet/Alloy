//! macOS Seatbelt backend via `sandbox-exec` (RFC-0005 §5.5).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::process::{spawn_supervised, SpawnSpec, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::SandboxError;

/// Seatbelt isolation backend.
pub struct MacosSeatbeltBackend;

const TEMPLATE: &str = include_str!("macos/alloy-check.sb.template");

impl MacosSeatbeltBackend {
    /// Execute under `/usr/bin/sandbox-exec`.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        let sbpl = ctx.exec_dir.join("alloy-check.sb");
        let body = render_sbpl(profile, &ctx)?;
        std::fs::write(&sbpl, body).map_err(SandboxError::Io)?;

        // Ready-byte pipe: trampoline writes one byte after sandbox_init succeeds.
        // MVP: invoke sandbox-exec directly; if it fails before exec, map to BackendCannotEnforce.
        let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            return Err(SandboxError::BackendUnavailable {
                backend: crate::sandbox::types::SandboxBackend::Seatbelt,
                message: "/usr/bin/sandbox-exec not found".into(),
            });
        }

        let mut argv = vec![
            "sandbox-exec".into(),
            "-f".into(),
            sbpl.display().to_string(),
            "--".into(),
            ctx.program.display().to_string(),
        ];
        // Pass remaining args; argv0 for the inner binary is handled by sandbox-exec.
        if ctx.argv.len() > 1 {
            argv.extend(ctx.argv[1..].iter().cloned());
        }

        let outcome = spawn_supervised(SpawnSpec {
            program: sandbox_exec,
            argv,
            cwd: ctx.cwd,
            env: ctx.env,
            stdout_cap: profile.stdout_cap,
            stderr_cap: profile.stderr_cap,
            exec_timeout: profile.exec_timeout,
            pre_exec: None,
        })
        .await?;

        // If sandbox-exec failed to apply profile, stderr usually mentions sandbox-exec.
        if outcome.exit_code == Some(1)
            && String::from_utf8_lossy(&outcome.stderr).contains("sandbox-exec")
        {
            return Err(SandboxError::BackendCannotEnforce(format!(
                "sandbox-exec profile apply failed: {}",
                String::from_utf8_lossy(&outcome.stderr)
            )));
        }
        Ok(outcome)
    }
}

fn render_sbpl(profile: &SandboxProfile, ctx: &IsolateContext) -> Result<String, SandboxError> {
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
        .replace("{{CARGO_HOME}}", &sbpl_literal(&ctx.cargo_home))
        .replace("{{RUSTUP_HOME}}", &sbpl_literal(&ctx.rustup_home));
    body.push('\n');
    body.push_str(&deny_clauses);
    Ok(body)
}

fn sbpl_literal(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\"', "\\\""))
}

/// Probe Seatbelt availability.
pub fn probe_seatbelt_sync() -> Result<String, String> {
    let p = Path::new("/usr/bin/sandbox-exec");
    if p.is_file() {
        Ok("sandbox-exec present".into())
    } else {
        Err("/usr/bin/sandbox-exec missing".into())
    }
}

#[allow(dead_code)]
fn write_temp_sbpl(dir: &Path, body: &str) -> Result<PathBuf, SandboxError> {
    let path = dir.join("probe.sb");
    let mut f = std::fs::File::create(&path).map_err(SandboxError::Io)?;
    f.write_all(body.as_bytes()).map_err(SandboxError::Io)?;
    Ok(path)
}

#[allow(dead_code)]
fn probe_with_profile(sbpl: &Path) -> Result<(), String> {
    use std::process::Command;
    let out = Command::new("/usr/bin/sandbox-exec")
        .args(["-f", &sbpl.display().to_string(), "--", "/usr/bin/true"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sandbox-exec probe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}
