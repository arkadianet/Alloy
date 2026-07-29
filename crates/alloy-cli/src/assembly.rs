//! RFC-0015 §6 — the composition root.
//!
//! One function per assembly depth, each returning one struct holding every
//! long-lived handle (CR1). Construction follows the §6.2 order: storage →
//! plane → graph → sandbox broker → edit engine → MCP host → adapters →
//! observability → context engine → capability registry → scheduler; the
//! caller arms signals (CR14) and then starts the runtime.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_index::{GraphOpenOptions, SqliteProjectGraph};
use alloy_runtime::{
    install_sqlite_event_sink, AlloyRuntime, AlloyStorage, AppliedEditSource, CapabilityExecutor,
    CapabilityRegistry, ContextEngine, DecisionLog, DefaultContextEngine, EventDecisionLog,
    EventLogEdits, FixRecordingVerifier, GraphViewHandle, LinearScheduler, LinearSchedulerDeps,
    McpVerifyCompileAdapter, McpVerifyTestAdapter, ModelProvider, OpenAiCompatibleProvider,
    OpenAiCompatibleSpec, ProcessCostMeterFactory, ProcessRunRouterProvider, ProjectGraph,
    RegistryCapabilityExecutor, RouterConfig, RuntimeConfig, RuntimeHandle, SchedConfig,
    SecretString, SessionGateHumanAdapter, SessionId, SessionPlane, SessionVerifyPermissions,
    SessionWorkerPermissions, TemplatePlanService, ToolCaller, ToolName, ToolSelector, Verifier,
    WorkerConfig, WorkerDeps,
};
use alloy_tools::mcp::{McpPlatform, ToolHandle, ToolHandleToolCaller};
use alloy_tools::{
    load_sandbox_profile, trusted_exec_path, EditEnginePatchBackend, GitEditEngine,
    GitEditEngineConfig, InProcessMcpHost, McpHostConfig, NativeSandboxBroker, NetworkPolicy,
    OperatorHomes, PatchApplyBackend, PathPolicy, SandboxBroker, StubPatchApplyBackend,
};

use crate::errx::{CliError, Exit};

/// Steps 1–3 + control plane: what every subcommand needs (CR11). Read-only
/// subcommands stop here — no broker probe, no MCP host, no scheduler.
pub struct ReadAssembly {
    /// The runtime host (phase `Configured`; caller starts it).
    pub rt: AlloyRuntime,
    /// Runtime handle.
    pub handle: RuntimeHandle,
    /// Durable storage with the SQLite event sink installed.
    pub storage: Arc<AlloyStorage>,
    /// Control plane.
    pub plane: SessionPlane,
    /// Resolved config (data_dir made absolute, CR5).
    pub cfg: RuntimeConfig,
}

/// Everything `run` / `resume` need (steps 1–13 minus `start`).
pub struct FullAssembly {
    /// The read-depth assembly this extends.
    pub base: ReadAssembly,
    /// Project graph (step 4).
    pub graph: Arc<SqliteProjectGraph>,
    /// Decision log (step 10).
    pub decisions: Arc<EventDecisionLog>,
    /// Per-run cost meters (step 10).
    pub cost_meters: Arc<ProcessCostMeterFactory>,
    /// Run-scoped router provider (CR20 — per-run routers, one provider).
    pub routers: Arc<ProcessRunRouterProvider>,
    /// Plan service (RFC-0009).
    pub plan: TemplatePlanService,
    /// The installed scheduler (kept for wiring assertions).
    pub scheduler: Arc<LinearScheduler>,
    /// PF10 — whether a `GitEditEngine` was assembled (false under readonly).
    pub edit_engine_assembled: bool,
}

/// Make a path absolute against the process CWD without touching the
/// filesystem (CR5 — merged check N2 rejects a relative `data_dir`).
pub fn absolutize(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Assemble steps 1–3 (+ the control plane) at phase `Configured`.
pub async fn assemble_read(mut cfg: RuntimeConfig) -> Result<ReadAssembly, CliError> {
    cfg.data_dir = absolutize(&cfg.data_dir);
    let mut rt = AlloyRuntime::new();
    rt.configure(cfg.clone())?;
    let handle = rt.handle();
    let storage = install_sqlite_event_sink(&handle, None).await?;
    // Step 9 before any adapter that captures it (§6.2 note).
    let plane = SessionPlane::new(handle.clone(), Arc::clone(&storage));
    Ok(ReadAssembly {
        rt,
        handle,
        storage,
        plane,
        cfg,
    })
}

/// Open the project graph (step 4) for an already-assembled base.
pub async fn open_graph(base: &ReadAssembly) -> Result<Arc<SqliteProjectGraph>, CliError> {
    let graph = SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(base.cfg.data_dir.clone()))
        .await
        .map_err(|e| CliError::new(Exit::Graph, format!("graph open: {e}")))?;
    Ok(Arc::new(graph))
}

