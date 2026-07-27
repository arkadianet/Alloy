//! The four in-process builtins registered as MCP tools (RFC-0006 §5.2).
//!
//! Every builtin shares one pipeline: parse arguments (pure), derive argv or a
//! canonical path, authorize, then dispatch. Preparation is deliberately split
//! from execution so the host can run only the dispatch under `call_timeout`
//! and so `InvalidArguments` always precedes `PermissionDenied`.
//!
//! Exec always goes through the RFC-0005 broker. No builtin constructs a
//! `Command` (the crate `clippy.toml` bans it outside `sandbox`).
//!
//! Author: arkadianet

pub(crate) mod apply_patch;
pub(crate) mod cargo_check;
pub(crate) mod cargo_test;
pub(crate) mod fs_read;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use alloy_runtime::{PermissionToken, ToolCall, ToolError, ToolName, ToolResult};
use serde_json::{json, Map, Value};

use crate::mcp::authz;
use crate::mcp::error::{map_sandbox_error, McpError};
use crate::mcp::patch::PatchApplyBackend;
use crate::sandbox::{ExecClass, PathPolicy, SandboxBroker, SandboxExecRequest, SandboxExecResult};

/// Hard cap on serialized `ToolCall.arguments` JSON bytes.
pub const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
/// Max `features` entries for `cargo_check`.
pub const MAX_FEATURES: usize = 64;
/// Max bytes for any single string field in arguments.
pub const MAX_ARG_STRING_BYTES: usize = 4096;

/// Identifies a registered in-process builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinToolId {
    /// `cargo check` under [`ExecClass::Check`](crate::sandbox::ExecClass).
    CargoCheck,
    /// `cargo test` under [`ExecClass::Test`](crate::sandbox::ExecClass).
    CargoTest,
    /// Jail-relative UTF-8 file read.
    FsRead,
    /// Patch application via the injected backend.
    ApplyPatch,
}

impl BuiltinToolId {
    /// Every builtin, in registration order.
    pub const ALL: [BuiltinToolId; 4] = [
        Self::CargoCheck,
        Self::CargoTest,
        Self::FsRead,
        Self::ApplyPatch,
    ];

    /// Registered catalog name.
    ///
    /// # Panics
    ///
    /// Never: the four literals are valid [`ToolName`]s, asserted by
    /// `builtin_names_are_valid`.
    #[must_use]
    pub fn name(self) -> ToolName {
        let raw = match self {
            Self::CargoCheck => "cargo_check",
            Self::CargoTest => "cargo_test",
            Self::FsRead => "fs_read",
            Self::ApplyPatch => "apply_patch",
        };
        ToolName::new(raw).expect("builtin tool names are valid")
    }

    /// Canonical disclosure tags (case-sensitive, exact equality).
    #[must_use]
    pub fn tags(self) -> &'static [&'static str] {
        match self {
            Self::CargoCheck => &["sel.compiler"],
            Self::CargoTest => &["sel.test"],
            Self::FsRead => &["sel.fs"],
            Self::ApplyPatch => &["sel.edit"],
        }
    }
}

/// Everything a builtin may reach: the broker, the host-owned path policy, the
/// trusted exec roots, and the injected patch backend. Deliberately no handle,
/// no registry, no obs.
pub(crate) struct BuiltinCtx<'a> {
    pub(crate) broker: &'a dyn SandboxBroker,
    pub(crate) path_policy: &'a PathPolicy,
    pub(crate) trusted_path: &'a [PathBuf],
    pub(crate) patch_backend: &'a dyn PatchApplyBackend,
}

/// A parsed, derived, and authorized call ready for dispatch.
#[derive(Debug)]
pub(crate) enum Prepared {
    /// Sandboxed `cargo check`.
    CargoCheck(CargoExec),
    /// Sandboxed `cargo test`.
    CargoTest(CargoExec),
    /// Canonical path read.
    FsRead(fs_read::PreparedRead),
    /// Backend patch application.
    ApplyPatch(crate::mcp::patch::ApplyPatchArgs),
}

