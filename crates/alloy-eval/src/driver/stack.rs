//! Live ControlPlane stack driver (RFC-0016 §5.9, feature `stack-driver`).
//!
//! Ports the production assembly proven by
//! `alloy-tools/tests/scheduler_repair_e2e.rs`: Landlock jail, hermetic
//! `CARGO_HOME`, `GitEditEngine`, MCP `cargo_check` / `apply_patch`,
//! `SqliteProjectGraph` ingest (syn-deep) + diagnostic seed, `CapabilityRegistry`,
//! `TomlModelRouter` + [`ScriptedProvider`], `GenerationDriver`,
//! `TemplatePlanService` / `LlmPlanService` + [`CapabilityPlanProposer`]
//! (LLM-arm smoke), and [`GenerationSwitchCapabilities`] (inert gen1
//! analyze/edit so the real `cargo_check` soft-fails and harvests
//! diagnostics; real registry on gen2).
//!
//! Activated only when [`live_stack_requested`] is true (`ALLOY_EVAL_LIVE_STACK=1`
//! plus this feature). Control-plane repair/edit/planning turns load committed
//! worker JSON under `recordings/` (`repair_plan.json`, `edit_patch.json`,
//! `planning_proposal.json`). Fixture `*.post` remains the naive-arm oracle
//! (`full_file_replace`) and offline criteria reference — control MUST NOT
//! construct its patch by reading the golden.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_index::{GraphOpenOptions, SqliteProjectGraph};
use alloy_runtime::adapters::SessionGateHumanAdapter;
use alloy_runtime::context::AssembleRequest;
use alloy_runtime::runtime::AlloyRuntime;
use alloy_runtime::session::SessionPlane;
use alloy_runtime::storage::{
    install_sqlite_event_sink, AlloyStorage, DagStore, SessionRows, StorageOpenOptions,
};
use alloy_runtime::types::ids::{DagId, NodeId, ProfileId, RunId, SessionId};
use alloy_runtime::SessionProvenance;
use alloy_runtime::{
    compiler_fingerprint_digest, policy_hash_digest, seed_graph_diagnostics, tool_versions_digest,
    Approval, BudgetPolicy, CapabilityExecContext, CapabilityExecError, CapabilityExecutor,
    CapabilityId, CapabilityOutcome, CapabilityPlanProposer, CapabilityRegistry, ChatMessage,
    ChatRole, CompletionRequest, ContextEngine, ContextProfile, CostMeterFactory,
    DefaultContextEngine, EndpointId, GateHumanAdapter, GenerationDriver, GenerationDriverDeps,
    GenerationPolicy, Goal, GraphViewHandle, LinearScheduler, LinearSchedulerDeps, LlmPlanService,
    McpVerifyCompileAdapter, ModelEndpoint, ModelProvider, ModelResponse, ModelTier,
    NodeExecContext, NodeExecRef, NodeKind, NullContextEngine, PlanContext, PlanFingerprints,
    PlanService, PlannerConfig, PlannerMode, ProcessCostMeterFactory, ProcessRunRouterProvider,
    ProjectGraph, ProposerDeps, ProviderId, RecordingDecisionLog, RecordingModelProvider,
    RegistryCapabilityExecutor, ResponseFormat, RetentionPolicy, RouterConfig, RunControlState,
    RunError, RunGoalRecord, RunRow, RuntimeConfig, SchedConfig, Session, SessionVerifyPermissions,
    SessionWorkerPermissions, TemplatePlanService, Timestamp, ToolCaller, ToolChoice, ToolName,
    ToolSelector, ToolchainRecord, UnavailableVerifyTest, Usage, Verifier, WorkerConfig,
    WorkerDeps, EDIT_SYSTEM, PLANNING_SYSTEM, REPAIR_SYSTEM,
};
use alloy_tools::mcp::{
    InProcessMcpHost, McpHostConfig, McpPlatform, ToolHandle, ToolHandleToolCaller,
};
use alloy_tools::{
    trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
    GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBackend,
    SandboxBroker, SandboxExecRequest, SandboxProfile,
};
use tokio_util::sync::CancellationToken;

use crate::cost_claim::derive_eval_usd;
use crate::driver::stack_live_options::StackLiveOptions;
use crate::error::{bound_message, EvalError, ReportError};
use crate::fingerprint::RequestFingerprint;
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::manifest::{FixtureTurnId, SuccessCriterion};
use crate::report::{CriterionResult, FixtureOutcome, FixtureStatus};
use crate::scripted::{ScriptOutcome, ScriptedProvider};
use crate::trajectory::EvalTrajectoryRecord;

const GOAL_TEXT: &str = "fix the compile error";
/// Scheduler / runtime run timeout for live stack-driver (also `SchedConfig`).
const LIVE_RUN_TIMEOUT: Duration = Duration::from_secs(300);
/// Outer poll slack beyond [`LIVE_RUN_TIMEOUT`] so the scheduler timeout wins.
const LIVE_POLL_SLACK: Duration = Duration::from_secs(60);

/// Live model provider: keyed smoke vs FIFO weight-arm recording.
enum LiveModelProvider {
    /// Null-context smoke path (fingerprint-keyed).
    Scripted(Arc<ScriptedProvider>),
    /// Weight arms: FIFO so [`DefaultContextEngine`] packs need not match
    /// [`NullContextEngine`] fingerprints.
    Recording {
        provider: Arc<RecordingModelProvider>,
        /// Scripted responses pushed in FIFO order (RecordingModelProvider does
        /// not retain outcomes after `complete`).
        responses: Vec<ModelResponse>,
    },
}

impl LiveModelProvider {
    fn as_dyn(&self) -> Arc<dyn ModelProvider> {
        match self {
            Self::Scripted(p) => Arc::clone(p) as Arc<dyn ModelProvider>,
            Self::Recording { provider, .. } => Arc::clone(provider) as Arc<dyn ModelProvider>,
        }
    }

    fn scripts_exhausted(&self) -> bool {
        match self {
            Self::Scripted(p) => p.is_exhausted(),
            Self::Recording {
                provider,
                responses,
            } => provider.recorded().len() >= responses.len(),
        }
    }
}

/// True when the live stack path should run: feature compiled in and
/// `ALLOY_EVAL_LIVE_STACK` is exactly `1` or `true` (case-insensitive).
#[must_use]
pub(crate) fn live_stack_requested() -> bool {
    match std::env::var("ALLOY_EVAL_LIVE_STACK") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

/// Generation 1 stand-in: inert analyze/edit so real `cargo_check` harvests
/// diagnostics; generation ≥2 dispatches the real RFC-0013 registry.
struct GenerationSwitchCapabilities {
    real: Arc<dyn CapabilityExecutor>,
}

#[async_trait::async_trait]
impl CapabilityExecutor for GenerationSwitchCapabilities {
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        if ctx.input.generation >= 2 {
            return self.real.execute(ctx).await;
        }
        match ctx.kind {
            NodeKind::Analyze | NodeKind::Edit => Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({ "generation_one": "inert" }),
            }),
            other => Err(CapabilityExecError::Internal(format!(
                "generation 1 has no worker for {other:?}"
            ))),
        }
    }
}

/// Live ControlPlane path for a holdout (or train) fixture (template planner).
pub(crate) async fn run_live(
    fixture: &LoadedFixture,
    cancel: Option<CancellationToken>,
) -> FixtureRunOutput {
    run_live_with_options(fixture, cancel, StackLiveOptions::template()).await
}

