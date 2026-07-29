//! Configuration loading (never writes `.env`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::RuntimeError;
use crate::types::budget::BudgetPolicy;

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
        // Only env overrides need resolving against the root; the defaults below are
        // already workspace-joined and must not be joined twice (that turned a relative
        // `--workspace ws` into `ws/ws/profiles/default.toml`).
        let profile = match env_path("ALLOY_PROFILE") {
            Some(p) => resolve_against(&workspace_root, p),
            None => workspace_root.join("profiles/default.toml"),
        };
        let router = match env_path("ALLOY_ROUTER") {
            Some(p) => resolve_against(&workspace_root, p),
            None => workspace_root.join("router.toml"),
        };
        Self {
            profile,
            router,
            example_env: workspace_root.join("example.env"),
            data_dir: None,
            workspace_root: Some(workspace_root),
        }
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
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
    /// Run timeout from the profile `[limits].run_timeout_secs` (RFC-0015 §5.6
    /// amendment A2); defaults to 30 minutes when the table is absent.
    pub run_timeout: Duration,
    /// From profile `[budgets]`, or [`BudgetPolicy::default`] when the table is absent (RFC-0007 §7.6).
    pub budget_policy: BudgetPolicy,
    /// From profile `[context]`, or [`crate::context::ContextProfile::v2_defaults`]
    /// when the table is absent (RFC-0012 §4.6).
    pub context_profile: crate::context::ContextProfile,
    /// `[profile].id` from the profile file (RFC-0015 PF4). `None` when the
    /// `[profile]` table is absent (pre-RFC-0015 skeleton profiles).
    pub profile_id: Option<String>,
    /// Parsed `[gates]` table, or defaults (RFC-0015 §5.6 amendment A1).
    pub gates: GatesConfig,
    /// Opaque echo of `[sandbox]` for the RFC-0015 CR6 cross-check. `None`
    /// when the table is absent (read-only subcommands tolerate that).
    pub sandbox_echo: Option<SandboxEcho>,
    /// Gate timeout from `[limits].gate_timeout_secs`; `None` waits
    /// indefinitely (RFC-0015 §5.2 `[limits]`).
    pub gate_timeout: Option<Duration>,
}

/// Parsed `[gates]` table (RFC-0015 amendment A1).
///
/// `allow_raw_bash` MUST be `false` and `require_cargo_check` MUST be `true`
/// in every catalog profile (rules PF6/PF7); [`RuntimeConfig::load`] fails
/// closed on violations, so a constructed value always satisfies both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatesConfig {
    /// Verification gate; MUST stay `true` (PF7).
    #[serde(default = "default_true")]
    pub require_cargo_check: bool,
    /// Human gate on public API changes.
    #[serde(default = "default_true")]
    pub require_human_on_public_api: bool,
    /// Human gate on new `unsafe`.
    #[serde(default = "default_true")]
    pub require_human_on_new_unsafe: bool,
    /// Human gate on new dependencies.
    #[serde(default = "default_true")]
    pub require_human_on_new_dependency: bool,
    /// Raw bash tool; MUST stay `false` (PF6).
    #[serde(default)]
    pub allow_raw_bash: bool,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            require_cargo_check: true,
            require_human_on_public_api: true,
            require_human_on_new_unsafe: true,
            require_human_on_new_dependency: true,
            allow_raw_bash: false,
        }
    }
}

