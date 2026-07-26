//! [`NativeSandboxBroker`] — production sandbox entry point (RFC-0005 §3.7).
//!
//! One `exec` is one ordered pipeline: token expiry, backend availability,
//! exec-grant match against the **pre-quarantine** argv, cwd jail membership,
//! per-execution scratch tree, quarantine rewrite, env scrub, deny-glob
//! snapshot, then a single handoff to the backend. Every step fails closed, and
//! the only thing the broker creates before the last denial can fire is the
//! scratch tree, which a guard removes on every exit path — including a
//! cancelled future.
//!
//! Author: arkadianet

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_runtime::{token_expired, Digest, Grant};
use async_trait::async_trait;
use globset::GlobSet;
use uuid::Uuid;

use crate::sandbox::backend::{
    allowlisted_ro_subtrees, collect_deny_paths, probe_all, run_isolated, IsolateContext,
};
use crate::sandbox::env::{apply_quarantine, scrub_env, OperatorHomes, ScrubInput};
use crate::sandbox::glob::compile_deny_globs;
use crate::sandbox::grant::{match_exec_grant, trusted_path_dirs, trusted_roots};
use crate::sandbox::path::PathPolicy;
use crate::sandbox::policy_digest::compute_policy_digest;
use crate::sandbox::process::into_exec_result;
use crate::sandbox::profile::{canonicalize_jail, SandboxProfile};
use crate::sandbox::types::{
    BackendStatus, DenialReason, NetworkPolicy, SandboxBackend, SandboxBroker, SandboxCapabilities,
    SandboxError, SandboxExecRequest, SandboxExecResult,
};

/// Jail-relative directory holding per-execution scratch trees (RFC-0005 §5.5).
const SCRATCH_DIR: &str = ".alloy-sbx";

/// Production broker: validates the profile and probes backends once, then
/// reuses the compiled policy for every execution.
pub struct NativeSandboxBroker {
    profile: SandboxProfile,
    capabilities: SandboxCapabilities,
    policy_digest: Digest,
    /// Deny globs compiled once — the profile cannot change after construction.
    deny_set: Arc<GlobSet>,
    /// Jail-only policy (no RO roots, no carve-out) for pre-flight cwd checks.
    base_policy: PathPolicy,
    /// Operator cargo/rustup homes (injected; never a process-global override).
    homes: OperatorHomes,
}

/// Elides the compiled matchers: a `GlobSet` dump is unreadable and the
/// `policy_digest` already identifies the policy they were built from.
impl std::fmt::Debug for NativeSandboxBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSandboxBroker")
            .field("profile", &self.profile)
            .field("capabilities", &self.capabilities)
            .field("policy_digest", &self.policy_digest)
            .finish_non_exhaustive()
    }
}

impl NativeSandboxBroker {
    /// Validate the profile, probe backends, and cache the compiled policy.
    ///
    /// Fails closed when the profile's `check` backend cannot isolate on this
    /// host: a broker that cannot enforce must not exist, because there is no
    /// bare-exec fallback. The `test` backend is deliberately allowed to be
    /// unavailable here — a host without a container runtime can still run
    /// checks — and [`exec`](SandboxBroker::exec) returns
    /// [`SandboxError::BackendUnavailable`] for
    /// [`ExecClass::Test`](crate::sandbox::ExecClass) instead.
    pub async fn new(profile: SandboxProfile) -> Result<Self, SandboxError> {
        Self::with_operator_homes(profile, OperatorHomes::resolve()?).await
    }

    /// Construct with explicit operator homes (tests / custom layouts).
    pub async fn with_operator_homes(
        profile: SandboxProfile,
        homes: OperatorHomes,
    ) -> Result<Self, SandboxError> {
        // Cheap and side-effect free, so it runs before the probes fork anything.
        validate_profile(&profile)?;

        // The profile is immutable, so everything derived from it is built once.
        let deny_set = Arc::new(compile_deny_globs(&profile.deny_globs)?);
        let base_policy = PathPolicy::from_profile(&profile, Vec::new())?;
        let policy_digest = compute_policy_digest(&profile);

        // Probes fork children and walk the filesystem; keep them off the worker.
        let capabilities = tokio::task::spawn_blocking(probe_all)
            .await
            .map_err(|e| SandboxError::Internal(format!("backend probe join: {e}")))?;

        tracing::info!(
            landlock = ?capabilities.landlock,
            seatbelt = ?capabilities.seatbelt,
            container = ?capabilities.container,
            "sandbox backend probe"
        );

        ensure_backend_available(profile.check_backend, &capabilities)?;

        Ok(Self {
            profile,
            capabilities,
            policy_digest,
            deny_set,
            base_policy,
            homes,
        })
    }

