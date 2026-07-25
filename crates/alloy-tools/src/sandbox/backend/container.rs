//! Container backend (docker/podman) — RFC-0005 §5.5.

#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::env::{compose_container_env, format_env_file, ScrubInput};
use crate::sandbox::process::{spawn_runtime_command, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxBackend, SandboxError};

/// Docker/Podman backend.
pub struct ContainerBackend;

impl ContainerBackend {
    /// Run `ctx.argv` inside a container with jail bind-mounted at identical path.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        let runtime = resolve_runtime()?;
        let image = profile.container_image.clone();

        let home = ctx.exec_dir.join("home");
        let tmp = ctx.exec_dir.join("tmp");
        let cargo_cache = ctx.exec_dir.join("cargo-cache");
        std::fs::create_dir_all(&home).map_err(SandboxError::Io)?;
        std::fs::create_dir_all(&tmp).map_err(SandboxError::Io)?;
        std::fs::create_dir_all(&cargo_cache).map_err(SandboxError::Io)?;

        let scrub = ScrubInput {
            child_home: &home,
            child_tmpdir: &tmp,
            cargo_home: &ctx.cargo_home,
            rustup_home: &ctx.rustup_home,
            env_allow: &[], // already applied into ctx.env by broker; rebuild from composition
            quarantine: profile.quarantine_deps,
            path_value: None,
        };
        // Prefer composing from policy table; merge any extra keys already scrubbed
        // by reconstructing allow list from ctx — broker passes full env via envfile.
        let mut env_map = compose_container_env(&scrub)?;
        // Overlay non-conflicting extras from ctx.env that are safe identifiers.
        for (k, v) in &ctx.env {
            let Some(ks) = k.to_str() else { continue };
            let Some(vs) = v.to_str() else { continue };
            if matches!(ks, "PATH" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN") {
                continue;
            }
            env_map
                .entry(ks.to_string())
                .or_insert_with(|| vs.to_string());
        }
        let envfile = ctx.exec_dir.join("envfile");
        let body = format_env_file(&env_map)?;
        write_mode_0600(&envfile, body.as_bytes())?;

        let cidfile = ctx.exec_dir.join("cid");
        // Ensure cidfile parent exists; runtime creates the file.
        let _ = std::fs::remove_file(&cidfile);

        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();

        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            format!("--cidfile={}", cidfile.display()),
            format!("--user={uid}:{gid}"),
            format!("--workdir={}", ctx.cwd.display()),
            format!("--env-file={}", envfile.display()),
            format!(
                "--volume={}:{}:rw",
                profile.fs_jail.display(),
                profile.fs_jail.display()
            ),
        ];

        if matches!(profile.network, NetworkPolicy::Deny) {
            args.push("--network=none".into());
        }

        // RO allowlisted cargo/rustup subtrees (not whole homes).
        for p in crate::sandbox::backend::allowlisted_ro_subtrees(&ctx.cargo_home, &ctx.rustup_home)
        {
            // Skip host toolchain bin for container (image supplies toolchain).
            if p.ends_with("bin") && p.starts_with(&ctx.cargo_home) {
                continue;
            }
            if p.ends_with("toolchains") {
                continue;
            }
            if p.file_name().and_then(|s| s.to_str()) == Some("settings.toml") {
                continue;
            }
            args.push(format!("--volume={}:{}:ro", p.display(), p.display()));
        }
        // Writable per-exec cargo cache.
        args.push(format!(
            "--volume={}:{}:rw",
            cargo_cache.display(),
            cargo_cache.display()
        ));

        // Deny / credential bind-overs.
        for path in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home)
            .into_iter()
            .chain(ctx.deny_paths.iter().cloned())
        {
            if path.is_file() {
                args.push(format!("--volume=/dev/null:{}:ro", path.display()));
            } else if path.is_dir() {
                let empty = ctx.exec_dir.join(format!(
                    "c-empty-{}",
                    path.file_name().and_then(|s| s.to_str()).unwrap_or("dir")
                ));
                std::fs::create_dir_all(&empty).map_err(SandboxError::Io)?;
                args.push(format!(
                    "--volume={}:{}:ro",
                    empty.display(),
                    path.display()
                ));
            }
        }

        args.push(image);
        args.extend(ctx.argv.iter().cloned());

        let host_env = BTreeMap::<OsString, OsString>::new();
        let outcome = spawn_runtime_command(
            &runtime,
            &args,
            Path::new("/"),
            &host_env,
            profile.stdout_cap,
            profile.stderr_cap,
            profile.exec_timeout,
        )
        .await;

        // Cleanup exec_dir best-effort after run (broker also cleans).
        if let Err(SandboxError::Timeout(_)) = &outcome {
            kill_cid(&runtime, &cidfile).await;
        }

        let outcome = outcome?;
        map_container_status(outcome)
    }
}

