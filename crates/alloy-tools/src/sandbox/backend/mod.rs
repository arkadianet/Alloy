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

/// Directories skipped during deny-glob walks (build/VCS noise only).
///
/// Do **not** prune `node_modules` — it commonly holds `.env` / secrets that
/// deny-globs must still bind over. Truncation is fail-closed (see below).
const SKIP_DIR_NAMES: &[&str] = &["target", ".git", ".alloy-sbx", "alloy-sbx-binds"];

/// Collect deny-glob matches under `jail`.
///
/// - Fail closed when the entry budget is exhausted: a truncated list would
///   leave in-jail secrets readable (the jail is Landlock RW).
/// - Uses `DirEntry::file_type()` so symlinks are never followed out of the jail.
/// - Prunes only build/VCS directories that are not expected to hold deny matches.
pub fn collect_deny_paths(
    jail: &Path,
    deny: &globset::GlobSet,
) -> Result<Vec<PathBuf>, SandboxError> {
    collect_deny_paths_with_budget(jail, deny, 10_000)
}

/// Same as [`collect_deny_paths`] with an explicit entry budget (testable).
pub fn collect_deny_paths_with_budget(
    jail: &Path,
    deny: &globset::GlobSet,
    max_entries: usize,
) -> Result<Vec<PathBuf>, SandboxError> {
    let mut out = Vec::new();
    let mut stack = vec![jail.to_path_buf()];
    let mut visited = 0usize;

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            visited += 1;
            if visited > max_entries {
                return Err(SandboxError::Internal(format!(
                    "deny-glob walk exceeded {max_entries} entries under {}; \
                     refusing to exec with a partial credential bind-over list",
                    jail.display()
                )));
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
///
/// Closed set matching RFC-0005 §5.5 item 4. Do **not** add `config.toml` —
/// operators may store registry tokens there, and the credential bind-over
/// only covers `credentials.toml` / `credentials`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::glob::{compile_deny_globs, default_deny_globs};

    #[test]
    fn deny_walk_budget_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path();
        // Enough sibling files to blow a tiny budget before hitting `.env`.
        for i in 0..40 {
            std::fs::write(jail.join(format!("pad-{i}")), b"x").unwrap();
        }
        std::fs::write(jail.join(".env"), b"SECRET=1\n").unwrap();
        let set = compile_deny_globs(&default_deny_globs()).unwrap();
        let err = collect_deny_paths_with_budget(jail, &set, 10).unwrap_err();
        assert!(
            matches!(err, SandboxError::Internal(ref m) if m.contains("exceeded")),
            "got {err:?}"
        );
    }

    #[test]
    fn deny_walk_finds_env_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path();
        std::fs::create_dir_all(jail.join("node_modules/pkg")).unwrap();
        std::fs::write(jail.join("node_modules/pkg/.env"), b"SECRET=1\n").unwrap();
        let set = compile_deny_globs(&default_deny_globs()).unwrap();
        let found = collect_deny_paths_with_budget(jail, &set, 10_000).unwrap();
        assert!(
            found.iter().any(|p| p.ends_with(".env")),
            "node_modules .env must be collected; got {found:?}"
        );
    }
}
