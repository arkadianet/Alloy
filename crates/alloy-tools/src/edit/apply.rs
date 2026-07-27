//! Host-side TextPatch application (RFC-0008 §5.9).
//!
//! Author: arkadianet

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use alloy_runtime::{EditError, FilePatch, Hunk, PatchSet, PermissionToken, TransactionId};

use crate::authz::{GrantGlobError, GrantMatcher};
use crate::edit::patch_parse::{
    authorize_create_path, read_utf8_file, reject_symlink_components, rel_from_abs,
    validate_hunk_line_counts, validate_rel_path,
};
use crate::sandbox::{PathAccess, PathPolicy, SandboxError};

/// Paths produced while applying a PatchSet.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileApplyOutcome {
    pub files_touched: Vec<String>,
    pub created_paths: Vec<String>,
    pub temp_paths: Vec<String>,
    pub created_dirs: Vec<String>,
}

/// Progress event emitted before/after host mutations.
#[derive(Debug, Clone)]
pub(crate) enum ApplyProgress {
    /// A temp path was created or is about to be created.
    TempPath(String),
    /// A final path was created by this transaction.
    CreatedPath(String),
    /// A parent directory was created by this transaction.
    CreatedDir(String),
}

/// Error with partial path bookkeeping for restore.
#[derive(Debug)]
pub(crate) struct FileApplyError {
    pub error: EditError,
    pub partial: FileApplyOutcome,
}

/// Apply file patches in vector order.
///
/// The token's `FsWrite` globs are compiled once here and reused for every
/// patched path (plus its temp path and any created parent), rather than
/// recompiled per authorization.
pub(crate) fn apply_file_patches<F>(
    patch: &PatchSet,
    policy: &PathPolicy,
    perms: &PermissionToken,
    tx: TransactionId,
    mut progress: F,
) -> Result<FileApplyOutcome, Box<FileApplyError>>
where
    F: FnMut(ApplyProgress),
{
    let mut out = FileApplyOutcome::default();
    let writes = match GrantMatcher::fs_write(perms) {
        Ok(writes) => writes,
        Err(GrantGlobError::Invalid(_)) => {
            return Err(Box::new(FileApplyError {
                error: EditError::InvalidRequest("grant glob".into()),
                partial: out,
            }))
        }
    };
    for file in &patch.files {
        let result = match file {
            FilePatch::Delete {
                path,
                validation_hunks,
            } => apply_delete(path, validation_hunks, policy, &writes, &mut out),
            FilePatch::Modify { path, hunks } => {
                apply_modify(path, hunks, policy, &writes, tx, &mut out, &mut progress)
            }
            FilePatch::Create { path, hunks } => {
                apply_create(path, hunks, policy, &writes, tx, &mut out, &mut progress)
            }
        };
        if let Err(error) = result {
            return Err(Box::new(FileApplyError {
                error,
                partial: out,
            }));
        }
    }
    out.files_touched.sort();
    out.files_touched.dedup();
    out.created_paths.sort();
    out.created_paths.dedup();
    out.temp_paths.sort();
    out.temp_paths.dedup();
    Ok(out)
}

