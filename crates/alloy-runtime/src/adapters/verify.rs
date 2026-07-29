//! MCP-backed verify adapters (RFC-0010 §3.6, §5.13).
//!
//! `McpVerifyCompileAdapter`/`McpVerifyTestAdapter` hold an injected
//! `Arc<dyn ToolCaller>` (never `ToolHandle` — rule M5) and turn one
//! `cargo_check`/`cargo_test` call into a [`Verdict`] or an
//! `AdapterError`, per §5.13.1's tool-call construction and §5.13.2's
//! total exit-code classification. Pass/fail/no-answer is decided by the
//! shared [`super::cargo_exit_verdict`] (research §7.11 items 5/6), the
//! same function `alloy-eval` uses — the two can no longer disagree.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::diagnostics::parse_rustc_diagnostics;
use super::perms::{VerifyClass, VerifyPermissions};
use super::tool_caller::{ToolCaller, ToolCallerError};
use super::{cargo_exit_verdict, NodeExecContext, Verdict, VerdictOutcome, Verifier};
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
impl Verifier for McpVerifyCompileAdapter {
    async fn verify(&self, ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
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
impl Verifier for McpVerifyTestAdapter {
    async fn verify(&self, ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
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
) -> Result<Verdict, AdapterError> {
    let token = perms.token_for(&ctx.meta, class).await?;
    let (name, arguments) = build_tool_call(class, &ctx.meta.workspace_root);
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
    let ran = classify_cargo_result(&result)?;

    // RL1-RL5: raw log MUST be put for every ran-to-a-verdict outcome, never
    // on a genuine error path above (those return before this point).
    let raw_artifact = put_raw_log(artifacts, ctx, &result).await?;

    let diagnostics = match class {
        VerifyClass::Compile => parse_rustc_diagnostics(cargo_stdout_utf8(&result.content)), // DG1-DG8
        VerifyClass::Test => vec![], // DG7: cargo_test output is not rustc JSON.
    };

    let outcome = match ran {
        // §7.11 items 5/6: the shared decision. Diagnostics win over a
        // clean exit; non-101 exits and signals are no-answer, not Fail.
        CargoRan::Exited(code) => cargo_exit_verdict(
            Some(code),
            diagnostics
                .iter()
                .any(|d| d.level == crate::types::diagnostic::DiagnosticLevel::Error),
        ),
        CargoRan::Inconclusive(reason) => VerdictOutcome::Inconclusive { reason },
    };
    Ok(Verdict {
        outcome,
        diagnostics,
        raw_artifact: Some(raw_artifact),
    })
}

/// §5.13.1: tool call construction. `workspace_root` MUST come from the
/// session row (via `ctx.meta`), never the node payload or environment (V1).
fn build_tool_call(class: VerifyClass, workspace_root: &Path) -> (ToolName, Value) {
    let workspace_root = workspace_root.display().to_string();
    match class {
        VerifyClass::Compile => (
            ToolName::new("cargo_check").expect("cargo_check is a valid tool name"),
            serde_json::json!({
                "workspace_root": workspace_root,
                "message_format": "json", // V2
            }),
        ),
        VerifyClass::Test => (
            ToolName::new("cargo_test").expect("cargo_test is a valid tool name"),
            serde_json::json!({ "workspace_root": workspace_root }),
        ),
    }
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
///
/// This stage only decides "did cargo produce an answer worth judging";
/// pass/fail itself belongs to [`cargo_exit_verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum CargoRan {
    /// Cargo ran to completion with this exit code.
    Exited(i64),
    /// Cargo ran but its answer is unusable (signal, truncated output, no
    /// exit code) — surfaces as `VerdictOutcome::Inconclusive` (§7.11
    /// item 6), never as an agent failure.
    Inconclusive(String),
}

/// §5.13.2 VC1-VC5: total classification of a `ToolCaller::call` success.
/// `content.exit_code` is authoritative when present (per the RFC table's
/// own column framing); `is_error()`/`error()` select which branch applies.
fn classify_cargo_result(result: &ToolResult) -> Result<CargoRan, AdapterError> {
    let exit_code = content_i64(&result.content, "exit_code");
    let signal = content_i64(&result.content, "signal");

    if !result.is_error() {
        return match exit_code {
            Some(0) => Ok(CargoRan::Exited(0)),
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
                // VC2: a signal is never a Compile/Test answer — OOM/kill.
                // §7.11 item 6: an Inconclusive verdict, not a failure label.
                return Ok(CargoRan::Inconclusive(format!(
                    "cargo killed by signal {sig}"
                )));
            }
            match exit_code {
                Some(101) => {
                    if content_bool(&result.content, "stdout_truncated") {
                        // VC5: truncated stdout on 101 is unreliable.
                        Ok(CargoRan::Inconclusive(
                            "stdout truncated on exit 101".into(),
                        ))
                    } else {
                        Ok(CargoRan::Exited(101)) // VC1: the compile/test failure signal.
                    }
                }
                Some(n) => Ok(CargoRan::Exited(n)),
                None => Ok(CargoRan::Inconclusive("no exit code reported".into())),
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

fn cargo_stdout_utf8(content: &Value) -> &str {
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
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoRan::Exited(0));
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
    fn vc1_exit_101_no_signal_not_truncated_is_a_fail_verdict() {
        let r = failed_result(Some(101), None, false, exec_failed(Some(101), None));
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoRan::Exited(101));
        // 101 is Fail on its own — independent of the diagnostics rule.
        assert_eq!(cargo_exit_verdict(Some(101), false), VerdictOutcome::Fail);
    }

    #[test]
    fn vc5_exit_101_truncated_stdout_is_inconclusive() {
        // §7.11 item 6: unusable output is a no-answer verdict, never an
        // agent failure and never a hard adapter error.
        let r = failed_result(Some(101), None, true, exec_failed(Some(101), None));
        assert!(matches!(
            classify_cargo_result(&r).unwrap(),
            CargoRan::Inconclusive(reason) if reason.contains("truncated")
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
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoRan::Exited(0));
    }

    #[test]
    fn vc2_any_signal_is_inconclusive_never_compile_regardless_of_exit_code() {
        for exit_code in [None, Some(101), Some(9)] {
            let r = failed_result(exit_code, Some(9), false, exec_failed(exit_code, Some(9)));
            assert!(
                matches!(
                    classify_cargo_result(&r).unwrap(),
                    CargoRan::Inconclusive(ref reason) if reason.contains("signal 9")
                ),
                "exit_code={exit_code:?}"
            );
        }
    }

    #[test]
    fn exit_code_outside_0_101_is_inconclusive_verdict() {
        // §7.11 item 6: cargo exiting 2 (bad args, ICE, …) decided nothing
        // about the agent's patch — Inconclusive, not a failure label.
        let r = failed_result(Some(2), None, false, exec_failed(Some(2), None));
        assert_eq!(classify_cargo_result(&r).unwrap(), CargoRan::Exited(2));
        assert!(matches!(
            cargo_exit_verdict(Some(2), false),
            VerdictOutcome::Inconclusive { reason } if reason.contains("exited 2")
        ));
    }

    #[test]
    fn no_exit_code_no_signal_is_inconclusive() {
        let r = failed_result(None, None, false, exec_failed(None, None));
        assert!(matches!(
            classify_cargo_result(&r).unwrap(),
            CargoRan::Inconclusive(reason) if reason.contains("no exit code")
        ));
    }

    /// §7.11 item 5: the shared decision closes the old blind spot — exit 0
    /// with error-level diagnostics is Fail, exactly as `alloy-eval`'s
    /// `compile_clean` always said.
    #[test]
    fn exit_0_with_error_diagnostics_is_fail() {
        assert_eq!(cargo_exit_verdict(Some(0), true), VerdictOutcome::Fail);
        assert_eq!(cargo_exit_verdict(Some(0), false), VerdictOutcome::Pass);
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
        let (name, args) = build_tool_call(VerifyClass::Compile, Path::new("/ws"));
        assert_eq!(name.as_str(), "cargo_check");
        assert_eq!(args["workspace_root"], "/ws");
        assert_eq!(args["message_format"], "json");
        assert_eq!(args.as_object().unwrap().len(), 2); // V4: no extra keys
    }

    #[test]
    fn build_tool_call_test_shape() {
        let (name, args) = build_tool_call(VerifyClass::Test, Path::new("/ws"));
        assert_eq!(name.as_str(), "cargo_test");
        assert_eq!(args["workspace_root"], "/ws");
        assert_eq!(args.as_object().unwrap().len(), 1);
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
        let outcome = adapter.verify(&ctx).await.unwrap();
        assert!(outcome.passed());
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
        let outcome = adapter.verify(&exec_ctx()).await.unwrap();
        assert_eq!(outcome.outcome, VerdictOutcome::Fail);
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
        let outcome = adapter.verify(&exec_ctx()).await.unwrap();
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
        let err = adapter.verify(&exec_ctx()).await.unwrap_err();
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
