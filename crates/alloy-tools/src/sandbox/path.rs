//! [`PathPolicy`] — jail membership, deny-globs, RO-root write checks.

use std::path::{Component, Path, PathBuf};

use globset::GlobSet;

use crate::sandbox::glob::{compile_deny_globs, deny_matches};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{DenialReason, SandboxError};

/// Access kind for [`PathPolicy::authorize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    /// Read access (allowed on RO roots).
    Read,
    /// Write access (rejected on RO roots except broker per-exec cache carve-out).
    Write,
}

/// Shared path authorization for exec cwd and future `fs_read` (RFC-0006).
#[derive(Clone)]
pub struct PathPolicy {
    jail: PathBuf,
    deny: GlobSet,
    read_only_roots: Vec<PathBuf>,
    /// Per-execution writable carve-out (under jail); Write allowed here.
    write_carve_out: Option<PathBuf>,
}

impl PathPolicy {
    /// Build from profile deny-globs and RO roots.
    pub fn from_profile(
        profile: &SandboxProfile,
        read_only_roots: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        Self::from_profile_with_carve_out(profile, read_only_roots, None)
    }

    /// Like [`Self::from_profile`] with an optional per-exec RW carve-out.
    pub fn from_profile_with_carve_out(
        profile: &SandboxProfile,
        read_only_roots: Vec<PathBuf>,
        write_carve_out: Option<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let jail = profile.fs_jail.clone();
        let deny = compile_deny_globs(&profile.deny_globs)?;
        let read_only_roots = read_only_roots
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        let write_carve_out = write_carve_out.and_then(|p| p.canonicalize().ok());
        Ok(Self {
            jail,
            deny,
            read_only_roots,
            write_carve_out,
        })
    }

    /// Canonicalize + jail membership + deny-glob + RO-root write check.
    pub fn authorize(&self, path: &Path, access: PathAccess) -> Result<PathBuf, SandboxError> {
        let canon = canonicalize_path(path)?;
        if !is_within(&canon, &self.jail) {
            return Err(SandboxError::Denied(DenialReason::PathDenied(format!(
                "outside jail: {}",
                canon.display()
            ))));
        }
        let rel = jail_relative(&canon, &self.jail)?;
        if deny_matches(&self.deny, &rel) {
            return Err(SandboxError::Denied(DenialReason::PathDenied(rel)));
        }
        if matches!(access, PathAccess::Write) {
            let in_carve = self
                .write_carve_out
                .as_ref()
                .is_some_and(|c| is_within(&canon, c));
            if !in_carve
                && self
                    .read_only_roots
                    .iter()
                    .any(|root| is_within(&canon, root))
            {
                return Err(SandboxError::Denied(DenialReason::PathDenied(format!(
                    "write to read-only root: {}",
                    canon.display()
                ))));
            }
        }
        Ok(canon)
    }

    /// Cwd must canonicalize inside jail; deny-glob applies.
    pub fn authorize_cwd(&self, cwd: &Path) -> Result<PathBuf, SandboxError> {
        let canon = canonicalize_path(cwd)
            .map_err(|_| SandboxError::Denied(DenialReason::CwdOutsideJail))?;
        if !is_within(&canon, &self.jail) {
            return Err(SandboxError::Denied(DenialReason::CwdOutsideJail));
        }
        let rel = jail_relative(&canon, &self.jail)?;
        if deny_matches(&self.deny, &rel) {
            return Err(SandboxError::Denied(DenialReason::PathDenied(rel)));
        }
        Ok(canon)
    }

    /// Borrow the canonical jail root.
    #[must_use]
    pub fn jail(&self) -> &Path {
        &self.jail
    }
}

/// Canonicalize `path`. If the final component is missing, canonicalize the
/// parent and join the name. Symlink targets that leave the jail are rejected
/// by the caller via [`is_within`] after this returns the resolved path.
pub(crate) fn canonicalize_path(path: &Path) -> Result<PathBuf, SandboxError> {
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(_) => {
            let parent = path.parent().ok_or_else(|| {
                SandboxError::Denied(DenialReason::PathDenied(path.display().to_string()))
            })?;
            let name = path.file_name().ok_or_else(|| {
                SandboxError::Denied(DenialReason::PathDenied(path.display().to_string()))
            })?;
            let parent_canon = parent.canonicalize().map_err(|e| {
                SandboxError::Denied(DenialReason::PathDenied(format!("{}: {e}", path.display())))
            })?;
            // Reject `..` in the final component path construction already handled by Path.
            for c in path.components() {
                if matches!(c, Component::ParentDir) {
                    // Still allow if canonicalize succeeds on parent; final join is fine.
                    break;
                }
            }
            Ok(parent_canon.join(name))
        }
    }
}

pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn jail_relative(canon: &Path, jail: &Path) -> Result<String, SandboxError> {
    let rel = canon
        .strip_prefix(jail)
        .map_err(|_| SandboxError::Denied(DenialReason::PathDenied(canon.display().to_string())))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::profile::SandboxProfile;

    fn policy_in(dir: &Path) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(dir.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, vec![]).unwrap()
    }

    #[test]
    fn path_policy_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let p = policy_in(&jail);
        // Path outside via .. after canonicalize
        let outside = jail.join("..").canonicalize().unwrap();
        let err = p.authorize(&outside, PathAccess::Read).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::PathDenied(_))
                | SandboxError::Denied(DenialReason::CwdOutsideJail)
        ));
    }

    #[test]
    fn path_policy_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = jail.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let p = policy_in(&jail);
        let err = p.authorize(&link, PathAccess::Read).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::PathDenied(_))
        ));
    }

    #[test]
    fn path_policy_write_rejects_ro_root() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let ro = jail.join("ro-root");
        std::fs::create_dir_all(&ro).unwrap();
        let ro_canon = ro.canonicalize().unwrap();
        let profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        let p = PathPolicy::from_profile(&profile, vec![ro_canon.clone()]).unwrap();
        let target = ro_canon.join("x");
        std::fs::write(&target, b"x").unwrap();
        let err = p.authorize(&target, PathAccess::Write).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::PathDenied(_))
        ));
        // Read still ok
        assert!(p.authorize(&target, PathAccess::Read).is_ok());
    }

    #[test]
    fn deny_dotenv_in_jail() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let dotenv = jail.join(".env");
        std::fs::write(&dotenv, b"SECRET=1").unwrap();
        let p = policy_in(&jail);
        let err = p.authorize(&dotenv, PathAccess::Read).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::PathDenied(_))
        ));
    }
}
