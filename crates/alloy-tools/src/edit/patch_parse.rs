//! TextPatch wire decoding and local PatchSet validation.
//!
//! Author: arkadianet

use std::collections::HashSet;
use std::path::{Component, Path};

use alloy_runtime::{
    token_expired, EditError, EditRequest, FilePatch, Grant, Hunk, PatchSet, PermissionToken,
};
use serde_json::Value;

use crate::authz::{self, GrantGlobError};
use crate::mcp::MAX_ARG_STRING_BYTES;
use crate::sandbox::{PathAccess, PathPolicy, SandboxError};

/// Backend-side patch payload ceiling.
pub(crate) const MAX_PATCH_BYTES: usize = 64 * 1024;
const MAX_FILES: usize = 256;
const MAX_HUNKS_PER_FILE: usize = 1024;
const MAX_LINES_PER_HUNK: usize = 10_000;

/// Decode the MCP `patch` value into the shared edit request envelope.
pub(crate) fn decode_patch_value(value: &Value) -> Result<EditRequest, EditError> {
    let payload_len = serde_json::to_vec(value)
        .map_err(|e| EditError::InvalidPatch(format!("patch json: {e}")))?
        .len();
    if payload_len > MAX_PATCH_BYTES {
        return Err(EditError::InvalidPatch("patch too large".into()));
    }
    match value {
        Value::String(s) => {
            if s.len() > MAX_PATCH_BYTES {
                return Err(EditError::InvalidPatch("patch too large".into()));
            }
            Ok(EditRequest::TextPatch {
                patch: parse_unified_diff(s)?,
            })
        }
        Value::Object(obj) => {
            if obj.contains_key("files") && obj.contains_key("kind") {
                return Err(EditError::InvalidPatch("ambiguous patch json".into()));
            }
            if obj.contains_key("files") {
                let patch: PatchSet = serde_json::from_value(value.clone())
                    .map_err(|e| EditError::InvalidPatch(e.to_string()))?;
                return Ok(EditRequest::TextPatch { patch });
            }
            match obj.get("kind").and_then(Value::as_str) {
                Some("text_patch") | Some("semantic_ops") => serde_json::from_value(value.clone())
                    .map_err(|e| EditError::InvalidPatch(e.to_string())),
                _ => Err(EditError::InvalidPatch("unrecognized patch json".into())),
            }
        }
        _ => Err(EditError::InvalidPatch("unrecognized patch json".into())),
    }
}

