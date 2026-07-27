//! Host-side TextPatch application (RFC-0008 §5.9).
//!
//! Author: arkadianet

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use alloy_runtime::{EditError, FilePatch, Hunk, PatchSet, PermissionToken, TransactionId};

use crate::authz::{self, GrantGlobError};
use crate::edit::patch_parse::{rel_from_abs, validate_rel_path};
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
    for file in &patch.files {
        let result = match file {
            FilePatch::Delete { path } => apply_delete(path, policy, perms, &mut out),
            FilePatch::Modify { path, hunks } => {
                apply_modify(path, hunks, policy, perms, tx, &mut out, &mut progress)
            }
            FilePatch::Create { path, hunks } => {
                apply_create(path, hunks, policy, perms, tx, &mut out, &mut progress)
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
    let (old_lines, _old_eof_newline) = split_lines(old_text);
    let mut new_lines = Vec::new();
    let mut old_idx = 0_usize;
    let mut final_eof_newline = true;
    for hunk in hunks {
        let start = if hunk.old_start == 0 {
            0
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
            let (prefix, content) = raw.split_at(1);
            match prefix {
                " " => {
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
                "-" => {
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
                "+" => new_lines.push(content.to_string()),
                _ => return Err(EditError::InvalidPatch("hunk line content".into())),
            }
        }
        final_eof_newline = hunk.eof_newline;
    }
    new_lines.extend_from_slice(&old_lines[old_idx..]);
    Ok(join_lines(&new_lines, final_eof_newline))
}

fn apply_delete(
    rel: &str,
    policy: &PathPolicy,
    perms: &PermissionToken,
    out: &mut FileApplyOutcome,
) -> Result<(), EditError> {
    authorize_write(policy, perms, rel)?;
    let path = policy.jail().join(rel);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EditError::Conflict("delete missing file".into())
        } else {
            EditError::Io(e.to_string())
        }
    })?;
    ensure_regular(rel, &meta)?;
    fs::remove_file(&path).map_err(|e| EditError::Io(e.to_string()))?;
    out.files_touched.push(rel.to_string());
    Ok(())
}

fn apply_modify<F>(
    rel: &str,
    hunks: &[Hunk],
    policy: &PathPolicy,
    perms: &PermissionToken,
    tx: TransactionId,
    out: &mut FileApplyOutcome,
    progress: &mut F,
) -> Result<(), EditError>
where
    F: FnMut(ApplyProgress),
{
    authorize_write(policy, perms, rel)?;
    let path = policy.jail().join(rel);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EditError::Conflict("modify missing file".into())
        } else {
            EditError::Io(e.to_string())
        }
    })?;
    ensure_regular(rel, &meta)?;
    let old = fs::read_to_string(&path).map_err(|e| EditError::Io(e.to_string()))?;
    let new_bytes = apply_hunks_to_text(rel, &old, hunks)?;
    let temp = temp_path_for(&path, tx)?;
    let temp_rel = rel_from_abs(policy.jail(), &temp)
        .ok_or_else(|| EditError::Internal("temp path outside jail".into()))?;
    authorize_write(policy, perms, &temp_rel)?;
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
    perms: &PermissionToken,
    tx: TransactionId,
    out: &mut FileApplyOutcome,
    progress: &mut F,
) -> Result<(), EditError>
where
    F: FnMut(ApplyProgress),
{
    if policy.jail().join(rel).exists() {
        return Err(EditError::Conflict("create exists".into()));
    }
    create_parents(rel, policy, perms, out, progress)?;
    authorize_write(policy, perms, rel)?;
    let path = policy.jail().join(rel);
    if path.exists() {
        return Err(EditError::Conflict("create exists".into()));
    }
    let new_bytes = apply_hunks_to_text(rel, "", hunks)?;
    let temp = temp_path_for(&path, tx)?;
    let temp_rel = rel_from_abs(policy.jail(), &temp)
        .ok_or_else(|| EditError::Internal("temp path outside jail".into()))?;
    authorize_write(policy, perms, &temp_rel)?;
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
    perms: &PermissionToken,
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
                authorize_write(policy, perms, &cur_rel)?;
                fs::create_dir(&cur_abs).map_err(|err| EditError::Io(err.to_string()))?;
                progress(ApplyProgress::CreatedDir(cur_rel.clone()));
                out.created_dirs.insert(0, cur_rel.clone());
            }
            Err(e) => return Err(EditError::Io(e.to_string())),
        }
    }
    Ok(())
}

fn authorize_write(
    policy: &PathPolicy,
    perms: &PermissionToken,
    rel: &str,
) -> Result<(), EditError> {
    validate_rel_path(rel)?;
    if !authz::has_fs_write_grant(perms) {
        return Err(EditError::MissingGrant("fs_write".into()));
    }
    match authz::fs_write_covers(perms, rel) {
        Ok(true) => {}
        Ok(false) => {
            return Err(EditError::PathNotCovered {
                path: rel.to_string(),
            })
        }
        Err(GrantGlobError::Invalid(_)) => {
            return Err(EditError::InvalidRequest("grant glob".into()))
        }
    }
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
    if body.is_empty() {
        (Vec::new(), eof_newline)
    } else {
        (
            body.split('\n').map(ToOwned::to_owned).collect(),
            eof_newline,
        )
    }
}

fn join_lines(lines: &[String], eof_newline: bool) -> Vec<u8> {
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
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![Grant::FsWrite(Glob("**".into()))],
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
}
