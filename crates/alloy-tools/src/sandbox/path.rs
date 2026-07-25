//! [`PathPolicy`] — jail membership, deny-globs, RO-root write checks.
//!
//! RFC-0005 §3.6: `Read` is allowed inside the jail **and** under any
//! `read_only_roots` entry (those roots — allowlisted `cargo_home` /
//! `rustup_home` subtrees, §5.5 — normally live outside the jail). `Write` is
//! allowed only inside the jail, and never under an RO root except the
//! broker-owned per-execution writable cache carve-out. Anything else is
//! denied.

use std::path::{Path, PathBuf};

use globset::GlobSet;

use crate::sandbox::glob::{compile_deny_globs, deny_matches};
use crate::sandbox::profile::{canonicalize_jail, SandboxProfile};
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
    ///
    /// The jail is re-canonicalized here instead of trusting `profile.fs_jail`:
    /// a profile can be constructed directly and the jail may have been
    /// replaced by a symlink since load. RO roots that do not resolve are
    /// dropped (e.g. `cargo_home/git` on a machine that never fetched a git
    /// dependency). The carve-out must resolve and must sit inside the jail —
    /// RFC-0005 §5.5 places it at `fs_jail/.alloy-sbx/<id>/…` — so a
    /// carve-out pointing at persistent state is rejected rather than accepted.
    pub(crate) fn from_profile_with_carve_out(
        profile: &SandboxProfile,
        read_only_roots: Vec<PathBuf>,
        write_carve_out: Option<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let jail = canonicalize_jail(profile.fs_jail.clone())?;
        let deny = compile_deny_globs(&profile.deny_globs)?;
        let read_only_roots = read_only_roots
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        let write_carve_out = match write_carve_out {
            None => None,
            Some(p) => {
                let canon = p.canonicalize().map_err(|e| {
                    SandboxError::Invalid(format!("write carve-out {}: {e}", p.display()))
                })?;
                if !is_within(&canon, &jail) {
                    return Err(SandboxError::Invalid(format!(
                        "write carve-out outside jail: {}",
                        canon.display()
                    )));
                }
                Some(canon)
            }
        };
        Ok(Self {
            jail,
            deny,
            read_only_roots,
            write_carve_out,
        })
    }

    /// Canonicalize + jail membership + deny-glob + RO-root write check.
    ///
    /// Reads resolve against the jail first, then the RO roots. Writes are only
    /// ever authorized inside the jail, and inside the jail only outside the RO
    /// roots (or within the per-exec carve-out).
    pub fn authorize(&self, path: &Path, access: PathAccess) -> Result<PathBuf, SandboxError> {
        let canon = canonicalize_path(path)?;

        if is_within(&canon, &self.jail) {
            self.check_deny(&canon, &self.jail)?;
            if matches!(access, PathAccess::Write) && !self.in_carve_out(&canon) {
                if let Some(root) = self.matching_ro_root(&canon) {
                    return Err(write_to_ro_root(&canon, root));
                }
            }
            return Ok(canon);
        }

        // Outside the jail an RO root (allowlisted cargo/rustup subtree) is
        // still readable; deny-globs are matched relative to that root so
        // `<cargo_home>/registry/id_rsa` denies like a jail-relative hit.
        if let Some(root) = self.matching_ro_root(&canon) {
            self.check_deny(&canon, root)?;
            if matches!(access, PathAccess::Write) {
                return Err(write_to_ro_root(&canon, root));
            }
            return Ok(canon);
        }

        Err(SandboxError::Denied(DenialReason::PathDenied(format!(
            "outside jail: {}",
            canon.display()
        ))))
    }

    /// Cwd must canonicalize inside jail; deny-glob applies.
    ///
    /// RO roots are irrelevant here: a child never runs with its cwd outside
    /// the workspace jail, even for a readable root.
    pub fn authorize_cwd(&self, cwd: &Path) -> Result<PathBuf, SandboxError> {
        let canon = canonicalize_path(cwd)
            .map_err(|_| SandboxError::Denied(DenialReason::CwdOutsideJail))?;
        if !is_within(&canon, &self.jail) {
            return Err(SandboxError::Denied(DenialReason::CwdOutsideJail));
        }
        self.check_deny(&canon, &self.jail)?;
        Ok(canon)
    }

    /// Borrow the canonical jail root.
    #[must_use]
    pub(crate) fn jail(&self) -> &Path {
        &self.jail
    }

    fn check_deny(&self, canon: &Path, root: &Path) -> Result<(), SandboxError> {
        let rel = relative_for_matching(canon, root)?;
        if deny_matches(&self.deny, &rel) {
            return Err(SandboxError::Denied(DenialReason::PathDenied(rel)));
        }
        Ok(())
    }

    fn matching_ro_root(&self, canon: &Path) -> Option<&Path> {
        self.read_only_roots
            .iter()
            .filter(|root| is_within(canon, root))
            // Deepest match wins so deny-globs are relative to the most
            // specific root when roots nest.
            .max_by_key(|root| root.components().count())
            .map(PathBuf::as_path)
    }

    fn in_carve_out(&self, canon: &Path) -> bool {
        self.write_carve_out
            .as_ref()
            .is_some_and(|c| is_within(canon, c))
    }
}