/// Parse a UTF-8 unified diff into a structured PatchSet.
pub(crate) fn parse_unified_diff(text: &str) -> Result<PatchSet, EditError> {
    if text.len() > MAX_PATCH_BYTES {
        return Err(EditError::InvalidPatch("patch too large".into()));
    }
    if text.contains('\0') {
        return Err(EditError::InvalidPatch("hunk line content".into()));
    }
    let raw_lines = split_patch_lines(text);
    let mut files = Vec::new();
    let mut i = 0;
    while i < raw_lines.len() {
        let line = raw_lines[i];
        if line.is_empty() || is_git_extended_header(line) {
            i += 1;
            continue;
        }
        if line.starts_with("rename ") || line.starts_with("copy ") {
            return Err(EditError::InvalidPatch("rename/copy unsupported".into()));
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            return Err(EditError::InvalidPatch("binary patch unsupported".into()));
        }
        if !line.starts_with("--- ") {
            return Err(EditError::InvalidPatch("unified diff header".into()));
        }
        let old_raw = header_path(line, "--- ")?;
        i += 1;
        if i >= raw_lines.len() {
            return Err(EditError::InvalidPatch("unified diff header".into()));
        }
        let new_line = raw_lines[i];
        if !new_line.starts_with("+++ ") {
            return Err(EditError::InvalidPatch("unified diff header".into()));
        }
        let new_raw = header_path(new_line, "+++ ")?;
        i += 1;

        let is_create = old_raw == "/dev/null";
        let is_delete = new_raw == "/dev/null";
        if is_create && is_delete {
            return Err(EditError::InvalidPatch("unrecognized patch json".into()));
        }
        let path = if is_create {
            normalize_diff_path(new_raw)?
        } else {
            normalize_diff_path(old_raw)?
        };
        if !is_create && !is_delete {
            let new_path = normalize_diff_path(new_raw)?;
            if new_path != path {
                return Err(EditError::InvalidPatch("rename/copy unsupported".into()));
            }
        }

        let mut hunks = Vec::new();
        while i < raw_lines.len() {
            let hline = raw_lines[i];
            if hline.starts_with("diff --git ") || hline.starts_with("--- ") {
                break;
            }
            if hline.starts_with("rename ") || hline.starts_with("copy ") {
                return Err(EditError::InvalidPatch("rename/copy unsupported".into()));
            }
            if hline.starts_with("Binary files ") || hline.starts_with("GIT binary patch") {
                return Err(EditError::InvalidPatch("binary patch unsupported".into()));
            }
            if hline.is_empty() {
                i += 1;
                continue;
            }
            if is_git_extended_header(hline) {
                i += 1;
                continue;
            }
            if !hline.starts_with("@@ ") {
                return Err(EditError::InvalidPatch("hunk header".into()));
            }
            let (old_start, old_lines, new_start, new_lines) = parse_hunk_header(hline)?;
            i += 1;
            let mut lines = Vec::new();
            let mut eof_newline = true;
            let mut old_eof_no_newline = false;
            let mut last_side = None;
            let mut old_marker_seen = false;
            let mut new_marker_seen = false;
            let mut old_count = 0_u32;
            let mut new_count = 0_u32;
            while i < raw_lines.len() {
                let l = raw_lines[i];
                if l == r"\ No newline at end of file" {
                    match last_side.take() {
                        Some('+') => {
                            eof_newline = false;
                            new_marker_seen = true;
                        }
                        Some(' ') => {
                            old_eof_no_newline = true;
                            old_marker_seen = true;
                            eof_newline = false;
                            new_marker_seen = true;
                        }
                        Some('-') => {
                            old_eof_no_newline = true;
                            old_marker_seen = true;
                        }
                        _ => {
                            return Err(EditError::InvalidPatch(
                                "no-newline marker without prior hunk line".into(),
                            ))
                        }
                    }
                    i += 1;
                    continue;
                }
                if old_count == old_lines && new_count == new_lines {
                    break;
                }
                if l.starts_with("@@ ") || l.starts_with("diff --git ") {
                    break;
                }
                let Some(&prefix) = l.as_bytes().first() else {
                    return Err(EditError::InvalidPatch("hunk line content".into()));
                };
                if !matches!(prefix, b' ' | b'-' | b'+') {
                    return Err(EditError::InvalidPatch("hunk line content".into()));
                }
                if old_marker_seen && matches!(prefix, b' ' | b'-') {
                    return Err(EditError::InvalidPatch(
                        "old no-newline marker before final old line".into(),
                    ));
                }
                if new_marker_seen && matches!(prefix, b' ' | b'+') {
                    return Err(EditError::InvalidPatch(
                        "new no-newline marker before final new line".into(),
                    ));
                }
                match prefix {
                    b' ' => {
                        old_count = old_count.saturating_add(1);
                        new_count = new_count.saturating_add(1);
                    }
                    b'-' => old_count = old_count.saturating_add(1),
                    b'+' => new_count = new_count.saturating_add(1),
                    _ => unreachable!("prefix validated"),
                }
                last_side = Some(char::from(prefix));
                lines.push(l.to_string());
                i += 1;
            }
            hunks.push(Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines,
                eof_newline,
                old_eof_no_newline,
            });
        }
        validate_hunk_shapes_for_action(&path, is_create, is_delete, &hunks)?;
        if is_create {
            files.push(FilePatch::Create { path, hunks });
        } else if is_delete {
            files.push(FilePatch::Delete { path });
        } else {
            files.push(FilePatch::Modify { path, hunks });
        }
    }
    Ok(PatchSet { files })
}