/// An authorized sandbox exec: intended argv plus the jail-checked cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoExec {
    /// Intended argv, already matched against the token's `ExecAllow` set.
    pub(crate) argv: Vec<String>,
    /// Canonical cwd returned by `PathPolicy::authorize_cwd`.
    pub(crate) cwd: PathBuf,
}

/// Parse arguments, derive argv / canonical path, then authorize.
///
/// Ordering is normative (RFC-0006 §5.1): argument validation is pure and runs
/// first, so a call that is both malformed and ungranted returns
/// `InvalidArguments`.
pub(crate) fn prepare(
    id: BuiltinToolId,
    ctx: &BuiltinCtx<'_>,
    call: &ToolCall,
    perms: &PermissionToken,
) -> Result<Prepared, McpError> {
    match id {
        BuiltinToolId::CargoCheck => {
            cargo_check::prepare(ctx, &call.arguments, perms).map(Prepared::CargoCheck)
        }
        BuiltinToolId::CargoTest => {
            cargo_test::prepare(ctx, &call.arguments, perms).map(Prepared::CargoTest)
        }
        BuiltinToolId::FsRead => {
            fs_read::prepare(ctx, &call.arguments, perms).map(Prepared::FsRead)
        }
        BuiltinToolId::ApplyPatch => {
            apply_patch::prepare(&call.arguments, perms).map(Prepared::ApplyPatch)
        }
    }
}

/// Dispatch a prepared call. Runs under the host `call_timeout` and cancel.
pub(crate) async fn execute(
    ctx: &BuiltinCtx<'_>,
    prepared: Prepared,
    perms: PermissionToken,
    session: Option<alloy_runtime::SessionId>,
    run: Option<alloy_runtime::RunId>,
) -> Result<ToolResult, McpError> {
    match prepared {
        Prepared::CargoCheck(p) => cargo_check::execute(ctx, p, perms).await,
        Prepared::CargoTest(p) => cargo_test::execute(ctx, p, perms).await,
        Prepared::FsRead(p) => fs_read::execute(p).await,
        Prepared::ApplyPatch(args) => apply_patch::execute(ctx, args, perms, session, run).await,
    }
}

// --- Shared cargo plumbing ---------------------------------------------------

/// Resolve `workspace_root` against the jail and require cwd membership.
///
/// Relative roots join the jail; absolute roots are taken as given and still
/// have to canonicalize inside the jail.
pub(crate) fn authorize_cargo_cwd(
    ctx: &BuiltinCtx<'_>,
    workspace_root: &str,
) -> Result<PathBuf, McpError> {
    let raw = Path::new(workspace_root);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        ctx.path_policy.jail().join(raw)
    };
    ctx.path_policy
        .authorize_cwd(&candidate)
        .map_err(map_sandbox_error)
}

/// Authorize `argv` through the shared RFC-0005 exec matcher for `class`.
pub(crate) fn authorize_cargo_exec(
    ctx: &BuiltinCtx<'_>,
    exec: &CargoExec,
    perms: &PermissionToken,
    class: ExecClass,
) -> Result<(), McpError> {
    let backend = ctx.broker.profile().backend_for(class);
    authz::authorize_exec(perms, &exec.argv, backend, &exec.cwd, ctx.trusted_path)
}

/// Run an authorized cargo exec through the broker and map the outcome.
///
/// A non-zero child exit is a *tool* failure, not a host failure: it comes back
/// as `Ok(ToolResult)` carrying `ExecutionFailed` so a repair loop can consume
/// the diagnostics.
pub(crate) async fn run_cargo(
    ctx: &BuiltinCtx<'_>,
    exec: CargoExec,
    perms: PermissionToken,
    class: ExecClass,
    name: ToolName,
) -> Result<ToolResult, McpError> {
    let started = Instant::now();
    let req = SandboxExecRequest::new(exec.argv, exec.cwd, perms, class);
    let result = ctx.broker.exec(req).await.map_err(map_sandbox_error)?;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(cargo_result_to_tool_result(name, &result, elapsed))
}

