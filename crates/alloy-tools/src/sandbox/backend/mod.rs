//! Collect deny-glob matches and shared backend helpers.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::sandbox::glob::deny_matches;
use crate::sandbox::process::SupervisedOutcome;
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxBackend, SandboxError};

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
    // Every backend refuses policies it will not enforce.
    if matches!(profile.network, NetworkPolicy::Allow) {
        return Err(SandboxError::Invalid(
            "network=allow unsupported in MVP".into(),
        ));
    }
    match backend {
        SandboxBackend::Landlock => {
            #[cfg(target_os = "linux")]
            {
                LinuxLandlockBackend::exec(profile, ctx).await
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

/// Directories skipped during deny-glob walks (build/VCS noise).
const SKIP_DIR_NAMES: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    ".alloy-sbx",
    "alloy-sbx-binds",
];

/// Collect deny-glob matches under `jail`.
///
/// - Does **not** fail closed when the entry budget is exhausted (logs + returns
///   what was found). Ordinary large workspaces must still exec.
/// - Uses `DirEntry::file_type()` so symlinks are never followed out of the jail.
/// - Prunes common build/VCS directories.
pub fn collect_deny_paths(
    jail: &Path,
    deny: &globset::GlobSet,
) -> Result<Vec<PathBuf>, SandboxError> {
    let mut out = Vec::new();
    let mut stack = vec![jail.to_path_buf()];
    let mut visited = 0usize;
    const MAX: usize = 10_000;
    let mut truncated = false;

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            visited += 1;
            if visited > MAX {
                truncated = true;
                break;
            }
            let path = ent.path();
            let name = ent.file_name();
            let name_str = name.to_string_lossy();

            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            // Never follow symlinks for descent; still deny-match the link path.
            let rel = match path.strip_prefix(jail) {
                Ok(r) => relative_components(r),
                Err(_) => continue,
            };
            if deny_matches(deny, &rel) {
                out.push(path.clone());
                continue; // do not descend into denied dirs
            }

            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if SKIP_DIR_NAMES.iter().any(|s| *s == name_str.as_ref()) {
                    continue;
                }
                stack.push(path);
            }
        }
        if truncated {
            break;
        }
    }

    if truncated {
        tracing::warn!(
            visited,
            matches = out.len(),
            "deny-glob walk truncated; spawn-time binds are a best-effort snapshot"
        );
    }
    Ok(out)
}

fn relative_components(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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