/// Validate dry-run/apply local rules that do not require git.
pub(crate) fn validate_patchset_local(
    patch: &PatchSet,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<Vec<String>, EditError> {
    if patch.files.is_empty() {
        return Err(EditError::EmptyPatch);
    }
    if patch.files.len() > MAX_FILES {
        return Err(EditError::InvalidPatch("too many files".into()));
    }
    let mut exact = HashSet::new();
    let mut folded = HashSet::new();
    let mut paths = Vec::with_capacity(patch.files.len());
    for file in &patch.files {
        let path = file.path();
        validate_rel_path(path)?;
        if is_digest_excluded_path(path) {
            return Err(EditError::InvalidPatch("path excluded from digest".into()));
        }
        if !exact.insert(path.to_string()) {
            return Err(EditError::InvalidPatch("duplicate path".into()));
        }
        if !folded.insert(case_fold_path(path)) {
            return Err(EditError::InvalidPatch("duplicate path".into()));
        }
        authorize_patch_path(policy, perms, file)?;
        match file {
            FilePatch::Modify { path, hunks } => {
                validate_modify_hunks(path, hunks)?;
                validate_existing_file_shape(policy, path, true)?;
                let old = read_utf8_file(&policy.jail().join(path))?;
                crate::edit::apply::apply_hunks_to_text(path, &old, hunks)?;
            }
            FilePatch::Create { path, hunks } => {
                validate_create_hunks(hunks)?;
                if policy.jail().join(path).exists() {
                    return Err(EditError::Conflict("create exists".into()));
                }
            }
            FilePatch::Delete { path } => {
                validate_existing_file_shape(policy, path, false)?;
                read_utf8_file(&policy.jail().join(path))?;
            }
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Reject semantic operation envelopes according to MVP fail-closed rules.
pub(crate) fn reject_semantic(req: &EditRequest) -> Result<(), EditError> {
    if let EditRequest::SemanticOps { ops } = req {
        if ops.is_empty() {
            return Err(EditError::InvalidRequest("semantic_ops empty".into()));
        }
        return Err(EditError::UnsupportedOp {
            op: ops[0].op_tag().to_string(),
        });
    }
    Ok(())
}

/// Check token expiry with RFC-0008 error variant.
pub(crate) fn check_expiry(perms: &PermissionToken) -> Result<(), EditError> {
    if token_expired(perms.expires.as_ref()) {
        return Err(EditError::TokenExpired);
    }
    Ok(())
}

/// Check explicit run attribution.
pub(crate) fn check_run(
    ctx_run: Option<alloy_runtime::RunId>,
    perms: &PermissionToken,
) -> Result<(), EditError> {
    if let Some(run) = ctx_run {
        if run != perms.run_id {
            return Err(EditError::InvalidRequest("run_id mismatch".into()));
        }
    }
    Ok(())
}

/// Validate a jail-relative path lexically.
pub(crate) fn validate_rel_path(path: &str) -> Result<(), EditError> {
    if path.is_empty()
        || path.len() > MAX_ARG_STRING_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || has_drive_prefix(path)
    {
        return Err(EditError::PathDenied {
            path: path.to_string(),
            reason: "invalid path".into(),
        });
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(EditError::PathDenied {
                path: path.to_string(),
                reason: "invalid path".into(),
            });
        }
        if segment == ".git" {
            return Err(EditError::PathDenied {
                path: path.to_string(),
                reason: "git metadata path".into(),
            });
        }
        if segment == ".alloy-sbx" {
            return Err(EditError::PathDenied {
                path: path.to_string(),
                reason: "sandbox scratch path".into(),
            });
        }
    }
    Ok(())
}

/// True when a path is excluded from WorkspaceDigest.
#[must_use]
pub(crate) fn is_digest_excluded_path(path: &str) -> bool {
    path == "target"
        || path.starts_with("target/")
        || path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with('.') && name.contains(".alloy-tmp-"))
}

fn authorize_patch_path(
    policy: &PathPolicy,
    perms: &PermissionToken,
    file: &FilePatch,
) -> Result<(), EditError> {
    let rel = file.path();
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
    match file {
        FilePatch::Create { path, .. } => authorize_create_path(policy, path),
        _ => {
            reject_symlink_components(policy, rel)?;
            policy
                .authorize(&policy.jail().join(rel), PathAccess::Write)
                .map(|_| ())
                .map_err(|e| path_policy_error(e, rel))
        }
    }
}

pub(crate) fn authorize_create_path(policy: &PathPolicy, rel: &str) -> Result<(), EditError> {
    if policy.deny_matches_rel(rel) {
        return Err(EditError::PathDenied {
            path: rel.to_string(),
            reason: "path denied".into(),
        });
    }
    let mut cur = policy.jail().to_path_buf();
    let mut missing_seen = false;
    let segments: Vec<&str> = rel.split('/').collect();
    for (idx, segment) in segments.iter().enumerate() {
        validate_rel_path(segment)?;
        let next = cur.join(segment);
        let is_final = idx + 1 == segments.len();
        if !missing_seen {
            match std::fs::symlink_metadata(&next) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(EditError::PathDenied {
                        path: rel.to_string(),
                        reason: "symlink parent".into(),
                    })
                }
                Ok(meta) if meta.is_dir() => {
                    cur = next;
                    continue;
                }
                Ok(_) if !is_final => {
                    return Err(EditError::PathDenied {
                        path: rel.to_string(),
                        reason: "not a directory".into(),
                    })
                }
                Ok(_) => {
                    policy
                        .authorize(&next, PathAccess::Write)
                        .map(|_| ())
                        .map_err(|e| path_policy_error(e, rel))?;
                    cur = next;
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    policy
                        .authorize(&cur, PathAccess::Write)
                        .map(|_| ())
                        .map_err(|e| path_policy_error(e, rel))?;
                    missing_seen = true;
                    // Single missing leaf under an existing parent: PathPolicy can
                    // authorize the prospective path (missing-final-component rule).
                    if is_final {
                        policy
                            .authorize(&next, PathAccess::Write)
                            .map(|_| ())
                            .map_err(|e| path_policy_error(e, rel))?;
                    }
                }
                Err(e) => return Err(EditError::Io(e.to_string())),
            }
        }
        cur = next;
    }
    Ok(())
}