    async fn exec_inner(
        &self,
        req: SandboxExecRequest,
        backend: SandboxBackend,
    ) -> Result<SandboxExecResult, SandboxError> {
        let run_id = req.perms.run_id;

        // Expiry is inclusive: `now == expires` is already expired.
        if token_expired(req.perms.expires.as_ref()) {
            return Err(SandboxError::TokenExpired);
        }

        // Capabilities were cached at construction, but a runtime can be removed
        // while the broker lives, and the test backend was never required then.
        ensure_backend_available(backend, &self.capabilities)?;

        let homes = self.homes.clone();
        // PATH search only looks inside bin directories; membership checks accept
        // the broader trusted roots, so resolution gets the union of the two.
        let path_dirs = trusted_path_dirs(Some(&homes.cargo_home), Some(&homes.rustup_home));
        let mut resolve_roots = path_dirs.clone();
        for root in trusted_roots(Some(&homes.cargo_home), Some(&homes.rustup_home)) {
            if !resolve_roots.contains(&root) {
                resolve_roots.push(root);
            }
        }

        // The grant match runs on the pre-quarantine argv and hands back the
        // binary it authorized; resolving again could land on another target.
        let matched = match_exec_grant(&req.perms, &req.argv, backend, &req.cwd, &resolve_roots)?;
        tracing::debug!(
            run_id = %run_id,
            allow_binary = %matched.allow.binary,
            "sandbox exec grant matched"
        );
        let granted_network = req
            .perms
            .grants
            .iter()
            .any(|g| matches!(g, Grant::Network(_)));
        if granted_network {
            // Profile deny wins over any token grant (§5.4); say so once.
            tracing::debug!(run_id = %run_id, "network grant ignored: profile denies egress");
        }

        // Pre-flight so an out-of-jail cwd is denied before anything is created;
        // the authoritative check runs against the per-exec policy below.
        self.base_policy.authorize_cwd(&req.cwd)?;

        let exec = ExecDir::create(self.base_policy.jail(), &Uuid::new_v4().to_string())?;

        // Allowlisted operator subtrees are readable (§5.5). Build artifacts
        // persist under the jail's own `target/` (do not force a per-exec
        // `CARGO_TARGET_DIR` that is deleted on every return).
        let read_only_roots = allowlisted_ro_subtrees(&homes.cargo_home, &homes.rustup_home);
        let policy = PathPolicy::from_profile(&self.profile, read_only_roots.clone())?;
        // `from_profile` re-canonicalizes the jail. Backends bind
        // `profile.fs_jail` verbatim, so a jail that moved since construction
        // would isolate a different tree than the one being authorized.
        if policy.jail() != self.base_policy.jail() {
            return Err(SandboxError::Invalid(format!(
                "fs_jail {} moved since broker construction: now resolves to {}",
                self.profile.fs_jail.display(),
                policy.jail().display()
            )));
        }
        let cwd = policy.authorize_cwd(&req.cwd)?;

        // Native backends always resolve to a host binary. Container
        // basename-form argv has none: the image supplies the tool, and the
        // backend resolves the runtime itself and passes only `ctx.argv` on.
        let (program, authority_basename) = match (&matched.resolved, backend) {
            (Some(resolved), _) => (
                resolved.resolved.clone(),
                resolved.authority_basename().to_string(),
            ),
            (None, SandboxBackend::Container) => {
                (PathBuf::from(&req.argv[0]), argv0_basename(&req.argv))
            }
            (None, native) => {
                return Err(SandboxError::Internal(format!(
                    "{native:?} backend produced no resolved binary"
                )))
            }
        };

        // Quarantine rewrites argv only after the grant matched (§6.2), and the
        // authority for "is this cargo" is the resolved binary, never argv alone.
        let (mut argv, _) = apply_quarantine(
            &req.argv,
            Some(&authority_basename),
            self.profile.quarantine_deps,
        )?;
        if let Some(resolved) = &matched.resolved {
            // argv0-dispatch: rustup shims switch on the name they were invoked
            // with, so the child keeps the caller's argv0, not the canonical one.
            argv[0] = resolved.original_argv0.clone();
        }

        let env = scrub_env(&ScrubInput {
            child_home: &exec.home,
            child_tmpdir: &exec.tmp,
            cargo_home: &homes.cargo_home,
            rustup_home: &homes.rustup_home,
            cargo_target_dir: None,
            env_allow: &req.env_allow,
            quarantine: self.profile.quarantine_deps,
            path_value: Some(path_value(&path_dirs)),
        })?;

        // Bind-overs are a spawn-time snapshot — see the module residual risk note.
        // Walk off the async worker; budget exhaustion is BackendCannotEnforce.
        let jail = policy.jail().to_path_buf();
        let deny_set = self.deny_set.clone();
        let deny_paths = tokio::task::spawn_blocking(move || collect_deny_paths(&jail, &deny_set))
            .await
            .map_err(|e| SandboxError::Internal(format!("deny-glob walk join: {e}")))??;

        let outcome = run_isolated(
            backend,
            &self.profile,
            IsolateContext {
                program,
                argv,
                cwd,
                env,
                exec_dir: exec.root.clone(),
                cargo_home: homes.cargo_home,
                rustup_home: homes.rustup_home,
                deny_paths,
                read_only_roots,
            },
        )
        .await?;

        Ok(into_exec_result(
            outcome,
            backend,
            self.policy_digest.clone(),
        ))
    }
}

