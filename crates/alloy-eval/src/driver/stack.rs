//! Live ControlPlane stack driver (RFC-0016 §5.9, feature `stack-driver`).
//!
//! Ports the production assembly proven by
//! `alloy-tools/tests/scheduler_repair_e2e.rs`: Landlock jail, hermetic
//! `CARGO_HOME`, `GitEditEngine`, MCP `cargo_check` / `apply_patch`,
//! `CapabilityRegistry`, `TomlModelRouter` + [`ScriptedProvider`],
//! `GenerationDriver`, `TemplatePlanService` / `LlmPlanService` (smoke),
//! and [`GenerationSwitchCapabilities`] (inert gen1 analyze/edit so the real
//! `cargo_check` soft-fails and harvests diagnostics; real registry on gen2).
//!
//! Activated only when [`live_stack_requested`] is true (`ALLOY_EVAL_LIVE_STACK=1`
//! plus this feature). Repair/edit JSON is synthesized from the fixture golden
//! `*.post` — that makes this **integration smoke**, not thesis evidence
//! (independent model outputs required for Appendix B citation).
//!
//! Author: arkadianet

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, Approval, BudgetPolicy,
    CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityId,
    CapabilityOutcome, CapabilityRegistry, ChatMessage, ChatRole, CompletionRequest, ContextEngine,
    CostMeterFactory, EndpointId, GateHumanAdapter, GenerationDriver, GenerationDriverDeps,
    GenerationPolicy, Goal, GraphViewHandle, LinearScheduler, LinearSchedulerDeps, LlmPlanService,
    McpVerifyCompileAdapter, ModelEndpoint, ModelProvider, ModelResponse, ModelTier, NodeKind,
    NullContextEngine, PlanContext, PlanFingerprints, PlanProposer, PlanService, PlannerConfig,
    PlannerMode, ProcessCostMeterFactory, ProcessRunRouterProvider, ProposeError,
    ProposedDagManifest, ProposedNodeSpec, ProviderId, RecordingDecisionLog,
    RegistryCapabilityExecutor, ResponseFormat, RetentionPolicy, RouterConfig, RunControlState,
    RunGoalRecord, RunRow, RuntimeConfig, SchedConfig, Session, SessionVerifyPermissions,
    SessionWorkerPermissions, TemplatePlanService, Timestamp, ToolCaller, ToolChoice, ToolName,
    ToolSelector, ToolchainRecord, UnavailableVerifyTest, Usage, Verifier, WorkerConfig,
    WorkerDeps, EDIT_SYSTEM, PROPOSAL_SCHEMA_VERSION, REPAIR_SYSTEM,
};
use alloy_tools::mcp::{
    InProcessMcpHost, McpHostConfig, McpPlatform, ToolHandle, ToolHandleToolCaller,
};
use alloy_tools::{
    trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
    GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBackend,
    SandboxBroker, SandboxExecRequest, SandboxProfile,
};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::cost_claim::derive_eval_usd;
use crate::error::{bound_message, EvalError, ReportError};
use crate::fingerprint::RequestFingerprint;
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::manifest::{FixtureTurnId, SuccessCriterion};
use crate::report::{CriterionResult, FixtureOutcome, FixtureStatus};
use crate::scripted::{ScriptOutcome, ScriptedProvider};
use crate::trajectory::EvalTrajectoryRecord;

const GOAL_TEXT: &str = "fix the compile error";

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

/// Non-gating LLM-arm smoke proposer: returns a valid
/// `ProposedDagManifest` matching the `repair_local_diagnostic` shape
/// (Analyze→Edit→VerifyCompile→GateHuman). Bypasses production
/// `CapabilityPlanProposer` / PlanningWorker — **not** RFC-0017 §12.4 flip
/// evidence. Replan reuses the stored prior source (GN10).
struct ScriptedProposer {
    queue: Mutex<VecDeque<Result<ProposedDagManifest, ProposeError>>>,
}

impl ScriptedProposer {
    fn new(results: Vec<Result<ProposedDagManifest, ProposeError>>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::from(results)),
        })
    }
}