/// Live ControlPlane path with an explicit [`PlannerMode`].
///
/// `PlannerMode::Template` uses [`TemplatePlanService`]. `PlannerMode::Llm`
/// wires [`LlmPlanService`] over production [`CapabilityPlanProposer`] +
/// PlanningWorker (model branch) driven by committed
/// `recordings/planning_proposal.json` (non-gating smoke — not RFC-0017
/// §12.4 flip evidence). Gen2 repair/edit turns load committed worker JSON;
/// replan reuses prior LLM source (GN10).
pub(crate) async fn run_live_with_mode(
    fixture: &LoadedFixture,
    cancel: Option<CancellationToken>,
    mode: PlannerMode,
) -> FixtureRunOutput {
    run_live_with_options(fixture, cancel, StackLiveOptions::template().planner(mode)).await
}

/// Live ControlPlane path with full [`StackLiveOptions`] (planner mode,
/// max_repair_generations ablation, optional context-profile weight arms).
pub(crate) async fn run_live_with_options(
    fixture: &LoadedFixture,
    cancel: Option<CancellationToken>,
    options: StackLiveOptions,
) -> FixtureRunOutput {
    let started = Instant::now();
    if cancelled(&cancel) {
        return cancelled_output(fixture, started);
    }

    match run_live_inner(fixture, &cancel, options).await {
        Ok(output) => output,
        Err(error) => error_output(fixture, started, error),
    }
}

/// Live naive baseline: apply golden `full_file_replace` then live `cargo_check`
/// without the control-plane DAG — fair thesis comparison under `stack-driver`.
pub(crate) async fn run_naive_live(
    fixture: &LoadedFixture,
    cancel: Option<CancellationToken>,
) -> FixtureRunOutput {
    let started = Instant::now();
    if cancelled(&cancel) {
        return cancelled_output(fixture, started);
    }

    match run_naive_live_inner(fixture, &cancel).await {
        Ok(output) => output,
        Err(error) => error_output(fixture, started, error),
    }
}