/// CR6 — assert the two `[sandbox]` readings agree before broker
/// construction. Two parsers reading one file differently is an internal
/// error, not a warning.
fn cross_check_sandbox(
    cfg: &RuntimeConfig,
    parsed: &alloy_tools::SandboxProfile,
) -> Result<(), CliError> {
    let Some(echo) = &cfg.sandbox_echo else {
        return Err(CliError::new(
            Exit::Config,
            format!(
                "profile {} has no [sandbox] table; run subcommands need one",
                cfg.profile_path.display()
            ),
        ));
    };
    let echo_deny = echo.network == "deny";
    let parsed_deny = matches!(parsed.network, NetworkPolicy::Deny);
    if echo_deny != parsed_deny || echo.quarantine_deps != parsed.quarantine_deps {
        return Err(CliError::new(
            Exit::Internal,
            format!(
                "[sandbox] cross-check failed for {}: RuntimeConfig read network={} quarantine_deps={}, load_sandbox_profile read network_deny={} quarantine_deps={} (CR6)",
                cfg.profile_path.display(),
                echo.network,
                echo.quarantine_deps,
                parsed_deny,
                parsed.quarantine_deps
            ),
        ));
    }
    Ok(())
}

/// Build the single OpenAI-compatible provider from `router.toml` (§5.5
/// step 6). Fails closed naming the key variable and `example.env`; never
/// reads a dotenv file.
fn build_provider(
    config: &RouterConfig,
    env_hint: &Path,
) -> Result<Arc<dyn ModelProvider>, CliError> {
    let provider_config = config
        .providers
        .first()
        .ok_or_else(|| CliError::new(Exit::Config, "router.toml must declare one [[providers]]"))?;
    let api_key = std::env::var(&provider_config.api_key_env)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            CliError::new(
                Exit::Config,
                format!(
                    "environment variable {} is unset or empty (export it; see {})",
                    provider_config.api_key_env,
                    env_hint.display()
                ),
            )
        })?;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
        id: provider_config.id.clone(),
        base_url: provider_config.base_url.clone(),
        api_key: SecretString::new(api_key),
        connect_timeout: config.policy.connect_timeout,
        request_timeout: config.policy.request_timeout,
    })
    .map_err(|e| CliError::new(Exit::Config, format!("provider: {e}")))?;
    Ok(Arc::new(provider))
}

