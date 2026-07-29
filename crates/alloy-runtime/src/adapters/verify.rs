//! MCP-backed verify adapters (RFC-0010 §3.6, §5.13).
//!
//! `McpVerifyCompileAdapter`/`McpVerifyTestAdapter` hold an injected
//! `Arc<dyn ToolCaller>` (never `ToolHandle` — rule M5) and turn one
//! `cargo_check`/`cargo_test` call into a `VerifyOutcome` or an
//! `AdapterError`, per §5.13.1's tool-call construction and §5.13.2's
//! total exit-code classification.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::diagnostics::parse_rustc_diagnostics;
use super::perms::{VerifyClass, VerifyPermissions};
use super::tool_caller::{ToolCaller, ToolCallerError};
use super::{NodeExecContext, VerifyCompileAdapter, VerifyOutcome, VerifyTestAdapter};
use crate::error::AdapterError;
use crate::storage::{ArtifactKind, ArtifactPut, ArtifactStore};
use crate::types::tools::{ToolCall, ToolError, ToolName, ToolResult};

/// Compile verification over an injected `ToolCaller`.
pub struct McpVerifyCompileAdapter {
    tools: Arc<dyn ToolCaller>,
    perms: Arc<dyn VerifyPermissions>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl McpVerifyCompileAdapter {
    /// Construct from the injected tool/permission/artifact seams.
    #[must_use]
    pub fn new(
        tools: Arc<dyn ToolCaller>,
        perms: Arc<dyn VerifyPermissions>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            tools,
            perms,
            artifacts,
        }
    }
}

#[async_trait]
impl VerifyCompileAdapter for McpVerifyCompileAdapter {
    async fn check(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
        run_verify(
            &self.tools,
            &self.perms,
            &self.artifacts,
            ctx,
            VerifyClass::Compile,
        )
        .await
    }
}

/// Test verification over an injected `ToolCaller`. Identical shape to
/// [`McpVerifyCompileAdapter`]; only the tool (`cargo_test`) differs.
pub struct McpVerifyTestAdapter {
    tools: Arc<dyn ToolCaller>,
    perms: Arc<dyn VerifyPermissions>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl McpVerifyTestAdapter {
    /// Construct from the injected tool/permission/artifact seams.
    #[must_use]
    pub fn new(
        tools: Arc<dyn ToolCaller>,
        perms: Arc<dyn VerifyPermissions>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            tools,
            perms,
            artifacts,
        }
    }
}

#[async_trait]
impl VerifyTestAdapter for McpVerifyTestAdapter {
    async fn test(&self, ctx: &NodeExecContext) -> Result<VerifyOutcome, AdapterError> {
        run_verify(
            &self.tools,
            &self.perms,
            &self.artifacts,
            ctx,
            VerifyClass::Test,
        )
        .await
    }
}

/// Shared body for both adapters: mint a token, call the tool once (V5),
/// classify the result (§5.13.2), always persist the raw log (RL1-RL5) for
/// a durable outcome, and parse diagnostics (empty for `Test` — DG7).
async fn run_verify(
    tools: &Arc<dyn ToolCaller>,
    perms: &Arc<dyn VerifyPermissions>,
    artifacts: &Arc<dyn ArtifactStore>,
    ctx: &NodeExecContext,
    class: VerifyClass,
) -> Result<VerifyOutcome, AdapterError> {
    let token = perms.token_for(&ctx.meta, class).await?;
    let (name, arguments) = build_tool_call(class, &ctx.meta.workspace_root, None, None);
    let call = ToolCall::new(name, arguments)
        .with_attribution(
            Some(ctx.meta.session_id),
            Some(ctx.meta.run_id),
            Some(ctx.meta.node_id),
        ) // V3
        .with_call_id(format!("{}:{}", ctx.meta.node_id, ctx.meta.attempt)); // V3, NX2

    let result = tools
        .call(call, token)
        .await
        .map_err(map_tool_caller_error)?;
    let outcome = classify_cargo_result(&result)?;

    // RL1-RL5: raw log MUST be put for both ok:true and ok:false, never on
    // a genuine error path above (those return before this point).
    let raw_artifact = put_raw_log(artifacts, ctx, &result).await?;

    let diagnostics = match class {
        VerifyClass::Compile => parse_rustc_diagnostics(cargo_stdout_utf8(&result.content)), // DG1-DG8
        VerifyClass::Test => vec![], // DG7: cargo_test output is not rustc JSON.
    };

    Ok(match outcome {
        CargoOutcome::Ok => VerifyOutcome {
            ok: true,
            diagnostics,
            raw_artifact: Some(raw_artifact),
        },
        CargoOutcome::SoftFail => VerifyOutcome {
            ok: false,
            diagnostics,
            raw_artifact: Some(raw_artifact),
        },
    })
}

