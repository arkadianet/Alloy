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
    /// Router TOML path.
    pub router: PathBuf,
    /// `example.env` path for error messages only.
    pub example_env: PathBuf,
    /// Optional explicit data dir override.
    pub data_dir: Option<PathBuf>,
    /// Optional workspace root for `.alloy` resolution.
    pub workspace_root: Option<PathBuf>,
}

/// Loaded runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Data directory (`.alloy` or XDG).
    pub data_dir: PathBuf,
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

fn default_usd() -> f64 {
    5.0
}
fn default_tokens() -> u64 {
    2_000_000
}

impl RuntimeConfig {
    /// Load TOML + process env. **Never writes `.env`.**
    ///
    /// Data dir precedence: `ALLOY_DATA_DIR` → `<workspace>/.alloy` → XDG.
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

        // Router may be incomplete until RFC-0007; require file existence if path set.
        if !paths.router.is_file() {
            return Err(RuntimeError::Config(format!(
                "missing router TOML at {} (copy router.toml.example; see {})",
                paths.router.display(),
                paths.example_env.display()
            )));
        }
        // Parse enough to surface TOML errors; content used later by RFC-0007.
        let router_raw = std::fs::read_to_string(&paths.router).map_err(|e| {
            RuntimeError::Config(format!("read router {}: {e}", paths.router.display()))
        })?;
        let _: toml::Value = toml::from_str(&router_raw).map_err(|e| {
            RuntimeError::Config(format!("parse router {}: {e}", paths.router.display()))
        })?;

        let data_dir = resolve_data_dir(&paths)?;

        let _ = profile.budgets.max_usd_per_run;
        let _ = profile.budgets.max_tokens_per_run;

        Ok(Self {
            data_dir,
            profile_path: paths.profile,
            router_path: paths.router,
            env_file_hint: paths.example_env,
            retain_full_prompts: profile.observability.retain_full_prompts,
            retain_tool_bodies: profile.observability.retain_tool_bodies,
            run_timeout: Duration::from_secs(60 * 30),
        })
    }
}

fn resolve_data_dir(paths: &ConfigPaths) -> Result<PathBuf, RuntimeError> {
    if let Ok(dir) = std::env::var("ALLOY_DATA_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    if let Some(root) = &paths.workspace_root {
        return Ok(root.join(".alloy"));
    }
    if let Some(explicit) = &paths.data_dir {
        return Ok(explicit.clone());
    }
    Ok(default_xdg_data_dir())
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

    #[test]
    fn load_never_writes_dotenv() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/default.toml");
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
        let router = dir.path().join("router.toml");
        fs::write(
            &router,
            r#"
[provider.default]
kind = "openai_compatible"
"#,
        )
        .unwrap();
        let example = dir.path().join("example.env");
        fs::write(&example, "ALLOY_API_KEY=\n").unwrap();
        let dotenv = dir.path().join(".env");
        assert!(!dotenv.exists());

        let cfg = RuntimeConfig::load(ConfigPaths {
            profile,
            router,
            example_env: example,
            data_dir: Some(dir.path().join("data")),
            workspace_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert!(cfg.data_dir.ends_with(".alloy") || cfg.data_dir.ends_with("data"));
        assert!(!dotenv.exists(), ".env must never be created");
    }
}