fn map_container_status(mut outcome: SupervisedOutcome) -> Result<SupervisedOutcome, SandboxError> {
    // When the runtime itself fails, exit codes follow docker conventions.
    if let Some(code) = outcome.exit_code {
        match code {
            125 => {
                return Err(SandboxError::BackendUnavailable {
                    backend: SandboxBackend::Container,
                    message: format!(
                        "container runtime failed to start: {}",
                        String::from_utf8_lossy(&outcome.stderr)
                    ),
                });
            }
            126 => {
                return Err(SandboxError::Internal(
                    "container conflict/usage error (126)".into(),
                ));
            }
            127 => {
                // Command not found in image — Ok with 127.
            }
            n if (128..=255).contains(&n) => {
                let sig = n - 128;
                if (1..=127).contains(&sig) {
                    outcome.exit_code = None;
                    outcome.signal = Some(sig);
                }
            }
            _ => {}
        }
    }
    Ok(outcome)
}

async fn kill_cid(runtime: &Path, cidfile: &Path) {
    if let Ok(cid) = std::fs::read_to_string(cidfile) {
        let cid = cid.trim();
        if cid.is_empty() {
            return;
        }
        let _ = spawn_runtime_command(
            runtime,
            &["kill".into(), "--signal".into(), "TERM".into(), cid.into()],
            Path::new("/"),
            &BTreeMap::new(),
            1024,
            1024,
            Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = spawn_runtime_command(
            runtime,
            &["kill".into(), cid.into()],
            Path::new("/"),
            &BTreeMap::new(),
            1024,
            1024,
            Duration::from_secs(5),
        )
        .await;
    }
}

fn write_mode_0600(path: &Path, bytes: &[u8]) -> Result<(), SandboxError> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(SandboxError::Io)?;
    f.write_all(bytes).map_err(SandboxError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = f.metadata().map_err(SandboxError::Io)?.permissions();
        perms.set_mode(0o600);
        f.set_permissions(perms).map_err(SandboxError::Io)?;
    }
    Ok(())
}

fn resolve_runtime() -> Result<PathBuf, SandboxError> {
    if let Ok(rt) = std::env::var("ALLOY_CONTAINER_RUNTIME") {
        if !rt.is_empty() {
            let p = PathBuf::from(&rt);
            if p.is_file() || which::which(&rt).is_ok() {
                return Ok(which::which(&rt).unwrap_or(p));
            }
            return Err(SandboxError::BackendUnavailable {
                backend: SandboxBackend::Container,
                message: format!("ALLOY_CONTAINER_RUNTIME `{rt}` not found"),
            });
        }
    }
    for name in ["docker", "podman"] {
        if let Ok(p) = which::which(name) {
            return Ok(p);
        }
    }
    Err(SandboxError::BackendUnavailable {
        backend: SandboxBackend::Container,
        message: "neither docker nor podman found on PATH; set ALLOY_CONTAINER_RUNTIME or install a runtime"
            .into(),
    })
}

/// Probe container runtime availability.
pub fn probe_container_sync() -> Result<String, String> {
    match resolve_runtime() {
        Ok(p) => Ok(format!("runtime={}", p.display())),
        Err(e) => Err(e.to_string()),
    }
}
