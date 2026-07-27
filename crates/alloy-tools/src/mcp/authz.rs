//! Token expiry and per-tool grant checks (RFC-0006 §5.5, RFC-0008 §3.8).
//!
//! Exec authorization reuses the RFC-0005 matcher (`sandbox::grant`) rather
//! than duplicating it: one authorization implementation, so a host pre-check
//! and the broker can never disagree about what a grant means.
//!
//! Filesystem grant-glob expansion lives in transport-neutral
//! [`crate::authz`] so EditEngine and MCP share one dialect (RFC-0008 AC 33).
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};

use alloy_runtime::{token_expired, Grant, PermissionToken};

use crate::authz::{self, GrantGlobError};
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
pub(crate) fn authorize_fs_read(perms: &PermissionToken, rel: &str) -> Result<(), McpError> {
    if !authz::has_fs_read_grant(perms) {
        return Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
            "fs_read".into(),
        )));
    }
    match authz::fs_read_covers(perms, rel) {
        Ok(true) => Ok(()),
        Ok(false) => Err(McpError::PermissionDenied(
            PermissionDenial::PathNotCovered(rel.to_string()),
        )),
        Err(GrantGlobError::Invalid(msg)) => {
            Err(McpError::InvalidToken(format!("grant glob: {msg}")))
        }
    }
}

/// Require at least one `Grant::FsWrite`.
///
/// Fine-grained per-path write grants are enforced via
/// [`authorize_fs_write_path`] after patch path extraction (RFC-0008).
pub(crate) fn authorize_fs_write(perms: &PermissionToken) -> Result<(), McpError> {
    if perms.grants.iter().any(|g| matches!(g, Grant::FsWrite(_))) {
        return Ok(());
    }
    Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
        "fs_write".into(),
    )))
}

/// Require `Grant::GitWrite` (RFC-0008 §3.8.4 — mutating `apply_patch` only).
pub(crate) fn authorize_git_write(perms: &PermissionToken) -> Result<(), McpError> {
    if perms.grants.iter().any(|g| matches!(g, Grant::GitWrite)) {
        return Ok(());
    }
    Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
        "git_write".into(),
    )))
}

/// Require an `FsWrite` grant covering the jail-relative path `rel`.
#[allow(dead_code)]
pub(crate) fn authorize_fs_write_path(perms: &PermissionToken, rel: &str) -> Result<(), McpError> {
    if !authz::has_fs_write_grant(perms) {
        return Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
            "fs_write".into(),
        )));
    }
    match authz::fs_write_covers(perms, rel) {
        Ok(true) => Ok(()),
        Ok(false) => Err(McpError::PermissionDenied(
            PermissionDenial::PathNotCovered(rel.to_string()),
        )),
        Err(GrantGlobError::Invalid(msg)) => {
            Err(McpError::InvalidToken(format!("grant glob: {msg}")))
        }
    }
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

    #[test]
    fn git_write_required() {
        assert!(matches!(
            authorize_git_write(&token(vec![Grant::FsWrite(Glob("**".into()))])),
            Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
                ref k
            ))) if k == "git_write"
        ));
        assert!(authorize_git_write(&token(vec![Grant::GitWrite])).is_ok());
    }

    #[test]
    fn fs_write_path_v4a_v4b() {
        assert!(matches!(
            authorize_fs_write_path(&token(vec![]), "a.rs"),
            Err(McpError::PermissionDenied(PermissionDenial::MissingGrant(
                ref k
            ))) if k == "fs_write"
        ));
        let t = token(vec![Grant::FsWrite(Glob("src/**".into()))]);
        assert!(authorize_fs_write_path(&t, "src/lib.rs").is_ok());
        assert!(matches!(
            authorize_fs_write_path(&t, "README.md"),
            Err(McpError::PermissionDenied(
                PermissionDenial::PathNotCovered(ref p)
            )) if p == "README.md"
        ));
    }
}
