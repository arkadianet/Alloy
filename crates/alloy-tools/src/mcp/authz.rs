//! Token expiry and per-tool grant checks (RFC-0006 §5.5).
//!
//! Exec authorization reuses the RFC-0005 matcher (`sandbox::grant`) rather
//! than duplicating it: one authorization implementation, so a host pre-check
//! and the broker can never disagree about what a grant means.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};

use alloy_runtime::{token_expired, Grant, PermissionToken};
use globset::{GlobBuilder, GlobSetBuilder};

use crate::mcp::error::{map_sandbox_error, McpError, PermissionDenial};
use crate::sandbox::grant::match_exec_grant;
use crate::sandbox::SandboxBackend;

/// Reject an expired token. Expiry is inclusive: `now == expires` is expired.
pub(crate) fn check_expiry(perms: &PermissionToken) -> Result<(), McpError> {
    if token_expired(perms.expires.as_ref()) {
        return Err(McpError::TokenExpired);
    }
    Ok(())
}

/// Exec pre-check through the shared RFC-0005 grant matcher.
///
/// Path-form and basename-form `ExecAllow.binary` both work because this is the
/// same function the broker runs before spawning.
pub(crate) fn authorize_exec(
    perms: &PermissionToken,
    argv: &[String],
    backend: SandboxBackend,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<(), McpError> {
    match_exec_grant(perms, argv, backend, cwd, trusted_path)
        .map(|_| ())
        .map_err(map_sandbox_error)
}

/// Require at least one `Grant::FsRead` glob covering `rel` (jail-relative).
///
/// All `FsRead` grant patterns are compiled into a single [`GlobSet`] so a
/// token with N grants pays one `build()`, not N, and an uncompilable pattern
/// fails independently of grant order.
pub(crate) fn authorize_fs_read(perms: &PermissionToken, rel: &str) -> Result<(), McpError> {
    let mut builder = GlobSetBuilder::new();
    let mut saw_grant = false;
    for grant in &perms.grants {
        let Grant::FsRead(glob) = grant else {
            continue;
        };
        saw_grant = true;
        add_fs_read_patterns(&mut builder, &glob.0)?;
    }
    if !saw_grant {
        return Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
            "fs_read".into(),
        )));
    }
    let set = builder
        .build()
        .map_err(|e| McpError::InvalidToken(format!("grant glob: {e}")))?;
    if set.is_match(rel) {
        return Ok(());
    }
    // `rel` is jail-relative, so echoing it leaks no operator layout.
    Err(McpError::PermissionDenied(
        PermissionDenial::PathNotCovered(rel.to_string()),
    ))
}

/// Require at least one `Grant::FsWrite`.
///
/// Fine-grained per-path write grants need patch-body path extraction and are
/// owned by RFC-0008.
pub(crate) fn authorize_fs_write(perms: &PermissionToken) -> Result<(), McpError> {
    if perms.grants.iter().any(|g| matches!(g, Grant::FsWrite(_))) {
        return Ok(());
    }
    Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
        "fs_write".into(),
    )))
}

/// Expand one `FsRead` grant glob under the RFC-0006 §5.5 dialect into `builder`.
///
/// Expansion mirrors the RFC-0005 deny-glob expansion so a grant and a deny
/// pattern spelled the same way cover the same jail-relative paths.
fn add_fs_read_patterns(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), McpError> {
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

fn add_glob(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), McpError> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(cfg!(target_os = "macos"))
        .backslash_escape(true)
        .build()
        .map_err(|e| McpError::InvalidToken(format!("grant glob `{pattern}`: {e}")))?;
    builder.add(glob);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{Glob, ProfileId, RunId, Timestamp};
    use std::time::Duration;

    fn token(grants: Vec<Grant>) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants,
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn token_expired_inclusive() {
        let mut t = token(vec![]);
        t.expires = Some(Timestamp::now());
        assert!(matches!(check_expiry(&t), Err(McpError::TokenExpired)));

        let mut future = token(vec![]);
        future.expires = Some(Timestamp(Timestamp::now().0 + Duration::from_secs(600)));
        assert!(check_expiry(&future).is_ok());

        assert!(check_expiry(&token(vec![])).is_ok());
    }

    #[test]
    fn fs_read_grant_examples_table() {
        // (grant glob, jail-relative path, expected match)
        let cases: &[(&str, &str, bool)] = &[
            ("src/main.rs", "src/main.rs", true),
            ("src/**", "src/main.rs", true),
            ("*.rs", "src/main.rs", true),
            ("*.rs", "main.rs", true),
            ("**/*.rs", "src/lib.rs", true),
            ("src/**", "README.md", false),
        ];
        for (pattern, rel, expect) in cases {
            let t = token(vec![Grant::FsRead(Glob((*pattern).into()))]);
            let got = authorize_fs_read(&t, rel).is_ok();
            assert_eq!(got, *expect, "glob={pattern:?} rel={rel:?}");
        }
        // §5.5 final row: grant glob `.env` covers the relative path, but
        // PathPolicy deny globs win first in the fs_read pipeline (integration:
        // `fs_read_dotenv_denied_integration`).
        let dotenv = token(vec![Grant::FsRead(Glob(".env".into()))]);
        assert!(
            authorize_fs_read(&dotenv, ".env").is_ok(),
            "grant alone would cover .env; PathPolicy deny is earlier in the pipeline"
        );
    }

    #[test]
    fn fs_read_requires_fs_read_grant() {
        let t = token(vec![Grant::GitWrite]);
        assert!(matches!(
            authorize_fs_read(&t, "src/main.rs"),
            Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
                ref k
            ))) if k == "fs_read"
        ));
    }

    #[test]
    fn fs_read_uncovered_path_reports_relative() {
        let t = token(vec![Grant::FsRead(Glob("src/**".into()))]);
        assert!(matches!(
            authorize_fs_read(&t, "README.md"),
            Err(McpError::PermissionDenied(
                PermissionDenial::PathNotCovered(ref p)
            )) if p == "README.md"
        ));
    }

    #[test]
    fn uncompilable_grant_glob_is_invalid_token() {
        let t = token(vec![Grant::FsRead(Glob("src/[".into()))]);
        assert!(matches!(
            authorize_fs_read(&t, "src/main.rs"),
            Err(McpError::InvalidToken(_))
        ));
    }

    #[test]
    fn fs_write_missing_grant() {
        assert!(matches!(
            authorize_fs_write(&token(vec![Grant::FsRead(Glob("**".into()))])),
            Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
                ref k
            ))) if k == "fs_write"
        ));
        assert!(authorize_fs_write(&token(vec![Grant::FsWrite(Glob("**".into()))])).is_ok());
    }
}