/// Assemble steps 4–12 on top of [`assemble_read`]'s base.
///
/// `readonly` applies PF10 structurally: no `GitEditEngine`, a refusing
/// patch backend, and no test-class or worker-write grants.
pub async fn assemble_full(
    base: ReadAssembly,
    workspace_root: &Path,
    readonly: bool,
) -> Result<FullAssembly, CliError> {
    let cfg = base.cfg.clone();
    let storage = Arc::clone(&base.storage);
    let handle = base.handle.clone();
    let plane = base.plane.clone();

    // Step 4 — project graph.
    let graph = open_graph(&base).await?;

    // §5.5 step 6 — the router must construct (config parses, the
    // `api_key_env` variable is set and non-empty) before the broker probe,
    // so a missing key is reported as EX_CONFIG before any sandbox check.
    let router_config = RouterConfig::load(&cfg.router_path).map_err(|e| {
        CliError::new(
            Exit::Config,
            format!(
                "router {}: {e} (copy router.toml.example)",
                cfg.router_path.display()
            ),
        )
    })?;
    let provider = build_provider(&router_config, &cfg.env_file_hint)?;

    // Step 5 — sandbox broker. PF5: the same profile path feeds both parsers.
    let sandbox_profile = load_sandbox_profile(&cfg.profile_path, workspace_root.to_path_buf())
        .map_err(|e| {
            CliError::new(
                Exit::Config,
                format!("sandbox profile {}: {e}", cfg.profile_path.display()),
            )
        })?;
    cross_check_sandbox(&cfg, &sandbox_profile)?;
    // CR10 — resolve operator homes once; broker and host share the value.
    let homes = OperatorHomes::resolve()
        .map_err(|e| CliError::new(Exit::Sandbox, format!("operator homes: {e}")))?;
    let broker = NativeSandboxBroker::with_operator_homes(sandbox_profile.clone(), homes.clone())
        .await
        .map_err(|e| {
            CliError::new(
                Exit::Sandbox,
                format!(
                    "sandbox check backend unavailable: {e} (consider check = \"container\" in {})",
                    cfg.profile_path.display()
                ),
            )
        })?;
    let broker: Arc<dyn SandboxBroker> = Arc::new(broker);

    // Step 6 — edit engine and patch backend. PF10: readonly assembles a
    // refusing stub instead — refusal is structural, not a handler check.
    let path_policy = PathPolicy::from_profile(&sandbox_profile, Vec::new())
        .map_err(|e| CliError::new(Exit::Sandbox, format!("path policy: {e}")))?;
    let (patch_backend, edit_engine_assembled): (Arc<dyn PatchApplyBackend>, bool) = if readonly {
        (Arc::new(StubPatchApplyBackend), false)
    } else {
        let engine = GitEditEngine::new(GitEditEngineConfig::new(
            Arc::clone(&broker),
            path_policy,
            trusted_exec_path(&homes),
            storage.artifacts() as _,
            storage.events() as _,
        ))
        .map_err(|e| CliError::new(Exit::Internal, format!("edit engine: {e}")))?;
        (
            Arc::new(EditEnginePatchBackend::new(
                Arc::new(engine) as Arc<dyn alloy_runtime::EditEngine>
            )),
            true,
        )
    };

    // Step 7 — MCP host: max_in_flight pinned to 1 (CR7), cancel a child of
    // the runtime token (CR8), no read-only roots beyond the jail (CR9).
    let host = Arc::new(
        InProcessMcpHost::new(
            Arc::clone(&broker),
            homes,
            Vec::new(),
            patch_backend,
            McpHostConfig::new()
                .with_max_in_flight(1)
                .with_cancel(handle.cancellation().child_token()),
        )
        .map_err(|e| CliError::new(Exit::Internal, format!("mcp host: {e}")))?,
    );

    // Step 8 — adapters (the gate adapter captures the step-9 plane built
    // in `assemble_read`).
    let verify_tools: Arc<dyn ToolCaller> = Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
        Arc::clone(&host) as Arc<dyn McpPlatform>,
        vec![
            ToolSelector::name(tool_name("cargo_check")),
            ToolSelector::name(tool_name("cargo_test")),
        ],
    )));
    let verify_perms = Arc::new(SessionVerifyPermissions::new(
        storage.sessions() as _,
        Some("check*".into()),
        if readonly { None } else { Some("test*".into()) },
    ));
    let verify_compile = Arc::new(McpVerifyCompileAdapter::new(
        Arc::clone(&verify_tools),
        Arc::clone(&verify_perms) as _,
        storage.artifacts() as _,
    ));
    let verify_test = Arc::new(McpVerifyTestAdapter::new(
        verify_tools,
        verify_perms as _,
        storage.artifacts() as _,
    ));
    // RFC-0011 IN1 (amendment A-0011-5): the verify path is the host's fix
    // ingest seam. The composition root hands it the graph; the CLI itself
    // never writes one (rule B5) and the scheduler only ever sees a
    // `Verifier`.
    let applied_edits: Arc<dyn AppliedEditSource> =
        Arc::new(EventLogEdits::new(storage.events() as _));
    let verify_compile: Arc<dyn Verifier> = Arc::new(FixRecordingVerifier::new(
        verify_compile as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&applied_edits),
    ));
    let verify_test: Arc<dyn Verifier> = Arc::new(FixRecordingVerifier::new(
        verify_test as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        applied_edits,
    ));
    let gate_human = Arc::new(SessionGateHumanAdapter::new(plane.clone()));

    // Step 10 — observability.
    let decisions = Arc::new(
        EventDecisionLog::from_handle(handle.clone(), Arc::clone(&storage))
            .map_err(|e| CliError::new(Exit::Internal, format!("decision log: {e}")))?,
    );
    let cost_meters = Arc::new(ProcessCostMeterFactory::new());

    // Context engine (RFC-0012, PF13) — profile-driven, between steps 4 and 11.
    let context: Arc<dyn ContextEngine> = Arc::new(DefaultContextEngine::new(
        cfg.context_profile.clone(),
        GraphViewHandle::new(Arc::clone(&graph) as Arc<dyn ProjectGraph>),
        storage.events() as _,
        storage.artifacts() as _,
        workspace_root.to_path_buf(),
    ));

    // Router provider (CR20) — the validated config + provider from the
    // §5.5 pass above; per-run routers are minted lazily against each run's
    // meter and released on run terminal (CR21).
    let routers = Arc::new(ProcessRunRouterProvider::new(
        router_config,
        provider,
        cfg.budget_policy.clone(),
        Some(Arc::clone(&decisions) as Arc<dyn DecisionLog>),
    ));

    // Step 11 — capability registry (RFC-0013), replacing the pre-merge
    // UnavailableCapabilityExecutor narrative of CR12.
    let worker_tools: Arc<dyn ToolCaller> = Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
        Arc::clone(&host) as Arc<dyn McpPlatform>,
        vec![
            ToolSelector::name(tool_name("fs_read")),
            ToolSelector::name(tool_name("apply_patch")),
        ],
    )));
    let worker_perms = Arc::new(SessionWorkerPermissions::new(
        storage.sessions() as _,
        Some("**".into()),
        if readonly { None } else { Some("**".into()) },
    ));
    let registry = CapabilityRegistry::mvp(WorkerDeps {
        routers: Arc::clone(&routers) as _,
        context,
        tools: worker_tools,
        perms: worker_perms as _,
        graph: GraphViewHandle::new(Arc::clone(&graph) as Arc<dyn ProjectGraph>),
        artifacts: storage.artifacts() as _,
        decisions: Arc::clone(&decisions) as _,
        sessions: storage.sessions() as _,
        config: WorkerConfig::default(),
    })
    .map_err(|e| CliError::new(Exit::Internal, format!("capability registry: {e}")))?;
    let capabilities: Arc<dyn CapabilityExecutor> =
        Arc::new(RegistryCapabilityExecutor::new(Arc::new(registry)));

    // Step 12 — scheduler over the same plane/runs Arc (CR2) and the runtime
    // cancel token (CR3), budget/timeout from the profile (CR4), absolute
    // data_dir (CR5).
    let sched = Arc::new(
        LinearScheduler::new(LinearSchedulerDeps {
            dags: storage.dags() as _,
            artifacts: storage.artifacts() as _,
            events: storage.events() as _,
            sessions: storage.sessions() as _,
            session_plane: plane.clone(),
            runs: plane.runs(),
            verify_compile: verify_compile as _,
            verify_test: verify_test as _,
            gate_human: gate_human as _,
            capabilities,
            decisions: Arc::clone(&decisions) as _,
            cost_meters: Arc::clone(&cost_meters) as _,
            runtime_cancel: handle.cancellation(),
            budget_policy: cfg.budget_policy.clone(),
            run_timeout: cfg.run_timeout,
            config: SchedConfig::new(cfg.data_dir.clone()),
        })
        .map_err(|e| CliError::new(Exit::State, format!("scheduler: {e}")))?,
    );
    handle.set_scheduler(Arc::clone(&sched) as _)?;

    let plan = TemplatePlanService::from_storage(&storage);

    Ok(FullAssembly {
        base,
        graph,
        decisions,
        cost_meters,
        routers,
        plan,
        scheduler: sched,
        edit_engine_assembled,
    })
}

