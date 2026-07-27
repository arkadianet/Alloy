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
/// pattern spelled the same way cover the same jail-relative paths.
#[allow(dead_code)]
pub(crate) fn expand_grant_glob(pattern: &str) -> Result<GlobSet, GrantGlobError> {
    let mut builder = GlobSetBuilder::new();
    add_fs_patterns(&mut builder, pattern)?;
    builder
        .build()
        .map_err(|e| GrantGlobError::Invalid(e.to_string()))
}

/// True when some `Grant::FsWrite` glob covers `rel` (jail-relative).
///
/// `Ok(false)` when grants exist but none match; caller distinguishes zero-grant
/// (`MissingGrant("fs_write")`) from a miss (`PathNotCovered`).
pub(crate) fn fs_write_covers(perms: &PermissionToken, rel: &str) -> Result<bool, GrantGlobError> {
    let mut builder = GlobSetBuilder::new();
    let mut saw_grant = false;
    for grant in &perms.grants {
        let Grant::FsWrite(glob) = grant else {
            continue;
        };
        saw_grant = true;
        add_fs_patterns(&mut builder, &glob.0)?;
    }
    if !saw_grant {
        return Ok(false);
    }
    let set = builder
        .build()
        .map_err(|e| GrantGlobError::Invalid(e.to_string()))?;
    Ok(set.is_match(rel))
}

/// True when some `Grant::FsRead` glob covers `rel` (jail-relative).
///
/// Same expansion as [`fs_write_covers`]. `Ok(false)` when grants exist but
/// none match; zero-grant is also `Ok(false)` — MCP wrappers distinguish.
pub(crate) fn fs_read_covers(perms: &PermissionToken, rel: &str) -> Result<bool, GrantGlobError> {
    let mut builder = GlobSetBuilder::new();
    let mut saw_grant = false;
    for grant in &perms.grants {
        let Grant::FsRead(glob) = grant else {
            continue;
        };
        saw_grant = true;
        add_fs_patterns(&mut builder, &glob.0)?;
    }
    if !saw_grant {
        return Ok(false);
    }
    let set = builder
        .build()
        .map_err(|e| GrantGlobError::Invalid(e.to_string()))?;
    Ok(set.is_match(rel))
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
    }
}
