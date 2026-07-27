//! Sandboxed git checkpoint and restore backend (RFC-0008 §5.6).
//!
//! Author: arkadianet

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alloy_runtime::{CheckpointId, EditError, FilePatch, Grant, PatchSet, PermissionToken};

use crate::edit::map_error::map_sandbox;
use crate::edit::patch_parse::is_digest_excluded_path;
use crate::redact::redacted_snippet;
use crate::sandbox::grant::match_exec_grant;
use crate::sandbox::{ExecClass, PathPolicy, SandboxBroker, SandboxExecRequest, SandboxExecResult};

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
const PLACEHOLDER_UUID: &str = "00000000-0000-0000-0000-000000000000";
const STDOUT_TRUNCATED: &str = "git stdout truncated; raise sandbox stdout_cap";

/// Cap on the stderr text carried in an [`EditError`] detail.
const MAX_STDERR_SNIPPET_BYTES: usize = 200;

/// Created checkpoint metadata.
#[derive(Debug, Clone)]
pub(crate) struct CreatedCheckpoint {
    pub checkpoint_sha: String,
    pub head_sha: String,
    pub tracked: BTreeSet<String>,
}

/// Return the checkpoint ref for an id.
#[must_use]
pub(crate) fn checkpoint_ref(id: CheckpointId) -> String {
    format!("refs/alloy/checkpoints/{id}")
}

/// Preflight all git argv shapes through the shared grant matcher.
pub(crate) fn preflight_git(
    perms: &PermissionToken,
    backend: crate::sandbox::SandboxBackend,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<(), EditError> {
    for grant in &perms.grants {
        let Grant::Exec(allow) = grant else {
            continue;
        };
        if allow.args_glob.is_some()
            && Path::new(&allow.binary)
                .file_name()
                .and_then(|s| s.to_str())
                == Some("git")
        {
            return Err(EditError::MissingGrant("exec:git args".into()));
        }
    }
    for argv in preflight_argvs() {
        let matched =
            match_exec_grant(perms, &argv, backend, cwd, trusted_path).map_err(map_sandbox)?;
        if matched.allow.args_glob.is_some() {
            return Err(EditError::MissingGrant("exec:git args".into()));
        }
    }
    Ok(())
}

/// Probe repo invariants and return HEAD SHA + tracked set (no checkpoint yet).
pub(crate) async fn prepare_repo_for_edit(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    patch: &PatchSet,
) -> Result<(String, BTreeSet<String>), EditError> {
    ensure_git_version(broker, policy, perms).await?;
    ensure_inside_work_tree(broker, policy, perms).await?;
    let head_sha = rev_parse_head(broker, policy, perms).await?;
    ensure_object_format(broker, policy, perms, &head_sha).await?;
    ensure_repo_state_clean(broker, policy, perms).await?;
    let tracked = tracked_set(broker, policy, perms).await?;
    ensure_tracked_policy(policy, &tracked, patch)?;
    Ok((head_sha, tracked))
}

/// Create a git checkpoint ref after [`prepare_repo_for_edit`].
pub(crate) async fn create_checkpoint(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    id: CheckpointId,
    head_sha: &str,
    tracked: BTreeSet<String>,
) -> Result<CreatedCheckpoint, EditError> {
    // Everything from here is checkpoint creation, not repo probing: a failing
    // `stash create` is `CheckpointFailed`, never `Git`.
    let stash = git_stdout_or(broker, policy, perms, &["stash", "create"], |result| {
        EditError::CheckpointFailed(with_stderr("stash create failed", result))
    })
    .await?;
    let checkpoint_sha = if stash.trim().is_empty() {
        head_sha.to_string()
    } else {
        stash.trim().to_string()
    };
    if !is_sha1(&checkpoint_sha) {
        return Err(EditError::CheckpointFailed("invalid checkpoint sha".into()));
    }
    let reference = checkpoint_ref(id);
    let result = git_exec(
        broker,
        policy,
        perms,
        &["update-ref", &reference, &checkpoint_sha, ZERO_SHA],
    )
    .await?;
    if result.exit_code != Some(0) {
        return Err(EditError::CheckpointFailed(with_stderr(
            "checkpoint ref exists",
            &result,
        )));
    }
    Ok(CreatedCheckpoint {
        checkpoint_sha,
        head_sha: head_sha.to_string(),
        tracked,
    })
}

/// Resolve a checkpoint ref to a SHA.
pub(crate) async fn resolve_checkpoint(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    id: CheckpointId,
) -> Result<String, EditError> {
    let reference = checkpoint_ref(id);
    let result = git_exec(broker, policy, perms, &["rev-parse", &reference]).await?;
    if result.exit_code != Some(0) {
        return Err(EditError::Git(with_stderr(
            "checkpoint ref not found",
            &result,
        )));
    }
    stdout_string(&result).map(|s| s.trim().to_string())
}

/// Restore a checkpoint tree and unlink engine-owned created/temp paths.
pub(crate) async fn restore_checkpoint(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    checkpoint_sha: &str,
    created_paths: &[String],
    temp_paths: &[String],
    created_dirs: &[String],
) -> Result<(), EditError> {
    let source = format!("--source={checkpoint_sha}");
    let result = git_exec(
        broker,
        policy,
        perms,
        &["restore", &source, "--staged", "--worktree", "--", ":/"],
    )
    .await?;
    if result.exit_code != Some(0) {
        // The engine wraps this into `RollbackFailed`, so the snippet is what an
        // operator sees when a restore leaves the tree dirty.
        return Err(EditError::Git(with_stderr("restore failed", &result)));
    }
    for rel in created_paths.iter().chain(temp_paths.iter()) {
        if policy.deny_matches_rel(rel) {
            continue;
        }
        match std::fs::remove_file(policy.jail().join(rel)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EditError::Io(e.to_string())),
        }
    }
    let mut dirs = created_dirs.to_vec();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    for rel in dirs {
        if policy.deny_matches_rel(&rel) {
            continue;
        }
        match std::fs::remove_dir(policy.jail().join(&rel)) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => return Err(EditError::Io(e.to_string())),
        }
    }
    Ok(())
}