/// Apply hunks to UTF-8 file text and return new bytes.
pub(crate) fn apply_hunks_to_text(
    rel: &str,
    old_text: &str,
    hunks: &[Hunk],
) -> Result<Vec<u8>, EditError> {
    for hunk in hunks {
        validate_hunk_line_counts(hunk)?;
    }
    let (old_lines, old_eof_newline) = split_lines(old_text);
    let mut new_lines = Vec::new();
    let mut old_idx = 0_usize;
    let mut final_eof_newline = old_eof_newline;
    for hunk in hunks {
        let start = if hunk.old_lines == 0 {
            usize::try_from(hunk.old_start)
                .map_err(|_| EditError::InvalidPatch("hunk header".into()))?
        } else if hunk.old_start == 0 {
            return Err(EditError::InvalidPatch("hunk header".into()));
        } else {
            usize::try_from(hunk.old_start - 1)
                .map_err(|_| EditError::InvalidPatch("hunk header".into()))?
        };
        if start < old_idx || start > old_lines.len() {
            return Err(EditError::ContextMismatch {
                path: rel.to_string(),
                detail: "hunk range".into(),
            });
        }
        new_lines.extend_from_slice(&old_lines[old_idx..start]);
        old_idx = start;
        for raw in &hunk.lines {
            let Some((&prefix, content)) = raw.as_bytes().split_first() else {
                return Err(EditError::InvalidPatch("hunk line content".into()));
            };
            let content = std::str::from_utf8(content)
                .map_err(|_| EditError::InvalidPatch("hunk line content".into()))?;
            match prefix {
                b' ' => {
                    let Some(actual) = old_lines.get(old_idx) else {
                        return Err(EditError::ContextMismatch {
                            path: rel.to_string(),
                            detail: "context past eof".into(),
                        });
                    };
                    if actual != content {
                        return Err(EditError::ContextMismatch {
                            path: rel.to_string(),
                            detail: "context mismatch".into(),
                        });
                    }
                    new_lines.push(content.to_string());
                    old_idx += 1;
                }
                b'-' => {
                    let Some(actual) = old_lines.get(old_idx) else {
                        return Err(EditError::ContextMismatch {
                            path: rel.to_string(),
                            detail: "delete past eof".into(),
                        });
                    };
                    if actual != content {
                        return Err(EditError::ContextMismatch {
                            path: rel.to_string(),
                            detail: "delete mismatch".into(),
                        });
                    }
                    old_idx += 1;
                }
                b'+' => new_lines.push(content.to_string()),
                _ => return Err(EditError::InvalidPatch("hunk line content".into())),
            }
        }
        if hunk.old_eof_no_newline {
            if old_idx != old_lines.len() {
                return Err(EditError::InvalidPatch(
                    "old no-newline marker before eof".into(),
                ));
            }
            if old_eof_newline {
                return Err(EditError::ContextMismatch {
                    path: rel.to_string(),
                    detail: "old file has trailing newline".into(),
                });
            }
        }
        if old_idx == old_lines.len() {
            final_eof_newline = hunk.eof_newline;
        }
    }
    new_lines.extend_from_slice(&old_lines[old_idx..]);
    Ok(join_lines(&new_lines, final_eof_newline))
}

/// Prove a unified-diff `Delete`'s hunks consume every line of `old_text`.
///
/// `apply_hunks_to_text` returns no bytes whenever the new side is empty, so the
/// residual `eof_newline` flag on the deletion hunks cannot make a fully removed
/// file look non-empty.
pub(crate) fn verify_hunks_delete_whole_file(
    rel: &str,
    old_text: &str,
    hunks: &[Hunk],
) -> Result<(), EditError> {
    if apply_hunks_to_text(rel, old_text, hunks)?.is_empty() {
        Ok(())
    } else {
        Err(EditError::ContextMismatch {
            path: rel.to_string(),
            detail: "delete leaves content".into(),
        })
    }
}

fn apply_delete(
    rel: &str,
    validation_hunks: &[Hunk],
    policy: &PathPolicy,
    writes: &GrantMatcher,
    out: &mut FileApplyOutcome,
) -> Result<(), EditError> {
    authorize_patch_path_write(policy, writes, rel)?;
    let path = policy.jail().join(rel);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EditError::Conflict("delete missing file".into())
        } else {
            EditError::Io(e.to_string())
        }
    })?;
    ensure_regular(rel, &meta)?;
    // Only a unified-diff `Delete` claims the file's contents; a structured one
    // removes the path as-is, so it must work on non-UTF-8 files too.
    if !validation_hunks.is_empty() {
        let old = read_utf8_file(&path)?;
        verify_hunks_delete_whole_file(rel, &old, validation_hunks)?;
    }
    fs::remove_file(&path).map_err(|e| EditError::Io(e.to_string()))?;
    out.files_touched.push(rel.to_string());
    Ok(())
}

fn apply_modify<F>(
    rel: &str,
    hunks: &[Hunk],
    policy: &PathPolicy,
    writes: &GrantMatcher,
    tx: TransactionId,
    out: &mut FileApplyOutcome,
    progress: &mut F,
) -> Result<(), EditError>
where
    F: FnMut(ApplyProgress),
{
    authorize_patch_path_write(policy, writes, rel)?;
    let path = policy.jail().join(rel);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EditError::Conflict("modify missing file".into())
        } else {
            EditError::Io(e.to_string())
        }
    })?;
    ensure_regular(rel, &meta)?;
    let old = read_utf8_file(&path)?;
    let new_bytes = apply_hunks_to_text(rel, &old, hunks)?;
    let temp = temp_path_for(&path, tx)?;
    let temp_rel = rel_from_abs(policy.jail(), &temp)
        .ok_or_else(|| EditError::Internal("temp path outside jail".into()))?;
    authorize_policy_write(policy, &temp_rel)?;
    progress(ApplyProgress::TempPath(temp_rel.clone()));
    out.temp_paths.push(temp_rel);
    write_temp(&temp, &new_bytes)?;
    fs::set_permissions(&temp, meta.permissions()).map_err(|e| EditError::Io(e.to_string()))?;
    fs::rename(&temp, &path).map_err(|e| EditError::Io(e.to_string()))?;
    out.files_touched.push(rel.to_string());
    Ok(())
}

