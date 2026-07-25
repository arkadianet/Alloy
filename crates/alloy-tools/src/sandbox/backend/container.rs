//! Container backend (docker/podman) — RFC-0005 §5.5.

#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::env::{compose_container_env, format_env_file, ScrubInput};
use crate::sandbox::grant::{trusted_path_dirs, trusted_roots};
use crate::sandbox::process::{spawn_runtime_command, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxBackend, SandboxError};

/// Docker/Podman backend.
pub struct ContainerBackend;

/// Ensures the container is killed on every exit path (including drop).
struct CidGuard {
    runtime: PathBuf,
    cidfile: PathBuf,
    armed: bool,
}

impl CidGuard {
    fn new(runtime: PathBuf, cidfile: PathBuf) -> Self {
        Self {
            runtime,
            cidfile,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CidGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best-effort synchronous kill; async kill is used on the Timeout path too.
        if let Ok(cid) = std::fs::read_to_string(&self.cidfile) {
            let cid = cid.trim();
            if cid.is_empty() {
                return;
            }
            let _ = std::process::Command::new(&self.runtime)
                .args(["kill", "--signal", "TERM", cid])
                .status();
            std::thread::sleep(Duration::from_millis(200));
            let _ = std::process::Command::new(&self.runtime)
                .args(["kill", cid])
                .status();
        }
    }
}

impl ContainerBackend {
    /// Run `ctx.argv` inside a container with jail bind-mounted at identical path.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        if matches!(profile.network, NetworkPolicy::Allow) {
            return Err(SandboxError::Invalid(
                "network=allow unsupported in MVP".into(),
            ));
        }

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
            env_allow: &[],
            quarantine: profile.quarantine_deps,
            path_value: None,
        };
        let mut env_map = compose_container_env(&scrub)?;
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
        let _ = std::fs::remove_file(&cidfile);
        let mut cid_guard = CidGuard::new(runtime.clone(), cidfile.clone());

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

        // network=Deny is required (validated above); always enforce.
        args.push("--network=none".into());

        for p in crate::sandbox::backend::allowlisted_ro_subtrees(&ctx.cargo_home, &ctx.rustup_home)
        {
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
        args.push(format!(
            "--volume={}:{}:rw",
            cargo_cache.display(),
            cargo_cache.display()
        ));

        // Empty RO dirs for directory denies — outside the jail.
        let bind_root = std::env::temp_dir()
            .join("alloy-sbx-binds")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&bind_root).map_err(SandboxError::Io)?;
        let mut empty_idx = 0usize;

        for path in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home)
            .into_iter()
            .chain(ctx.deny_paths.iter().cloned())
        {
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_file() || meta.file_type().is_symlink() {
                args.push(format!("--volume=/dev/null:{}:ro", path.display()));
            } else if meta.is_dir() {
                let empty = bind_root.join(format!("empty-{empty_idx}"));
                empty_idx += 1;
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

        // Runtime needs a usable host env (PATH, HOME, DOCKER_HOST, XDG_RUNTIME_DIR).
        let host_env = runtime_host_env();
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

        match &outcome {
            Err(SandboxError::Timeout(_)) => {
                kill_cid_async(&runtime, &cidfile).await;
            }
            Err(_) => {
                // Drop guard also kills; kick TERM early for non-timeout failures.
                kill_cid_async(&runtime, &cidfile).await;
            }
            Ok(_) => {
                cid_guard.disarm();
            }
        }

        let outcome = outcome?;
        cid_guard.disarm();
        map_container_status(outcome)
    }
}

fn runtime_host_env() -> BTreeMap<OsString, OsString> {
    let mut map = BTreeMap::new();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "CONTAINER_HOST",
    ] {
        if let Some(v) = std::env::var_os(key) {
            map.insert(OsString::from(key), v);
        }
    }
    map
}