/// Load tracked set without mutating the repo.
pub(crate) async fn tracked_set(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<BTreeSet<String>, EditError> {
    let result = git_exec(broker, policy, perms, &["ls-files", "-z"]).await?;
    if result.exit_code != Some(0) {
        return Err(EditError::Git(with_stderr("ls-files failed", &result)));
    }
    if result.stdout_truncated {
        return Err(EditError::Environment(STDOUT_TRUNCATED.into()));
    }
    parse_ls_files(&result.stdout)
}

/// Fail if any tracked path matches deny-globs.
pub(crate) fn ensure_no_tracked_denied(
    policy: &PathPolicy,
    tracked: &BTreeSet<String>,
) -> Result<(), EditError> {
    for rel in tracked {
        if policy.deny_matches_rel(rel) {
            return Err(EditError::TrackedDeniedPath { path: rel.clone() });
        }
    }
    Ok(())
}

fn ensure_tracked_policy(
    policy: &PathPolicy,
    tracked: &BTreeSet<String>,
    patch: &PatchSet,
) -> Result<(), EditError> {
    ensure_no_tracked_denied(policy, tracked)?;
    for file in &patch.files {
        let rel = file.path();
        if is_digest_excluded_path(rel) {
            return Err(EditError::InvalidPatch("path excluded from digest".into()));
        }
        match file {
            FilePatch::Modify { .. } | FilePatch::Delete { .. } if !tracked.contains(rel) => {
                return Err(EditError::UntrackedPath {
                    path: rel.to_string(),
                })
            }
            FilePatch::Create { .. } if tracked.contains(rel) => {
                return Err(EditError::UntrackedPath {
                    path: rel.to_string(),
                })
            }
            _ => {}
        }
        let parts: Vec<&str> = rel.split('/').collect();
        for i in 1..parts.len() {
            let prefix = parts[..i].join("/");
            if policy.jail().join(&prefix).join(".git").is_dir() {
                return Err(EditError::Environment(
                    "submodule path not supported".into(),
                ));
            }
        }
    }
    Ok(())
}

async fn ensure_git_version(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<(), EditError> {
    // An unusable git is an environment problem, including when the probe itself
    // fails: `Git` is reserved for a working git refusing an operation.
    let out = git_stdout_or(broker, policy, perms, &["--version"], |result| {
        EditError::Environment(with_stderr("git --version failed", result))
    })
    .await?;
    if parse_git_version(&out)
        .is_some_and(|(major, minor)| major > 2 || (major == 2 && minor >= 23))
    {
        Ok(())
    } else {
        Err(EditError::Environment("git version < 2.23".into()))
    }
}

async fn ensure_inside_work_tree(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<(), EditError> {
    let not_a_repo = |result: &SandboxExecResult| {
        EditError::Environment(with_stderr("not a git repository", result))
    };
    let inside = git_stdout_or(
        broker,
        policy,
        perms,
        &["rev-parse", "--is-inside-work-tree"],
        not_a_repo,
    )
    .await?;
    if inside.trim() != "true" {
        return Err(EditError::Environment("not a git repository".into()));
    }
    let top = git_stdout_or(
        broker,
        policy,
        perms,
        &["rev-parse", "--show-toplevel"],
        not_a_repo,
    )
    .await?;
    // `PathPolicy::from_profile` canonicalizes the jail (RFC-0005 §3.6), so only
    // git's answer needs resolving before the two are compared.
    let top = PathBuf::from(top.trim())
        .canonicalize()
        .map_err(|_| EditError::Environment("repo toplevel != jail".into()))?;
    if top != policy.jail() {
        return Err(EditError::Environment("repo toplevel != jail".into()));
    }
    let git_meta = std::fs::symlink_metadata(policy.jail().join(".git"))
        .map_err(|_| EditError::Environment("linked worktree not supported".into()))?;
    if !git_meta.is_dir() {
        return Err(EditError::Environment(
            "linked worktree not supported".into(),
        ));
    }
    Ok(())
}

async fn rev_parse_head(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<String, EditError> {
    let result = git_exec(
        broker,
        policy,
        perms,
        &["rev-parse", "-q", "--verify", "HEAD"],
    )
    .await?;
    if result.exit_code != Some(0) {
        return Err(EditError::Environment(
            "empty repository: make initial commit".into(),
        ));
    }
    let sha = stdout_string(&result)?.trim().to_string();
    if is_sha1(&sha) {
        Ok(sha)
    } else {
        Err(EditError::Environment("unsupported object format".into()))
    }
}

async fn ensure_object_format(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    head_sha: &str,
) -> Result<(), EditError> {
    let result = git_exec(
        broker,
        policy,
        perms,
        &["rev-parse", "--show-object-format"],
    )
    .await?;
    if result.exit_code == Some(0) {
        let fmt = stdout_string(&result)?;
        if fmt.trim() != "sha1" {
            return Err(EditError::Environment("unsupported object format".into()));
        }
    }
    if !is_sha1(head_sha) {
        return Err(EditError::Environment("unsupported object format".into()));
    }
    Ok(())
}

async fn ensure_repo_state_clean(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
) -> Result<(), EditError> {
    let unmerged = git_stdout(
        broker,
        policy,
        perms,
        &["diff", "--name-only", "--diff-filter=U"],
    )
    .await?;
    if !unmerged.trim().is_empty() {
        return Err(EditError::Conflict(
            "repo state not clean for checkpoint".into(),
        ));
    }
    for rel in [
        ".git/MERGE_HEAD",
        ".git/CHERRY_PICK_HEAD",
        ".git/REVERT_HEAD",
        ".git/rebase-merge",
        ".git/rebase-apply",
        ".git/BISECT_LOG",
    ] {
        if std::fs::symlink_metadata(policy.jail().join(rel)).is_ok() {
            return Err(EditError::Conflict(
                "repo state not clean for checkpoint".into(),
            ));
        }
    }
    if std::fs::symlink_metadata(policy.jail().join(".git/index.lock")).is_ok() {
        return Err(EditError::Git("index.lock present".into()));
    }
    Ok(())
}

async fn git_stdout(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    tail: &[&str],
) -> Result<String, EditError> {
    git_stdout_or(broker, policy, perms, tail, |result| {
        EditError::Git(with_stderr("git command failed", result))
    })
    .await
}

/// Run git, require exit 0, and classify a non-zero exit through `on_failure`.
///
/// Callers pick the taxonomy: a probe that fails because the environment is
/// unusable is `Environment`, a checkpoint step that fails is `CheckpointFailed`,
/// and only a working git refusing an operation is `Git` (RFC-0008 §5.4).
async fn git_stdout_or<F>(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    tail: &[&str],
    on_failure: F,
) -> Result<String, EditError>
where
    F: FnOnce(&SandboxExecResult) -> EditError,
{
    let result = git_exec(broker, policy, perms, tail).await?;
    if result.exit_code != Some(0) {
        return Err(on_failure(&result));
    }
    stdout_string(&result)
}

/// `message`, plus git's stderr when it said anything useful.
///
/// Absolute paths are redacted and the text is capped, so the detail is safe to
/// surface through `apply_patch` while still naming the actual git complaint.
fn with_stderr(message: &str, result: &SandboxExecResult) -> String {
    let snippet = redacted_snippet(
        &String::from_utf8_lossy(&result.stderr),
        MAX_STDERR_SNIPPET_BYTES,
    );
    if snippet.is_empty() {
        message.to_string()
    } else {
        format!("{message}: {snippet}")
    }
}

async fn git_exec(
    broker: &dyn SandboxBroker,
    policy: &PathPolicy,
    perms: &PermissionToken,
    tail: &[&str],
) -> Result<SandboxExecResult, EditError> {
    let argv = git_argv(tail);
    let req = SandboxExecRequest::new(
        argv,
        policy.jail().to_path_buf(),
        perms.clone(),
        ExecClass::Check,
    );
    broker.exec(req).await.map_err(map_sandbox)
}

fn stdout_string(result: &SandboxExecResult) -> Result<String, EditError> {
    if result.stdout_truncated {
        return Err(EditError::Environment(STDOUT_TRUNCATED.into()));
    }
    String::from_utf8(result.stdout.clone())
        .map_err(|_| EditError::Environment("git stdout not utf-8".into()))
}

fn git_argv(tail: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git",
        "-c",
        "user.name=alloy",
        "-c",
        "user.email=alloy@localhost",
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.eol=lf",
        "-c",
        "filter.lfs.smudge=",
        "-c",
        "filter.lfs.clean=",
        "-c",
        "filter.lfs.process=",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    argv.extend(tail.iter().map(|s| (*s).to_string()));
    argv
}

fn preflight_argvs() -> Vec<Vec<String>> {
    vec![
        git_argv(&["--version"]),
        git_argv(&["rev-parse", "--is-inside-work-tree"]),
        git_argv(&["rev-parse", "-q", "--verify", "HEAD"]),
        git_argv(&["rev-parse", "--show-toplevel"]),
        git_argv(&["rev-parse", "--show-object-format"]),
        git_argv(&["ls-files", "-z"]),
        git_argv(&["diff", "--name-only", "--diff-filter=U"]),
        git_argv(&["stash", "create"]),
        git_argv(&[
            "update-ref",
            &checkpoint_ref_placeholder(),
            ZERO_SHA,
            ZERO_SHA,
        ]),
        git_argv(&[
            "restore",
            &format!("--source={ZERO_SHA}"),
            "--staged",
            "--worktree",
            "--",
            ":/",
        ]),
        git_argv(&["rev-parse", &checkpoint_ref_placeholder()]),
        git_argv(&["rev-parse", "HEAD"]),
    ]
}

fn checkpoint_ref_placeholder() -> String {
    format!("refs/alloy/checkpoints/{PLACEHOLDER_UUID}")
}

fn parse_ls_files(stdout: &[u8]) -> Result<BTreeSet<String>, EditError> {
    let mut out = BTreeSet::new();
    for part in stdout.split(|b| *b == 0) {
        if part.is_empty() {
            continue;
        }
        let rel = std::str::from_utf8(part)
            .map_err(|_| EditError::Environment("non-utf8 tracked path".into()))?;
        out.insert(rel.to_string());
    }
    Ok(out)
}

fn parse_git_version(out: &str) -> Option<(u64, u64)> {
    let version = out.split_whitespace().nth(2)?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn is_sha1(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxBackend;
    use alloy_runtime::{ExecAllow, Glob, Grant, ProfileId, RunId};

    fn token(grants: Vec<Grant>) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants,
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn checkpoint_ref_shape() {
        let id = CheckpointId::new();
        assert!(checkpoint_ref(id).starts_with("refs/alloy/checkpoints/"));
        assert!(checkpoint_ref(id).ends_with(&id.to_string()));
    }

    #[test]
    fn git_prefix_on_every_preflight_shape() {
        for argv in preflight_argvs() {
            assert_eq!(argv[0], "git");
            assert!(argv.windows(2).any(|w| w == ["-c", "user.name=alloy"]));
            assert!(argv.windows(2).any(|w| w == ["-c", "filter.lfs.process="]));
            assert!(!argv.iter().any(|a| a == "--stdin"));
        }
    }

    #[test]
    fn stderr_snippet_is_redacted_capped_and_optional() {
        let result = SandboxExecResult::synthetic(
            Some(128),
            None,
            SandboxBackend::Landlock,
            alloy_runtime::Digest::sha256(b"policy"),
        );
        assert_eq!(with_stderr("restore failed", &result), "restore failed");

        let with_paths = result.clone().with_stdio(
            Vec::new(),
            b"fatal: unable to write /home/op/repo/.git/index\n".to_vec(),
        );
        assert_eq!(
            with_stderr("restore failed", &with_paths),
            "restore failed: fatal: unable to write <path>"
        );

        let long = result.with_stdio(Vec::new(), vec![b'x'; MAX_STDERR_SNIPPET_BYTES * 2]);
        let detail = with_stderr("restore failed", &long);
        assert!(detail.len() <= "restore failed: ".len() + MAX_STDERR_SNIPPET_BYTES + 3);
        assert!(detail.ends_with("..."));
    }

    #[test]
    fn parse_ls_files_rejects_non_utf8() {
        assert!(matches!(
            parse_ls_files(b"a\0\xff\0"),
            Err(EditError::Environment(ref m)) if m == "non-utf8 tracked path"
        ));
    }

    #[test]
    fn args_glob_git_preflight_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = preflight_git(
            &token(vec![Grant::Exec(ExecAllow {
                binary: "git".into(),
                args_glob: Some("*".into()),
            })]),
            SandboxBackend::Landlock,
            dir.path(),
            &[PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        )
        .unwrap_err();
        assert!(matches!(err, EditError::MissingGrant(ref g) if g == "exec:git args"));
    }

    #[test]
    fn tracked_policy_checks_patch_membership() {
        let dir = tempfile::tempdir().unwrap();
        let profile =
            crate::sandbox::SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
        let policy = PathPolicy::from_profile(&profile, Vec::new()).unwrap();
        let tracked = ["a.txt"].into_iter().map(str::to_string).collect();
        let patch = PatchSet {
            files: vec![FilePatch::Modify {
                path: "b.txt".into(),
                hunks: vec![],
            }],
        };
        assert!(matches!(
            ensure_tracked_policy(&policy, &tracked, &patch),
            Err(EditError::UntrackedPath { ref path }) if path == "b.txt"
        ));
        let patch = PatchSet {
            files: vec![FilePatch::Create {
                path: "a.txt".into(),
                hunks: vec![],
            }],
        };
        assert!(matches!(
            ensure_tracked_policy(&policy, &tracked, &patch),
            Err(EditError::UntrackedPath { ref path }) if path == "a.txt"
        ));
        let _ = Glob("**".into());
    }
}
