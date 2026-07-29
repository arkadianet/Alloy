//! `McpToolchainRunner` — the production [`ToolchainRunner`] (RFC-0014 §3.3).
//!
//! Lives beside `McpVerifyCompileAdapter` and is built from the same two
//! collaborators (`Arc<dyn ToolCaller>` plus a permission source). Rule LB9:
//! there is exactly one cargo argv construction path — `build_tool_call` in
//! `adapters::verify` — and both the verify adapters and this runner go
//! through it. No process is ever spawned here (SC1/DN2).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use super::{scope_package, selector_args, LangError, RustToolchain, Scope, TestSelector};
use crate::adapters::verify::{build_tool_call, cargo_stdout_utf8, classify_cargo_result};
use crate::adapters::{NodeExecRef, ToolCaller, VerifyClass, VerifyPermissions};
use crate::types::tools::ToolCall;

/// MCP-backed [`super::ToolchainRunner`]: cargo is reached through the
/// host's `cargo_check`/`cargo_test` tools under a `PermissionToken`
/// (DN2, SC1).
pub struct McpToolchainRunner {
    tools: Arc<dyn ToolCaller>,
    perms: Arc<dyn VerifyPermissions>,
    /// Attribution stamped on out-of-band calls (DN7: this runner serves
    /// callers outside the scheduler's verify path).
    attribution: NodeExecRef,
    /// Toolchain identity provisioned by the composition root, when it
    /// probed one. See [`Self::with_probed_toolchain`].
    probed: Option<RustToolchain>,
}

impl McpToolchainRunner {
    /// Construct from the injected tool/permission seams plus the
    /// attribution to stamp on out-of-band cargo calls.
    #[must_use]
    pub fn new(
        tools: Arc<dyn ToolCaller>,
        perms: Arc<dyn VerifyPermissions>,
        attribution: NodeExecRef,
    ) -> Self {
        Self {
            tools,
            perms,
            attribution,
            probed: None,
        }
    }

    /// Provision the toolchain identity `probe` reports.
    ///
    /// The MCP host registers no `rustc -V`/`cargo -V` tool and RFC-0014
    /// adds none (§1.4: no MCP-host change), so the version probe is
    /// captured by the composition root — the unsandboxed host process —
    /// and handed in here. Without it, `probe` fails closed with
    /// [`LangError::Toolchain`].
    #[must_use]
    pub fn with_probed_toolchain(mut self, toolchain: RustToolchain) -> Self {
        self.probed = Some(toolchain);
        self
    }

    async fn run(
        &self,
        class: VerifyClass,
        root: &Path,
        package: Option<&str>,
        test_name_filter: Option<&str>,
    ) -> Result<(bool, String), LangError> {
        let token = self
            .perms
            .token_for(&self.attribution, class)
            .await
            .map_err(|e| LangError::Toolchain(e.to_string()))?;
        let (name, arguments) = build_tool_call(class, root, package, test_name_filter);
        let call = ToolCall::new(name, arguments).with_attribution(
            Some(self.attribution.session_id),
            Some(self.attribution.run_id),
            Some(self.attribution.node_id),
        );
        let result = self
            .tools
            .call(call, token)
            .await
            .map_err(|e| LangError::Toolchain(e.to_string()))?;
        // DN6: a run that compiles/tests nothing successfully is a soft
        // outcome carrying output, not an error. Everything the verify
        // classification treats as a genuine failure becomes `Toolchain`.
        let ok = match classify_cargo_result(&result) {
            Ok(outcome) => outcome.is_ok(),
            Err(e) => return Err(LangError::Toolchain(e.to_string())),
        };
        Ok((ok, cargo_stdout_utf8(&result.content).to_string()))
    }
}