fn apply_create<F>(
    rel: &str,
    hunks: &[Hunk],
    policy: &PathPolicy,
    writes: &GrantMatcher,
    tx: TransactionId,
    out: &mut FileApplyOutcome,
    progress: &mut F,
) -> Result<(), EditError>
where
    F: FnMut(ApplyProgress),
{
    authorize_patch_create(policy, writes, rel)?;
    if policy.jail().join(rel).exists() {
        return Err(EditError::Conflict("create exists".into()));
    }
    create_parents(rel, policy, out, progress)?;
    authorize_policy_write(policy, rel)?;
    let path = policy.jail().join(rel);
    if path.exists() {
        return Err(EditError::Conflict("create exists".into()));
    }
    let new_bytes = apply_hunks_to_text(rel, "", hunks)?;
    let temp = temp_path_for(&path, tx)?;
    let temp_rel = rel_from_abs(policy.jail(), &temp)
        .ok_or_else(|| EditError::Internal("temp path outside jail".into()))?;
    authorize_policy_write(policy, &temp_rel)?;
    progress(ApplyProgress::TempPath(temp_rel.clone()));
    out.temp_paths.push(temp_rel);
    write_temp(&temp, &new_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&temp)
            .map_err(|e| EditError::Io(e.to_string()))?
            .permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&temp, perms).map_err(|e| EditError::Io(e.to_string()))?;
    }
    fs::rename(&temp, &path).map_err(|e| EditError::Io(e.to_string()))?;
    progress(ApplyProgress::CreatedPath(rel.to_string()));
    out.created_paths.push(rel.to_string());
    out.files_touched.push(rel.to_string());
    Ok(())
}

fn create_parents<F>(
    rel: &str,
    policy: &PathPolicy,
    out: &mut FileApplyOutcome,
    progress: &mut F,
) -> Result<(), EditError>
where
    F: FnMut(ApplyProgress),
{
    let mut cur_rel = String::new();
    let mut cur_abs = policy.jail().to_path_buf();
    let segments: Vec<&str> = rel.split('/').collect();
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        validate_rel_path(segment)?;
        if !cur_rel.is_empty() {
            cur_rel.push('/');
        }
        cur_rel.push_str(segment);
        cur_abs.push(segment);
        match fs::symlink_metadata(&cur_abs) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(EditError::PathDenied {
                    path: rel.to_string(),
                    reason: "symlink parent".into(),
                })
            }
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => {
                return Err(EditError::PathDenied {
                    path: rel.to_string(),
                    reason: "not a directory".into(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                authorize_policy_write(policy, &cur_rel)?;
                fs::create_dir(&cur_abs).map_err(|err| EditError::Io(err.to_string()))?;
                progress(ApplyProgress::CreatedDir(cur_rel.clone()));
                out.created_dirs.insert(0, cur_rel.clone());
            }
            Err(e) => return Err(EditError::Io(e.to_string())),
        }
    }
    Ok(())
}

fn authorize_patch_path_write(
    policy: &PathPolicy,
    writes: &GrantMatcher,
    rel: &str,
) -> Result<(), EditError> {
    validate_rel_path(rel)?;
    authorize_patch_grant(writes, rel)?;
    authorize_policy_write(policy, rel)
}

fn authorize_patch_create(
    policy: &PathPolicy,
    writes: &GrantMatcher,
    rel: &str,
) -> Result<(), EditError> {
    validate_rel_path(rel)?;
    authorize_patch_grant(writes, rel)?;
    authorize_create_path(policy, rel)
}

fn authorize_patch_grant(writes: &GrantMatcher, rel: &str) -> Result<(), EditError> {
    if !writes.has_grant() {
        return Err(EditError::MissingGrant("fs_write".into()));
    }
    if !writes.covers(rel) {
        return Err(EditError::PathNotCovered {
            path: rel.to_string(),
        });
    }
    Ok(())
}

