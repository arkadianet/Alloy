//! WorkspaceDigest computation (RFC-0008 §5.8).
//!
//! Author: arkadianet

use std::collections::BTreeSet;
use std::fs;

use alloy_runtime::{Digest, EditError, WorkspaceDigest};

use crate::edit::patch_parse::is_digest_excluded_path;
use crate::sandbox::PathPolicy;

/// Compute a digest over tracked files plus transaction-created paths.
pub(crate) fn compute_workspace_digest(
    policy: &PathPolicy,
    tracked: &BTreeSet<String>,
    created_paths: &[String],
    max_files: u64,
    max_bytes: u64,
) -> Result<WorkspaceDigest, EditError> {
    let mut paths = BTreeSet::new();
    for path in tracked {
        if include_path(policy, path) {
            paths.insert(path.clone());
        }
    }
    for path in created_paths {
        if include_path(policy, path) && policy.jail().join(path).exists() {
            paths.insert(path.clone());
        }
    }

    let mut encoding = String::new();
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    for rel in paths {
        let path = policy.jail().join(&rel);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(|e| EditError::Io(e.to_string()))?;
        file_count = file_count.saturating_add(1);
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        if file_count > max_files {
            return Err(EditError::DigestLimitExceeded("file count".into()));
        }
        if total_bytes > max_bytes {
            return Err(EditError::DigestLimitExceeded("byte count".into()));
        }
        let digest = Digest::sha256(&bytes);
        encoding.push_str(&rel);
        encoding.push('\0');
        encoding.push_str(digest.as_hex());
        encoding.push('\n');
    }
    Ok(WorkspaceDigest {
        tree: Digest::sha256(encoding.as_bytes()),
        file_count,
        total_bytes,
    })
}

fn include_path(policy: &PathPolicy, rel: &str) -> bool {
    !(rel == ".git"
        || rel.starts_with(".git/")
        || rel == ".alloy-sbx"
        || rel.starts_with(".alloy-sbx/")
        || is_digest_excluded_path(rel)
        || policy.deny_matches_rel(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxProfile;

    fn policy(dir: &std::path::Path) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(dir.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, Vec::new()).unwrap()
    }

    #[test]
    fn digest_excludes_target_and_denies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::write(dir.path().join("target/x"), b"x").unwrap();
        let tracked = ["a.txt", ".env", "target/x"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let digest = compute_workspace_digest(&policy(dir.path()), &tracked, &[], 10, 10).unwrap();
        assert_eq!(digest.file_count, 1);
        assert_eq!(digest.total_bytes, 1);
    }

    #[test]
    fn digest_limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"abc").unwrap();
        let tracked = ["a.txt"].into_iter().map(str::to_string).collect();
        assert!(matches!(
            compute_workspace_digest(&policy(dir.path()), &tracked, &[], 0, 10),
            Err(EditError::DigestLimitExceeded(_))
        ));
        assert!(matches!(
            compute_workspace_digest(&policy(dir.path()), &tracked, &[], 10, 1),
            Err(EditError::DigestLimitExceeded(_))
        ));
    }
}