async fn run_live_inner(
    fixture: &LoadedFixture,
    cancel: &Option<CancellationToken>,
    options: StackLiveOptions,
) -> Result<FixtureRunOutput, EvalError> {
    let started = Instant::now();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fixture, cancel, options);
        return Err(sandbox_unavailable(
            "live stack-driver requires Linux/Landlock",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        let mode = options.planner;
        let max_repair_generations = options.max_repair_generations;
        landlock_or_error().await?;
        let Some(cargo_bin) = which_cargo() else {
            return Err(EvalError::Internal("cargo not on PATH".into()));
        };
        let real_homes = OperatorHomes::resolve()
            .map_err(|e| EvalError::Internal(bound_message(format!("operator homes: {e}"))))?;

        let workspace_src = fixture.root.join(&fixture.manifest.workspace.path);
        let work_dir = tempfile::tempdir().map_err(EvalError::Io)?;
        let workspace_root = work_dir.path().join("workspace");
        copy_dir_all(&workspace_src, &workspace_root)?;
        // Drop golden sibling so the jail mirrors a real crate tree.
        let _ = std::fs::remove_file(
            workspace_root.join(format!("{}.post", fixture.manifest.naive_target_path)),
        );
        let workspace_root = workspace_root.canonicalize().map_err(EvalError::Io)?;
        let jail = workspace_root.clone();

        let homes_root = tempfile::tempdir().map_err(EvalError::Io)?;
        let Some(homes) = hermetic_cargo_home(homes_root.path(), &real_homes, &cargo_bin) else {
            return Err(EvalError::Internal(
                "could not stage a hermetic CARGO_HOME".into(),
            ));
        };

        let mut profile = SandboxProfile::default_for_jail(jail.clone())
            .map_err(|e| EvalError::Internal(bound_message(format!("sandbox profile: {e}"))))?;
        profile.check_backend = SandboxBackend::Landlock;
        profile.exec_timeout = Duration::from_secs(240);
        let broker = NativeSandboxBroker::with_operator_homes(profile.clone(), homes.clone())
            .await
            .map_err(|e| {
                EvalError::Internal(bound_message(format!(
                    "landlock broker construct failed after Available probe: {e}"
                )))
            })?;
        let broker = Arc::new(broker);
        init_git_repo(&broker, &jail).await?;

        if cancelled(cancel) {
            return Ok(cancelled_output(fixture, started));
        }

        let pre_source =
            std::fs::read_to_string(workspace_root.join(&fixture.manifest.naive_target_path))
                .map_err(EvalError::Io)?;
        // Control-plane patches come from committed recordings/* JSON — never
        // from fixture.paths.golden / *.post (naive-arm oracle only).

        let runtime_dir = tempfile::tempdir().map_err(EvalError::Io)?;
        let mut rt = AlloyRuntime::new();
        rt.configure(RuntimeConfig {
            data_dir: runtime_dir.path().join("runtime"),
            data_dir_rule: "eval-stack-driver",
            profile_path: runtime_dir.path().join("profiles/default.toml"),
            router_path: runtime_dir.path().join("router.toml"),
            // Hint path only — never the operator secret env file (RFC-0016 §10.2).
            env_file_hint: runtime_dir.path().join("env_file_hint"),
            retain_full_prompts: false,
            retain_tool_bodies: false,
            run_timeout: LIVE_RUN_TIMEOUT,
            budget_policy: BudgetPolicy::default(),
            context_profile: options
                .context_profile
                .clone()
                .unwrap_or_else(ContextProfile::v2_defaults),
            capture: Default::default(),
            planner: PlannerConfig {
                mode,
                ..PlannerConfig::new()
            },
            profile_id: None,
            gates: Default::default(),
            sandbox_echo: None,
            gate_timeout: None,
            max_repair_generations,
        })
        .map_err(|e| EvalError::Internal(bound_message(format!("runtime configure: {e}"))))?;
        let handle = rt
            .start()
            .await
            .map_err(|e| EvalError::Internal(bound_message(format!("runtime start: {e}"))))?;
        let storage = install_sqlite_event_sink(
            &handle,
            Some(StorageOpenOptions::for_data_dir(
                runtime_dir.path().join("storage"),
            )),
        )
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("storage: {e}"))))?;
        // All post-start work runs in this block; storage+runtime close exactly
        // once afterward regardless of Ok/Err (cancelled outcomes included).
        let result = async {
            let plane = SessionPlane::new(handle.clone(), Arc::clone(&storage));

            let read_only_roots: Vec<PathBuf> = Vec::new();
            let path_policy = PathPolicy::from_profile(&profile, read_only_roots.clone())
                .map_err(|e| EvalError::Internal(bound_message(format!("path policy: {e}"))))?;
            let engine = Arc::new(
                GitEditEngine::new(GitEditEngineConfig::new(
                    Arc::clone(&broker) as Arc<dyn SandboxBroker>,
                    path_policy,
                    trusted_exec_path(&homes),
                    storage.artifacts(),
                    storage.events(),
                ))
                .map_err(|e| EvalError::Internal(bound_message(format!("edit engine: {e}"))))?,
            );
            let host = Arc::new(
                InProcessMcpHost::new(
                    Arc::clone(&broker) as Arc<dyn SandboxBroker>,
                    homes,
                    read_only_roots,
                    Arc::new(EditEnginePatchBackend::new(
                        engine as Arc<dyn alloy_runtime::EditEngine>,
                    )),
                    McpHostConfig::new(),
                )
                .map_err(|e| EvalError::Internal(bound_message(format!("mcp host: {e}"))))?,
            );

            let session_id = seed_session(&storage, workspace_root.clone()).await?;
            let dag_id = DagId::new();
            let run_id = seed_run(&storage, session_id, dag_id).await?;

            let sched_dir = runtime_dir.path().join("scheduler");
            let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
            let cost_meters = Arc::new(ProcessCostMeterFactory::new());

            let verify_tools: Arc<dyn ToolCaller> =
                Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
                    Arc::clone(&host) as Arc<dyn McpPlatform>,
                    vec![ToolSelector::name(ToolName::new("cargo_check").unwrap())],
                )));
            let verify_perms: Arc<dyn alloy_runtime::VerifyPermissions> = Arc::new(
                SessionVerifyPermissions::new(storage.sessions(), Some("check*".into()), None),
            );
            let verify_compile: Arc<dyn Verifier> = Arc::new(McpVerifyCompileAdapter::new(
                verify_tools,
                verify_perms,
                storage.artifacts(),
            ));

            let worker_tools: Arc<dyn ToolCaller> =
                Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
                    Arc::clone(&host) as Arc<dyn McpPlatform>,
                    vec![
                        ToolSelector::name(ToolName::new("fs_read").unwrap()),
                        ToolSelector::name(ToolName::new("apply_patch").unwrap()),
                    ],
                )));
            let router_config = RouterConfig::from_str("eval-stack", router_toml())
                .map_err(|e| EvalError::Internal(bound_message(format!("router config: {e}"))))?;
            let llm_planning = mode == PlannerMode::Llm;
            let use_default_context = options.context_profile.is_some();
            let provider = build_live_provider(
                fixture,
                &fixture.endpoint,
                llm_planning,
                use_default_context,
            )
            .await?;
            let routers = Arc::new(ProcessRunRouterProvider::new(
                router_config,
                provider.as_dyn(),
                BudgetPolicy::default(),
                Some(Arc::clone(&decisions) as _),
            ));
            let worker_config = WorkerConfig::default();
            // Production parity with CLI assembly: open + rebuild SqliteProjectGraph
            // so WorkerDeps / DefaultContextEngine can resolve Symbol/Callers when
            // AssembleRequest carries file pins or diagnostic seed paths. Edit turns
            // with empty seeds still honestly degrade as graph_empty (no seeds ≠
            // null handle). Committed-recording smoke still ignores PromptPack shape.
            let graph_store = Arc::new(
                SqliteProjectGraph::open(GraphOpenOptions::for_data_dir(
                    runtime_dir.path().join("graph"),
                ))
                .await
                .map_err(|e| EvalError::Internal(bound_message(format!("graph open: {e}"))))?,
            );
            let ingest = graph_store
                .rebuild_reported(&workspace_root)
                .await
                .map_err(|e| EvalError::Internal(bound_message(format!("graph rebuild: {e}"))))?;
            if ingest.items == 0 {
                return Err(EvalError::Internal(bound_message(format!(
                    "graph rebuild ingested 0 items under {}",
                    workspace_root.display()
                ))));
            }
            let graph_handle =
                GraphViewHandle::new(Arc::clone(&graph_store) as Arc<dyn ProjectGraph>);
            // CLI bootstrap_diagnostics parity: gen-1 WorkingSet/repair can read
            // GraphQuery::Diagnostics instead of assembling with empty seeds only.
            {
                let exec_ctx = NodeExecContext {
                    meta: NodeExecRef {
                        session_id,
                        run_id,
                        dag_id,
                        node_id: NodeId::new(),
                        workspace_root: workspace_root.clone(),
                        attempt: 1,
                    },
                    cancellation: cancel.clone().unwrap_or_default(),
                };
                match seed_graph_diagnostics(
                    verify_compile.as_ref(),
                    graph_store.as_ref(),
                    &exec_ctx,
                )
                .await
                {
                    Ok(report) => tracing::info!(
                        recorded = report.recorded,
                        errors = report.errors,
                        "stack-driver seeded diagnostics"
                    ),
                    Err(e) => tracing::warn!(error = %e, "stack-driver diagnostic seed skipped"),
                }
            }
            let context: Arc<dyn ContextEngine> = match &options.context_profile {
                Some(profile) => Arc::new(DefaultContextEngine::new(
                    profile.clone(),
                    graph_handle.clone(),
                    storage.events() as _,
                    storage.artifacts() as _,
                    workspace_root.clone(),
                )),
                None => Arc::new(NullContextEngine::with_goal(GOAL_TEXT)),
            };
            let deps = WorkerDeps {
                routers,
                context,
                tools: worker_tools,
                perms: Arc::new(SessionWorkerPermissions::new(
                    storage.sessions(),
                    Some("**".into()),
                    Some("**".into()),
                )),
                graph: graph_handle,
                artifacts: storage.artifacts(),
                decisions: Arc::clone(&decisions) as _,
                sessions: storage.sessions(),
                config: worker_config.clone(),
            };
            let registry = CapabilityRegistry::mvp_with(deps, llm_planning).map_err(|e| {
                EvalError::Internal(bound_message(format!("capability registry: {e}")))
            })?;
            let real_caps: Arc<dyn CapabilityExecutor> =
                Arc::new(RegistryCapabilityExecutor::new(Arc::new(registry)));
            let caps: Arc<dyn CapabilityExecutor> = Arc::new(GenerationSwitchCapabilities {
                real: Arc::clone(&real_caps),
            });

            let scheduler = Arc::new(build_scheduler(
                &storage,
                &plane,
                sched_dir,
                &caps,
                &verify_compile,
                &decisions,
                &cost_meters,
                cancel,
            )?);
            handle
                .set_scheduler(scheduler as _)
                .map_err(|e| EvalError::Internal(bound_message(format!("set scheduler: {e}"))))?;

            let runtime_cancel = cancel.clone().unwrap_or_default();
            // CapabilityPlanProposer uses the real registry executor — not
            // GenerationSwitchCapabilities (Plan is never a gen1 DAG node).
            let plans = build_plan_service(
                &storage,
                &decisions,
                mode,
                Arc::clone(&real_caps),
                workspace_root.clone(),
                runtime_cancel.clone(),
                Arc::clone(&cost_meters),
                worker_config.enable_review,
            );
            PlanService::plan(&*plans, plan_ctx(session_id, run_id, dag_id))
                .await
                .map_err(|e| EvalError::Internal(bound_message(format!("plan: {e}"))))?;

            let driver = Arc::new(GenerationDriver::new(GenerationDriverDeps {
                handle: handle.clone(),
                plans: Arc::clone(&plans),
                runs: plane.runs(),
                dags: storage.dags() as _,
                sessions: storage.sessions() as _,
                events: storage.events() as _,
                decisions: Arc::clone(&decisions) as _,
                cost_meters: Arc::clone(&cost_meters) as _,
                budget_policy: BudgetPolicy::default(),
                cancellation: runtime_cancel,
                fingerprints: fingerprints(),
                policy: GenerationPolicy {
                    max_repair_generations,
                },
            }));
            plane.set_executor(driver as _);

            let runs = plane.runs();
            let start_runs = Arc::clone(&runs);
            let run_task = tokio::spawn(async move { start_runs.start(run_id).await });

            // Auto-approve GateHuman when WaitingApproval so batch runs finish.
            // Outer deadline is LIVE_RUN_TIMEOUT + slack so SchedConfig::run_timeout
            // can surface first. After each approval, keep polling (cancel /
            // subsequent gates / finish) instead of unbounded-awaiting run_task.
            let deadline = tokio::time::Instant::now() + LIVE_RUN_TIMEOUT + LIVE_POLL_SLACK;
            let mut human_interventions = 0u32;
            loop {
                if cancelled(cancel) {
                    run_task.abort();
                    return Ok(cancelled_output(fixture, started));
                }
                if run_task.is_finished() {
                    // Ablation max_repair_generations=0: gen1 inert analyze/edit
                    // fails verify and the run ends without opening GateHuman.
                    run_task
                        .await
                        .map_err(|e| EvalError::Internal(bound_message(format!("run join: {e}"))))?
                        .map_err(|e| {
                            EvalError::Internal(bound_message(format!("run failed early: {e}")))
                        })?;
                    return assemble_live_control_output(
                        fixture,
                        started,
                        &storage,
                        run_id,
                        dag_id,
                        &workspace_root,
                        &pre_source,
                        &broker,
                        &jail,
                        &cost_meters,
                        &provider,
                        human_interventions,
                    )
                    .await;
                }
                let dag = storage
                    .dags()
                    .get(dag_id)
                    .await
                    .map_err(|e| EvalError::Internal(bound_message(format!("dag get: {e}"))))?
                    .ok_or_else(|| EvalError::Internal("dag missing during poll".into()))?;
                if dag.state == alloy_runtime::DagState::WaitingApproval {
                    let gate_id = dag
                        .nodes
                        .values()
                        .find(|n| n.kind == NodeKind::GateHuman)
                        .and_then(|n| n.approval.as_ref())
                        .map(|a| a.gate)
                        .ok_or_else(|| {
                            EvalError::Internal("gate node missing ApprovalSpec".into())
                        })?;
                    // The scheduler can mark `DagState::WaitingApproval` before
                    // `ApprovalRequested` is durable / before the run row flips
                    // (run_controller approve race, 2026-07-29). Retry that
                    // window instead of failing the fixture and shutting the
                    // store under a still-running GateHuman waiter.
                    match runs.approve(run_id, gate_id, Approval::Allow).await {
                        Ok(()) => {
                            human_interventions = human_interventions.saturating_add(1);
                        }
                        Err(e)
                            if matches!(
                                &e,
                                RunError::InvalidPhase(m) if m == "not waiting approval"
                            ) =>
                        {
                            tracing::debug!(
                                %run_id,
                                %gate_id,
                                error = %e,
                                "approve raced ahead of ApprovalRequested; retrying"
                            );
                        }
                        Err(e) => {
                            run_task.abort();
                            let _ = run_task.await;
                            return Err(EvalError::Internal(bound_message(format!(
                                "approve: {e}"
                            ))));
                        }
                    }
                    // Continue polling so cancellation, a later gate, or finish are
                    // observed under the same deadline (no unbounded join here).
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                if tokio::time::Instant::now() >= deadline {
                    run_task.abort();
                    let _ = run_task.await;
                    return Err(EvalError::Internal(
                        "live stack-driver: poll deadline exceeded (beyond scheduler run_timeout)"
                            .into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        .await;
        let _ = shutdown_runtime(rt, storage).await;
        // Keep tempdirs alive until shutdown completes.
        drop(homes_root);
        drop(work_dir);
        drop(runtime_dir);
        result
    }
}

async fn run_naive_live_inner(
    fixture: &LoadedFixture,
    cancel: &Option<CancellationToken>,
) -> Result<FixtureRunOutput, EvalError> {
    let started = Instant::now();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fixture, cancel);
        return Err(sandbox_unavailable(
            "live stack-driver naive path requires Linux/Landlock",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        landlock_or_error().await?;
        let Some(cargo_bin) = which_cargo() else {
            return Err(EvalError::Internal("cargo not on PATH".into()));
        };
        let real_homes = OperatorHomes::resolve()
            .map_err(|e| EvalError::Internal(bound_message(format!("operator homes: {e}"))))?;

        let workspace_src = fixture.root.join(&fixture.manifest.workspace.path);
        let work_dir = tempfile::tempdir().map_err(EvalError::Io)?;
        let workspace_root = work_dir.path().join("workspace");
        copy_dir_all(&workspace_src, &workspace_root)?;
        let _ = std::fs::remove_file(
            workspace_root.join(format!("{}.post", fixture.manifest.naive_target_path)),
        );
        let workspace_root = workspace_root.canonicalize().map_err(EvalError::Io)?;
        let jail = workspace_root.clone();

        let pre_source =
            std::fs::read_to_string(workspace_root.join(&fixture.manifest.naive_target_path))
                .map_err(EvalError::Io)?;
        let golden = std::fs::read_to_string(&fixture.paths.golden).map_err(EvalError::Io)?;
        std::fs::write(
            workspace_root.join(&fixture.manifest.naive_target_path),
            &golden,
        )
        .map_err(EvalError::Io)?;

        if cancelled(cancel) {
            return Ok(cancelled_output(fixture, started));
        }

        let homes_root = tempfile::tempdir().map_err(EvalError::Io)?;
        let Some(homes) = hermetic_cargo_home(homes_root.path(), &real_homes, &cargo_bin) else {
            return Err(EvalError::Internal(
                "could not stage a hermetic CARGO_HOME".into(),
            ));
        };

        let mut profile = SandboxProfile::default_for_jail(jail.clone())
            .map_err(|e| EvalError::Internal(bound_message(format!("sandbox profile: {e}"))))?;
        profile.check_backend = SandboxBackend::Landlock;
        profile.exec_timeout = Duration::from_secs(240);
        let broker = NativeSandboxBroker::with_operator_homes(profile, homes)
            .await
            .map_err(|e| {
                EvalError::Internal(bound_message(format!(
                    "landlock broker construct failed after Available probe: {e}"
                )))
            })?;
        let broker = Arc::new(broker);

        let compile_clean = live_compile_clean(&broker, &jail).await?;
        let unsafe_introduced = unsafe_introduced(&pre_source, &golden);
        // Naive installs no scripted provider keys under the live path.
        let scripts_exhausted = true;
        let criteria = live_criteria(fixture, compile_clean, unsafe_introduced, scripts_exhausted);
        let status = if criteria.iter().all(|c| c.passed) {
            FixtureStatus::Pass
        } else {
            FixtureStatus::Fail
        };
        let trajectories = naive_live_trajectories(fixture, status, Some(compile_clean));

        drop(homes_root);
        drop(work_dir);

        Ok(FixtureRunOutput {
            outcome: FixtureOutcome {
                fixture_id: fixture.manifest.id.clone(),
                set: fixture.manifest.set,
                status,
                criteria,
                wall_ms: elapsed_ms(started),
                model_calls: 0,
                tokens_in: None,
                tokens_out: None,
                cost_usd: None,
                retry_count: Some(0),
                human_interventions: Some(0),
                unsafe_introduced: Some(unsafe_introduced),
                compile_clean: Some(compile_clean),
                error: None,
            },
            trajectories,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn assemble_live_control_output(
    fixture: &LoadedFixture,
    started: Instant,
    storage: &AlloyStorage,
    run_id: RunId,
    dag_id: DagId,
    workspace_root: &Path,
    pre_source: &str,
    broker: &Arc<NativeSandboxBroker>,
    jail: &Path,
    cost_meters: &Arc<ProcessCostMeterFactory>,
    provider: &LiveModelProvider,
    human_interventions: u32,
) -> Result<FixtureRunOutput, EvalError> {
    let run_row = storage
        .sessions()
        .get_run(run_id)
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("get_run: {e}"))))?
        .ok_or_else(|| EvalError::Internal("run row missing".into()))?;
    let succeeded = run_row.state == "succeeded";
    let fixed_source =
        std::fs::read_to_string(workspace_root.join(&fixture.manifest.naive_target_path))
            .map_err(EvalError::Io)?;
    let compile_clean = succeeded && live_compile_clean(broker, jail).await?;
    let meter = cost_meters.meter_for(run_id).snapshot();
    let model_calls = meter.model_calls.min(u32::MAX as u64) as u32;
    let tokens_in = if meter.tokens_in > 0 {
        Some(meter.tokens_in)
    } else {
        None
    };
    let tokens_out = if meter.tokens_out > 0 {
        Some(meter.tokens_out)
    } else {
        None
    };
    let cost_usd = match (tokens_in, tokens_out) {
        (Some(input_tokens), Some(output_tokens)) => derive_eval_usd(
            &fixture.endpoint,
            &Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
            },
        ),
        _ => None,
    };
    let final_dag = storage
        .dags()
        .get(dag_id)
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("final dag: {e}"))))?;
    let retry_count = final_dag
        .as_ref()
        .map(|d| u32::try_from(d.generation.saturating_sub(1)).unwrap_or(u32::MAX));
    let scripts_exhausted = provider.scripts_exhausted();
    let unsafe_introduced = unsafe_introduced(pre_source, &fixed_source);
    let criteria = live_criteria(fixture, compile_clean, unsafe_introduced, scripts_exhausted);
    let status = if !succeeded {
        FixtureStatus::Fail
    } else if criteria.iter().all(|c| c.passed) {
        FixtureStatus::Pass
    } else {
        FixtureStatus::Fail
    };
    let trajectories = live_trajectories(fixture, provider, status, Some(compile_clean));
    Ok(FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status,
            criteria,
            wall_ms: elapsed_ms(started),
            model_calls,
            tokens_in,
            tokens_out,
            cost_usd,
            retry_count,
            human_interventions: Some(human_interventions),
            unsafe_introduced: Some(unsafe_introduced),
            compile_clean: Some(compile_clean),
            error: None,
        },
        trajectories,
    })
}