pub(crate) fn reject_symlink_components(policy: &PathPolicy, rel: &str) -> Result<(), EditError> {
    let mut current = policy.jail().to_path_buf();
    let segments: Vec<&str> = rel.split('/').collect();
    for (idx, segment) in segments.iter().enumerate() {
        current.push(segment);
        let is_final = idx + 1 == segments.len();
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(EditError::PathDenied {
                    path: rel.to_string(),
                    reason: "symlink component".into(),
                })
            }
            Ok(meta) if !is_final && !meta.is_dir() => {
                return Err(EditError::PathDenied {
                    path: rel.to_string(),
                    reason: "not a directory".into(),
                })
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(EditError::Io(e.to_string())),
        }
    }
    Ok(())
}

fn validate_existing_file_shape(
    policy: &PathPolicy,
    rel: &str,
    modify: bool,
) -> Result<(), EditError> {
    reject_symlink_components(policy, rel)?;
    let path = policy.jail().join(rel);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if modify {
                Err(EditError::Conflict("modify missing file".into()))
            } else {
                Err(EditError::Conflict("delete missing file".into()))
            }
        }
        Err(e) => return Err(EditError::Io(e.to_string())),
    };
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

fn validate_modify_hunks(path: &str, hunks: &[Hunk]) -> Result<(), EditError> {
    if hunks.is_empty() {
        return Err(EditError::InvalidPatch("empty hunks".into()));
    }
    validate_common_hunks(path, hunks)?;
    for hunk in hunks {
        if hunk.old_start == 0 && hunk.old_lines != 0 {
            return Err(EditError::InvalidPatch("modify old_start".into()));
        }
    }
    Ok(())
}

fn validate_create_hunks(hunks: &[Hunk]) -> Result<(), EditError> {
    if hunks.len() != 1 {
        return Err(EditError::InvalidPatch("create hunk shape".into()));
    }
    let hunk = &hunks[0];
    if hunk.old_start != 0 || hunk.old_lines != 0 {
        return Err(EditError::InvalidPatch("create hunk shape".into()));
    }
    if hunk
        .lines
        .iter()
        .any(|line| !line.starts_with('+') || line.contains('\0') || line.contains('\n'))
    {
        return Err(EditError::InvalidPatch("create hunk shape".into()));
    }
    validate_hunk_line_counts(hunk)?;
    Ok(())
}

fn validate_common_hunks(path: &str, hunks: &[Hunk]) -> Result<(), EditError> {
    if hunks.len() > MAX_HUNKS_PER_FILE {
        return Err(EditError::InvalidPatch("too many hunks".into()));
    }
    let mut prev_old_start: Option<u64> = None;
    let mut prev_old_end: Option<u64> = None;
    let mut prev_was_insertion = false;
    let mut delta: i64 = 0;
    for hunk in hunks {
        if hunk.lines.len() > MAX_LINES_PER_HUNK {
            return Err(EditError::InvalidPatch("hunk too large".into()));
        }
        let old_start = if hunk.old_lines == 0 {
            u64::from(hunk.old_start)
        } else {
            u64::from(hunk.old_start.saturating_sub(1))
        };
        if let Some(prev) = prev_old_start {
            if old_start < prev {
                return Err(EditError::InvalidPatch("hunk order".into()));
            }
            if old_start == prev && hunk.old_lines == 0 && prev_was_insertion {
                return Err(EditError::OverlappingHunks {
                    path: path.to_string(),
                });
            }
        }
        let old_end = old_start.saturating_add(u64::from(hunk.old_lines));
        if let Some(prev_end) = prev_old_end {
            if old_start < prev_end {
                return Err(EditError::OverlappingHunks {
                    path: path.to_string(),
                });
            }
        }
        validate_hunk_line_counts(hunk)?;
        let insertion_offset = if hunk.old_lines == 0 { 1 } else { 0 };
        let expected_new = i64::from(hunk.old_start) + delta + insertion_offset;
        if i64::from(hunk.new_start) != expected_new {
            return Err(EditError::InvalidPatch("hunk new_start".into()));
        }
        delta += i64::from(hunk.new_lines) - i64::from(hunk.old_lines);
        prev_old_start = Some(old_start);
        prev_old_end = Some(old_end);
        prev_was_insertion = hunk.old_lines == 0;
    }
    Ok(())
}

