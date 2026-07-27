//! Transport-neutral grant-glob matching (RFC-0008 §3.8.3).
//!
//! Shared by MCP FsRead/FsWrite prepare and EditEngine fine-grained path
//! authorization so there is exactly one expansion dialect.
//!
//! Author: arkadianet

use alloy_runtime::{Grant, PermissionToken};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use thiserror::Error;

/// Grant glob compilation failure.
#[derive(Debug, Error)]
pub(crate) enum GrantGlobError {
    /// Uncompilable or invalid grant glob pattern.
    #[error("grant glob: {0}")]
    Invalid(String),
}

/// Shared glob expansion used by FsRead and FsWrite (single implementation).
///
/// Expansion mirrors the RFC-0005 deny-glob dialect so a grant and a deny
/// pattern spelled the same way cover the same jail-relative paths. Every
/// matcher below compiles through this function, so there is exactly one
/// expansion dialect in the crate (AC 33).
pub(crate) fn expand_grant_glob(pattern: &str) -> Result<GlobSet, GrantGlobError> {
    let mut builder = GlobSetBuilder::new();
    add_fs_patterns(&mut builder, pattern)?;
    builder
        .build()
        .map_err(|e| GrantGlobError::Invalid(e.to_string()))
}

/// Grant globs of one kind, compiled once and reused for many paths.
///
/// The one-shot helpers recompile the token's globs on every call, which is
/// `O(paths × grants)` glob compilations in the validate/apply hot paths. Hold a
/// matcher for the duration of one authorization pass instead.
pub(crate) struct GrantMatcher {
    /// One compiled set per grant of the matched kind, in token order.
    sets: Vec<GlobSet>,
}

impl GrantMatcher {
    /// Compile every `Grant::FsWrite` glob in `perms`.
    pub(crate) fn fs_write(perms: &PermissionToken) -> Result<Self, GrantGlobError> {
        Self::compile(perms.grants.iter().filter_map(|g| match g {
            Grant::FsWrite(glob) => Some(glob.0.as_str()),
            _ => None,
        }))
    }

    /// Compile every `Grant::FsRead` glob in `perms`.
    pub(crate) fn fs_read(perms: &PermissionToken) -> Result<Self, GrantGlobError> {
        Self::compile(perms.grants.iter().filter_map(|g| match g {
            Grant::FsRead(glob) => Some(glob.0.as_str()),
            _ => None,
        }))
    }

    /// Whether the token carried at least one grant of the matched kind.
    ///
    /// Callers distinguish zero-grant (`MissingGrant`) from a miss
    /// (`PathNotCovered`), so this is not the same question as [`Self::covers`].
    pub(crate) fn has_grant(&self) -> bool {
        !self.sets.is_empty()
    }

    /// True when some compiled grant glob covers `rel` (jail-relative).
    pub(crate) fn covers(&self, rel: &str) -> bool {
        self.sets.iter().any(|set| set.is_match(rel))
    }

    fn compile<'a>(patterns: impl Iterator<Item = &'a str>) -> Result<Self, GrantGlobError> {
        let sets = patterns
            .map(expand_grant_glob)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { sets })
    }
}

/// True when some `Grant::FsWrite` glob covers `rel` (jail-relative).
///
/// `Ok(false)` when grants exist but none match; caller distinguishes zero-grant
/// (`MissingGrant("fs_write")`) from a miss (`PathNotCovered`). Prefer
/// [`GrantMatcher::fs_write`] when authorizing more than one path.
pub(crate) fn fs_write_covers(perms: &PermissionToken, rel: &str) -> Result<bool, GrantGlobError> {
    Ok(GrantMatcher::fs_write(perms)?.covers(rel))
}

/// True when some `Grant::FsRead` glob covers `rel` (jail-relative).
///
/// Same expansion as [`fs_write_covers`]. `Ok(false)` when grants exist but
/// none match; zero-grant is also `Ok(false)` — MCP wrappers distinguish.
pub(crate) fn fs_read_covers(perms: &PermissionToken, rel: &str) -> Result<bool, GrantGlobError> {
    Ok(GrantMatcher::fs_read(perms)?.covers(rel))
}

/// Whether the token carries at least one `FsRead` grant (regardless of match).
pub(crate) fn has_fs_read_grant(perms: &PermissionToken) -> bool {
    perms.grants.iter().any(|g| matches!(g, Grant::FsRead(_)))
}

/// Whether the token carries at least one `FsWrite` grant (regardless of match).
pub(crate) fn has_fs_write_grant(perms: &PermissionToken) -> bool {
    perms.grants.iter().any(|g| matches!(g, Grant::FsWrite(_)))
}

fn add_fs_patterns(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), GrantGlobError> {
    if pattern.contains('/') {
        add_glob(builder, pattern)?;
        if !pattern.starts_with("**/") {
            add_glob(builder, &format!("**/{pattern}"))?;
        }
    } else {
        add_glob(builder, pattern)?;
        add_glob(builder, &format!("**/{pattern}"))?;
    }
    Ok(())
}

fn add_glob(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), GrantGlobError> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(cfg!(target_os = "macos"))
        .backslash_escape(true)
        .build()
        .map_err(|e| GrantGlobError::Invalid(format!("`{pattern}`: {e}")))?;
    builder.add(glob);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{Glob, ProfileId, RunId};

    fn token(grants: Vec<Grant>) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants,
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn fs_write_covers_table() {
        let cases: &[(&str, &str, bool)] = &[
            ("src/main.rs", "src/main.rs", true),
            ("src/**", "src/main.rs", true),
            ("*.rs", "src/main.rs", true),
            ("*.rs", "main.rs", true),
            ("**/*.rs", "src/lib.rs", true),
            ("src/**", "README.md", false),
        ];
        for (pattern, rel, expect) in cases {
            let t = token(vec![Grant::FsWrite(Glob((*pattern).into()))]);
            assert_eq!(
                fs_write_covers(&t, rel).unwrap(),
                *expect,
                "glob={pattern:?} rel={rel:?}"
            );
        }
    }

    #[test]
    fn zero_fs_write_grants_returns_false() {
        let t = token(vec![Grant::FsRead(Glob("**".into()))]);
        assert!(!fs_write_covers(&t, "a.rs").unwrap());
        assert!(!has_fs_write_grant(&t));
    }

    #[test]
    fn expand_grant_glob_rejects_bad_pattern() {
        assert!(expand_grant_glob("src/[").is_err());
        assert!(
            GrantMatcher::fs_write(&token(vec![Grant::FsWrite(Glob("src/[".into()))])).is_err()
        );
    }

    #[test]
    fn compiled_matcher_agrees_with_one_shot_helper() {
        let t = token(vec![
            Grant::FsWrite(Glob("src/**".into())),
            Grant::FsWrite(Glob("*.toml".into())),
        ]);
        let matcher = GrantMatcher::fs_write(&t).unwrap();
        assert!(matcher.has_grant());
        for rel in ["src/main.rs", "Cargo.toml", "README.md", "docs/a/b.rs"] {
            assert_eq!(
                matcher.covers(rel),
                fs_write_covers(&t, rel).unwrap(),
                "rel={rel:?}"
            );
        }
        assert!(!GrantMatcher::fs_write(&token(vec![Grant::GitWrite]))
            .unwrap()
            .has_grant());
    }
}