fn live_criteria(
    fixture: &LoadedFixture,
    compile_clean: bool,
    unsafe_introduced: bool,
    scripts_exhausted: bool,
) -> Vec<CriterionResult> {
    fixture
        .manifest
        .success_criteria
        .iter()
        .map(|criterion| match criterion {
            SuccessCriterion::CompileClean => CriterionResult {
                name: *criterion,
                passed: compile_clean,
                detail: if compile_clean {
                    String::new()
                } else {
                    "compile not clean".to_owned()
                },
            },
            SuccessCriterion::ExpectedDiagnosticsCleared => CriterionResult {
                name: *criterion,
                passed: compile_clean,
                detail: if compile_clean {
                    String::new()
                } else {
                    "expected diagnostics remain (live compile not clean)".to_owned()
                },
            },
            SuccessCriterion::NoNewUnsafe => CriterionResult {
                name: *criterion,
                passed: !unsafe_introduced,
                detail: if unsafe_introduced {
                    "unsafe introduced".to_owned()
                } else {
                    String::new()
                },
            },
            SuccessCriterion::ScriptTurnsConsumed => {
                let passed = !fixture.manifest.require_consume_all || scripts_exhausted;
                CriterionResult {
                    name: *criterion,
                    passed,
                    detail: if passed {
                        String::new()
                    } else {
                        "unconsumed scripted worker turns".to_owned()
                    },
                }
            }
        })
        .collect()
}