pub(crate) fn validate_hunk_line_counts(hunk: &Hunk) -> Result<(), EditError> {
    let mut old_count = 0_u32;
    let mut new_count = 0_u32;
    for line in &hunk.lines {
        if line.contains('\0') || line.contains('\n') {
            return Err(EditError::InvalidPatch("hunk line content".into()));
        }
        let Some(prefix) = line.chars().next() else {
            return Err(EditError::InvalidPatch("hunk line content".into()));
        };
        match prefix {
            ' ' => {
                old_count = old_count.saturating_add(1);
                new_count = new_count.saturating_add(1);
            }
            '-' => old_count = old_count.saturating_add(1),
            '+' => new_count = new_count.saturating_add(1),
            _ => return Err(EditError::InvalidPatch("hunk line content".into())),
        }
    }
    if old_count != hunk.old_lines || new_count != hunk.new_lines {
        return Err(EditError::InvalidPatch("hunk line count".into()));
    }
    Ok(())
}

fn validate_hunk_shapes_for_action(
    path: &str,
    is_create: bool,
    is_delete: bool,
    hunks: &[Hunk],
) -> Result<(), EditError> {
    if is_create {
        validate_create_hunks(hunks)
    } else if is_delete {
        validate_delete_hunks(hunks)
    } else {
        validate_modify_hunks(path, hunks)
    }
}

fn validate_delete_hunks(hunks: &[Hunk]) -> Result<(), EditError> {
    if hunks.len() > MAX_HUNKS_PER_FILE {
        return Err(EditError::InvalidPatch("too many hunks".into()));
    }
    let mut next_old_start = 1_u64;
    for hunk in hunks {
        if hunk.lines.len() > MAX_LINES_PER_HUNK {
            return Err(EditError::InvalidPatch("hunk too large".into()));
        }
        validate_hunk_line_counts(hunk)?;
        if u64::from(hunk.old_start) != next_old_start
            || hunk.old_lines == 0
            || hunk.new_start != 0
            || hunk.new_lines != 0
            || hunk
                .lines
                .iter()
                .any(|line| line.as_bytes().first() != Some(&b'-'))
        {
            return Err(EditError::InvalidPatch(
                "delete must remove entire file".into(),
            ));
        }
        next_old_start = next_old_start.saturating_add(u64::from(hunk.old_lines));
    }
    Ok(())
}

fn parse_hunk_header(line: &str) -> Result<(u32, u32, u32, u32), EditError> {
    let rest = line
        .strip_prefix("@@ ")
        .ok_or_else(|| EditError::InvalidPatch("hunk header".into()))?;
    let end = rest
        .find(" @@")
        .ok_or_else(|| EditError::InvalidPatch("hunk header".into()))?;
    let mut parts = rest[..end].split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| EditError::InvalidPatch("hunk header".into()))?;
    let new = parts
        .next()
        .ok_or_else(|| EditError::InvalidPatch("hunk header".into()))?;
    if parts.next().is_some() {
        return Err(EditError::InvalidPatch("hunk header".into()));
    }
    let (old_start, old_lines) = parse_range(old, '-')?;
    let (new_start, new_lines) = parse_range(new, '+')?;
    Ok((old_start, old_lines, new_start, new_lines))
}

fn parse_range(s: &str, sigil: char) -> Result<(u32, u32), EditError> {
    let body = s
        .strip_prefix(sigil)
        .ok_or_else(|| EditError::InvalidPatch("hunk header".into()))?;
    let (start, count) = match body.split_once(',') {
        Some((start, count)) => (start, count),
        None => (body, "1"),
    };
    let start = start
        .parse::<u32>()
        .map_err(|_| EditError::InvalidPatch("hunk header".into()))?;
    let count = count
        .parse::<u32>()
        .map_err(|_| EditError::InvalidPatch("hunk header".into()))?;
    Ok((start, count))
}

fn header_path<'a>(line: &'a str, prefix: &str) -> Result<&'a str, EditError> {
    let rest = line
        .strip_prefix(prefix)
        .ok_or_else(|| EditError::InvalidPatch("unified diff header".into()))?;
    Ok(rest.split('\t').next().unwrap_or(rest).trim_end())
}

fn normalize_diff_path(raw: &str) -> Result<String, EditError> {
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    validate_rel_path(stripped)?;
    Ok(stripped.to_string())
}