fn authorize_policy_write(policy: &PathPolicy, rel: &str) -> Result<(), EditError> {
    validate_rel_path(rel)?;
    reject_symlink_components(policy, rel)?;
    policy
        .authorize(&policy.jail().join(rel), PathAccess::Write)
        .map(|_| ())
        .map_err(|err| map_path_error(err, rel))
}

fn ensure_regular(rel: &str, meta: &fs::Metadata) -> Result<(), EditError> {
    if meta.file_type().is_symlink() {
        return Err(EditError::PathDenied {
            path: rel.to_string(),
            reason: "symlink".into(),
        });
    }
    if !meta.is_file() {
        return Err(EditError::PathDenied {
            path: rel.to_string(),
            reason: "not a regular file".into(),
        });
    }
    Ok(())
}

fn temp_path_for(path: &Path, tx: TransactionId) -> Result<std::path::PathBuf, EditError> {
    let parent = path
        .parent()
        .ok_or_else(|| EditError::Internal("target without parent".into()))?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| EditError::PathDenied {
            path: "<target>".into(),
            reason: "invalid utf-8".into(),
        })?;
    Ok(parent.join(format!(".{name}.alloy-tmp-{tx}")))
}

fn write_temp(path: &Path, bytes: &[u8]) -> Result<(), EditError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| EditError::Io(e.to_string()))?;
    file.write_all(bytes)
        .map_err(|e| EditError::Io(e.to_string()))?;
    file.sync_all().map_err(|e| EditError::Io(e.to_string()))?;
    Ok(())
}

fn split_lines(text: &str) -> (Vec<String>, bool) {
    if text.is_empty() {
        return (Vec::new(), true);
    }
    let eof_newline = text.ends_with('\n');
    let body = if eof_newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    (
        body.split('\n').map(ToOwned::to_owned).collect(),
        eof_newline,
    )
}

fn join_lines(lines: &[String], eof_newline: bool) -> Vec<u8> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut text = lines.join("\n");
    if eof_newline {
        text.push('\n');
    }
    text.into_bytes()
}