/// Opaque `[sandbox]` echo for the RFC-0015 CR6 cross-check.
///
/// The schema owner is `alloy-tools::load_sandbox_profile`; this echo exists
/// only so the composition root can assert both parsers read the same
/// `network` / `quarantine_deps` values from the same file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxEcho {
    /// Backend for the check class.
    pub check: String,
    /// Backend for the test class.
    pub test: String,
    /// Network policy string; MUST be `"deny"` (PF8).
    #[serde(default = "default_deny")]
    pub network: String,
    /// Dependency quarantine; MUST be `true` (PF8).
    #[serde(default = "default_true")]
    pub quarantine_deps: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    #[serde(default)]
    profile: Option<ProfileSection>,
    #[serde(default)]
    gates: Option<GatesConfig>,
    #[serde(default)]
    sandbox: Option<SandboxEcho>,
    #[serde(default)]
    budgets: Option<BudgetsSection>,
    #[serde(default)]
    observability: ObservabilitySection,
    /// Raw `[context]` table; RFC-0012 owns the schema
    /// (`ContextProfile::from_toml_table`, rules D2/D19).
    #[serde(default)]
    context: Option<toml::Table>,
    #[serde(default)]
    limits: Option<LimitsSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileSection {
    id: String,
    #[serde(default)]
    #[allow(dead_code)] // documentation-only field; parsed so PF14 accepts it
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetsSection {
    #[serde(default = "default_usd")]
    max_usd_per_run: f64,
    #[serde(default = "default_tokens")]
    max_tokens_per_run: u64,
    #[serde(default = "default_one")]
    max_parallel_nodes: u32,
    #[serde(default = "default_one")]
    max_parallel_cargo: u32,
    #[serde(default = "default_one")]
    max_parallel_edits: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservabilitySection {
    #[serde(default)]
    retain_full_prompts: bool,
    #[serde(default)]
    retain_tool_bodies: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsSection {
    #[serde(default = "default_run_timeout_secs")]
    run_timeout_secs: u64,
    #[serde(default)]
    gate_timeout_secs: Option<u64>,
}

fn default_usd() -> f64 {
    5.0
}
fn default_tokens() -> u64 {
    2_000_000
}
fn default_one() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn default_deny() -> String {
    "deny".into()
}
fn default_run_timeout_secs() -> u64 {
    60 * 30
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
    ///
    /// Router file existence is required; full `router.toml` schema ownership is RFC-0007
    /// ([`crate::router::RouterConfig`]). This loader does **not** parse `[provider.*]`.
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

        let (data_dir, data_dir_rule) = resolve_data_dir(&paths)?;

        let context_profile = match &profile.context {
            Some(table) => crate::context::ContextProfile::from_toml_table(table).map_err(|e| {
                RuntimeError::Config(format!(
                    "profile {} [context]: {e}",
                    paths.profile.display()
                ))
            })?,
            None => crate::context::ContextProfile::v2_defaults(),
        };

        // RFC-0015 PF13: weights must be finite, non-negative, and sum to
        // 1.0 ± 1e-6 (stricter than RFC-0012's own normalizing validate).
        {
            let w = &context_profile.weights;
            let sum = f64::from(w.conversation) + f64::from(w.working_set) + f64::from(w.artifacts);
            if !sum.is_finite() || (sum - 1.0).abs() > 1e-6 {
                return Err(RuntimeError::Config(format!(
                    "profile {} [context].weights must sum to 1.0 (got {sum}) (RFC-0015 PF13)",
                    paths.profile.display()
                )));
            }
        }

        let budget_policy = match profile.budgets {
            Some(b) => BudgetPolicy {
                max_usd_per_run: b.max_usd_per_run,
                max_tokens_per_run: b.max_tokens_per_run,
                max_parallel_nodes: b.max_parallel_nodes,
                max_parallel_cargo: b.max_parallel_cargo,
                max_parallel_edits: b.max_parallel_edits,
            },
            None => BudgetPolicy::default(),
        };

        // RFC-0015 fail-closed profile validation (PF6/PF7/PF8/PF12).
        let profile_display = paths.profile.display();
        let gates = profile.gates.unwrap_or_default();
        if gates.allow_raw_bash {
            return Err(RuntimeError::Config(format!(
                "profile {profile_display} [gates].allow_raw_bash must be false in every catalog profile (RFC-0015 PF6)"
            )));
        }
        if !gates.require_cargo_check {
            return Err(RuntimeError::Config(format!(
                "profile {profile_display} [gates].require_cargo_check must be true in every catalog profile (RFC-0015 PF7)"
            )));
        }
        if let Some(sandbox) = &profile.sandbox {
            if sandbox.network != "deny" {
                return Err(RuntimeError::Config(format!(
                    "profile {profile_display} [sandbox].network must be \"deny\" (RFC-0015 PF8), got {:?}",
                    sandbox.network
                )));
            }
            if !sandbox.quarantine_deps {
                return Err(RuntimeError::Config(format!(
                    "profile {profile_display} [sandbox].quarantine_deps must be true (RFC-0015 PF8)"
                )));
            }
        }
        for (name, value) in [
            ("max_parallel_nodes", budget_policy.max_parallel_nodes),
            ("max_parallel_cargo", budget_policy.max_parallel_cargo),
            ("max_parallel_edits", budget_policy.max_parallel_edits),
        ] {
            if value != 1 {
                return Err(RuntimeError::Config(format!(
                    "profile {profile_display} [budgets].{name} must be 1 (RFC-0015 PF12; host parallel honesty), got {value}"
                )));
            }
        }

        let (run_timeout, gate_timeout) = match &profile.limits {
            Some(l) => (
                Duration::from_secs(l.run_timeout_secs),
                l.gate_timeout_secs.map(Duration::from_secs),
            ),
            None => (Duration::from_secs(default_run_timeout_secs()), None),
        };

        Ok(Self {
            data_dir,
            data_dir_rule,
            profile_path: paths.profile,
            router_path: paths.router,
            env_file_hint: paths.example_env,
            retain_full_prompts: profile.observability.retain_full_prompts,
            retain_tool_bodies: profile.observability.retain_tool_bodies,
            run_timeout,
            budget_policy,
            context_profile,
            profile_id: profile.profile.map(|p| p.id),
            gates,
            sandbox_echo: profile.sandbox,
            gate_timeout,
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

/// Shared minimal valid `router.toml` body for tests.
///
/// Public so integration tests (separate crates) can reuse the same fixture as
/// unit tests without duplicating the TOML literal.
#[must_use]
pub fn default_router_toml() -> &'static str {
    r#"
[policy]
default_tier = "standard"

[[providers]]
id = "openai-compatible-main"
kind = "openai_compatible"
base_url = "https://api.example.com/v1/"
api_key_env = "ALLOY_API_KEY"

[[providers.endpoints]]
id = "team-workhorse"
display_name = "Workhorse"
model = "REPLACE_ME"
tiers = ["standard"]
max_context = 200000
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0

[capability_tiers]
repair = "standard"
"#
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
        fs::write(&router, default_router_toml()).unwrap();
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
        assert_eq!(cfg.budget_policy.max_usd_per_run, 1.0);
        assert_eq!(cfg.budget_policy.max_tokens_per_run, 100);
        assert_eq!(cfg.budget_policy.max_parallel_nodes, 1);
        assert_eq!(fs::read_to_string(&dotenv).unwrap(), "SENTINEL=1\n");
    }

    #[test]
    fn load_does_not_parse_provider_peek() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, router, example) = write_fixtures(dir.path());
        // Garbage that would fail a provisional [provider.*] schema is fine —
        // load only requires the file to exist.
        fs::write(&router, "# not parsed by RuntimeConfig::load\n").unwrap();
        let cfg = RuntimeConfig::load(ConfigPaths {
            profile,
            router,
            example_env: example,
            data_dir: None,
            workspace_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert!(cfg.router_path.is_file());
    }

    #[test]
    fn absent_budgets_uses_policy_default() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/default.toml");
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        fs::write(
            &profile,
            r#"
[profile]
id = "default"
[observability]
retain_full_prompts = false
"#,
        )
        .unwrap();
        let router = dir.path().join("router.toml");
        fs::write(&router, "# exists\n").unwrap();
        let example = dir.path().join("example.env");
        fs::write(&example, "ALLOY_API_KEY=\n").unwrap();
        let cfg = RuntimeConfig::load(ConfigPaths {
            profile,
            router,
            example_env: example,
            data_dir: None,
            workspace_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert_eq!(cfg.budget_policy, BudgetPolicy::default());
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

    /// Defaults are already workspace-joined; they must not be joined a second
    /// time. Only an absolute `workspace_root` hid this — a relative root such as
    /// `--workspace alloy` yielded `alloy/alloy/profiles/default.toml`.
    #[test]
    fn for_workspace_does_not_double_join_relative_root() {
        let paths = ConfigPaths::for_workspace(PathBuf::from("ws"));
        assert_eq!(paths.profile, PathBuf::from("ws/profiles/default.toml"));
        assert_eq!(paths.router, PathBuf::from("ws/router.toml"));
        assert_eq!(paths.example_env, PathBuf::from("ws/example.env"));
    }
}