fn live_trajectories(
    fixture: &LoadedFixture,
    provider: &LiveModelProvider,
    status: FixtureStatus,
    compile_clean: Option<bool>,
) -> Vec<EvalTrajectoryRecord> {
    let mut caps_seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let turns: Vec<(
        ModelEndpoint,
        CompletionRequest,
        RequestFingerprint,
        Option<ModelResponse>,
    )> = match provider {
        LiveModelProvider::Scripted(p) => p
            .recorded()
            .into_iter()
            .map(|inv| (inv.endpoint, inv.request, inv.fingerprint, inv.response))
            .collect(),
        LiveModelProvider::Recording {
            provider,
            responses,
        } => provider
            .recorded()
            .into_iter()
            .zip(responses.iter().cloned())
            .map(|((endpoint, request), response)| {
                let fingerprint = RequestFingerprint::of(&request);
                (endpoint, request, fingerprint, Some(response))
            })
            .collect(),
    };
    turns
        .into_iter()
        .map(|(endpoint, request, fingerprint, response)| {
            let capability = capability_from_request(&request);
            let ordinal = {
                let entry = caps_seen.entry(capability.clone()).or_insert(0);
                let n = *entry;
                *entry = entry.saturating_add(1);
                n
            };
            let turn_id = FixtureTurnId {
                capability: CapabilityId::new(capability).unwrap_or_else(|_| {
                    CapabilityId::new("repair").expect("repair capability id is valid")
                }),
                node: None,
                ordinal,
            };
            let empty = ModelResponse {
                text: None,
                structured: None,
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: None,
                    output_tokens: None,
                },
                provider_request_id: None,
                finish_reason: None,
            };
            let mut row = EvalTrajectoryRecord::from_response(
                fixture.manifest.id.clone(),
                fixture.manifest.set,
                turn_id,
                fingerprint,
                &endpoint,
                response.as_ref().unwrap_or(&empty),
                None,
                status,
                compile_clean,
            );
            // Fixture-level completion: failed/error fixtures must not look
            // like successfully completed turns.
            row.complete_ok = status == FixtureStatus::Pass;
            row
        })
        .collect()
}

fn naive_live_trajectories(
    fixture: &LoadedFixture,
    status: FixtureStatus,
    compile_clean: Option<bool>,
) -> Vec<EvalTrajectoryRecord> {
    let Some(turn) = fixture
        .manifest
        .turns
        .iter()
        .find(|t| t.turn_id.capability.as_str() == "repair" && t.turn_id.ordinal == 0)
    else {
        return Vec::new();
    };
    let fingerprint = RequestFingerprint::of(&turn.request);
    let mut row = EvalTrajectoryRecord::from_response(
        fixture.manifest.id.clone(),
        fixture.manifest.set,
        turn.turn_id.clone(),
        fingerprint,
        &fixture.endpoint,
        &ModelResponse {
            text: None,
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
            },
            provider_request_id: None,
            finish_reason: None,
        },
        None,
        status,
        compile_clean,
    );
    row.complete_ok = status == FixtureStatus::Pass;
    vec![row]
}

