//! Sandbox backend dispatch and probes.

#![allow(clippy::disallowed_methods)]

mod container;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod probe;

pub use container::ContainerBackend;
#[cfg(target_os = "linux")]
pub use linux::LinuxLandlockBackend;
#[cfg(target_os = "macos")]
pub use macos::MacosSeatbeltBackend;
pub use probe::probe_all;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::sandbox::process::SupervisedOutcome;
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxBackend, SandboxError};

/// Prepared isolation for one exec.
pub struct IsolateContext {
    /// Absolute canonical executable (native) or runtime binary (container).
    pub program: PathBuf,
    /// Argv for the child (argv0 = original).
    pub argv: Vec<String>,
    /// Canonical cwd inside jail.
    pub cwd: PathBuf,
    /// Child environment.
    pub env: BTreeMap<OsString, OsString>,
    /// Per-exec directory under jail.
    pub exec_dir: PathBuf,
    /// Operator cargo home.
    pub cargo_home: PathBuf,
    /// Operator rustup home.
    pub rustup_home: PathBuf,
    /// Deny paths to bind-over (absolute).
    pub deny_paths: Vec<PathBuf>,
    /// RO roots to allow.
    pub read_only_roots: Vec<PathBuf>,
}

/// Run `ctx` under `backend`.
pub async fn run_isolated(
    backend: SandboxBackend,
    profile: &SandboxProfile,
    ctx: IsolateContext,
) -> Result<SupervisedOutcome, SandboxError> {
    match backend {
        SandboxBackend::Landlock => {
            #[cfg(target_os = "linux")]
            {
                if matches!(profile.network, NetworkPolicy::Deny) {
                    LinuxLandlockBackend::exec(profile, ctx).await
                } else {
                    Err(SandboxError::Invalid(
                        "network=allow unsupported in MVP".into(),
                    ))
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (profile, ctx);
                Err(SandboxError::UnsupportedOs)
            }
        }
        SandboxBackend::Seatbelt => {
            #[cfg(target_os = "macos")]
            {
                MacosSeatbeltBackend::exec(profile, ctx).await
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (profile, ctx);
                Err(SandboxError::UnsupportedOs)
            }
        }
        SandboxBackend::Container => ContainerBackend::exec(profile, ctx).await,
    }
}

/// Collect deny-glob matches under `jail` (bounded walk).
pub fn collect_deny_paths(
    jail: &Path,
    deny: &globset::GlobSet,
) -> Result<Vec<PathBuf>, SandboxError> {
    use crate::sandbox::glob::deny_matches;

    let mut out = Vec::new();
    let mut stack = vec![jail.to_path_buf()];
    let mut visited = 0usize;
    const MAX: usize = 10_000;
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            visited += 1;
            if visited > MAX {
                return Err(SandboxError::Internal(
                    "deny-glob walk exceeded 10000 entries".into(),
                ));
            }
            let path = ent.path();
            let rel = match path.strip_prefix(jail) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if deny_matches(deny, &rel) {
                out.push(path.clone());
            }
            if path.is_dir() {
                // Don't descend into denied dirs for further matches? Still collect the dir itself.
                if !deny_matches(deny, &rel) {
                    stack.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// Allowlisted RO cargo/rustup subtrees that exist.
pub fn allowlisted_ro_subtrees(cargo_home: &Path, rustup_home: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for p in [
        cargo_home.join("registry"),
        cargo_home.join("git"),
        cargo_home.join("bin"),
        rustup_home.join("toolchains"),
    ] {
        if p.exists() {
            v.push(p);
        }
    }
    let settings = rustup_home.join("settings.toml");
    if settings.is_file() {
        v.push(settings);
    }
    v
}

/// Credential files to bind `/dev/null` over when present.
pub fn credential_bind_targets(cargo_home: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for name in ["credentials.toml", "credentials"] {
        let p = cargo_home.join(name);
        if p.is_file() {
            v.push(p);
        }
    }
    v
}