fn cargo_result_to_tool_result(
    name: ToolName,
    result: &SandboxExecResult,
    duration_ms: u64,
) -> ToolResult {
    let content = json!({
        "exit_code": result.exit_code,
        "signal": result.signal,
        "stdout_utf8": String::from_utf8_lossy(&result.stdout),
        "stderr_utf8": String::from_utf8_lossy(&result.stderr),
        "stdout_truncated": result.stdout_truncated,
        "stderr_truncated": result.stderr_truncated,
        "duration_ms": result.duration_ms,
        "backend": result.backend,
        "policy_digest": result.policy_digest,
    });
    if result.exit_code == Some(0) {
        return ToolResult::ok(name, content, duration_ms);
    }
    let error = ToolError::ExecutionFailed {
        exit_code: result.exit_code,
        signal: result.signal,
        message: format!("{name} failed"),
    };
    ToolResult::err(name, content, error, duration_ms)
}

// --- Hand-rolled argument validation (RFC-0006 §5.5) -------------------------
//
// No schema crate in MVP. These helpers enforce the size caps before the grant
// check so model input never relies on the broker argv caps as first defence.

/// Validate the size cap and require a JSON object root with known keys only.
pub(crate) fn object_args<'a>(
    arguments: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, McpError> {
    // Count serialized size without buffering the whole payload.
    let mut counter = CapWriter {
        used: 0,
        cap: MAX_ARGUMENT_BYTES,
    };
    match serde_json::to_writer(&mut counter, arguments) {
        Ok(()) => {}
        Err(_) if counter.used > MAX_ARGUMENT_BYTES => {
            return Err(McpError::InvalidArguments("arguments too large".into()));
        }
        Err(e) => {
            return Err(McpError::InvalidArguments(format!(
                "arguments not serializable: {e}"
            )));
        }
    }
    let obj = arguments
        .as_object()
        .ok_or_else(|| McpError::InvalidArguments("type error: arguments must be object".into()))?;
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(McpError::InvalidArguments(format!(
                "additional property: {key}"
            )));
        }
    }
    Ok(obj)
}

/// Counts JSON bytes and errors once the host argument cap is exceeded.
struct CapWriter {
    used: usize,
    cap: usize,
}

impl Write for CapWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.used = self.used.saturating_add(buf.len());
        if self.used > self.cap {
            return Err(io::Error::other("arguments too large"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_string(field: &str, s: &str) -> Result<(), McpError> {
    if s.len() > MAX_ARG_STRING_BYTES {
        return Err(McpError::InvalidArguments(format!(
            "string too long: {field}"
        )));
    }
    if s.contains('\0') {
        return Err(McpError::InvalidArguments(format!(
            "NUL byte in string: {field}"
        )));
    }
    Ok(())
}

/// Required, non-empty string field.
pub(crate) fn required_string(obj: &Map<String, Value>, field: &str) -> Result<String, McpError> {
    let raw = obj
        .get(field)
        .ok_or_else(|| McpError::InvalidArguments(format!("missing property: {field}")))?;
    let s = raw
        .as_str()
        .ok_or_else(|| McpError::InvalidArguments(format!("type error: {field}")))?;
    if s.is_empty() {
        return Err(McpError::InvalidArguments(format!("empty string: {field}")));
    }
    check_string(field, s)?;
    Ok(s.to_string())
}

/// Optional string field. Absent or `null` is omission; `""` is rejected.
pub(crate) fn optional_string(
    obj: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, McpError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Err(McpError::InvalidArguments(format!("empty string: {field}")));
            }
            check_string(field, s)?;
            Ok(Some(s.clone()))
        }
        Some(_) => Err(McpError::InvalidArguments(format!("type error: {field}"))),
    }
}

/// Optional boolean field with a default.
pub(crate) fn optional_bool(
    obj: &Map<String, Value>,
    field: &str,
    default: bool,
) -> Result<bool, McpError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(McpError::InvalidArguments(format!("type error: {field}"))),
    }
}

