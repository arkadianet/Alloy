//! WorkspaceDigest computation (RFC-0008 §5.8).
//!
//! Author: arkadianet

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;

use alloy_runtime::{Digest, DigestHasher, EditError, WorkspaceDigest};

use crate::edit::patch_parse::is_digest_excluded_path;
use crate::sandbox::PathPolicy;

/// Chunk size for content hashing; large enough to keep syscall overhead low
/// without scaling memory with file size.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Compute a digest over tracked files plus transaction-created paths.
///
/// The tree encoding (`path\0<content-sha-hex>\n` per sorted path, hashed as one
/// stream) and both counters are byte-identical to a buffered implementation;
/// neither the per-file contents nor the encoding is ever held whole in memory,
/// so a 50k-file workspace costs one chunk buffer rather than its own size.
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

    let mut tree = DigestHasher::new();
    let mut buf = vec![0_u8; READ_CHUNK_BYTES];
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
        file_count = file_count.saturating_add(1);
        if file_count > max_files {
            return Err(EditError::DigestLimitExceeded("file count".into()));
        }
        // Refuse on the recorded length before opening the file: a single
        // oversized blob must not be read at all, let alone buffered.
        let remaining = max_bytes.saturating_sub(total_bytes);
        if meta.len() > remaining {
            return Err(EditError::DigestLimitExceeded("byte count".into()));
        }
        let (content, hashed) = hash_file_capped(&path, remaining, &mut buf)?;
        total_bytes = total_bytes.saturating_add(hashed);
        tree.update(rel.as_bytes());
        tree.update(b"\0");
        tree.update(content.as_hex().as_bytes());
        tree.update(b"\n");
    }
    Ok(WorkspaceDigest {
        tree: tree.finish(),
        file_count,
        total_bytes,
    })
}

/// Hash `path` in chunks, returning its content digest and byte length.
///
/// `remaining` is re-checked while reading because the recorded metadata length
/// is only a hint: a file can grow between `symlink_metadata` and the read.
fn hash_file_capped(
    path: &Path,
    remaining: u64,
    buf: &mut [u8],
) -> Result<(Digest, u64), EditError> {
    let mut file = fs::File::open(path).map_err(|e| EditError::Io(e.to_string()))?;
    let mut content = DigestHasher::new();
    let mut hashed = 0_u64;
    loop {
        let read = file.read(buf).map_err(|e| EditError::Io(e.to_string()))?;
        if read == 0 {
            break;
        }
        hashed = hashed.saturating_add(read as u64);
        if hashed > remaining {
            return Err(EditError::DigestLimitExceeded("byte count".into()));
        }
        content.update(&buf[..read]);
    }
    Ok((content.finish(), hashed))
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

    /// The streamed tree hash must equal the documented encoding byte for byte.
    #[test]
    fn digest_encoding_matches_rfc_shape_across_chunk_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'z'; READ_CHUNK_BYTES * 2 + 7];
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("big.bin"), &big).unwrap();
        let tracked = ["a.txt", "big.bin"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let digest =
            compute_workspace_digest(&policy(dir.path()), &tracked, &[], 10, 1 << 30).unwrap();

        let expected = format!(
            "a.txt\0{}\nbig.bin\0{}\n",
            Digest::sha256(b"a").as_hex(),
            Digest::sha256(&big).as_hex()
        );
        assert_eq!(digest.tree, Digest::sha256(expected.as_bytes()));
        assert_eq!(digest.file_count, 2);
        assert_eq!(digest.total_bytes, 1 + big.len() as u64);
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
