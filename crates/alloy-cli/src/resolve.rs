//! RFC-0015 §5.4/§5.5 — config resolution and fail-closed validation glue.
//!
//! Precedence (PR1): process environment > CLI flags > profile TOML >
//! built-in defaults — pinned to the merged `resolve_data_dir` /
//! `ConfigPaths::for_workspace` behaviour. This module owns no TOML parsing
//! (T9); everything comes back through `RuntimeConfig::load`.
//!
//! Author: arkadianet

use std::path::PathBuf;

use alloy_runtime::{validate_mvp_profile, ConfigPaths, ProfileId, RuntimeConfig, MVP_PROFILES};

use crate::args::Globals;
use crate::errx::{CliError, Exit};

/// Resolved invocation context shared by every handler.
pub struct Ctx {
    /// Absolute workspace root (session rows require absolute paths).
    pub workspace_abs: PathBuf,
    /// Loaded, validated runtime config.
    pub cfg: RuntimeConfig,
    /// Selected catalog profile id (PF1/PF4/PR3 reconciled).
    pub profile: String,
    /// `--json`.
    pub json: bool,
    /// `--quiet`.
    pub quiet: bool,
}

impl Ctx {
    /// Whether the selected profile is `readonly` (PF9/PF10).
    #[must_use]
    pub fn readonly(&self) -> bool {
        self.profile == "readonly"
    }
}

fn env_nonempty(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.is_empty())
}

/// §5.5 steps 1–4: validate the catalog id, load and validate the profile,
/// reconcile `[profile].id` against the selected id (PF4/PR3).
pub fn resolve(globals: &Globals) -> Result<Ctx, CliError> {
    // Step 1 — catalog id via the single merged validator (CL4/PF1).
    if let Some(profile) = &globals.profile {
        let id = ProfileId::new(profile.clone())
            .map_err(|e| CliError::new(Exit::Usage, format!("--profile {profile:?}: {e}")))?;
        validate_mvp_profile(&id).map_err(|_| {
            CliError::new(
                Exit::Usage,
                format!("--profile {profile:?} is not in the catalog {MVP_PROFILES:?}"),
            )
        })?;
    }

    // PR2 — the workspace is the input to path resolution; ConfigPaths joins
    // exactly once. `--profile <id>` maps to `profiles/<id>.toml` unless
    // ALLOY_PROFILE (env beats flags, PR1) supplies the file.
    let mut paths = ConfigPaths::for_workspace(globals.workspace.clone());
    if let Some(profile) = &globals.profile {
        if !env_nonempty("ALLOY_PROFILE") {
            paths.profile = globals.workspace.join(format!("profiles/{profile}.toml"));
        }
    }

    // Step 2 — load + PF6/PF7/PF8/PF12/PF13/PF14 (enforced in the loader).
    let cfg = RuntimeConfig::load(paths)?;

    // Steps 3 — [profile].id must equal the selected catalog id (PF4/PR3).
    let selected = match (&globals.profile, &cfg.profile_id) {
        (Some(flag), Some(file_id)) => {
            if flag != file_id {
                return Err(CliError::new(
                    Exit::Config,
                    format!(
                        "profile id mismatch: --profile {flag:?} but {} declares [profile].id = {file_id:?} (PF4/PR3)",
                        cfg.profile_path.display()
                    ),
                ));
            }
            flag.clone()
        }
        (Some(flag), None) => flag.clone(),
        (None, Some(file_id)) => {
            let id = ProfileId::new(file_id.clone()).map_err(|e| {
                CliError::new(
                    Exit::Config,
                    format!("{} [profile].id: {e}", cfg.profile_path.display()),
                )
            })?;
            validate_mvp_profile(&id).map_err(|_| {
                CliError::new(
                    Exit::Config,
                    format!(
                        "{} [profile].id = {file_id:?} is not in the catalog {MVP_PROFILES:?}",
                        cfg.profile_path.display()
                    ),
                )
            })?;
            file_id.clone()
        }
        (None, None) => "default".to_owned(),
    };

    let workspace_abs = crate::assembly::absolutize(&globals.workspace);
    Ok(Ctx {
        workspace_abs,
        cfg,
        profile: selected,
        json: globals.json,
        quiet: globals.quiet,
    })
}