fn tool_name(name: &str) -> ToolName {
    ToolName::new(name).expect("builtin tool names are valid")
}

/// CR16 — shutdown reverses construction: (1) cancel; (2) drain when
/// `Running`; (3) shutdown; (4) storage close; (5) graph close. Extends the
/// merged `graceful_shutdown` rather than replacing it.
pub async fn shutdown_all(
    rt: AlloyRuntime,
    storage: &AlloyStorage,
    graph: Option<&SqliteProjectGraph>,
    grace: std::time::Duration,
) -> Result<(), CliError> {
    rt.handle().cancellation().cancel();
    crate::graceful_shutdown(rt, grace).await?;
    if let Err(e) = storage.close().await {
        tracing::warn!(error = %e, "storage close failed during shutdown");
    }
    if let Some(graph) = graph {
        if let Err(e) = graph.close().await {
            tracing::warn!(error = %e, "graph close failed during shutdown");
        }
    }
    Ok(())
}

/// The `<data_dir>/cli/last_session` marker: how `events` / `index` find
/// "the most recent session in this workspace" without a session-listing
/// API (SEC5 permits data-dir subtrees).
pub fn write_last_session(data_dir: &Path, session: SessionId) {
    let dir = data_dir.join("cli");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("last_session"), session.to_string());
    }
}

