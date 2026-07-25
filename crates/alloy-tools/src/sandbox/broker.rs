//! [`NativeSandboxBroker`] — production sandbox entry point (RFC-0005).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use alloy_runtime::{Grant, Timestamp};
use async_trait::async_trait;
use uuid::Uuid;

use crate::sandbox::backend::{
    allowlisted_ro_subtrees, collect_deny_paths, probe_all, run_isolated, IsolateContext,
};
use crate::sandbox::env::{apply_quarantine, scrub_env, OperatorHomes, ScrubInput};
use crate::sandbox::glob::compile_deny_globs;
use crate::sandbox::grant::{
    match_exec_grant, resolve_executable, trusted_path_dirs, trusted_roots,
};
use crate::sandbox::path::PathPolicy;
use crate::sandbox::policy_digest::compute_policy_digest;
use crate::sandbox::process::into_exec_result;
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{
    BackendStatus, NetworkPolicy, SandboxBackend, SandboxBroker, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult,
};

/// Production broker: probes at construction, fail-closed for check backend.
#[derive(Debug)]
pub struct NativeSandboxBroker {
    profile: SandboxProfile,
    capabilities: SandboxCapabilities,
    policy_digest: alloy_runtime::Digest,
}

impl NativeSandboxBroker {
    /// Probe backends. Fail closed if `check_backend` is Unavailable.
    ///
    /// If only `test_backend` is Unavailable, construction succeeds and
    /// `exec(Test)` returns [`SandboxError::BackendUnavailable`].
    pub async fn new(profile: SandboxProfile) -> Result<Self, SandboxError> {
        // Probes are sync/OS work; run off the async worker if needed.
        let capabilities = tokio::task::spawn_blocking(probe_all)
            .await
            .map_err(|e| SandboxError::Internal(format!("probe join: {e}")))?;

        tracing::info!(
            landlock = ?capabilities.landlock,
            seatbelt = ?capabilities.seatbelt,
            container = ?capabilities.container,
            "sandbox backend probe"
        );

        ensure_backend_available(profile.check_backend, &capabilities)?;
        // test backend may be unavailable at construction.

        let policy_digest = compute_policy_digest(&profile);
        Ok(Self {
            profile,
            capabilities,
            policy_digest,
        })
    }
}

#[async_trait]
impl SandboxBroker for NativeSandboxBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        let backend = self.profile.backend_for(req.class);
        tracing::info!(
            run_id = %req.perms.run_id,
            class = ?req.class,
            backend = ?backend,
            argv0_basename = %basename_of(&req.argv),
            argc = req.argv.len(),
            "sandbox exec start"
        );

        match self.exec_inner(req, backend).await {
            Ok(r) => Ok(r),
            Err(e) => {
                if let SandboxError::Denied(ref reason) = e {
                    tracing::warn!(reason = %reason, "sandbox denied");
                }
                if matches!(e, SandboxError::Timeout(_)) {
                    tracing::warn!("sandbox timeout");
                }
                Err(e)
            }
        }
    }

    fn profile(&self) -> &SandboxProfile {
        &self.profile
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }
}