#[async_trait]
impl SandboxBroker for NativeSandboxBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        let backend = self.profile.backend_for(req.class);
        let run_id = req.perms.run_id;

        // RFC-0005 §9: never the full argv, never env values.
        tracing::info!(
            run_id = %run_id,
            class = ?req.class,
            backend = ?backend,
            argv0_basename = %argv0_basename(&req.argv),
            argc = req.argv.len(),
            "sandbox exec start"
        );

        let result = self.exec_inner(req, backend).await;
        match &result {
            Err(SandboxError::Denied(reason)) => {
                tracing::warn!(run_id = %run_id, reason = %reason, "sandbox exec denied");
            }
            Err(SandboxError::Timeout(after)) => {
                tracing::warn!(
                    run_id = %run_id,
                    timeout_secs = after.as_secs(),
                    "sandbox exec timed out"
                );
            }
            _ => {}
        }
        result
    }

    fn profile(&self) -> &SandboxProfile {
        &self.profile
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }
}

/// Reject a profile no backend can enforce, before any host work happens.
fn validate_profile(profile: &SandboxProfile) -> Result<(), SandboxError> {
    if matches!(profile.network, NetworkPolicy::Allow) {
        return Err(SandboxError::Invalid(
            "network=allow is rejected in MVP; no backend can enforce per-host egress".into(),
        ));
    }
    if profile.exec_timeout.is_zero() {
        return Err(SandboxError::Invalid(
            "exec_timeout must be greater than zero".into(),
        ));
    }
    // A zero cap would drop every byte of the child's output on the floor.
    if profile.stdout_cap == 0 {
        return Err(SandboxError::Invalid(
            "stdout_cap must be greater than zero".into(),
        ));
    }
    if profile.stderr_cap == 0 {
        return Err(SandboxError::Invalid(
            "stderr_cap must be greater than zero".into(),
        ));
    }
    if !profile.fs_jail.is_absolute() {
        return Err(SandboxError::Invalid(format!(
            "fs_jail must be absolute: {}",
            profile.fs_jail.display()
        )));
    }
    // Backends bind `fs_jail` verbatim while every path decision compares
    // canonical paths, so a jail that is not already canonical would isolate a
    // different tree than the one the policy authorizes.
    let canonical = canonicalize_jail(profile.fs_jail.clone())?;
    if canonical != profile.fs_jail {
        return Err(SandboxError::Invalid(format!(
            "fs_jail must be canonical: {} resolves to {}",
            profile.fs_jail.display(),
            canonical.display()
        )));
    }
    let uses_container = profile.check_backend == SandboxBackend::Container
        || profile.test_backend == SandboxBackend::Container;
    if uses_container && profile.container_image.trim().is_empty() {
        return Err(SandboxError::Invalid(
            "container_image must be set when a class uses the container backend".into(),
        ));
    }
    Ok(())
}

