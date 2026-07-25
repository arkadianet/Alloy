//! Configuration loading (never writes `.env`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::RuntimeError;

/// Paths consulted by [`RuntimeConfig::load`].
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Profile TOML path.
    pub profile: PathBuf,
    /// Active router TOML path (user-owned `router.toml`, not the `.example` template).
    pub router: PathBuf,
    /// `example.env` path for error messages only.
    pub example_env: PathBuf,
    /// Optional explicit data dir override (checked after `ALLOY_DATA_DIR`, before workspace/XDG).
    pub data_dir: Option<PathBuf>,
    /// Optional workspace root for `.alloy` resolution.
    pub workspace_root: Option<PathBuf>,
}

impl ConfigPaths {
    /// Build paths for a workspace, honoring `ALLOY_PROFILE` / `ALLOY_ROUTER`.
    ///
    /// - Profile default: `<workspace>/profiles/default.toml`
    /// - Router default: `<workspace>/router.toml` (copy from `router.toml.example`)
    /// - `ALLOY_DATA_DIR` is read later by [`RuntimeConfig::load`] (not stored here)
    /// - Relative override paths resolve against `workspace_root`
    ///
    /// Never reads or writes a `.env` file — only process environment.
    #[must_use]
    pub fn for_workspace(workspace_root: PathBuf) -> Self {
        let profile = env_path_or("ALLOY_PROFILE", || {
            workspace_root.join("profiles/default.toml")
        });
        let router = env_path_or("ALLOY_ROUTER", || workspace_root.join("router.toml"));
        Self {
            profile: resolve_against(&workspace_root, profile),
            router: resolve_against(&workspace_root, router),
            example_env: workspace_root.join("example.env"),
            data_dir: None,
            workspace_root: Some(workspace_root),
        }
    }
}

fn env_path_or(key: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default)
}

fn resolve_against(workspace_root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

/// Loaded runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Data directory (`.alloy` or XDG).
    pub data_dir: PathBuf,
    /// Which precedence rule selected [`Self::data_dir`].
    pub data_dir_rule: &'static str,
    /// Profile path.
    pub profile_path: PathBuf,
    /// Router path.
    pub router_path: PathBuf,
    /// Hint path to `example.env` (docs/errors only).
    pub env_file_hint: PathBuf,
    /// Retain full prompts in logs (default false).
    pub retain_full_prompts: bool,
    /// Retain tool bodies in logs (default false).
    pub retain_tool_bodies: bool,
    /// Default run timeout.
    pub run_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    budgets: BudgetsSection,
    #[serde(default)]
    observability: ObservabilitySection,
}

#[derive(Debug, Default, Deserialize)]
struct BudgetsSection {
    #[serde(default = "default_usd")]
    max_usd_per_run: f64,
    #[serde(default = "default_tokens")]
    max_tokens_per_run: u64,
}

#[derive(Debug, Default, Deserialize)]
struct ObservabilitySection {
    #[serde(default)]
    retain_full_prompts: bool,
    #[serde(default)]
    retain_tool_bodies: bool,
}

#[derive(Debug, Deserialize)]
struct RouterFile {
    #[serde(default)]
    provider: std::collections::BTreeMap<String, ProviderSection>,
}

#[derive(Debug, Deserialize)]
struct ProviderSection {
    #[serde(default)]
    api_key_env: Option<String>,
}

fn default_usd() -> f64 {
    5.0
}
fn default_tokens() -> u64 {
    2_000_000
}

