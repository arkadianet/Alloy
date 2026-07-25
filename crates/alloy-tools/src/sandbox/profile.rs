//! Sandbox profile loading from Appendix B `[sandbox]` TOML.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_runtime::Glob;
use serde::Deserialize;

use crate::sandbox::glob::default_deny_globs;
use crate::sandbox::types::{ExecClass, NetworkPolicy, SandboxBackend, SandboxError};

fn default_network() -> String {
    "deny".into()
}
fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    1800
}
fn default_cap() -> usize {
    2_097_152
}

/// Wire DTO for `[sandbox]` — not the runtime profile.
#[derive(Debug, Clone, Deserialize)]
struct SandboxConfigToml {
    check: String,
    test: String,
    #[serde(default = "default_network")]
    network: String,
    #[serde(default = "default_true")]
    quarantine_deps: bool,
    #[serde(default = "default_timeout")]
    exec_timeout_secs: u64,
    #[serde(default = "default_cap")]
    stdout_cap: usize,
    #[serde(default = "default_cap")]
    stderr_cap: usize,
    /// Optional container image. Overridden by `ALLOY_CONTAINER_IMAGE` when set.
    #[serde(default)]
    container_image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    sandbox: Option<SandboxConfigToml>,
}

/// Runtime sandbox profile (immutable after construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    /// Backend for [`ExecClass::Check`].
    pub check_backend: SandboxBackend,
    /// Backend for [`ExecClass::Test`].
    pub test_backend: SandboxBackend,
    /// Network policy (MVP: always Deny after load).
    pub network: NetworkPolicy,
    /// Force cargo offline / block fetch family.
    pub quarantine_deps: bool,
    /// Absolute canonical workspace jail.
    pub fs_jail: PathBuf,
    /// Credential / secret deny globs.
    pub deny_globs: Vec<Glob>,
    /// Wall-clock exec timeout.
    pub exec_timeout: Duration,
    /// Stdout capture cap in bytes.
    pub stdout_cap: usize,
    /// Stderr capture cap in bytes.
    pub stderr_cap: usize,
    /// Container image when any class uses Container.
    pub container_image: String,
}

impl SandboxProfile {
    /// Appendix B defaults for a canonical jail path.
    pub fn default_for_jail(fs_jail: PathBuf) -> Result<Self, SandboxError> {
        let fs_jail = canonicalize_jail(fs_jail)?;
        let check_backend = if cfg!(target_os = "macos") {
            SandboxBackend::Seatbelt
        } else {
            SandboxBackend::Landlock
        };
        Ok(Self {
            check_backend,
            test_backend: SandboxBackend::Container,
            network: NetworkPolicy::Deny,
            quarantine_deps: true,
            fs_jail,
            deny_globs: default_deny_globs(),
            exec_timeout: Duration::from_secs(default_timeout()),
            stdout_cap: default_cap(),
            stderr_cap: default_cap(),
            container_image: default_container_image(),
        })
    }

    /// Backend for the given exec class.
    #[must_use]
    pub fn backend_for(&self, class: ExecClass) -> SandboxBackend {
        match class {
            ExecClass::Check => self.check_backend,
            ExecClass::Test => self.test_backend,
        }
    }
}