fn capability_from_request(request: &CompletionRequest) -> String {
    let sys = request
        .messages
        .iter()
        .find(|m| m.role == ChatRole::System)
        .map(|m| m.content.as_str())
        .unwrap_or("");
    if sys == PLANNING_SYSTEM || sys.contains("plan a linear chain") {
        "planning".into()
    } else if sys == EDIT_SYSTEM || sys.contains("EditWorker") {
        "edit".into()
    } else {
        // Default / REPAIR_SYSTEM / unknown → repair.
        "repair".into()
    }
}

fn unsafe_introduced(pre: &str, post: &str) -> bool {
    // No LanguageBackend unsafe detector exists today; count keyword
    // occurrences in code after stripping comments/string literals.
    let pre_keys = unsafe_occurrence_keys(pre);
    let post_keys = unsafe_occurrence_keys(post);
    let mut remaining = std::collections::HashMap::<String, usize>::new();
    for key in pre_keys {
        *remaining.entry(key).or_insert(0) += 1;
    }
    for key in post_keys {
        match remaining.get_mut(&key) {
            Some(n) if *n > 0 => *n -= 1,
            _ => return true, // new or replaced occurrence
        }
    }
    false
}

/// Strip `//`, `/* */`, and string/char literals so comment/string `unsafe`
/// does not count toward [`NoNewUnsafe`].
fn strip_rust_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            out.push(' ');
            out.push(' ');
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push(' ');
            out.push(' ');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i + 1 < bytes.len() {
                out.push(' ');
                out.push(' ');
                i += 2;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out.push(' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    continue;
                }
                let c = bytes[i];
                out.push(if c == b'\n' { '\n' } else { ' ' });
                i += 1;
                if c == b'"' {
                    break;
                }
            }
            continue;
        }
        if bytes[i] == b'\'' {
            out.push(' ');
            i += 1;
            let start = i;
            while i < bytes.len() && i - start < 4 {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    continue;
                }
                let c = bytes[i];
                out.push(' ');
                i += 1;
                if c == b'\'' {
                    break;
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn unsafe_occurrence_keys(src: &str) -> Vec<String> {
    let cleaned = strip_rust_comments_and_strings(src);
    let bytes = cleaned.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'u'
            && i + 6 <= bytes.len()
            && &cleaned[i..i + 6] == "unsafe"
            && (i == 0 || !is_ident_byte(bytes[i - 1]))
            && (i + 6 == bytes.len() || !is_ident_byte(bytes[i + 6]))
            && (i == 0 || is_unsafe_predecessor(bytes[i - 1]))
        {
            let after = skip_ws(&cleaned, i + 6);
            let (form, next) = read_form(&cleaned, after);
            keys.push(match form.as_str() {
                "fn" | "trait" | "impl" | "mod" | "extern" => {
                    let ident = read_ident(&cleaned, skip_ws(&cleaned, next))
                        .map(|(id, _)| id)
                        .unwrap_or_default();
                    format!("{form}:{ident}")
                }
                other => other.to_owned(),
            });
            i += 6;
            continue;
        }
        i += 1;
    }
    keys
}

fn is_unsafe_predecessor(b: u8) -> bool {
    matches!(b, b'(' | b',' | b' ' | b'\t' | b'\n' | b'\r')
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn read_form(s: &str, i: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    if i >= bytes.len() {
        return ("bare".into(), i);
    }
    match bytes[i] {
        b'{' => ("block".into(), i + 1),
        b'(' => ("paren".into(), i + 1),
        b'!' => ("macro".into(), i + 1),
        _ => {
            if let Some((word, j)) = read_ident(s, i) {
                (word, j)
            } else {
                ("bare".into(), i)
            }
        }
    }
}

fn read_ident(s: &str, i: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    Some((s[i..j].to_owned(), j))
}

fn sandbox_unavailable(detail: &str) -> EvalError {
    EvalError::SandboxUnavailable(bound_message(detail.to_owned()))
}

fn cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(false)
}

fn cancelled_output(fixture: &LoadedFixture, started: Instant) -> FixtureRunOutput {
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status: FixtureStatus::Error,
            criteria: vec![],
            wall_ms: elapsed_ms(started),
            model_calls: 0,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: Some(ReportError::cancelled()),
        },
        trajectories: vec![],
    }
}

fn error_output(fixture: &LoadedFixture, started: Instant, error: EvalError) -> FixtureRunOutput {
    let report = ReportError::from_eval(&error);
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status: FixtureStatus::Error,
            criteria: vec![],
            wall_ms: elapsed_ms(started),
            model_calls: 0,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: Some(report),
        },
        trajectories: vec![],
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn require_landlock() -> bool {
    match std::env::var("ALLOY_REQUIRE_LANDLOCK") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
async fn landlock_or_error() -> Result<(), EvalError> {
    let dir = tempfile::tempdir().map_err(EvalError::Io)?;
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf())
        .map_err(|e| EvalError::Internal(bound_message(format!("sandbox profile: {e}"))))?;
    profile.check_backend = SandboxBackend::Landlock;
    let (available, detail) = match NativeSandboxBroker::new(profile).await {
        Ok(b) => match &b.capabilities().landlock {
            BackendStatus::Available { detail } => (true, detail.clone()),
            BackendStatus::Unavailable { reason } => (false, reason.clone()),
            other => (false, format!("{other:?}")),
        },
        Err(e) => (false, format!("NativeSandboxBroker::new: {e}")),
    };
    if available {
        return Ok(());
    }
    if require_landlock() {
        return Err(sandbox_unavailable(&format!(
            "ALLOY_REQUIRE_LANDLOCK=1 but Landlock is Unavailable: {detail}"
        )));
    }
    Err(sandbox_unavailable(&format!(
        "landlock unavailable ({detail}); set ALLOY_REQUIRE_LANDLOCK=1 to fail hard"
    )))
}