/// §5.13.1: tool call construction. `workspace_root` MUST come from the
/// session row (via `ctx.meta`), never the node payload or environment (V1).
///
/// RFC-0014 LB9: this is the **only** cargo argv construction path — the
/// verify adapters (no `package`, no filter) and `McpToolchainRunner`
/// (scope/selector arguments) both go through it. `test_name_filter` is
/// meaningful for `Test` only and ignored for `Compile`.
pub(crate) fn build_tool_call(
    class: VerifyClass,
    workspace_root: &Path,
    package: Option<&str>,
    test_name_filter: Option<&str>,
) -> (ToolName, Value) {
    let workspace_root = workspace_root.display().to_string();
    let (name, mut arguments) = match class {
        VerifyClass::Compile => (
            ToolName::new("cargo_check").expect("cargo_check is a valid tool name"),
            serde_json::json!({
                "workspace_root": workspace_root,
                "message_format": "json", // V2
            }),
        ),
        VerifyClass::Test => {
            let mut args = serde_json::json!({ "workspace_root": workspace_root });
            if let Some(filter) = test_name_filter {
                args["test_name_filter"] = Value::String(filter.to_string());
            }
            (
                ToolName::new("cargo_test").expect("cargo_test is a valid tool name"),
                args,
            )
        }
    };
    if let Some(package) = package {
        arguments["package"] = Value::String(package.to_string());
    }
    (name, arguments)
}

async fn put_raw_log(
    artifacts: &Arc<dyn ArtifactStore>,
    ctx: &NodeExecContext,
    result: &ToolResult,
) -> Result<crate::types::ids::ArtifactId, AdapterError> {
    let bytes = serde_json::to_vec(&result.content)
        .map_err(|e| AdapterError::Artifact(format!("encode raw log: {e}")))?;
    let mut labels = serde_json::Map::new();
    labels.insert("alloy.envelope".into(), Value::String("verify_raw".into()));
    labels.insert(
        "alloy.dag_id".into(),
        Value::String(ctx.meta.dag_id.to_string()),
    );
    labels.insert(
        "alloy.node_id".into(),
        Value::String(ctx.meta.node_id.to_string()),
    );
    artifacts
        .put(ArtifactPut {
            bytes,
            kind: ArtifactKind::Log,
            content_type: Some("text/plain".into()),
            session_id: Some(ctx.meta.session_id),
            run_id: Some(ctx.meta.run_id),
            labels,
        })
        .await
        .map_err(|e| AdapterError::Artifact(e.to_string())) // RL3
}

/// Outcome of §5.13.2's classification, before diagnostics/raw-log handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoOutcome {
    /// Exit 0.
    Ok,
    /// Exit 101, no signal, not truncated — a normal compile/test failure
    /// (VC1: this is a soft outcome, not an error).
    SoftFail,
}

