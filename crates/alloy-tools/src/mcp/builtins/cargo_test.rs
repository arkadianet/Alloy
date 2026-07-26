//! `cargo_test` builtin (RFC-0006 §5.7).
//!
//! Author: arkadianet

use alloy_runtime::{PermissionToken, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::builtins::{
    authorize_cargo_cwd, authorize_cargo_exec, object_args, optional_integer, optional_string,
    required_string, run_cargo, BuiltinCtx, BuiltinToolId, CargoExec,
};
use crate::mcp::error::McpError;
use crate::sandbox::ExecClass;

/// `cargo_test` arguments (schema: RFC-0006 §5.3.2).
///
/// There is deliberately no `timeout_secs`: the host `call_timeout` and the
/// profile `exec_timeout` own deadlines, so a schema knob would do nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoTestArgs {
    /// Workspace root; relative paths resolve against the sandbox jail.
    pub workspace_root: String,
    /// Restrict to one package (`-p`).
    #[serde(default)]
    pub package: Option<String>,
    /// Test-name filter, passed after `--`.
    #[serde(default)]
    pub test_name_filter: Option<String>,
    /// Cargo job count (`--jobs`).
    #[serde(default)]
    pub jobs: Option<u32>,
}

const ALLOWED_KEYS: &[&str] = &["workspace_root", "package", "test_name_filter", "jobs"];

/// Parse and validate arguments without touching the filesystem.
pub(crate) fn parse(arguments: &Value) -> Result<CargoTestArgs, McpError> {
    let obj = object_args(arguments, ALLOWED_KEYS)?;
    let jobs = optional_integer(obj, "jobs", 1, u64::from(u32::MAX))?
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
    Ok(CargoTestArgs {
        workspace_root: required_string(obj, "workspace_root")?,
        package: optional_string(obj, "package")?,
        test_name_filter: optional_string(obj, "test_name_filter")?,
        jobs,
    })
}

/// Build the intended argv (RFC-0006 §5.7).
///
/// The filter goes **after** `--` so it is a test-name filter rather than a
/// cargo option.
#[must_use]
pub(crate) fn build_argv(args: &CargoTestArgs) -> Vec<String> {
    let mut argv = vec!["cargo".to_string(), "test".to_string()];
    if let Some(package) = &args.package {
        argv.push("-p".into());
        argv.push(package.clone());
    }
    if let Some(jobs) = args.jobs {
        argv.push("--jobs".into());
        argv.push(jobs.to_string());
    }
    argv.push("--".into());
    argv.push("--nocapture".into());
    if let Some(filter) = &args.test_name_filter {
        argv.push(filter.clone());
    }
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
    authorize_cargo_exec(ctx, &exec, perms, ExecClass::Test)?;
    Ok(exec)
}

/// Dispatch through the sandbox broker under [`ExecClass::Test`].
pub(crate) async fn execute(
    ctx: &BuiltinCtx<'_>,
    exec: CargoExec,
    perms: PermissionToken,
) -> Result<ToolResult, McpError> {
    run_cargo(
        ctx,
        exec,
        perms,
        ExecClass::Test,
        BuiltinToolId::CargoTest.name(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn argv_cargo_test_mapping() {
        // RFC-0006 §5.7 worked example.
        let args = parse(&json!({
            "workspace_root": ".",
            "jobs": 2,
            "test_name_filter": "foo"
        }))
        .unwrap();
        let argv = build_argv(&args);
        assert_eq!(
            argv,
            vec!["cargo", "test", "--jobs", "2", "--", "--nocapture", "foo"]
        );
        assert_eq!(argv[1..].join(" "), "test --jobs 2 -- --nocapture foo");

        let plain = parse(&json!({ "workspace_root": "." })).unwrap();
        assert_eq!(
            build_argv(&plain),
            vec!["cargo", "test", "--", "--nocapture"]
        );

        let pkg = parse(&json!({ "workspace_root": ".", "package": "alloy-tools" })).unwrap();
        assert_eq!(
            build_argv(&pkg),
            vec!["cargo", "test", "-p", "alloy-tools", "--", "--nocapture"]
        );
    }

    #[test]
    fn cargo_test_rejects_empty_optional_strings() {
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "test_name_filter": "" })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: test_name_filter"
        ));
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "package": "" })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: package"
        ));
    }

    #[test]
    fn cargo_test_rejects_zero_jobs() {
        assert!(matches!(
            parse(&json!({ "workspace_root": ".", "jobs": 0 })),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("out of range")
        ));
        assert!(parse(&json!({ "workspace_root": ".", "jobs": null }))
            .unwrap()
            .jobs
            .is_none());
    }
}