fn split_patch_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n')
        .map(|line| {
            let Some(without_lf) = line.strip_suffix('\n') else {
                return line;
            };
            without_lf.strip_suffix('\r').unwrap_or(without_lf)
        })
        .collect()
}

fn is_git_extended_header(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
}

pub(crate) fn read_utf8_file(path: &Path) -> Result<String, EditError> {
    let bytes = std::fs::read(path).map_err(|e| EditError::Io(e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| EditError::InvalidPatch("file not utf-8".into()))
}

fn path_policy_error(err: SandboxError, rel: &str) -> EditError {
    match err {
        SandboxError::Denied(_) => EditError::PathDenied {
            path: rel.to_string(),
            reason: "path denied".into(),
        },
        SandboxError::TokenExpired => EditError::TokenExpired,
        other => EditError::Io(other.to_string()),
    }
}

fn has_drive_prefix(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn case_fold_path(path: &str) -> String {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        path.to_ascii_lowercase()
    } else {
        path.to_string()
    }
}

/// Relative path from an absolute path under the jail, if UTF-8.
pub(crate) fn rel_from_abs(jail: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(jail).ok()?;
    let mut out = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(s) => out.push(s.to_str()?.to_string()),
            _ => return None,
        }
    }
    Some(out.join("/"))
}