impl CargoOutcome {
    /// `true` for a clean exit.
    pub(crate) fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// §5.13.2 VC1-VC5: total classification of a `ToolCaller::call` success.
/// `content.exit_code` is authoritative when present (per the RFC table's
/// own column framing); `is_error()`/`error()` select which branch applies.
pub(crate) fn classify_cargo_result(result: &ToolResult) -> Result<CargoOutcome, AdapterError> {
    let exit_code = content_i64(&result.content, "exit_code");
    let signal = content_i64(&result.content, "signal");

    if !result.is_error() {
        return match exit_code {
            Some(0) => Ok(CargoOutcome::Ok),
            None => Err(AdapterError::Internal(
                "cargo result missing exit_code".into(),
            )),
            Some(n) => Err(AdapterError::Internal(format!(
                "cargo result invariant: ok with exit {n}"
            ))),
        };
    }

    // VC4: is_error()==true implies error().is_some() by ToolResult's own
    // deserialize/construction invariant; defend rather than panic.
    let Some(err) = result.error() else {
        return Err(AdapterError::Internal(
            "tool result is_error without an error".into(),
        ));
    };

    match err {
        ToolError::ExecutionFailed { .. } => {
            if let Some(sig) = signal {
                // VC2: a signal is never Compile/Test — OOM/kill class.
                return Err(AdapterError::ToolFailure(ToolError::Transient {
                    code: "cargo_signal".into(),
                    message: format!("signal {sig}"),
                }));
            }
            match exit_code {
                Some(101) => {
                    if content_bool(&result.content, "stdout_truncated") {
                        // VC5: truncated stdout on 101 is unreliable — retry.
                        Err(AdapterError::ToolFailure(ToolError::Transient {
                            code: "cargo_output_truncated".into(),
                            message: "stdout truncated on exit 101".into(),
                        }))
                    } else {
                        Ok(CargoOutcome::SoftFail) // VC1: the only soft-fail signal.
                    }
                }
                Some(n) => Err(AdapterError::ToolFailure(ToolError::Permanent {
                    code: format!("cargo_exit_{n}"),
                    message: format!("cargo exited {n}"),
                })),
                None => Err(AdapterError::ToolFailure(ToolError::Transient {
                    code: "cargo_no_exit".into(),
                    message: "no exit code reported".into(),
                })),
            }
        }
        ToolError::Transient { .. } | ToolError::Permanent { .. } => {
            Err(AdapterError::ToolFailure(err.clone()))
        }
        ToolError::InvalidArgs { message, .. } => Err(AdapterError::Internal(format!(
            "adapter built invalid arguments: {message}"
        ))),
    }
}

fn content_i64(content: &Value, key: &str) -> Option<i64> {
    content.get(key).and_then(Value::as_i64)
}

fn content_bool(content: &Value, key: &str) -> bool {
    content.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn cargo_stdout_utf8(content: &Value) -> &str {
    content
        .get("stdout_utf8")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// §3.5 second table: `ToolCallerError` -> `AdapterError`.
fn map_tool_caller_error(e: ToolCallerError) -> AdapterError {
    match e {
        ToolCallerError::PermissionDenied(m) => AdapterError::PermissionDenied(m),
        ToolCallerError::TokenExpired => {
            AdapterError::PermissionDenied("permission token expired".into())
        }
        ToolCallerError::InvalidToken(m) => {
            AdapterError::PermissionDenied(format!("invalid permission token: {m}"))
        }
        ToolCallerError::UnknownTool(n) => {
            AdapterError::Internal(format!("tool not registered: {n}"))
        }
        ToolCallerError::InvalidArguments(m) => {
            AdapterError::Internal(format!("adapter built invalid arguments: {m}"))
        }
        ToolCallerError::Unsupported(m) => {
            AdapterError::Internal(format!("unsupported tool path: {m}"))
        }
        ToolCallerError::ShuttingDown => AdapterError::ShuttingDown,
        ToolCallerError::Cancelled => AdapterError::Cancelled,
        ToolCallerError::Timeout => AdapterError::Timeout,
        ToolCallerError::Sandbox(m) => AdapterError::ToolFailure(ToolError::Permanent {
            code: "sandbox".into(),
            message: m,
        }),
        ToolCallerError::Internal(m) => AdapterError::Internal(m),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::adapters::NodeExecRef;
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::ids::{DagId, NodeId, RunId, SessionId};

    // ---- classify_cargo_result (VC1-VC5, table-tested) ----

    fn ok_result(exit_code: i64) -> ToolResult {
        let name = ToolName::new("cargo_check").unwrap();
        ToolResult::ok(
            name,
            serde_json::json!({"exit_code": exit_code, "signal": null, "stdout_truncated": false}),
            1,
        )
    }

    fn failed_result(
        exit_code: Option<i64>,
        signal: Option<i64>,
        stdout_truncated: bool,
        tool_error: ToolError,
    ) -> ToolResult {
        let name = ToolName::new("cargo_check").unwrap();
        ToolResult::err(
            name,
            serde_json::json!({
                "exit_code": exit_code,
                "signal": signal,
                "stdout_truncated": stdout_truncated,
            }),
            tool_error,
            1,
        )
    }

    fn exec_failed(exit_code: Option<i64>, signal: Option<i64>) -> ToolError {
        ToolError::ExecutionFailed {
            exit_code: exit_code.map(|n| n as i32),
            signal: signal.map(|n| n as i32),
            message: "cargo failed".into(),
        }
    }

    #[test]
    fn exit_0_ok_is_ok_outcome() {
        let r = ok_result(0);
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoOutcome::Ok);
    }

    #[test]
    fn ok_result_missing_exit_code_is_internal() {
        let name = ToolName::new("cargo_check").unwrap();
        let r = ToolResult::ok(name, serde_json::json!({}), 1);
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(err, AdapterError::Internal(m) if m.contains("missing exit_code")));
    }

    #[test]
    fn ok_result_nonzero_exit_is_internal_invariant() {
        let r = ok_result(5);
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(err, AdapterError::Internal(m) if m.contains("invariant")));
    }

    #[test]
    fn vc1_exit_101_no_signal_not_truncated_is_soft_fail() {
        let r = failed_result(Some(101), None, false, exec_failed(Some(101), None));
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoOutcome::SoftFail);
    }