fn which_cargo() -> Option<PathBuf> {
    [
        std::env::var_os("CARGO").map(PathBuf::from),
        Some(PathBuf::from("/usr/bin/cargo")),
        std::env::var_os("CARGO_HOME").map(|h| PathBuf::from(h).join("bin/cargo")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/cargo")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

fn hermetic_cargo_home(
    root: &Path,
    real: &OperatorHomes,
    cargo_bin: &Path,
) -> Option<OperatorHomes> {
    let cargo_home_bin = root.join("cargo-home/bin");
    std::fs::create_dir_all(&cargo_home_bin).ok()?;
    std::fs::copy(cargo_bin, cargo_home_bin.join("cargo")).ok()?;
    Some(OperatorHomes::new(
        root.join("cargo-home"),
        real.rustup_home.clone(),
    ))
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), EvalError> {
    std::fs::create_dir_all(dst).map_err(EvalError::Io)?;
    for entry in std::fs::read_dir(src).map_err(EvalError::Io)? {
        let entry = entry.map_err(EvalError::Io)?;
        let name = entry.file_name();
        if name.as_os_str() == "target" {
            continue;
        }
        let to = dst.join(&name);
        if entry.file_type().map_err(EvalError::Io)?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if entry.file_type().map_err(EvalError::Io)?.is_file() {
            std::fs::copy(entry.path(), &to).map_err(EvalError::Io)?;
        }
    }
    Ok(())
}

fn router_toml() -> &'static str {
    r#"
[policy]
default_tier = "standard"
max_in_flight = 2
shutdown_grace_ms = 50

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "http://127.0.0.1:1"
api_key_env = "ALLOY_STACK_DRIVER_UNUSED_KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "operator-configured"
tiers = ["standard"]
supports_structured_output = true
max_context = 65536
input_usd_per_mtok = 2.0
output_usd_per_mtok = 4.0

[capability_tiers]
repair = "standard"
edit = "standard"
review = "standard"
planning = "standard"
"#
}

fn scripted_endpoint_for(fixture_endpoint: &ModelEndpoint) -> ModelEndpoint {
    // Router.toml binds provider/endpoint ids; keep prices from the fixture
    // when present so cost derivation stays fixture-local.
    ModelEndpoint {
        id: EndpointId::new("endpoint").unwrap(),
        provider: ProviderId::new("provider").unwrap(),
        display_name: "Endpoint".into(),
        model: "operator-configured".into(),
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: true,
        supports_json_schema: false,
        json_schema_strict: false,
        max_context: 65536,
        input_usd_per_mtok: fixture_endpoint.input_usd_per_mtok.or(Some(2.0)),
        output_usd_per_mtok: fixture_endpoint.output_usd_per_mtok.or(Some(4.0)),
        temperature: None,
    }
}

async fn worker_request(capability: &str, system: &'static str) -> CompletionRequest {
    let engine = NullContextEngine::with_goal(GOAL_TEXT);
    let pack = engine
        .assemble(AssembleRequest {
            session: SessionId::new(),
            node: NodeId::new(),
            capability: CapabilityId::new(capability).unwrap(),
            token_budget: 1024,
            must_include: vec![],
        })
        .await
        .expect("null engine assembles");
    let mut messages = vec![ChatMessage {
        role: ChatRole::System,
        content: system.to_owned(),
    }];
    messages.extend(pack.messages);
    CompletionRequest {
        messages,
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: ResponseFormat::JsonObject,
        temperature: None,
        max_output_tokens: None,
    }
}

fn load_recording_json(
    fixture: &LoadedFixture,
    name: &str,
) -> Result<serde_json::Value, EvalError> {
    let path = fixture.root.join("recordings").join(name);
    let bytes = std::fs::read(&path).map_err(|e| {
        EvalError::Internal(bound_message(format!(
            "load committed worker JSON {}: {e}",
            path.display()
        )))
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        EvalError::Internal(bound_message(format!(
            "parse committed worker JSON {}: {e}",
            path.display()
        )))
    })
}

async fn build_live_provider(
    fixture: &LoadedFixture,
    fixture_endpoint: &ModelEndpoint,
    llm_planning: bool,
    use_recording_fifo: bool,
) -> Result<LiveModelProvider, EvalError> {
    let repair = load_recording_json(fixture, "repair_plan.json")?;
    let edit = load_recording_json(fixture, "edit_patch.json")?;
    let planning = if llm_planning {
        Some(load_recording_json(fixture, "planning_proposal.json")?)
    } else {
        None
    };

    if use_recording_fifo {
        // Weight arms: DefaultContextEngine changes PromptPack bytes, so
        // NullContextEngine fingerprints miss. FIFO ignores request identity.
        let provider = Arc::new(RecordingModelProvider::new(
            ProviderId::new("provider").unwrap(),
        ));
        let mut responses = Vec::new();
        if let Some(plan) = planning {
            let response = scripted_model_response(plan);
            provider.push(Ok(response.clone()));
            responses.push(response);
        }
        let repair_response = scripted_model_response(repair);
        provider.push(Ok(repair_response.clone()));
        responses.push(repair_response);
        let edit_response = scripted_model_response(edit);
        provider.push(Ok(edit_response.clone()));
        responses.push(edit_response);
        return Ok(LiveModelProvider::Recording {
            provider,
            responses,
        });
    }

    let endpoint = scripted_endpoint_for(fixture_endpoint);
    let provider = ScriptedProvider::new(ProviderId::new("provider").unwrap(), endpoint)
        .map_err(|e| EvalError::Internal(bound_message(format!("scripted provider: {e}"))))?;
    if let Some(plan) = planning {
        provider.insert(
            RequestFingerprint::of(&worker_request("planning", PLANNING_SYSTEM).await),
            ScriptOutcome::Response(scripted_model_response(plan)),
        );
    }
    provider.insert(
        RequestFingerprint::of(&worker_request("repair", REPAIR_SYSTEM).await),
        ScriptOutcome::Response(scripted_model_response(repair)),
    );
    provider.insert(
        RequestFingerprint::of(&worker_request("edit", EDIT_SYSTEM).await),
        ScriptOutcome::Response(scripted_model_response(edit)),
    );
    Ok(LiveModelProvider::Scripted(Arc::new(provider)))
}

fn scripted_model_response(value: serde_json::Value) -> ModelResponse {
    ModelResponse {
        text: Some(value.to_string()),
        structured: Some(value),
        tool_calls: vec![],
        usage: Usage {
            input_tokens: Some(200),
            output_tokens: Some(60),
        },
        provider_request_id: Some("stack-driver".into()),
        finish_reason: Some("stop".into()),
    }
}

fn git_token(run_id: RunId) -> alloy_runtime::PermissionToken {
    alloy_runtime::PermissionToken {
        profile: ProfileId::new("default").unwrap(),
        grants: vec![alloy_runtime::Grant::Exec(alloy_runtime::ExecAllow {
            binary: "git".into(),
            args_glob: None,
        })],
        expires: None,
        run_id,
    }
}

async fn run_git(
    broker: &Arc<NativeSandboxBroker>,
    jail: &Path,
    args: &[&str],
) -> Result<(), EvalError> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|s| (*s).to_string()));
    let result = broker
        .exec(SandboxExecRequest::new(
            argv,
            jail.to_path_buf(),
            git_token(RunId::new()),
            ExecClass::Check,
        ))
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("git exec: {e}"))))?;
    if result.exit_code != Some(0) {
        return Err(EvalError::Internal(bound_message(format!(
            "git {:?} stderr={}",
            args,
            String::from_utf8_lossy(&result.stderr)
        ))));
    }
    Ok(())
}

async fn init_git_repo(broker: &Arc<NativeSandboxBroker>, jail: &Path) -> Result<(), EvalError> {
    run_git(broker, jail, &["init"]).await?;
    run_git(broker, jail, &["add", "."]).await?;
    run_git(
        broker,
        jail,
        &[
            "-c",
            "user.name=alloy",
            "-c",
            "user.email=alloy@localhost",
            "commit",
            "-m",
            "init",
        ],
    )
    .await
}

async fn seed_session(
    storage: &AlloyStorage,
    workspace_root: PathBuf,
) -> Result<SessionId, EvalError> {
    let session = Session {
        id: SessionId::new(),
        workspace_root,
        profile: ProfileId::new("default").unwrap(),
        budget: BudgetPolicy::default(),
        language_backends: vec![],
        created_at: Timestamp::now(),
    };
    storage
        .sessions()
        .upsert_session(&session, &SessionProvenance::unknown())
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("upsert session: {e}"))))?;
    Ok(session.id)
}