/// Read the marker back; `None` when absent or unparseable.
#[must_use]
pub fn read_last_session(data_dir: &Path) -> Option<SessionId> {
    let raw = std::fs::read_to_string(data_dir.join("cli/last_session")).ok()?;
    SessionId::parse(raw.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::ConfigPaths;

    fn write_workspace(dir: &Path) {
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles/default.toml"),
            include_str!("../../../profiles/default.toml"),
        )
        .unwrap();
        let router = alloy_runtime::default_router_toml()
            .replace("ALLOY_API_KEY", "ALLOY_CLI_ASSEMBLY_TEST_KEY");
        std::fs::write(dir.join("router.toml"), router).unwrap();
        std::fs::write(dir.join("example.env"), "ALLOY_CLI_ASSEMBLY_TEST_KEY=\n").unwrap();
    }

    fn load_cfg(dir: &Path) -> RuntimeConfig {
        RuntimeConfig::load(ConfigPaths::for_workspace(dir.to_path_buf())).unwrap()
    }

    /// CR5 — `--workspace .` (a relative root) still yields an absolute
    /// `data_dir`, hence an absolute `SchedConfig.data_dir`.
    #[tokio::test]
    async fn sched_data_dir_is_absolute() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let mut cfg = load_cfg(dir.path());
        // Simulate a relative workspace resolution product.
        cfg.data_dir = PathBuf::from("./relative-alloy-data");
        let cwd_guard = dir.path().join("cwd-anchor");
        std::fs::create_dir_all(&cwd_guard).unwrap();
        let base = assemble_read(cfg).await.unwrap();
        assert!(base.cfg.data_dir.is_absolute());
        let storage = Arc::clone(&base.storage);
        shutdown_all(
            base.rt,
            &storage,
            None,
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        // Clean the relative dir the test created under the process CWD.
        let _ = std::fs::remove_dir_all("./relative-alloy-data");
    }

    /// CR6 — divergent `[sandbox]` readings are an internal error.
    #[tokio::test]
    async fn sandbox_table_cross_check() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let mut cfg = load_cfg(dir.path());
        let parsed =
            alloy_tools::SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
        // Agreeing readings pass.
        cross_check_sandbox(&cfg, &parsed).unwrap();
        // A divergent echo fails as internal, not a warning.
        cfg.sandbox_echo.as_mut().unwrap().quarantine_deps = false;
        // (This echo would already fail RuntimeConfig::load; the cross-check
        // still refuses it independently.)
        let err = cross_check_sandbox(&cfg, &parsed).unwrap_err();
        assert_eq!(err.exit.code(), Exit::Internal.code());
        // A missing echo is a config error naming the profile path.
        cfg.sandbox_echo = None;
        let err = cross_check_sandbox(&cfg, &parsed).unwrap_err();
        assert_eq!(err.exit.code(), Exit::Config.code());
    }

    /// §6.2 full assembly, landlock-gated: constructs in the documented
    /// order, installs the scheduler, and applies PF10 structurally.
    /// Skips (not fails) when Landlock or cargo are unavailable, unless
    /// ALLOY_REQUIRE_LANDLOCK=1.
    #[tokio::test]
    async fn full_assembly_constructs_and_readonly_omits_edit_engine() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        std::env::set_var("ALLOY_CLI_ASSEMBLY_TEST_KEY", "test-key-value");

        for readonly in [false, true] {
            // Fresh data dir per pass: the scheduler lock is per data_dir.
            let ws = tempfile::tempdir().unwrap();
            write_workspace(ws.path());
            let cfg = load_cfg(ws.path());
            let base = assemble_read(cfg).await.unwrap();
            match assemble_full(base, ws.path(), readonly).await {
                Ok(full) => {
                    // The scheduler was built and installed (CR1/step 12).
                    assert!(Arc::strong_count(&full.scheduler) >= 2);
                    // PF10 — readonly assembles no GitEditEngine.
                    assert_eq!(full.edit_engine_assembled, !readonly);
                    let storage = Arc::clone(&full.base.storage);
                    shutdown_all(
                        full.base.rt,
                        &storage,
                        Some(&full.graph),
                        std::time::Duration::from_millis(50),
                    )
                    .await
                    .unwrap();
                }
                Err(e) if e.exit.code() == Exit::Sandbox.code() => {
                    assert!(
                        std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_none(),
                        "ALLOY_REQUIRE_LANDLOCK=1 but sandbox unavailable: {e}"
                    );
                    eprintln!("skip: sandbox unavailable ({e})");
                    return;
                }
                Err(e) => panic!("assemble_full failed: {e}"),
            }
        }
    }
}