fn map_container_status(mut outcome: SupervisedOutcome) -> Result<SupervisedOutcome, SandboxError> {
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
            127 => {}
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

async fn kill_cid_async(runtime: &Path, cidfile: &Path) {
    if let Ok(cid) = std::fs::read_to_string(cidfile) {
        let cid = cid.trim().to_string();
        if cid.is_empty() {
            return;
        }
        let _ = spawn_runtime_command(
            runtime,
            &["kill".into(), "--signal".into(), "TERM".into(), cid.clone()],
            Path::new("/"),
            &runtime_host_env(),
            1024,
            1024,
            Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = spawn_runtime_command(
            runtime,
            &["kill".into(), cid],
            Path::new("/"),
            &runtime_host_env(),
            1024,
            1024,
            Duration::from_secs(5),
        )
        .await;
    }
}

fn write_mode_0600(path: &Path, bytes: &[u8]) -> Result<(), SandboxError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(SandboxError::Io)?;
    f.write_all(bytes).map_err(SandboxError::Io)?;
    Ok(())
}

fn resolve_runtime() -> Result<PathBuf, SandboxError> {
    let homes = crate::sandbox::env::OperatorHomes::resolve().ok();
    let mut roots = trusted_path_dirs(
        homes.as_ref().map(|h| h.cargo_home.as_path()),
        homes.as_ref().map(|h| h.rustup_home.as_path()),
    );
    roots.extend(trusted_roots(
        homes.as_ref().map(|h| h.cargo_home.as_path()),
        homes.as_ref().map(|h| h.rustup_home.as_path()),
    ));

    if let Ok(rt) = std::env::var("ALLOY_CONTAINER_RUNTIME") {
        if !rt.is_empty() {
            return resolve_named_runtime(&rt, &roots);
        }
    }
    for name in ["docker", "podman"] {
        if let Ok(p) = resolve_named_runtime(name, &roots) {
            return Ok(p);
        }
    }
    Err(SandboxError::BackendUnavailable {
        backend: SandboxBackend::Container,
        message: "neither docker nor podman found on trusted PATH; set ALLOY_CONTAINER_RUNTIME or install a runtime"
            .into(),
    })
}

fn resolve_named_runtime(name: &str, roots: &[PathBuf]) -> Result<PathBuf, SandboxError> {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        let canon = p
            .canonicalize()
            .map_err(|e| SandboxError::BackendUnavailable {
                backend: SandboxBackend::Container,
                message: format!("ALLOY_CONTAINER_RUNTIME `{name}`: {e}"),
            })?;
        ensure_trusted(&canon, roots)?;
        return Ok(canon);
    }
    // Search trusted bin dirs only.
    for dir in roots {
        let candidate = dir.join(name);
        if candidate.is_file() {
            if let Ok(canon) = candidate.canonicalize() {
                ensure_trusted(&canon, roots)?;
                return Ok(canon);
            }
        }
    }
    Err(SandboxError::BackendUnavailable {
        backend: SandboxBackend::Container,
        message: format!("container runtime `{name}` not found on trusted PATH"),
    })
}

fn ensure_trusted(path: &Path, roots: &[PathBuf]) -> Result<(), SandboxError> {
    let ok = roots.iter().any(|r| {
        r.canonicalize()
            .map(|rr| path.starts_with(rr))
            .unwrap_or(false)
            || path.starts_with(r)
    });
    if ok {
        Ok(())
    } else {
        Err(SandboxError::BackendUnavailable {
            backend: SandboxBackend::Container,
            message: format!("container runtime {} outside trusted roots", path.display()),
        })
    }
}

/// Probe container runtime availability (CLI present under trusted roots).
pub fn probe_container_sync() -> Result<String, String> {
    match resolve_runtime() {
        Ok(p) => {
            // Best-effort daemon ping; absence is still Available for CLI presence
            // but detail notes whether the daemon answered.
            let ping = std::process::Command::new(&p)
                .args(["info"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match ping {
                Ok(s) if s.success() => Ok(format!("runtime={} daemon=ok", p.display())),
                _ => Ok(format!(
                    "runtime={} daemon=unreachable (exec may return 125)",
                    p.display()
                )),
            }
        }
        Err(e) => Err(e.to_string()),
    }
}