impl std::fmt::Debug for McpToolchainRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolchainRunner")
            .field("probed", &self.probed)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl super::ToolchainRunner for McpToolchainRunner {
    async fn check_json(&self, root: &Path, scope: &Scope) -> Result<String, LangError> {
        let (package, degraded) = scope_package(scope);
        if degraded {
            // DN3: note the File → Workspace degradation; counts and paths
            // only, never file contents (LO4).
            tracing::debug!(?scope, "diagnostics scope degraded to workspace (DN3)");
        }
        let (_ok, stdout) = self
            .run(VerifyClass::Compile, root, package.as_deref(), None)
            .await?;
        Ok(stdout)
    }

    async fn test(&self, root: &Path, sel: &TestSelector) -> Result<(bool, String), LangError> {
        let (package, filter) = selector_args(sel);
        self.run(
            VerifyClass::Test,
            root,
            package.as_deref(),
            filter.as_deref(),
        )
        .await
    }

    async fn probe(&self) -> Result<RustToolchain, LangError> {
        // Fail closed rather than guess: toolchain identity is either the
        // one the composition root probed or unavailable (TC1).
        self.probed.clone().ok_or_else(|| {
            LangError::Toolchain(
                "no toolchain probe provisioned: the MCP host registers no version tool".into(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::super::ToolchainRunner;
    use super::*;
    use crate::adapters::ToolCallerError;
    use crate::error::AdapterError;
    use crate::types::ids::{DagId, NodeId, ProfileId, RunId, SessionId};
    use crate::types::permission::PermissionToken;
    use crate::types::tools::{ToolError, ToolName, ToolResult};

    struct StaticToolCaller {
        outcomes: Mutex<std::collections::VecDeque<Result<ToolResult, ToolCallerError>>>,
        calls: Mutex<Vec<ToolCall>>,
    }
    impl StaticToolCaller {
        fn new(outcomes: Vec<Result<ToolResult, ToolCallerError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes.into()),
                calls: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl ToolCaller for StaticToolCaller {
        async fn call(
            &self,
            call: ToolCall,
            _perms: PermissionToken,
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
        ) -> Result<PermissionToken, AdapterError> {
            Ok(PermissionToken {
                profile: ProfileId::new("default").unwrap(),
                grants: vec![],
                expires: None,
                run_id: ctx.run_id,
            })
        }
    }

    fn attribution() -> NodeExecRef {
        NodeExecRef {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            dag_id: DagId::new(),
            node_id: NodeId::new(),
            workspace_root: std::path::PathBuf::from("/ws"),
            attempt: 0, // out-of-band: no dispatch attempt
        }
    }

    fn check_ok(stdout: &str) -> ToolResult {
        ToolResult::ok(
            ToolName::new("cargo_check").unwrap(),
            json!({"exit_code": 0, "signal": null, "stdout_utf8": stdout,
                   "stdout_truncated": false}),
            1,
        )
    }

    fn runner(tools: Arc<StaticToolCaller>) -> McpToolchainRunner {
        McpToolchainRunner::new(tools, Arc::new(AlwaysGrantPerms), attribution())
    }

    // LB9: the runner goes through the shared argv path — same tool name and
    // shape as the verify adapter, plus the scope's package argument.
    #[tokio::test]
    async fn check_json_uses_the_shared_argv_path_with_scope_package() {
        let tools = StaticToolCaller::new(vec![Ok(check_ok("{}"))]);
        let r = runner(Arc::clone(&tools));
        let scope = Scope::Crate(crate::types::ids::CrateId::new("toy-core").unwrap());
        let out = r.check_json(Path::new("/ws"), &scope).await.unwrap();
        assert_eq!(out, "{}");
        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].name.as_str(), "cargo_check");
        assert_eq!(calls[0].arguments["workspace_root"], "/ws");
        assert_eq!(calls[0].arguments["message_format"], "json");
        assert_eq!(calls[0].arguments["package"], "toy-core");
        assert_eq!(calls[0].arguments.as_object().unwrap().len(), 3);
    }

    // DN6: a soft compile failure still yields stdout, not an error.
    #[tokio::test]
    async fn soft_fail_returns_stdout_not_an_error() {
        let result = ToolResult::err(
            ToolName::new("cargo_check").unwrap(),
            json!({"exit_code": 101, "signal": null, "stdout_utf8": "diag",
                   "stdout_truncated": false}),
            ToolError::ExecutionFailed {
                exit_code: Some(101),
                signal: None,
                message: "cargo failed".into(),
            },
            1,
        );
        let tools = StaticToolCaller::new(vec![Ok(result)]);
        let out = runner(tools)
            .check_json(Path::new("/ws"), &Scope::Workspace)
            .await
            .unwrap();
        assert_eq!(out, "diag");
    }

    // DN6: a genuine tool failure is `LangError::Toolchain`.
    #[tokio::test]
    async fn tool_caller_failure_is_toolchain_error() {
        let tools = StaticToolCaller::new(vec![Err(ToolCallerError::ShuttingDown)]);
        let err = runner(tools)
            .check_json(Path::new("/ws"), &Scope::Workspace)
            .await
            .unwrap_err();
        assert!(matches!(err, LangError::Toolchain(_)));
    }

    #[tokio::test]
    async fn test_maps_selector_and_reports_exit_ok() {
        let result = ToolResult::ok(
            ToolName::new("cargo_test").unwrap(),
            json!({"exit_code": 0, "signal": null, "stdout_utf8": "summary",
                   "stdout_truncated": false}),
            1,
        );
        let tools = StaticToolCaller::new(vec![Ok(result)]);
        let r = runner(Arc::clone(&tools));
        let (ok, out) = r
            .test(Path::new("/ws"), &TestSelector::Filter("io::reads".into()))
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(out, "summary");
        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].name.as_str(), "cargo_test");
        assert_eq!(calls[0].arguments["test_name_filter"], "io::reads");
        assert!(calls[0].arguments.get("message_format").is_none());
    }

    // TC1: probe reports the provisioned identity or fails closed.
    #[tokio::test]
    async fn probe_reports_provisioned_toolchain_or_fails_closed() {
        let tools = StaticToolCaller::new(vec![]);
        let bare = runner(Arc::clone(&tools));
        assert!(matches!(
            bare.probe().await.unwrap_err(),
            LangError::Toolchain(_)
        ));

        let toolchain = RustToolchain {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1".into(),
            cargo_version: "cargo 1.97.1".into(),
            host_triple: None,
        };
        let provisioned = runner(tools).with_probed_toolchain(toolchain.clone());
        assert_eq!(provisioned.probe().await.unwrap(), toolchain);
    }
}