    #[test]
    fn vc5_exit_101_truncated_stdout_is_transient() {
        let r = failed_result(Some(101), None, true, exec_failed(Some(101), None));
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(
            err,
            AdapterError::ToolFailure(ToolError::Transient { code, .. }) if code == "cargo_output_truncated"
        ));
    }

    #[test]
    fn vc5_truncated_stdout_on_exit_0_is_ignored() {
        let name = ToolName::new("cargo_check").unwrap();
        let r = ToolResult::ok(
            name,
            serde_json::json!({"exit_code": 0, "signal": null, "stdout_truncated": true}),
            1,
        );
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoOutcome::Ok);
    }

    #[test]
    fn vc2_any_signal_is_transient_never_compile_regardless_of_exit_code() {
        for exit_code in [None, Some(101), Some(9)] {
            let r = failed_result(exit_code, Some(9), false, exec_failed(exit_code, Some(9)));
            let err = classify_cargo_result(&r).unwrap_err();
            assert!(
                matches!(
                    err,
                    AdapterError::ToolFailure(ToolError::Transient { ref code, .. }) if code == "cargo_signal"
                ),
                "exit_code={exit_code:?}"
            );
        }
    }

    #[test]
    fn exit_code_outside_0_101_no_signal_is_permanent() {
        let r = failed_result(Some(2), None, false, exec_failed(Some(2), None));
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(
            err,
            AdapterError::ToolFailure(ToolError::Permanent { code, .. }) if code == "cargo_exit_2"
        ));
    }

    #[test]
    fn no_exit_code_no_signal_is_transient_no_exit() {
        let r = failed_result(None, None, false, exec_failed(None, None));
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(
            err,
            AdapterError::ToolFailure(ToolError::Transient { code, .. }) if code == "cargo_no_exit"
        ));
    }

    #[test]
    fn transient_tool_error_passes_through() {
        let r = failed_result(
            None,
            None,
            false,
            ToolError::Transient {
                code: "io".into(),
                message: "disk".into(),
            },
        );
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(
            err,
            AdapterError::ToolFailure(ToolError::Transient { code, .. }) if code == "io"
        ));
    }

    #[test]
    fn permanent_tool_error_passes_through() {
        let r = failed_result(
            None,
            None,
            false,
            ToolError::Permanent {
                code: "usage".into(),
                message: "bad flag".into(),
            },
        );
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(
            err,
            AdapterError::ToolFailure(ToolError::Permanent { code, .. }) if code == "usage"
        ));
    }

    #[test]
    fn invalid_args_tool_error_is_internal() {
        let r = failed_result(
            None,
            None,
            false,
            ToolError::InvalidArgs {
                message: "bad shape".into(),
            },
        );
        let err = classify_cargo_result(&r).unwrap_err();
        assert!(matches!(err, AdapterError::Internal(m) if m.contains("bad shape")));
    }

    // ---- map_tool_caller_error (§3.5 second table) ----

    #[test]
    fn map_tool_caller_error_covers_every_variant() {
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::PermissionDenied("x".into())),
            AdapterError::PermissionDenied(m) if m == "x"
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::TokenExpired),
            AdapterError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::InvalidToken("bad".into())),
            AdapterError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::UnknownTool("x".into())),
            AdapterError::Internal(_)
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::InvalidArguments("x".into())),
            AdapterError::Internal(_)
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::Unsupported("x".into())),
            AdapterError::Internal(_)
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::ShuttingDown),
            AdapterError::ShuttingDown
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::Cancelled),
            AdapterError::Cancelled
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::Timeout),
            AdapterError::Timeout
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::Sandbox("redacted".into())),
            AdapterError::ToolFailure(ToolError::Permanent { code, .. }) if code == "sandbox"
        ));
        assert!(matches!(
            map_tool_caller_error(ToolCallerError::Internal("x".into())),
            AdapterError::Internal(_)
        ));
    }

    // ---- build_tool_call (§5.13.1 V1-V4) ----

    #[test]
    fn build_tool_call_compile_shape() {
        let (name, args) = build_tool_call(VerifyClass::Compile, Path::new("/ws"), None, None);
        assert_eq!(name.as_str(), "cargo_check");
        assert_eq!(args["workspace_root"], "/ws");
        assert_eq!(args["message_format"], "json");
        assert_eq!(args.as_object().unwrap().len(), 2); // V4: no extra keys
    }

    #[test]
    fn build_tool_call_test_shape() {
        let (name, args) = build_tool_call(VerifyClass::Test, Path::new("/ws"), None, None);
        assert_eq!(name.as_str(), "cargo_test");
        assert_eq!(args["workspace_root"], "/ws");
        assert_eq!(args.as_object().unwrap().len(), 1);
    }

    // RFC-0014 LB9: the shared path carries scope/selector arguments for the
    // toolchain runner without changing the verify adapters' shape above.
    #[test]
    fn build_tool_call_carries_package_and_filter_for_the_lang_seam() {
        let (_, args) = build_tool_call(
            VerifyClass::Compile,
            Path::new("/ws"),
            Some("toy-core"),
            None,
        );
        assert_eq!(args["package"], "toy-core");
        assert_eq!(args.as_object().unwrap().len(), 3);
        let (_, args) = build_tool_call(
            VerifyClass::Test,
            Path::new("/ws"),
            Some("toy-core"),
            Some("io::reads"),
        );
        assert_eq!(args["package"], "toy-core");
        assert_eq!(args["test_name_filter"], "io::reads");
        assert_eq!(args.as_object().unwrap().len(), 3);
    }

    // ---- end-to-end adapter tests over a recording ToolCaller double ----

    struct StaticToolCaller {
        outcomes: StdMutex<std::collections::VecDeque<Result<ToolResult, ToolCallerError>>>,
        calls: StdMutex<Vec<ToolCall>>,
    }
    impl StaticToolCaller {
        fn new(outcomes: Vec<Result<ToolResult, ToolCallerError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: StdMutex::new(outcomes.into()),
                calls: StdMutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl ToolCaller for StaticToolCaller {
        async fn call(
            &self,
            call: ToolCall,
            _perms: crate::types::permission::PermissionToken,
        ) -> Result<ToolResult, ToolCallerError> {
            self.calls.lock().unwrap().push(call);
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ToolCallerError::Internal("exhausted".into())))
        }
    }

    struct AlwaysGrantPerms;
    #[async_trait]
    impl VerifyPermissions for AlwaysGrantPerms {
        async fn token_for(
            &self,
            ctx: &NodeExecRef,
            _class: VerifyClass,
        ) -> Result<crate::types::permission::PermissionToken, AdapterError> {
            Ok(crate::types::permission::PermissionToken {
                profile: crate::types::ids::ProfileId::new("default").unwrap(),
                grants: vec![],
                expires: None,
                run_id: ctx.run_id,
            })
        }
    }

    fn exec_ctx() -> NodeExecContext {
        NodeExecContext {
            meta: NodeExecRef {
                session_id: SessionId::new(),
                run_id: RunId::new(),
                dag_id: DagId::new(),
                node_id: NodeId::new(),
                workspace_root: std::path::PathBuf::from("/ws"),
                attempt: 1,
            },
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    async fn open_store() -> (tempfile::TempDir, AlloyStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        (dir, storage)
    }

    #[tokio::test]
    async fn check_success_puts_raw_log_and_parses_diagnostics() {
        let (_dir, storage) = open_store().await;
        let name = ToolName::new("cargo_check").unwrap();
        let stdout = compiler_message("warning", "unused variable");
        let tool_result = ToolResult::ok(
            name,
            serde_json::json!({
                "exit_code": 0, "signal": null,
                "stdout_utf8": stdout, "stderr_utf8": "",
                "stdout_truncated": false, "stderr_truncated": false,
            }),
            5,
        );
        let tools = StaticToolCaller::new(vec![Ok(tool_result)]);
        let adapter = McpVerifyCompileAdapter::new(
            Arc::clone(&tools) as Arc<dyn ToolCaller>,
            Arc::new(AlwaysGrantPerms),
            storage.artifacts(),
        );
        let ctx = exec_ctx();
        let outcome = adapter.check(&ctx).await.unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(
            outcome.diagnostics[0].level,
            crate::types::diagnostic::DiagnosticLevel::Warning
        );
        assert!(outcome.raw_artifact.is_some());

        // V3/NX2: attribution and call_id must reflect the dispatched node/attempt.
        {
            let calls = tools.calls.lock().unwrap();
            assert_eq!(calls[0].name.as_str(), "cargo_check");
            assert_eq!(calls[0].node, Some(ctx.meta.node_id));
            assert_eq!(calls[0].session, Some(ctx.meta.session_id));
            assert_eq!(calls[0].run, Some(ctx.meta.run_id));
            assert_eq!(
                calls[0].call_id.as_deref(),
                Some(format!("{}:{}", ctx.meta.node_id, ctx.meta.attempt).as_str())
            );
        }
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn check_soft_fail_returns_ok_false_with_diagnostics() {
        let (_dir, storage) = open_store().await;
        let name = ToolName::new("cargo_check").unwrap();
        let stdout = compiler_message("error", "mismatched types");
        let tool_result = ToolResult::err(
            name,
            serde_json::json!({
                "exit_code": 101, "signal": null,
                "stdout_utf8": stdout, "stderr_utf8": "",
                "stdout_truncated": false, "stderr_truncated": false,
            }),
            exec_failed(Some(101), None),
            5,
        );
        let tools = StaticToolCaller::new(vec![Ok(tool_result)]);
        let adapter = McpVerifyCompileAdapter::new(
            tools as Arc<dyn ToolCaller>,
            Arc::new(AlwaysGrantPerms),
            storage.artifacts(),
        );
        let outcome = adapter.check(&exec_ctx()).await.unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.diagnostics.len(), 1);
        assert!(
            outcome.raw_artifact.is_some(),
            "RL2: raw log put for ok:false too"
        );
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_adapter_never_produces_diagnostics_dg7() {
        let (_dir, storage) = open_store().await;
        let name = ToolName::new("cargo_test").unwrap();
        // Test output happens to look like a compiler message; DG7 forbids
        // parsing it as one anyway.
        let stdout = compiler_message("error", "should not be parsed");
        let tool_result = ToolResult::ok(
            name,
            serde_json::json!({
                "exit_code": 0, "signal": null,
                "stdout_utf8": stdout, "stderr_utf8": "",
                "stdout_truncated": false, "stderr_truncated": false,
            }),
            5,
        );
        let tools = StaticToolCaller::new(vec![Ok(tool_result)]);
        let adapter = McpVerifyTestAdapter::new(
            tools as Arc<dyn ToolCaller>,
            Arc::new(AlwaysGrantPerms),
            storage.artifacts(),
        );
        let outcome = adapter.test(&exec_ctx()).await.unwrap();
        assert!(outcome.diagnostics.is_empty());
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn tool_caller_error_never_reaches_artifact_store() {
        let (_dir, storage) = open_store().await;
        let tools = StaticToolCaller::new(vec![Err(ToolCallerError::ShuttingDown)]);
        let adapter = McpVerifyCompileAdapter::new(
            tools as Arc<dyn ToolCaller>,
            Arc::new(AlwaysGrantPerms),
            storage.artifacts(),
        );
        let err = adapter.check(&exec_ctx()).await.unwrap_err();
        assert!(matches!(err, AdapterError::ShuttingDown));
        storage.close().await.unwrap();
    }

    fn compiler_message(level: &str, message: &str) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "target": {"name": "demo"},
            "message": {
                "code": null,
                "level": level,
                "message": message,
                "spans": [],
                "children": [],
            }
        })
        .to_string()
    }
}