/// Load `[sandbox]` from a profile TOML path.
///
/// Missing `[sandbox]` → [`SandboxError::Invalid`]. `network = "allow"` → Invalid
/// in MVP. Unknown keys under `[sandbox]` are ignored (serde default).
pub fn load_sandbox_profile(
    profile_toml: &Path,
    fs_jail: PathBuf,
) -> Result<SandboxProfile, SandboxError> {
    let raw = std::fs::read_to_string(profile_toml).map_err(|e| {
        SandboxError::Invalid(format!("read profile {}: {e}", profile_toml.display()))
    })?;
    let file: ProfileFile = toml::from_str(&raw).map_err(|e| {
        SandboxError::Invalid(format!("parse profile {}: {e}", profile_toml.display()))
    })?;
    let Some(cfg) = file.sandbox else {
        return Err(SandboxError::Invalid("missing [sandbox] section".into()));
    };

    let network = parse_network(&cfg.network)?;
    if matches!(network, NetworkPolicy::Allow) {
        return Err(SandboxError::Invalid(
            "network=allow is rejected in MVP; use deny".into(),
        ));
    }

    // Positivity matches validate_profile — report with the profile path here.
    if cfg.exec_timeout_secs == 0 {
        return Err(SandboxError::Invalid(format!(
            "exec_timeout_secs must be greater than zero in {}",
            profile_toml.display()
        )));
    }
    if cfg.stdout_cap == 0 {
        return Err(SandboxError::Invalid(format!(
            "stdout_cap must be greater than zero in {}",
            profile_toml.display()
        )));
    }
    if cfg.stderr_cap == 0 {
        return Err(SandboxError::Invalid(format!(
            "stderr_cap must be greater than zero in {}",
            profile_toml.display()
        )));
    }

    // Precedence: ALLOY_CONTAINER_IMAGE (env) > profile `container_image` >
    // compiled default. Env wins so CI/operators can pin without editing TOML.
    let image = if let Ok(env_image) = std::env::var("ALLOY_CONTAINER_IMAGE") {
        if !env_image.is_empty() {
            if cfg
                .container_image
                .as_ref()
                .is_some_and(|c| c != &env_image)
            {
                tracing::info!(
                    env = %env_image,
                    profile = ?cfg.container_image,
                    "ALLOY_CONTAINER_IMAGE overrides profile container_image"
                );
            }
            env_image
        } else {
            cfg.container_image.unwrap_or_else(default_container_image)
        }
    } else {
        cfg.container_image.unwrap_or_else(default_container_image)
    };

    Ok(SandboxProfile {
        check_backend: parse_backend(&cfg.check)?,
        test_backend: parse_backend(&cfg.test)?,
        network,
        quarantine_deps: cfg.quarantine_deps,
        fs_jail: canonicalize_jail(fs_jail)?,
        deny_globs: default_deny_globs(),
        exec_timeout: Duration::from_secs(cfg.exec_timeout_secs),
        stdout_cap: cfg.stdout_cap,
        stderr_cap: cfg.stderr_cap,
        container_image: image,
    })
}

fn default_container_image() -> String {
    "docker.io/library/rust:1.97.1-bookworm".into()
}

fn parse_backend(s: &str) -> Result<SandboxBackend, SandboxError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "landlock" => Ok(SandboxBackend::Landlock),
        "seatbelt" => Ok(SandboxBackend::Seatbelt),
        "container" => Ok(SandboxBackend::Container),
        other => Err(SandboxError::Invalid(format!(
            "unknown sandbox backend `{other}`"
        ))),
    }
}

fn parse_network(s: &str) -> Result<NetworkPolicy, SandboxError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "deny" => Ok(NetworkPolicy::Deny),
        "allow" => Ok(NetworkPolicy::Allow),
        other => Err(SandboxError::Invalid(format!(
            "unknown network policy `{other}`"
        ))),
    }
}

pub(crate) fn canonicalize_jail(fs_jail: PathBuf) -> Result<PathBuf, SandboxError> {
    let canon = fs_jail.canonicalize().map_err(|e| {
        SandboxError::Invalid(format!("fs_jail canonicalize {}: {e}", fs_jail.display()))
    })?;
    if !canon.is_absolute() {
        return Err(SandboxError::Invalid(
            "fs_jail must be absolute after canonicalize".into(),
        ));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn profile_missing_section_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.toml");
        std::fs::write(&path, "[profile]\nid = \"default\"\n").unwrap();
        let err = load_sandbox_profile(&path, dir.path().to_path_buf()).unwrap_err();
        assert!(matches!(err, SandboxError::Invalid(ref m) if m.contains("missing [sandbox]")));
    }

    #[test]
    fn network_allow_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "[sandbox]\ncheck = \"landlock\"\ntest = \"container\"\nnetwork = \"allow\"\n"
        )
        .unwrap();
        let err = load_sandbox_profile(&path, dir.path().to_path_buf()).unwrap_err();
        assert!(matches!(err, SandboxError::Invalid(ref m) if m.contains("network=allow")));
    }

    #[test]
    fn loads_default_deny() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.toml");
        std::fs::write(
            &path,
            r#"
[sandbox]
check = "landlock"
test = "container"
network = "deny"
quarantine_deps = true
"#,
        )
        .unwrap();
        let p = load_sandbox_profile(&path, dir.path().to_path_buf()).unwrap();
        assert_eq!(p.network, NetworkPolicy::Deny);
        assert!(p.quarantine_deps);
        assert_eq!(p.check_backend, SandboxBackend::Landlock);
        assert_eq!(p.test_backend, SandboxBackend::Container);
    }
}