/// Whether mutating apply has GitWrite.
pub(crate) fn require_git_write(perms: &PermissionToken) -> Result<(), EditError> {
    if perms.grants.iter().any(|g| matches!(g, Grant::GitWrite)) {
        Ok(())
    } else {
        Err(EditError::MissingGrant("git_write".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxProfile;
    use alloy_runtime::{Glob, ProfileId, RunId};

    fn token(grants: Vec<Grant>) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants,
            expires: None,
            run_id: RunId::new(),
        }
    }

    fn policy(dir: &Path) -> PathPolicy {
        let profile = SandboxProfile::default_for_jail(dir.to_path_buf()).unwrap();
        PathPolicy::from_profile(&profile, Vec::new()).unwrap()
    }

    #[test]
    fn decode_wire_shapes() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        assert!(matches!(
            decode_patch_value(&Value::String(diff.into())).unwrap(),
            EditRequest::TextPatch { .. }
        ));
        let obj = serde_json::json!({"files":[{"action":"delete","path":"a.txt"}]});
        assert!(matches!(
            decode_patch_value(&obj).unwrap(),
            EditRequest::TextPatch { .. }
        ));
        let amb = serde_json::json!({"kind":"text_patch","files":[]});
        assert!(matches!(
            decode_patch_value(&amb),
            Err(EditError::InvalidPatch(ref m)) if m == "ambiguous patch json"
        ));
        assert!(matches!(
            decode_patch_value(&serde_json::json!(7)),
            Err(EditError::InvalidPatch(ref m)) if m == "unrecognized patch json"
        ));
    }

    #[test]
    fn path_escape_and_digest_excluded_rejected() {
        for bad in [
            "/x",
            "../x",
            "a/../b",
            "a\\b",
            ".git/hooks/x",
            ".alloy-sbx/x",
        ] {
            assert!(matches!(
                validate_rel_path(bad),
                Err(EditError::PathDenied { .. })
            ));
        }
        assert!(is_digest_excluded_path("target/debug/x"));
        assert!(is_digest_excluded_path("src/.lib.alloy-tmp-abc"));
    }

    #[test]
    fn patch_caps_and_hunk_shape() {
        let patch = PatchSet { files: vec![] };
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            ),
            Err(EditError::EmptyPatch)
        ));
        let h = Hunk {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
            lines: vec![" x".into()],
            eof_newline: true,
            old_eof_no_newline: false,
        };
        assert!(matches!(
            validate_create_hunks(&[h]),
            Err(EditError::InvalidPatch(ref m)) if m == "create hunk shape"
        ));
    }

    #[test]
    fn duplicate_and_grant_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let patch = PatchSet {
            files: vec![
                FilePatch::Delete {
                    path: "a.txt".into(),
                },
                FilePatch::Delete {
                    path: "a.txt".into(),
                },
            ],
        };
        assert!(matches!(
            validate_patchset_local(&patch, &policy(dir.path()), &token(vec![Grant::FsWrite(Glob("**".into()))])),
            Err(EditError::InvalidPatch(ref m)) if m == "duplicate path"
        ));
        let patch = PatchSet {
            files: vec![FilePatch::Delete {
                path: "a.txt".into(),
            }],
        };
        assert!(matches!(
            validate_patchset_local(&patch, &policy(dir.path()), &token(vec![])),
            Err(EditError::MissingGrant(ref g)) if g == "fs_write"
        ));
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("src/**".into()))])
            ),
            Err(EditError::PathNotCovered { ref path }) if path == "a.txt"
        ));
    }

    #[test]
    fn eof_old_side_marker_context_mismatch() {
        let diff = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-one
\\ No newline at end of file
+two
";
        let patch = parse_unified_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &patch.files[0] else {
            panic!("expected modify");
        };
        // File currently HAS trailing newline → old-side marker must fail.
        let err = crate::edit::apply::apply_hunks_to_text("a.txt", "one\n", hunks).unwrap_err();
        assert!(matches!(err, EditError::ContextMismatch { .. }));

        // File truly lacks trailing newline → applies.
        let bytes = crate::edit::apply::apply_hunks_to_text("a.txt", "one", hunks).unwrap();
        assert_eq!(bytes, b"two\n");
    }

    #[test]
    fn dotenv_create_denied_on_validate() {
        let dir = tempfile::tempdir().unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Create {
                path: ".env".into(),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["+SECRET=1".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        };
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            ),
            Err(EditError::PathDenied { ref path, .. }) if path == ".env"
        ));
    }

    #[test]
    fn overlapping_hunks_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Modify {
                path: "a.txt".into(),
                hunks: vec![
                    Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 1,
                        lines: vec!["-a".into(), "+A".into()],
                        eof_newline: true,
                        old_eof_no_newline: false,
                    },
                    Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 1,
                        lines: vec!["-a".into(), "+Z".into()],
                        eof_newline: true,
                        old_eof_no_newline: false,
                    },
                ],
            }],
        };
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            ),
            Err(EditError::OverlappingHunks { .. })
        ));
    }

    #[test]
    fn git_style_create_and_delete_headers_validate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "old\n").unwrap();
        let diff = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+new
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 2222222..0000000
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-old
";
        let patch = parse_unified_diff(diff).unwrap();
        assert!(matches!(
            &patch.files[0],
            FilePatch::Create { path, .. } if path == "new.txt"
        ));
        assert!(matches!(
            &patch.files[1],
            FilePatch::Delete { path } if path == "old.txt"
        ));
        assert_eq!(
            serde_json::to_value(&patch).unwrap()["files"][1],
            serde_json::json!({"action": "delete", "path": "old.txt"})
        );
        assert_eq!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            )
            .unwrap(),
            vec!["new.txt", "old.txt"]
        );
    }

    #[test]
    fn extended_headers_skip_but_rename_copy_and_binary_reject() {
        let mode_change = "\
diff --git a/a.txt b/a.txt
old mode 100644
new mode 100755
dissimilarity index 100%
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
";
        assert!(parse_unified_diff(mode_change).is_ok());

        for (diff, message) in [
            (
                "diff --git a/a b/b\nsimilarity index 100%\nrename from a\nrename to b\n",
                "rename/copy unsupported",
            ),
            (
                "diff --git a/a b/b\ncopy from a\ncopy to b\n",
                "rename/copy unsupported",
            ),
            (
                "diff --git a/a b/a\nBinary files a/a and b/a differ\n",
                "binary patch unsupported",
            ),
        ] {
            assert!(matches!(
                parse_unified_diff(diff),
                Err(EditError::InvalidPatch(ref got)) if got == message
            ));
        }
    }

    #[test]
    fn delete_hunks_enforce_shape_and_caps_before_discard() {
        let context_delete = "\
--- a/a.txt
+++ /dev/null
@@ -1 +0,0 @@
 line
";
        assert!(matches!(
            parse_unified_diff(context_delete),
            Err(EditError::InvalidPatch(_))
        ));

        let empty = Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 0,
            new_lines: 0,
            lines: vec![],
            eof_newline: true,
            old_eof_no_newline: false,
        };
        assert!(matches!(
            validate_delete_hunks(&vec![empty.clone(); MAX_HUNKS_PER_FILE + 1]),
            Err(EditError::InvalidPatch(ref message)) if message == "too many hunks"
        ));
        let mut too_large = empty;
        too_large.lines = vec!["-x".into(); MAX_LINES_PER_HUNK + 1];
        assert!(matches!(
            validate_delete_hunks(&[too_large]),
            Err(EditError::InvalidPatch(ref message)) if message == "hunk too large"
        ));
    }

    #[test]
    fn parser_preserves_content_carriage_returns() {
        let final_content_cr = "--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+value\r";
        let patch = parse_unified_diff(final_content_cr).unwrap();
        let FilePatch::Create { hunks, .. } = &patch.files[0] else {
            panic!("expected create");
        };
        assert_eq!(hunks[0].lines, vec!["+value\r"]);

        let crlf = "--- /dev/null\r\n+++ b/b.txt\r\n@@ -0,0 +1 @@\r\n+value\r\n";
        let patch = parse_unified_diff(crlf).unwrap();
        let FilePatch::Create { hunks, .. } = &patch.files[0] else {
            panic!("expected create");
        };
        assert_eq!(hunks[0].lines, vec!["+value"]);
    }

    #[test]
    fn zero_length_insertion_range_is_valid() {
        let diff = "\
--- a/a.txt
+++ b/a.txt
@@ -1,0 +2 @@
+two
";
        let patch = parse_unified_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &patch.files[0] else {
            panic!("expected modify");
        };
        assert_eq!(
            crate::edit::apply::apply_hunks_to_text("a.txt", "one\n", hunks).unwrap(),
            b"one\ntwo\n"
        );
    }

    #[test]
    fn create_rejects_non_directory_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("parent"), "file").unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Create {
                path: "parent/child.txt".into(),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec!["+child".into()],
                    eof_newline: true,
                    old_eof_no_newline: false,
                }],
            }],
        };
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            ),
            Err(EditError::PathDenied { ref reason, .. }) if reason == "not a directory"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn every_patch_path_rejects_symlink_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/file.txt"), "old\n").unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let hunks = vec![Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines: vec!["-old".into(), "+new".into()],
            eof_newline: true,
            old_eof_no_newline: false,
        }];
        let patches = [
            PatchSet {
                files: vec![FilePatch::Modify {
                    path: "link/file.txt".into(),
                    hunks: hunks.clone(),
                }],
            },
            PatchSet {
                files: vec![FilePatch::Delete {
                    path: "link/file.txt".into(),
                }],
            },
            PatchSet {
                files: vec![FilePatch::Create {
                    path: "link/new.txt".into(),
                    hunks,
                }],
            },
        ];
        for patch in patches {
            assert!(matches!(
                validate_patchset_local(
                    &patch,
                    &policy(dir.path()),
                    &token(vec![Grant::FsWrite(Glob("**".into()))])
                ),
                Err(EditError::PathDenied { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_is_invalid_patch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.txt"), [0xff]).unwrap();
        let patch = PatchSet {
            files: vec![FilePatch::Delete {
                path: "bad.txt".into(),
            }],
        };
        assert!(matches!(
            validate_patchset_local(
                &patch,
                &policy(dir.path()),
                &token(vec![Grant::FsWrite(Glob("**".into()))])
            ),
            Err(EditError::InvalidPatch(ref message)) if message == "file not utf-8"
        ));
    }

    #[test]
    fn old_no_newline_marker_must_follow_final_old_line() {
        let diff = "\
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1 @@
-one
\\ No newline at end of file
-two
+new
";
        assert!(matches!(
            parse_unified_diff(diff),
            Err(EditError::InvalidPatch(ref message))
                if message == "old no-newline marker before final old line"
        ));
    }

    #[test]
    fn semantic_ops_all_variants_unsupported() {
        use alloy_runtime::SemanticEditOp;
        let ops = vec![
            SemanticEditOp::RenameType {
                from_path: "a".into(),
                to_name: "B".into(),
                update_references: true,
            },
            SemanticEditOp::UpdateImports {
                file: "a".into(),
                add: vec![],
                remove: vec![],
            },
            SemanticEditOp::ReplaceBody {
                item_path: "a".into(),
                new_body: "b".into(),
            },
            SemanticEditOp::InsertImpl {
                file: "a".into(),
                type_path: "T".into(),
                body: "{}".into(),
            },
            SemanticEditOp::AddMethod {
                item_path: "a".into(),
                method_source: "fn x() {}".into(),
            },
            SemanticEditOp::MoveModule {
                from_path: "a".into(),
                to_path: "b".into(),
            },
            SemanticEditOp::ExtractTrait {
                type_path: "T".into(),
                trait_name: "Tr".into(),
                method_names: vec![],
            },
            SemanticEditOp::SplitCrate {
                source_crate: "a".into(),
                new_crate: "b".into(),
                move_paths: vec![],
            },
            SemanticEditOp::AddField {
                type_path: "T".into(),
                field_source: "x: u8".into(),
            },
        ];
        for op in ops {
            let tag = op.op_tag().to_string();
            let req = EditRequest::SemanticOps { ops: vec![op] };
            assert!(matches!(
                reject_semantic(&req),
                Err(EditError::UnsupportedOp { op: ref got }) if got == &tag
            ));
        }
    }
}