impl RuntimeConfig {
    /// Load TOML + process env. **Never writes `.env`.**
    ///
    /// Data dir precedence:
    /// 1. `ALLOY_DATA_DIR` (process env, if set and non-empty)
    /// 2. [`ConfigPaths::data_dir`] programmatic override
    /// 3. `<workspace>/.alloy` when [`ConfigPaths::workspace_root`] is set
    /// 4. XDG (`$XDG_DATA_HOME/alloy` or `~/.local/share/alloy`)
    ///
    /// Profile/router path overrides (`ALLOY_PROFILE`, `ALLOY_ROUTER`) are applied when
    /// constructing paths via [`ConfigPaths::for_workspace`], not by parsing `.env` files.
    pub fn load(paths: ConfigPaths) -> Result<Self, RuntimeError> {
        if !paths.profile.is_file() {
            return Err(RuntimeError::Config(format!(
                "missing profile TOML at {} (see {})",
                paths.profile.display(),
                paths.example_env.display()
            )));
        }
        let profile_raw = std::fs::read_to_string(&paths.profile).map_err(|e| {
            RuntimeError::Config(format!("read profile {}: {e}", paths.profile.display()))
        })?;
        let profile: ProfileFile = toml::from_str(&profile_raw).map_err(|e| {
            RuntimeError::Config(format!("parse profile {}: {e}", paths.profile.display()))
        })?;

        if !paths.router.is_file() {
            return Err(RuntimeError::Config(format!(
                "missing router TOML at {} (copy router.toml.example; see {})",
                paths.router.display(),
                paths.example_env.display()
            )));
        }
        let router_raw = std::fs::read_to_string(&paths.router).map_err(|e| {
            RuntimeError::Config(format!("read router {}: {e}", paths.router.display()))
        })?;
        let router: RouterFile = toml::from_str(&router_raw).map_err(|e| {
            RuntimeError::Config(format!("parse router {}: {e}", paths.router.display()))
        })?;

        for (name, provider) in &router.provider {
            if let Some(key) = &provider.api_key_env {
                match std::env::var(key) {
                    Ok(v) if !v.is_empty() => {}
                    _ => {
                        tracing::warn!(
                            provider = %name,
                            env_key = %key,
                            hint = %paths.example_env.display(),
                            "router api_key_env is unset; provider calls will fail later (RFC-0007). Alloy never writes .env"
                        );
                    }
                }
            }
        }

        let (data_dir, data_dir_rule) = resolve_data_dir(&paths)?;

        let _ = profile.budgets.max_usd_per_run;
        let _ = profile.budgets.max_tokens_per_run;

        Ok(Self {
            data_dir,
            data_dir_rule,
            profile_path: paths.profile,
            router_path: paths.router,
            env_file_hint: paths.example_env,
            retain_full_prompts: profile.observability.retain_full_prompts,
            retain_tool_bodies: profile.observability.retain_tool_bodies,
            run_timeout: Duration::from_secs(60 * 30),
        })
    }
}

fn resolve_data_dir(paths: &ConfigPaths) -> Result<(PathBuf, &'static str), RuntimeError> {
    if let Ok(dir) = std::env::var("ALLOY_DATA_DIR") {
        if !dir.is_empty() {
            return Ok((PathBuf::from(dir), "ALLOY_DATA_DIR"));
        }
    }
    // Programmatic override — same precedence tier as env, wins over workspace/XDG.
    if let Some(explicit) = &paths.data_dir {
        return Ok((explicit.clone(), "ConfigPaths.data_dir"));
    }
    if let Some(root) = &paths.workspace_root {
        return Ok((root.join(".alloy"), "<workspace>/.alloy"));
    }
    Ok((default_xdg_data_dir(), "XDG_DATA_HOME/alloy"))
}

fn default_xdg_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Path::new(&xdg).join("alloy");
        }
    }
    dirs_fallback_home().join(".local/share/alloy")
}

fn dirs_fallback_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_fixtures(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let profile = dir.join("profiles/default.toml");
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        fs::write(
            &profile,
            r#"
[profile]
id = "default"
[budgets]
max_usd_per_run = 1.0
max_tokens_per_run = 100
[observability]
retain_full_prompts = false
retain_tool_bodies = false
"#,
        )
        .unwrap();
        let router = dir.join("router.toml");
        fs::write(
            &router,
            r#"
[provider.default]
kind = "openai_compatible"
api_key_env = "ALLOY_API_KEY"
"#,
        )
        .unwrap();
        let example = dir.join("example.env");
        fs::write(&example, "ALLOY_API_KEY=\n").unwrap();
        (profile, router, example)
    }

    #[test]
    fn load_never_writes_dotenv_and_preserves_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, router, example) = write_fixtures(dir.path());
        let dotenv = dir.path().join(".env");
        fs::write(&dotenv, "SENTINEL=1\n").unwrap();

        let cfg = RuntimeConfig::load(ConfigPaths {
            profile,
            router,
            example_env: example,
            data_dir: None,
            workspace_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert_eq!(cfg.data_dir_rule, "<workspace>/.alloy");
        assert_eq!(fs::read_to_string(&dotenv).unwrap(), "SENTINEL=1\n");
    }

    #[test]
    fn explicit_data_dir_overrides_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, router, example) = write_fixtures(dir.path());
        let explicit = dir.path().join("explicit-data");
        let cfg = RuntimeConfig::load(ConfigPaths {
            profile,
            router,
            example_env: example,
            data_dir: Some(explicit.clone()),
            workspace_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert_eq!(cfg.data_dir, explicit);
        assert_eq!(cfg.data_dir_rule, "ConfigPaths.data_dir");
    }

    #[test]
    fn for_workspace_defaults_to_active_router_toml() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::for_workspace(dir.path().to_path_buf());
        assert_eq!(paths.router, dir.path().join("router.toml"));
        assert_eq!(paths.profile, dir.path().join("profiles/default.toml"));
        assert_eq!(paths.example_env, dir.path().join("example.env"));
    }
}