#[async_trait::async_trait]
impl PlanProposer for ScriptedProposer {
    async fn propose(&self, _ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(Err(ProposeError::Unavailable("script exhausted".into())))
    }
}

/// Linear repair chain matching `repair_local_diagnostic` / RFC-0017 AC shape.
fn repair_local_manifest() -> ProposedDagManifest {
    let node = |name: &str, kind: NodeKind, reason: Option<&str>| ProposedNodeSpec {
        name: name.into(),
        kind,
        approval_reason: reason.map(String::from),
    };
    ProposedDagManifest {
        schema_version: PROPOSAL_SCHEMA_VERSION,
        nodes: vec![
            node("analyze", NodeKind::Analyze, None),
            node("edit", NodeKind::Edit, None),
            node("verify", NodeKind::VerifyCompile, None),
            node(
                "gate",
                NodeKind::GateHuman,
                Some("Approve before completion"),
            ),
        ],
        rationale: "stack-driver llm holdout: repair_local_diagnostic shape".into(),
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
    run_live_with_mode(fixture, cancel, PlannerMode::Template).await
}

/// Live ControlPlane path with an explicit [`PlannerMode`].
///
/// `PlannerMode::Template` uses [`TemplatePlanService`]. `PlannerMode::Llm`
/// wires [`LlmPlanService`] over a [`ScriptedProposer`] (non-gating smoke;
/// not production proposing). Gen2 repair/edit [`ScriptedProvider`] turns
/// are unchanged; replan reuses prior LLM source.
pub(crate) async fn run_live_with_mode(
    fixture: &LoadedFixture,
    cancel: Option<CancellationToken>,
    mode: PlannerMode,
) -> FixtureRunOutput {
    let started = Instant::now();
    if cancelled(&cancel) {
        return cancelled_output(fixture, started);
    }

    match run_live_inner(fixture, &cancel, mode).await {
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
    mode: PlannerMode,
) -> Result<FixtureRunOutput, EvalError> {
    let started = Instant::now();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fixture, cancel, mode);
        return Err(sandbox_unavailable(
            "live stack-driver requires Linux/Landlock",
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
        let golden = std::fs::read_to_string(&fixture.paths.golden).map_err(EvalError::Io)?;
        let rel_target = fixture.manifest.naive_target_path.clone();
        let fix_diff = unified_diff(&rel_target, &pre_source, &golden);

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
            run_timeout: Duration::from_secs(300),
            budget_policy: BudgetPolicy::default(),
            context_profile: alloy_runtime::ContextProfile::v2_defaults(),
            capture: Default::default(),
            planner: PlannerConfig {
                mode,
                ..PlannerConfig::new()
            },
            profile_id: None,
            gates: Default::default(),
            sandbox_echo: None,
            gate_timeout: None,
            max_repair_generations: 2,
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
        let provider = build_scripted_provider(&fixture.endpoint, &rel_target, &fix_diff).await?;
        let routers = Arc::new(ProcessRunRouterProvider::new(
            router_config,
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            BudgetPolicy::default(),
            Some(Arc::clone(&decisions) as _),
        ));
        let deps = WorkerDeps {
            routers,
            context: Arc::new(NullContextEngine::with_goal(GOAL_TEXT)),
            tools: worker_tools,
            perms: Arc::new(SessionWorkerPermissions::new(
                storage.sessions(),
                Some("**".into()),
                Some("**".into()),
            )),
            graph: GraphViewHandle::null(),
            artifacts: storage.artifacts(),
            decisions: Arc::clone(&decisions) as _,
            sessions: storage.sessions(),
            config: WorkerConfig::default(),
        };
        let registry = CapabilityRegistry::mvp(deps)
            .map_err(|e| EvalError::Internal(bound_message(format!("capability registry: {e}"))))?;
        let real_caps: Arc<dyn CapabilityExecutor> =
            Arc::new(RegistryCapabilityExecutor::new(Arc::new(registry)));
        let caps: Arc<dyn CapabilityExecutor> =
            Arc::new(GenerationSwitchCapabilities { real: real_caps });

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

        let plans = build_plan_service(&storage, &decisions, mode);
        PlanService::plan(&*plans, plan_ctx(session_id, run_id, dag_id))
            .await
            .map_err(|e| EvalError::Internal(bound_message(format!("plan: {e}"))))?;

        let runtime_cancel = cancel.clone().unwrap_or_default();
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
                max_repair_generations: 2,
            },
        }));
        plane.set_executor(driver as _);

        let runs = plane.runs();
        let start_runs = Arc::clone(&runs);
        let run_task = tokio::spawn(async move { start_runs.start(run_id).await });

        // Auto-approve GateHuman when WaitingApproval so batch runs finish.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        let mut human_interventions = 0u32;
        loop {
            if cancelled(cancel) {
                run_task.abort();
                let _ = shutdown_runtime(rt, storage).await;
                return Ok(cancelled_output(fixture, started));
            }
            if run_task.is_finished() {
                let _ = shutdown_runtime(rt, storage).await;
                return Err(EvalError::Internal(
                    "live stack-driver: run returned before GateHuman opened".into(),
                ));
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
                    .ok_or_else(|| EvalError::Internal("gate node missing ApprovalSpec".into()))?;
                runs.approve(run_id, gate_id, Approval::Allow)
                    .await
                    .map_err(|e| EvalError::Internal(bound_message(format!("approve: {e}"))))?;
                human_interventions = human_interventions.saturating_add(1);
                run_task
                    .await
                    .map_err(|e| EvalError::Internal(bound_message(format!("run join: {e}"))))?
                    .map_err(|e| EvalError::Internal(bound_message(format!("run failed: {e}"))))?;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                run_task.abort();
                let _ = shutdown_runtime(rt, storage).await;
                return Err(EvalError::Internal(
                    "live stack-driver: WaitingApproval timeout".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

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
        let compile_clean = succeeded && live_compile_clean(&broker, &jail).await?;

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

        let scripts_exhausted = provider.is_exhausted();
        let unsafe_introduced = unsafe_introduced(&pre_source, &fixed_source);
        let criteria = live_criteria(fixture, compile_clean, unsafe_introduced, scripts_exhausted);

        let status = if !succeeded {
            FixtureStatus::Fail
        } else if criteria.iter().all(|c| c.passed) {
            FixtureStatus::Pass
        } else {
            FixtureStatus::Fail
        };

        let trajectories = live_trajectories(fixture, &provider, status, Some(compile_clean));

        let _ = shutdown_runtime(rt, storage).await;
        // Keep tempdirs alive until shutdown completes.
        drop(homes_root);
        drop(work_dir);
        drop(runtime_dir);

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
    provider: &ScriptedProvider,
    status: FixtureStatus,
    compile_clean: Option<bool>,
) -> Vec<EvalTrajectoryRecord> {
    let mut caps_seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    provider
        .recorded()
        .into_iter()
        .map(|inv| {
            let capability = capability_from_request(&inv.request);
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
            let mut row = EvalTrajectoryRecord::from_response(
                fixture.manifest.id.clone(),
                fixture.manifest.set,
                turn_id,
                inv.fingerprint,
                &inv.endpoint,
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
            // from_response marks complete_ok from usage; force success for a
            // consumed scripted invocation even when tokens were not metered.
            row.complete_ok = true;
            row
        })
        .collect()
}

/// One observational row for live naive (no model calls): the ordinal-0 repair
/// turn identity from the manifest, so scrub/determinism still has a trajectory.
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
    row.complete_ok = true;
    vec![row]
}

fn capability_from_request(request: &CompletionRequest) -> String {
    let sys = request
        .messages
        .iter()
        .find(|m| m.role == ChatRole::System)
        .map(|m| m.content.as_str())
        .unwrap_or("");
    if sys == EDIT_SYSTEM || sys.contains("EditWorker") {
        "edit".into()
    } else {
        // Default / REPAIR_SYSTEM / unknown → repair (live stack only scripts those two).
        "repair".into()
    }
}

fn unsafe_introduced(pre: &str, post: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(^|\s)unsafe(\s|!|\(|\{)").expect("unsafe line regex is valid")
    });
    let count = |src: &str| src.lines().filter(|line| re.is_match(line)).count();
    count(post) > count(pre)
}

fn sandbox_unavailable(detail: &str) -> EvalError {
    EvalError::Internal(bound_message(format!(
        "stack_driver_sandbox_unavailable: {detail}"
    )))
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
    let mut report = ReportError::from_eval(&error);
    if matches!(&error, EvalError::Internal(m) if m.contains("stack_driver_sandbox_unavailable")) {
        report.kind = "stack_driver_sandbox_unavailable".to_owned();
    }
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

/// Full-file unified diff suitable for `apply_patch` / `GitEditEngine`.
fn unified_diff(rel_path: &str, before: &str, after: &str) -> String {
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();
    let old_n = old_lines.len();
    let new_n = new_lines.len();
    let mut out = String::new();
    out.push_str(&format!("--- a/{rel_path}\n+++ b/{rel_path}\n"));
    out.push_str(&format!("@@ -1,{old_n} +1,{new_n} @@\n"));
    for line in &old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
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

fn scripted_response(value: serde_json::Value) -> ScriptOutcome {
    ScriptOutcome::Response(ModelResponse {
        text: Some(value.to_string()),
        structured: Some(value),
        tool_calls: vec![],
        usage: Usage {
            input_tokens: Some(200),
            output_tokens: Some(60),
        },
        provider_request_id: Some("stack-driver".into()),
        finish_reason: Some("stop".into()),
    })
}

async fn build_scripted_provider(
    fixture_endpoint: &ModelEndpoint,
    rel_target: &str,
    fix_diff: &str,
) -> Result<Arc<ScriptedProvider>, EvalError> {
    let endpoint = scripted_endpoint_for(fixture_endpoint);
    let provider = ScriptedProvider::new(ProviderId::new("provider").unwrap(), endpoint)
        .map_err(|e| EvalError::Internal(bound_message(format!("scripted provider: {e}"))))?;
    provider.insert(
        RequestFingerprint::of(&worker_request("repair", REPAIR_SYSTEM).await),
        scripted_response(serde_json::json!({
            "summary": format!("repair {rel_target} so the crate compiles cleanly"),
            "target_files": [rel_target],
            "steps": [{
                "file": rel_target,
                "rationale": "apply the golden fix as a minimal unified diff",
                "anchor_line": 1,
            }],
            "needs_replan": false,
            "confidence": 0.9,
        })),
    );
    provider.insert(
        RequestFingerprint::of(&worker_request("edit", EDIT_SYSTEM).await),
        scripted_response(serde_json::json!({
            "patch": fix_diff,
            "summary": format!("apply golden fix to {rel_target}"),
            "confidence": 0.85,
        })),
    );
    Ok(Arc::new(provider))
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

fn build_plan_service(
    storage: &AlloyStorage,
    decisions: &Arc<RecordingDecisionLog>,
    mode: PlannerMode,
) -> Arc<dyn PlanService> {
    let template = TemplatePlanService::from_storage(storage);
    match mode {
        PlannerMode::Template => Arc::new(template),
        PlannerMode::Llm => {
            let proposer = ScriptedProposer::new(vec![Ok(repair_local_manifest())]);
            Arc::new(LlmPlanService::new(
                template,
                proposer,
                storage.artifacts(),
                Arc::clone(decisions) as _,
                PlannerConfig {
                    mode: PlannerMode::Llm,
                    ..PlannerConfig::new()
                },
                true,
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
        run_timeout: Duration::from_secs(300),
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
    storage
        .close()
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("storage close: {e}"))))?;
    rt.shutdown()
        .await
        .map_err(|e| EvalError::Internal(bound_message(format!("runtime shutdown: {e}"))))?;
    Ok(())
}