/// Map a probe status to a hard error; `NotApplicable` means the wrong host OS.
fn ensure_backend_available(
    backend: SandboxBackend,
    caps: &SandboxCapabilities,
) -> Result<(), SandboxError> {
    let status = match backend {
        SandboxBackend::Landlock => &caps.landlock,
        SandboxBackend::Seatbelt => &caps.seatbelt,
        SandboxBackend::Container => &caps.container,
    };
    match status {
        BackendStatus::Available { .. } => Ok(()),
        BackendStatus::Unavailable { reason } => Err(SandboxError::BackendUnavailable {
            backend,
            message: reason.clone(),
        }),
        BackendStatus::NotApplicable => Err(SandboxError::UnsupportedOs),
    }
}

/// Per-execution scratch tree at `<jail>/.alloy-sbx/<exec_id>` (RFC-0005 §5.5).
///
/// Holds the child `HOME` and `TMPDIR`. Dropping removes the tree, so a
/// cancelled `exec` future leaves nothing behind; the shared `.alloy-sbx`
/// parent is removed only when it is empty, which keeps concurrent executions
/// in one jail out of each other's way. Broker-owned bind sources outside the
/// jail belong to the backends. Build artifacts use the jail's `target/`.
#[derive(Debug)]
struct ExecDir {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
}

impl ExecDir {
    fn create(jail: &Path, exec_id: &str) -> Result<Self, SandboxError> {
        let root = jail.join(SCRATCH_DIR).join(exec_id);
        std::fs::create_dir_all(&root).map_err(SandboxError::Io)?;

        // A sandboxed child can leave `.alloy-sbx` behind as a symlink, and
        // creating through it would put the child's HOME/TMPDIR outside the
        // jail. The jail is canonical, so the tree just created must equal its
        // own canonical form.
        let canonical = root.canonicalize().map_err(SandboxError::Io)?;
        if canonical != root {
            let _ = std::fs::remove_dir(&canonical);
            return Err(SandboxError::Denied(DenialReason::PathDenied(format!(
                "per-exec directory {} escapes the jail: resolves to {}",
                root.display(),
                canonical.display()
            ))));
        }

        let dir = Self {
            home: root.join("home"),
            tmp: root.join("tmp"),
            root,
        };
        // `dir` owns the tree from here: a failure below cleans up on drop.
        for path in [&dir.home, &dir.tmp] {
            std::fs::create_dir_all(path).map_err(SandboxError::Io)?;
        }
        Ok(dir)
    }
}

impl Drop for ExecDir {
    fn drop(&mut self) {
        let root = self.root.clone();
        let cleanup = move || {
            if let Err(e) = std::fs::remove_dir_all(&root) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        error = %e,
                        path = %root.display(),
                        "per-exec directory cleanup failed"
                    );
                }
            }
            if let Some(parent) = root.parent() {
                // Succeeds only once the last concurrent execution has finished.
                let _ = std::fs::remove_dir(parent);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                drop(handle.spawn_blocking(cleanup));
            }
            Err(_) => cleanup(),
        }
    }
}

/// Join the trusted bin directories into the child's `PATH`.
fn path_value(dirs: &[PathBuf]) -> OsString {
    let mut value = OsString::new();
    for dir in dirs {
        if !value.is_empty() {
            value.push(":");
        }
        value.push(dir);
    }
    value
}

/// Basename of `argv[0]` for logging and container quarantine detection.
fn argv0_basename(argv: &[String]) -> String {
    let Some(argv0) = argv.first() else {
        return String::new();
    };
    Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argv0)
        .to_string()
}