fn write_to_ro_root(canon: &Path, root: &Path) -> SandboxError {
    SandboxError::Denied(DenialReason::PathDenied(format!(
        "write to read-only root {}: {}",
        root.display(),
        canon.display()
    )))
}

/// Canonicalize `path`. If the final component is missing, canonicalize the
/// parent and join the name. Symlink targets that leave the jail are rejected
/// by the caller via [`is_within`] after this returns the resolved path.
///
/// A trailing `..` cannot slip through the missing-final-component branch:
/// [`Path::file_name`] returns `None` for such a path, so it is denied. Any
/// earlier `..` is resolved by canonicalizing the parent.
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
            Ok(parent_canon.join(name))
        }
    }
}

pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Render `canon` relative to `root` with `/` separators (no leading `/`) for
/// deny-glob matching (RFC-0005 §3.6 step 3).
///
/// Built component-by-component rather than with `Path::to_string_lossy` on the
/// whole path: on Unix a literal `\` is a legal filename byte, so a blanket
/// `\` → `/` rewrite would invent separators. Components are used as-is when
/// they are valid UTF-8; only a non-UTF-8 component falls back to a lossy
/// rendering, and only for matching — the canonical `PathBuf` returned to the
/// caller keeps its original bytes. The U+FFFD substitutions a lossy component
/// introduces cannot make a path look like a literal deny pattern such as
/// `.env`, and wildcard patterns such as `*.pem` still match, so matching stays
/// conservative.
fn relative_for_matching(canon: &Path, root: &Path) -> Result<String, SandboxError> {
    let rel = canon
        .strip_prefix(root)
        .map_err(|_| SandboxError::Denied(DenialReason::PathDenied(canon.display().to_string())))?;
    let mut out = String::new();
    for component in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        let os = component.as_os_str();
        match os.to_str() {
            Some(s) => out.push_str(s),
            None => out.push_str(&os.to_string_lossy()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::profile::SandboxProfile;

    fn policy_in(dir: &Path) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(dir.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, vec![]).unwrap()
    }

    fn policy_with_ro(jail: &Path, ro_roots: Vec<PathBuf>) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(jail.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, ro_roots).unwrap()
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

    /// RFC-0005 §3.6: writes to an RO root are denied, reads are not — and the
    /// RO roots that matter (`CARGO_HOME` subtrees, §5.5) live outside the jail.
    #[test]
    fn path_policy_write_rejects_ro_root() {
        let jail_dir = tempfile::tempdir().unwrap();
        let jail = jail_dir.path().canonicalize().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        let registry = cargo_home.path().join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        let registry = registry.canonicalize().unwrap();
        assert!(!is_within(&registry, &jail));

        let p = policy_with_ro(&jail, vec![registry.clone()]);
        let target = registry.join("cached.crate");
        std::fs::write(&target, b"x").unwrap();

        let err = p.authorize(&target, PathAccess::Write).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::PathDenied(_))
        ));
        assert_eq!(
            p.authorize(&target, PathAccess::Read).unwrap(),
            target,
            "read of an RO root outside the jail must be authorized"
        );
    }

    #[test]
    fn path_policy_read_allows_ro_root_outside_jail() {
        let jail_dir = tempfile::tempdir().unwrap();
        let jail = jail_dir.path().canonicalize().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        let src = cargo_home.path().join("registry/src/crate-1.0");
        std::fs::create_dir_all(&src).unwrap();
        let ro_root = cargo_home.path().join("registry").canonicalize().unwrap();
        let file = src.canonicalize().unwrap().join("lib.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let p = policy_with_ro(&jail, vec![ro_root]);
        assert_eq!(p.authorize(&file, PathAccess::Read).unwrap(), file);
        // Its parent directory reads too, and the RO root itself.
        assert!(p.authorize(src.parent().unwrap(), PathAccess::Read).is_ok());
    }

    #[test]
    fn path_policy_outside_jail_and_ro_roots_denied() {
        let jail_dir = tempfile::tempdir().unwrap();
        let jail = jail_dir.path().canonicalize().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let file = elsewhere.path().canonicalize().unwrap().join("f");
        std::fs::write(&file, b"x").unwrap();

        let p = policy_with_ro(&jail, vec![]);
        for access in [PathAccess::Read, PathAccess::Write] {
            let err = p.authorize(&file, access).unwrap_err();
            assert!(
                matches!(err, SandboxError::Denied(DenialReason::PathDenied(ref m)) if m.contains("outside jail")),
                "unexpected error: {err}"
            );
        }
        let err = p.authorize_cwd(elsewhere.path()).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::CwdOutsideJail)
        ));
    }

    #[test]
    fn deny_globs_apply_relative_to_ro_root() {
        let jail_dir = tempfile::tempdir().unwrap();
        let jail = jail_dir.path().canonicalize().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        let ro_root = cargo_home.path().canonicalize().unwrap();
        let secret = ro_root.join("id_rsa");
        std::fs::write(&secret, b"key").unwrap();

        let p = policy_with_ro(&jail, vec![ro_root]);
        let err = p.authorize(&secret, PathAccess::Read).unwrap_err();
        assert!(
            matches!(err, SandboxError::Denied(DenialReason::PathDenied(ref m)) if m == "id_rsa"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_carve_out_under_jail_allows_write() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let cache = jail.join(".alloy-sbx/exec-1/cargo-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let ro = jail.join("ro-root");
        std::fs::create_dir_all(&ro).unwrap();

        let profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        let p = PathPolicy::from_profile_with_carve_out(
            &profile,
            vec![ro.canonicalize().unwrap(), cache.canonicalize().unwrap()],
            Some(cache.clone()),
        )
        .unwrap();

        let in_cache = cache.canonicalize().unwrap().join("unpacked");
        assert!(p.authorize(&in_cache, PathAccess::Write).is_ok());
        let in_ro = ro.canonicalize().unwrap().join("x");
        assert!(p.authorize(&in_ro, PathAccess::Write).is_err());
        assert!(p.authorize(&in_ro, PathAccess::Read).is_ok());
    }

    #[test]
    fn write_carve_out_outside_jail_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let profile = SandboxProfile::default_for_jail(jail).unwrap();
        let Err(err) = PathPolicy::from_profile_with_carve_out(
            &profile,
            vec![],
            Some(outside.path().to_path_buf()),
        ) else {
            panic!("carve-out outside the jail must be rejected");
        };
        assert!(
            matches!(err, SandboxError::Invalid(ref m) if m.contains("carve-out outside jail"))
        );
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

    #[test]
    fn jail_relative_keeps_backslash_component() {
        let rel = relative_for_matching(Path::new("/jail/a\\b/c"), Path::new("/jail")).unwrap();
        #[cfg(unix)]
        assert_eq!(rel, "a\\b/c");
        #[cfg(windows)]
        assert_eq!(rel, "a/b/c");
    }
}