async fn seed_run(
    storage: &AlloyStorage,
    session_id: SessionId,
    dag_id: DagId,
) -> Result<RunId, EvalError> {
    let run_id = RunId::new();
    let goal = RunGoalRecord {
        goal: Goal {
            text: GOAL_TEXT.into(),
            constraints: vec![],
            attachments: vec![],
        },
        dag_id,
        trajectory_id: Some(alloy_runtime::TrajectoryId::new()),
        trajectory_schema: alloy_runtime::TRAJECTORY_SCHEMA_VERSION,
    };
    let row = RunRow {
        id: run_id,
        session_id,
        goal_json: serde_json::to_value(&goal)
            .map_err(|e| EvalError::Json(bound_message(e.to_string())))?,
        state: RunControlState::Created.as_str().into(),
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    };
    storage
        .sessions()
        .upsert_run(&row)
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("upsert run: {e}"))))?;
    Ok(run_id)
}

fn fingerprints() -> PlanFingerprints {
    let toolchain = ToolchainRecord {
        channel: "1.97.1".into(),
        rustc_version: "rustc 1.97.1 (stack-driver)".into(),
        cargo_version: "cargo 1.97.1 (stack-driver)".into(),
    };
    PlanFingerprints {
        policy_hash: policy_hash_digest(
            &ProfileId::new("default").unwrap(),
            &BudgetPolicy::default(),
        ),
        tool_versions: tool_versions_digest(&toolchain),
        compiler_fingerprint: compiler_fingerprint_digest(&toolchain, "x86_64-unknown-linux-gnu"),
    }
}

fn plan_ctx(session: SessionId, run: RunId, dag: DagId) -> PlanContext {
    let prints = fingerprints();
    PlanContext {
        session_id: session,
        run_id: run,
        dag_id: dag,
        goal: Goal {
            text: GOAL_TEXT.into(),
            constraints: vec![],
            attachments: vec![],
        },
        template_override: None,
        policy_hash: prints.policy_hash,
        tool_versions: prints.tool_versions,
        compiler_fingerprint: prints.compiler_fingerprint,
        prior_source: None,
        prior_proposal_artifact: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_plan_service(
    storage: &AlloyStorage,
    decisions: &Arc<RecordingDecisionLog>,
    mode: PlannerMode,
    real_caps: Arc<dyn CapabilityExecutor>,
    workspace_root: PathBuf,
    cancellation: CancellationToken,
    cost_meters: Arc<ProcessCostMeterFactory>,
    enable_review: bool,
) -> Arc<dyn PlanService> {
    let template = TemplatePlanService::from_storage(storage);
    match mode {
        PlannerMode::Template => Arc::new(template),
        PlannerMode::Llm => {
            let cfg = PlannerConfig {
                mode: PlannerMode::Llm,
                ..PlannerConfig::new()
            };
            let proposer = CapabilityPlanProposer::new(
                real_caps,
                ProposerDeps {
                    workspace_root,
                    cancellation,
                    cost_meters: cost_meters as _,
                    budget_policy: BudgetPolicy::default(),
                },
                cfg.clone(),
            );
            Arc::new(LlmPlanService::new(
                template,
                Arc::new(proposer),
                storage.artifacts(),
                Arc::clone(decisions) as _,
                cfg,
                enable_review,
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_scheduler(
    storage: &AlloyStorage,
    plane: &SessionPlane,
    sched_dir: PathBuf,
    capabilities: &Arc<dyn CapabilityExecutor>,
    verify_compile: &Arc<dyn Verifier>,
    decisions: &Arc<RecordingDecisionLog>,
    cost_meters: &Arc<ProcessCostMeterFactory>,
    cancel: &Option<CancellationToken>,
) -> Result<LinearScheduler, EvalError> {
    let mut config = SchedConfig::new(sched_dir);
    config.max_backoff = Duration::from_secs(1);
    LinearScheduler::new(LinearSchedulerDeps {
        dags: storage.dags(),
        artifacts: storage.artifacts(),
        events: storage.events(),
        sessions: storage.sessions(),
        session_plane: plane.clone(),
        runs: plane.runs(),
        verify_compile: Arc::clone(verify_compile),
        verify_test: Arc::new(UnavailableVerifyTest),
        gate_human: Arc::new(SessionGateHumanAdapter::new(plane.clone()))
            as Arc<dyn GateHumanAdapter>,
        capabilities: Arc::clone(capabilities),
        decisions: Arc::clone(decisions) as _,
        cost_meters: Arc::clone(cost_meters) as _,
        runtime_cancel: cancel.clone().unwrap_or_default(),
        budget_policy: BudgetPolicy::default(),
        run_timeout: LIVE_RUN_TIMEOUT,
        config,
    })
    .map_err(|e| EvalError::Internal(bound_message(format!("scheduler: {e}"))))
}

async fn live_compile_clean(
    broker: &Arc<NativeSandboxBroker>,
    jail: &Path,
) -> Result<bool, EvalError> {
    let token = alloy_runtime::PermissionToken {
        profile: ProfileId::new("default").unwrap(),
        grants: vec![alloy_runtime::Grant::Exec(alloy_runtime::ExecAllow {
            binary: "cargo".into(),
            args_glob: Some("check*".into()),
        })],
        expires: None,
        run_id: RunId::new(),
    };
    let result = broker
        .exec(SandboxExecRequest::new(
            vec![
                "cargo".into(),
                "check".into(),
                "--message-format=json".into(),
                "--quiet".into(),
            ],
            jail.to_path_buf(),
            token,
            ExecClass::Check,
        ))
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("cargo check: {e}"))))?;
    Ok(result.exit_code == Some(0))
}

async fn shutdown_runtime(rt: AlloyRuntime, storage: Arc<AlloyStorage>) -> Result<(), EvalError> {
    let storage_err = storage
        .close()
        .await
        .err()
        .map(|e| EvalError::Internal(bound_message(format!("storage close: {e}"))));
    let runtime_err = rt
        .shutdown()
        .await
        .err()
        .map(|e| EvalError::Internal(bound_message(format!("runtime shutdown: {e}"))));
    match (storage_err, runtime_err) {
        (Some(e), _) => Err(e),
        (None, Some(e)) => Err(e),
        (None, None) => Ok(()),
    }
}

#[cfg(test)]
mod unsafe_introduced_tests {
    use super::{unsafe_introduced, unsafe_occurrence_keys};

    #[test]
    fn counts_multiple_unsafe_on_one_line() {
        assert_eq!(
            unsafe_occurrence_keys("fn a() { unsafe { let _ = unsafe { 1 }; } }\n").len(),
            2
        );
    }

    #[test]
    fn accepts_comma_and_paren_predecessors() {
        assert_eq!(unsafe_occurrence_keys("f(unsafe { 1 })\n").len(), 1);
        assert_eq!(unsafe_occurrence_keys("a,unsafe { 1 }\n").len(), 1);
    }

    #[test]
    fn ignores_comment_and_string_unsafe() {
        assert!(unsafe_occurrence_keys("// unsafe { }\n").is_empty());
        assert!(unsafe_occurrence_keys("let s = \"unsafe { }\";\n").is_empty());
    }

    #[test]
    fn replacement_of_one_unsafe_with_another_is_introduction() {
        assert!(unsafe_introduced(
            "unsafe fn a() {}\n",
            "unsafe fn b() {}\n"
        ));
    }

    #[test]
    fn same_unsafe_form_is_not_introduction() {
        assert!(!unsafe_introduced(
            "fn a() { unsafe { 1 } }\n",
            "fn a() { unsafe { 2 } }\n"
        ));
    }
}