/// Test helper: token expiry boundary exactly as `exec` evaluates it.
#[cfg(test)]
pub(crate) fn token_is_expired(perms: &alloy_runtime::PermissionToken) -> bool {
    token_expired(perms.expires.as_ref())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_runtime::{ExecAllow, PermissionToken, ProfileId, RunId};

    use super::*;

    fn profile_for(jail: &Path) -> SandboxProfile {
        SandboxProfile::default_for_jail(jail.to_path_buf()).unwrap()
    }

    async fn reject(profile: SandboxProfile, needle: &str) {
        let err = NativeSandboxBroker::new(profile).await.unwrap_err();
        assert!(
            matches!(err, SandboxError::Invalid(ref m) if m.contains(needle)),
            "expected Invalid containing {needle:?}, got {err:?}"
        );
    }

    #[test]
    fn token_expired_compares_offsetdatetime() {
        let now = alloy_runtime::Timestamp::now().0;
        let perms = PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![Grant::Exec(ExecAllow {
                binary: "true".into(),
                args_glob: None,
            })],
            // Equality boundary: `now == expires` must count as expired.
            expires: Some(alloy_runtime::Timestamp(now)),
            run_id: RunId::new(),
        };
        assert!(token_is_expired(&perms));

        let future = PermissionToken {
            expires: Some(alloy_runtime::Timestamp(now + Duration::from_secs(3600))),
            ..perms
        };
        assert!(!token_is_expired(&future));
    }

    #[tokio::test]
    async fn new_rejects_network_allow() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = profile_for(dir.path());
        profile.network = NetworkPolicy::Allow;
        reject(profile, "network=allow").await;
    }

    #[tokio::test]
    async fn new_rejects_zero_timeout_and_caps() {
        let dir = tempfile::tempdir().unwrap();

        let mut zero_timeout = profile_for(dir.path());
        zero_timeout.exec_timeout = Duration::ZERO;
        reject(zero_timeout, "exec_timeout").await;

        let mut zero_stdout = profile_for(dir.path());
        zero_stdout.stdout_cap = 0;
        reject(zero_stdout, "stdout_cap").await;

        let mut zero_stderr = profile_for(dir.path());
        zero_stderr.stderr_cap = 0;
        reject(zero_stderr, "stderr_cap").await;
    }

    #[tokio::test]
    async fn new_rejects_relative_jail() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = profile_for(dir.path());
        profile.fs_jail = PathBuf::from("relative/jail");
        reject(profile, "must be absolute").await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn new_rejects_symlinked_jail() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut profile = profile_for(dir.path());
        profile.fs_jail = link;
        reject(profile, "must be canonical").await;
    }

    #[tokio::test]
    async fn new_rejects_container_without_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = profile_for(dir.path());
        profile.test_backend = SandboxBackend::Container;
        profile.container_image = String::new();
        reject(profile, "container_image").await;
    }

    #[test]
    fn exec_dir_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();

        let root = {
            let exec = ExecDir::create(&jail, "exec-1").unwrap();
            assert!(exec.home.is_dir());
            assert!(exec.tmp.is_dir());
            exec.root.clone()
        };

        assert!(!root.exists(), "per-exec tree must not outlive the guard");
        assert!(
            !jail.join(SCRATCH_DIR).exists(),
            "empty scratch parent must be removed too"
        );
    }

    /// A child that replaces `.alloy-sbx` with a symlink must not redirect the
    /// next execution's HOME / TMPDIR / cargo cache out of the jail.
    #[test]
    #[cfg(unix)]
    fn exec_dir_rejects_symlinked_scratch_parent() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), jail.join(SCRATCH_DIR)).unwrap();

        let err = ExecDir::create(&jail, "exec-1").unwrap_err();
        assert!(
            matches!(err, SandboxError::Denied(DenialReason::PathDenied(ref m)) if m.contains("escapes the jail")),
            "got {err:?}"
        );
        assert!(
            !outside.path().join("exec-1").exists(),
            "the escaped directory must be removed again"
        );
    }

    #[test]
    fn path_value_joins_trusted_dirs() {
        let value = path_value(&[PathBuf::from("/usr/bin"), PathBuf::from("/op/.cargo/bin")]);
        assert_eq!(value, OsString::from("/usr/bin:/op/.cargo/bin"));
        assert_eq!(path_value(&[]), OsString::new());
    }

    #[test]
    fn argv0_basename_uses_final_component() {
        assert_eq!(argv0_basename(&["/usr/bin/cargo".into()]), "cargo");
        assert_eq!(argv0_basename(&["cargo".into()]), "cargo");
        assert_eq!(argv0_basename(&[]), "");
    }
}
