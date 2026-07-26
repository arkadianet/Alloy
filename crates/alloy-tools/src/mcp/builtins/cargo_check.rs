//! `cargo_check` builtin (RFC-0006 §5.6).
//!
//! Author: arkadianet

use alloy_runtime::{PermissionToken, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::builtins::{
    authorize_cargo_cwd, authorize_cargo_exec, object_args, optional_bool, optional_string,
    optional_string_array, required_string, run_cargo, BuiltinCtx, BuiltinToolId, CargoExec,
    MAX_FEATURES,
};
use crate::mcp::error::McpError;
use crate::sandbox::ExecClass;

/// `cargo_check` arguments (schema: RFC-0006 §5.3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoCheckArgs {
    /// Workspace root; relative paths resolve against the sandbox jail.
    pub workspace_root: String,
    /// Restrict to one package (`-p`).
    #[serde(default)]
    pub package: Option<String>,
    /// Feature list; ignored when `all_features` is set.
    #[serde(default)]
    pub features: Vec<String>,
    /// Pass `--all-features` instead of individual `--features` flags.
    #[serde(default)]
    pub all_features: bool,
    /// Only `"json"` is accepted in MVP.
    #[serde(default = "default_message_format")]
    pub message_format: String,
}

fn default_message_format() -> String {
    "json".into()
}

const ALLOWED_KEYS: &[&str] = &[
    "workspace_root",
    "package",
    "features",
    "all_features",
    "message_format",
];

/// Parse and validate arguments without touching the filesystem.
pub(crate) fn parse(arguments: &Value) -> Result<CargoCheckArgs, McpError> {
    let obj = object_args(arguments, ALLOWED_KEYS)?;
    let message_format = match optional_string(obj, "message_format")? {
        None => default_message_format(),
        Some(f) if f == "json" => f,
        Some(_) => {
            return Err(McpError::InvalidArguments(
                "message_format must be \"json\"".into(),
            ))
        }
    };
    Ok(CargoCheckArgs {
        workspace_root: required_string(obj, "workspace_root")?,
        package: optional_string(obj, "package")?,
        features: optional_string_array(obj, "features", MAX_FEATURES)?,
        all_features: optional_bool(obj, "all_features", false)?,
        message_format,
    })
}

/// Build the intended argv (RFC-0006 §5.6). Empty strings are already rejected.
#[must_use]
pub(crate) fn build_argv(args: &CargoCheckArgs) -> Vec<String> {
    let mut argv = vec!["cargo".to_string(), "check".to_string()];
    if let Some(package) = &args.package {
        argv.push("-p".into());
        argv.push(package.clone());
    }
    if args.all_features {
        argv.push("--all-features".into());
    } else {
        for feature in &args.features {
            argv.push("--features".into());
            argv.push(feature.clone());
        }
    }
    argv.push("--message-format".into());
    argv.push("json".into());
    argv
}

/// Parse, derive argv and cwd, then authorize the exec.
pub(crate) fn prepare(
    ctx: &BuiltinCtx<'_>,
    arguments: &Value,
    perms: &PermissionToken,
) -> Result<CargoExec, McpError> {
    let args = parse(arguments)?;
    let argv = build_argv(&args);
    let cwd = authorize_cargo_cwd(ctx, &args.workspace_root)?;
    let exec = CargoExec { argv, cwd };
    authorize_cargo_exec(ctx, &exec, perms, ExecClass::Check)?;
    Ok(exec)
}

/// Dispatch through the sandbox broker under [`ExecClass::Check`].
pub(crate) async fn execute(
    ctx: &BuiltinCtx<'_>,
    exec: CargoExec,
    perms: PermissionToken,
) -> Result<ToolResult, McpError> {
    run_cargo(
        ctx,
        exec,
        perms,
        ExecClass::Check,
        BuiltinToolId::CargoCheck.name(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn argv_cargo_check_mapping() {
        let base = parse(&json!({ "workspace_root": "." })).unwrap();
        assert_eq!(
            build_argv(&base),
            vec!["cargo", "check", "--message-format", "json"]
        );

        let with_pkg = parse(&json!({
            "workspace_root": ".",
            "package": "alloy-tools",
            "features": ["a", "b"]
        }))
        .unwrap();
        assert_eq!(
            build_argv(&with_pkg),
            vec![
                "cargo",
                "check",
                "-p",
                "alloy-tools",
                "--features",
                "a",
                "--features",
                "b",
                "--message-format",
                "json"
            ]
        );

        let all = parse(&json!({
            "workspace_root": ".",
            "features": ["a"],
            "all_features": true
        }))
        .unwrap();
        assert_eq!(
            build_argv(&all),
            vec![
                "cargo",
                "check",
                "--all-features",
                "--message-format",
                "json"
            ]
        );
    }

    #[test]
    fn cargo_check_rejects_empty_optional_strings() {
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "package": "" })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: package"
        ));
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "features": [""] })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: features"
        ));
        assert!(matches!(
            parse(&json!({ "workspace_root": "" })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: workspace_root"
        ));
    }

    #[test]
    fn cargo_check_rejects_other_message_format() {
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "message_format": "human" })),
            Err(McpError::InvalidArguments(ref m)) if m.contains("message_format")
        ));
        assert!(parse(&json!({ "workspace_root": ".", "message_format": "json" })).is_ok());
    }

    #[test]
    fn cargo_check_rejects_unknown_property() {
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "timeout_secs": 5 })),
            Err(McpError::InvalidArguments(ref m)) if m == "additional property: timeout_secs"
        ));
    }
}