/// Optional array of non-empty strings, capped at `max_len` entries.
pub(crate) fn optional_string_array(
    obj: &Map<String, Value>,
    field: &str,
    max_len: usize,
) -> Result<Vec<String>, McpError> {
    let items = match obj.get(field) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(McpError::InvalidArguments(format!("type error: {field}"))),
    };
    if items.len() > max_len {
        return Err(McpError::InvalidArguments(format!(
            "too many entries: {field} exceeds {max_len}"
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = item
            .as_str()
            .ok_or_else(|| McpError::InvalidArguments(format!("type error: {field}")))?;
        if s.is_empty() {
            return Err(McpError::InvalidArguments(format!("empty string: {field}")));
        }
        check_string(field, s)?;
        out.push(s.to_string());
    }
    Ok(out)
}

/// Optional integer field constrained to `min..=max`.
pub(crate) fn optional_integer(
    obj: &Map<String, Value>,
    field: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, McpError> {
    let raw = match obj.get(field) {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let n = raw
        .as_u64()
        .ok_or_else(|| McpError::InvalidArguments(format!("type error: {field}")))?;
    if n < min || n > max {
        return Err(McpError::InvalidArguments(format!(
            "out of range: {field} must be {min}..={max}"
        )));
    }
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_names_and_tags() {
        let names: Vec<String> = BuiltinToolId::ALL
            .iter()
            .map(|id| id.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["cargo_check", "cargo_test", "fs_read", "apply_patch"]
        );
        assert_eq!(BuiltinToolId::CargoCheck.tags(), &["sel.compiler"]);
        assert_eq!(BuiltinToolId::CargoTest.tags(), &["sel.test"]);
        assert_eq!(BuiltinToolId::FsRead.tags(), &["sel.fs"]);
        assert_eq!(BuiltinToolId::ApplyPatch.tags(), &["sel.edit"]);
    }

    #[test]
    fn arguments_too_large() {
        let big = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        let args = json!({ "workspace_root": big });
        assert!(matches!(
            object_args(&args, &["workspace_root"]),
            Err(McpError::InvalidArguments(ref m)) if m == "arguments too large"
        ));
    }

    #[test]
    fn unknown_keys_rejected() {
        let args = json!({ "path": "a", "nope": 1 });
        assert!(matches!(
            object_args(&args, &["path"]),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("additional property: nope")
        ));
    }

    #[test]
    fn non_object_root_rejected() {
        assert!(matches!(
            object_args(&json!([1, 2]), &[]),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("type error:")
        ));
    }

    #[test]
    fn optional_string_rejects_empty_but_allows_null() {
        let obj = json!({ "package": null });
        let obj = obj.as_object().unwrap();
        assert_eq!(optional_string(obj, "package").unwrap(), None);

        let empty = json!({ "package": "" });
        assert!(matches!(
            optional_string(empty.as_object().unwrap(), "package"),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: package"
        ));
    }

    #[test]
    fn string_array_caps_and_rejects_empty_entries() {
        let over = json!({ "features": vec!["f"; MAX_FEATURES + 1] });
        assert!(matches!(
            optional_string_array(over.as_object().unwrap(), "features", MAX_FEATURES),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("too many entries")
        ));

        let empty_entry = json!({ "features": ["a", ""] });
        assert!(matches!(
            optional_string_array(empty_entry.as_object().unwrap(), "features", MAX_FEATURES),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: features"
        ));
    }

    #[test]
    fn integer_range_enforced() {
        let obj = json!({ "jobs": 0 });
        assert!(matches!(
            optional_integer(obj.as_object().unwrap(), "jobs", 1, u64::from(u32::MAX)),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("out of range")
        ));
        let obj = json!({ "jobs": "2" });
        assert!(matches!(
            optional_integer(obj.as_object().unwrap(), "jobs", 1, 4),
            Err(McpError::InvalidArguments(ref m)) if m == "type error: jobs"
        ));
    }

    #[test]
    fn nul_and_overlong_strings_rejected() {
        let nul = json!({ "path": "a\0b" });
        assert!(matches!(
            required_string(nul.as_object().unwrap(), "path"),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("NUL byte")
        ));
        let long = json!({ "path": "x".repeat(MAX_ARG_STRING_BYTES + 1) });
        assert!(matches!(
            required_string(long.as_object().unwrap(), "path"),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("string too long")
        ));
    }
}