fn map_path_error(err: SandboxError, rel: &str) -> EditError {
    match err {
        SandboxError::Denied(_) => EditError::PathDenied {
            path: rel.to_string(),
            reason: "path denied".into(),
        },
        SandboxError::TokenExpired => EditError::TokenExpired,
        other => EditError::Io(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxProfile;
    use alloy_runtime::{Glob, Grant, Hunk, ProfileId, RunId};

    fn token() -> PermissionToken {
        token_with_glob("**")
    }

    fn token_with_glob(glob: &str) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![Grant::FsWrite(Glob(glob.into()))],
            expires: None,
            run_id: RunId::new(),
        }
    }

    fn policy(dir: &Path) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(dir.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, Vec::new()).unwrap()
    }

    fn hunk(lines: Vec<&str>) -> Hunk {
        Hunk {
            old_start: 1,
            old_lines: lines
                .iter()
                .filter(|l| l.starts_with(' ') || l.starts_with('-'))
                .count() as u32,
            new_start: 1,
            new_lines: lines
                .iter()
                .filter(|l| l.starts_with(' ') || l.starts_with('+'))
                .count() as u32,
            lines: lines.into_iter().map(str::to_string).collect(),
            eof_newline: true,
            old_eof_no_newline: false,
        }
    }

    #[test]
    fn apply_hunks_modifies_context() {
        let bytes = apply_hunks_to_text(
            "a.txt",
            "one\ntwo\n",
            &[hunk(vec![" one", "-two", "+three"])],
        )
        .unwrap();
        assert_eq!(bytes, b"one\nthree\n");
    }

    #[test]
    fn context_mismatch_rejected() {
        let err = apply_hunks_to_text("a.txt", "one\n", &[hunk(vec![" two"])]).unwrap_err();
        assert!(matches!(err, EditError::ContextMismatch { .. }));
    }

    #[test]
    fn preserve_no_trailing_newline() {
        let mut h = hunk(vec!["-one", "+two"]);
        h.eof_newline = false;
        let bytes = apply_hunks_to_text("a.txt", "one", &[h]).unwrap();
        assert_eq!(bytes, b"two");
    }

    #[test]
    fn invalid_hunk_prefixes_are_panic_free() {
        for line in ["", "écontent"] {
            let malformed = Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 0,
                lines: vec![line.into()],
                eof_newline: true,
                old_eof_no_newline: false,
            };
            assert!(matches!(
                apply_hunks_to_text("a.txt", "one\n", &[malformed]),
                Err(EditError::InvalidPatch(ref message)) if message == "hunk line content"
            ));
        }
    }

    #[test]
    fn mid_file_hunk_preserves_original_eof() {
        let mut mid_file = hunk(vec!["-one", "+ONE"]);
        mid_file.eof_newline = false;
        let bytes = apply_hunks_to_text("a.txt", "one\ntwo\n", &[mid_file]).unwrap();
        assert_eq!(bytes, b"ONE\ntwo\n");
    }

    #[test]
    fn one_empty_line_is_not_an_empty_file() {
        assert_eq!(split_lines("\n"), (vec![String::new()], true));
        let bytes = apply_hunks_to_text("a.txt", "\n", &[hunk(vec![" ", "+after"])]).unwrap();
        assert_eq!(bytes, b"\nafter\n");
    }

    #[test]
    fn zero_length_range_inserts_after_old_start() {
        let insertion = Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 2,
            new_lines: 1,
            lines: vec!["+two".into()],
            eof_newline: true,
            old_eof_no_newline: false,
        };
        let bytes = apply_hunks_to_text("a.txt", "one\n", &[insertion]).unwrap();
        assert_eq!(bytes, b"one\ntwo\n");
    }

    #[test]
    fn create_records_parents_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Create {
                path: "a/b.txt".into(),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["+hi".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        };
        let mut progress = Vec::new();
        let out = apply_file_patches(
            &patch,
            &policy(dir.path()),
            &token(),
            TransactionId::new(),
            |p| progress.push(p),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
            "hi\n"
        );
        assert_eq!(out.created_paths, vec!["a/b.txt"]);
        assert_eq!(out.created_dirs, vec!["a"]);
        assert!(!progress.is_empty());
    }

    /// Structured deletes remove the path as-is, so a file the engine cannot
    /// decode as text is still deletable; a unified-diff delete instead has to
    /// prove its hunks match the bytes on disk before unlinking anything.
    #[test]
    fn delete_requires_content_proof_only_when_the_diff_claims_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("latin1.bin"), [b'c', b'a', b'f', 0xe9]).unwrap();
        let structured = PatchSet {
            files: vec![FilePatch::Delete {
                path: "latin1.bin".into(),
                validation_hunks: vec![],
            }],
        };
        apply_file_patches(
            &structured,
            &policy(dir.path()),
            &token(),
            TransactionId::new(),
            |_| {},
        )
        .unwrap();
        assert!(!dir.path().join("latin1.bin").exists());

        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let stale = PatchSet {
            files: vec![FilePatch::Delete {
                path: "a.txt".into(),
                validation_hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 0,
                    new_lines: 0,
                    lines: vec!["-one".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        };
        let err = apply_file_patches(
            &stale,
            &policy(dir.path()),
            &token(),
            TransactionId::new(),
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(
            err.error,
            EditError::ContextMismatch { ref detail, .. } if detail == "delete leaves content"
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\n",
            "a delete that cannot prove the file contents must not unlink it"
        );
    }

    /// A delete of a file with no trailing newline still reduces to zero bytes,
    /// so the hunks' residual `eof_newline` must not make it look non-empty.
    #[test]
    fn whole_file_delete_proof_ignores_eof_newline() {
        let hunk = Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 0,
            new_lines: 0,
            lines: vec!["-one".into()],
            eof_newline: true,
            old_eof_no_newline: true,
        };
        verify_hunks_delete_whole_file("a.txt", "one", std::slice::from_ref(&hunk)).unwrap();
        // The old-side no-newline marker is still an assertion about the file.
        assert!(matches!(
            verify_hunks_delete_whole_file("a.txt", "one\n", &[hunk]),
            Err(EditError::ContextMismatch { ref detail, .. })
                if detail == "old file has trailing newline"
        ));
    }

    #[test]
    fn create_only_requires_grant_for_final_patch_path() {
        let dir = tempfile::tempdir().unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Create {
                path: "a/b.txt".into(),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["+hi".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        };
        apply_file_patches(
            &patch,
            &policy(dir.path()),
            &token_with_glob("a/b.txt"),
            TransactionId::new(),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
            "hi\n"
        );
    }
}