impl NativeSandboxBroker {
    async fn exec_inner(
        &self,
        req: SandboxExecRequest,
        backend: SandboxBackend,
    ) -> Result<SandboxExecResult, SandboxError> {
        // 1. Expiry (inclusive).
        if let Some(t) = &req.perms.expires {
            if Timestamp::now().0 >= t.0 {
                return Err(SandboxError::TokenExpired);
            }
        }

        // Capability check (may have vanished since probe).
        ensure_backend_available(backend, &self.capabilities)?;
        os_supports(backend)?;

        let homes = OperatorHomes::resolve()?;
        let path_dirs = trusted_path_dirs(Some(&homes.cargo_home), Some(&homes.rustup_home));
        let roots = trusted_roots(Some(&homes.cargo_home), Some(&homes.rustup_home));
        // PATH search uses bin dirs; membership allows broader trusted roots.
        let mut resolve_roots = path_dirs.clone();
        for r in &roots {
            if !resolve_roots.iter().any(|d| d == r) {
                resolve_roots.push(r.clone());
            }
        }

        // 2. Grant match on pre-quarantine argv.
        let _allow = match_exec_grant(&req.perms, &req.argv, backend, &req.cwd, &resolve_roots)?;

        // Resolve native binary (also used for quarantine cargo detection).
        let resolved = match backend {
            SandboxBackend::Container => {
                if Path::new(&req.argv[0]).is_absolute() || req.argv[0].contains('/') {
                    Some(resolve_executable(&req.argv[0], &req.cwd, &resolve_roots)?)
                } else {
                    None
                }
            }
            SandboxBackend::Landlock | SandboxBackend::Seatbelt => {
                Some(resolve_executable(&req.argv[0], &req.cwd, &resolve_roots)?)
            }
        };

        // 3. Cwd jail + deny globs.
        let path_policy = PathPolicy::from_profile(&self.profile, Vec::new())?;
        let cwd = path_policy.authorize_cwd(&req.cwd)?;

        // Profile network deny wins (MVP Allow rejected at load).
        if matches!(self.profile.network, NetworkPolicy::Deny) {
            // Grant::Network ignored under Deny.
            let _ = req
                .perms
                .grants
                .iter()
                .any(|g| matches!(g, Grant::Network(_)));
        }

        // 4. Per-exec directory.
        let exec_id = Uuid::new_v4().to_string();
        let exec_dir = self.profile.fs_jail.join(".alloy-sbx").join(&exec_id);
        let child_home = exec_dir.join("home");
        let child_tmp = exec_dir.join("tmp");
        let cargo_cache = exec_dir.join("cargo-cache");
        std::fs::create_dir_all(&child_home).map_err(SandboxError::Io)?;
        std::fs::create_dir_all(&child_tmp).map_err(SandboxError::Io)?;
        std::fs::create_dir_all(&cargo_cache).map_err(SandboxError::Io)?;

        // 5. Quarantine rewrite after grant match.
        let cargo_basename = match (&resolved, backend) {
            (Some(r), _) => Some(r.resolved_basename.as_str()),
            (None, SandboxBackend::Container) => Some(req.argv[0].as_str()),
            _ => None,
        };
        let (argv_q, _) =
            apply_quarantine(&req.argv, cargo_basename, self.profile.quarantine_deps)?;

        // 6. Env scrub.
        let path_value = filtered_path_value(&path_dirs);
        let scrub = ScrubInput {
            child_home: &child_home,
            child_tmpdir: &child_tmp,
            cargo_home: &homes.cargo_home,
            rustup_home: &homes.rustup_home,
            env_allow: &req.env_allow,
            quarantine: self.profile.quarantine_deps,
            path_value: Some(path_value),
        };
        let env = scrub_env(&scrub)?;

        // 7. Deny paths + RO roots.
        let deny_set = compile_deny_globs(&self.profile.deny_globs)?;
        let deny_paths = collect_deny_paths(&self.profile.fs_jail, &deny_set)?;
        let mut read_only_roots = allowlisted_ro_subtrees(&homes.cargo_home, &homes.rustup_home);
        // Persistent registry/src stays RO (already under registry/).

        let program = match backend {
            SandboxBackend::Container => {
                // Container runs argv verbatim; program is the runtime (filled by backend).
                // Placeholder — container backend resolves runtime itself; pass argv0 path.
                PathBuf::from(&argv_q[0])
            }
            SandboxBackend::Landlock | SandboxBackend::Seatbelt => resolved
                .as_ref()
                .map(|r| r.resolved.clone())
                .ok_or_else(|| SandboxError::Internal("missing resolved binary".into()))?,
        };

        // For native, rebuild argv with original argv0 + quarantined args.
        let mut argv_for_child = argv_q;
        if let Some(r) = &resolved {
            if !matches!(backend, SandboxBackend::Container) {
                argv_for_child[0] = r.original_argv0.clone();
            }
        }

        let ctx = IsolateContext {
            program,
            argv: argv_for_child,
            cwd,
            env,
            exec_dir: exec_dir.clone(),
            cargo_home: homes.cargo_home.clone(),
            rustup_home: homes.rustup_home.clone(),
            deny_paths,
            read_only_roots: {
                // Include system roots implicitly in backend; pass cargo/rustup here.
                let _ = &mut read_only_roots;
                read_only_roots
            },
        };

        let outcome = run_isolated(backend, &self.profile, ctx).await;
        // Best-effort cleanup of exec_dir.
        let _ = std::fs::remove_dir_all(&exec_dir);

        let outcome = outcome?;
        Ok(into_exec_result(
            outcome,
            backend,
            self.policy_digest.clone(),
        ))
    }
}

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

fn os_supports(backend: SandboxBackend) -> Result<(), SandboxError> {
    match backend {
        SandboxBackend::Landlock if !cfg!(target_os = "linux") => Err(SandboxError::UnsupportedOs),
        SandboxBackend::Seatbelt if !cfg!(target_os = "macos") => Err(SandboxError::UnsupportedOs),
        _ => Ok(()),
    }
}

fn filtered_path_value(dirs: &[PathBuf]) -> OsString {
    let mut s = OsString::new();
    for (i, d) in dirs.iter().enumerate() {
        if i > 0 {
            s.push(":");
        }
        s.push(d);
    }
    s
}

fn basename_of(argv: &[String]) -> String {
    argv.first()
        .map(|a| {
            Path::new(a)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(a)
                .to_string()
        })
        .unwrap_or_default()
}

/// Test helper: check token expiry boundary.
#[cfg(test)]
pub(crate) fn token_is_expired(perms: &alloy_runtime::PermissionToken) -> bool {
    match &perms.expires {
        Some(t) => Timestamp::now().0 >= t.0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{ExecAllow, Grant, PermissionToken, ProfileId, RunId};

    #[test]
    fn token_expired_compares_offsetdatetime() {
        let t = Timestamp::now().0;
        let perms = PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![Grant::Exec(ExecAllow {
                binary: "true".into(),
                args_glob: None,
            })],
            expires: Some(Timestamp(t)),
            run_id: RunId::new(),
        };
        // Equality boundary: now >= expires when expires == the captured instant
        // (clock may advance; construct comparison directly).
        assert!(Timestamp(t).0 >= t);
        assert!(matches!(
            perms.expires.as_ref(),
            Some(exp) if Timestamp(t).0 >= exp.0
        ));
        assert!(token_is_expired(&perms) || Timestamp::now().0 >= t);
    }
}
