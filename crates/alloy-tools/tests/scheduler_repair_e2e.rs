//! RFC-0010 §11.3 / RFC-0013 §15.4 — cross-subsystem end-to-end test.
//!
//! Real SQLite storage, a real `LinearScheduler` built through the
//! production `LinearScheduler::new`, a real MCP host running `cargo_check`
//! inside a real Landlock jail over a tiny fixture crate with a deliberate
//! type error, the **real RFC-0013 capability registry** (T20 replaced the
//! RFC-0010 stub) completing against `alloy-eval`'s `ScriptedProvider`
//! through the production `TomlModelRouter`, a real `GitEditEngine` behind
//! the `apply_patch` builtin, and a gate approved through the real
//! `RunController`.
//!
//! Traces RFC-0013 Appendix A (`repair_local_diagnostic`, generation 2) —
//! now on the **production path** (RFC-0017 MG5 / AC 22): generation 1 is
//! planned by the real `TemplatePlanService`, dispatched through
//! `RunController::start` with the `GenerationDriver` installed as the run
//! executor, and runs with an inert capability stub so the real
//! `cargo_check` soft-fails and harvests genuine rustc diagnostics; the
//! driver admits the `VerifyCompile` `Fail` (GN1–GN7) and replans through
//! `PlanService::replan`, whose `PlanPersistence` seeds generation 2's root
//! (`analyze`) with the SD1–SD10 `FromPredecessors` envelope — no
//! hand-crafted seed remains in this test; generation 2 dispatches the real
//! `RepairWorker` and `EditWorker`, whose scripted completions produce a
//! plan and a unified diff that the patch builtin applies; `verify` passes;
//! the gate opens and is approved; the run reaches `Succeeded` with exactly
//! one `RunAccepted` / `RunCompleted` / `RunFinished` (AM-0003-2), every
//! node carrying `output_ref`, and the meter showing exactly two model
//! calls (T20).
//!
//! This MUST live here, not in `alloy-runtime` (§11.3 / RFC-0013 §2.4): only
//! this crate owns `ToolHandle`/`InProcessMcpHost`/`NativeSandboxBroker`.
//!
//! Skip policy: mirrors `sandbox_rfc0005.rs`'s `landlock_or_skip` — absent a
//! real, working Landlock jail this test skips (not fails) unless
//! `ALLOY_REQUIRE_LANDLOCK=1`.
//!
//! Author: arkadianet

#[cfg(not(target_os = "linux"))]
#[test]
fn repair_local_diagnostic_e2e_is_linux_only() {
    assert!(
        std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_none(),
        "ALLOY_REQUIRE_LANDLOCK=1 but this OS has no Landlock backend"
    );
    eprintln!("skip: scheduler_repair_e2e is Linux/Landlock-only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use alloy_eval::{RequestFingerprint, ScriptOutcome, ScriptedProvider};
    use alloy_runtime::adapters::SessionGateHumanAdapter;
    use alloy_runtime::capabilities::EditAppliedPayload as CapEditAppliedPayload;
    use alloy_runtime::context::AssembleRequest;
    use alloy_runtime::runtime::AlloyRuntime;
    use alloy_runtime::session::SessionPlane;
    use alloy_runtime::storage::{
        install_sqlite_event_sink, AlloyStorage, ArtifactKind, ArtifactStore, DagStore, EventStore,
        SessionRows, StorageOpenOptions,
    };
    use alloy_runtime::types::ids::{DagId, NodeId, ProfileId, RunId, SessionId};
    use alloy_runtime::SessionProvenance;
    use alloy_runtime::{
        compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, Approval,
        BudgetPolicy, CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityId,
        CapabilityOutcome, CapabilityRegistry, ChatMessage, ChatRole, CompletionRequest,
        ContextEngine, CostSnapshot, DagState, DecisionKind, EndpointId, GateHumanAdapter,
        GenerationDriver, GenerationDriverDeps, GenerationPolicy, Goal, GraphViewHandle,
        LinearScheduler, LinearSchedulerDeps, McpVerifyCompileAdapter, ModelEndpoint,
        ModelProvider, ModelResponse, ModelTier, NodeInputEnvelope, NodeInputPayload, NodeKind,
        NodeOutputEnvelope, NullContextEngine, PlanContext, PlanFingerprints, PlanProducedPayload,
        PlanService, ProcessCostMeterFactory, ProcessRunRouterProvider, ProviderId,
        RecordingDecisionLog, RegistryCapabilityExecutor, RepairPlanPayload, ResponseFormat,
        RetentionPolicy, RouterConfig, RunControlState, RunGoalRecord, RunRow, RuntimeConfig,
        RuntimeEvent, RuntimeHandle, SchedConfig, Session, SessionEventType,
        SessionVerifyPermissions, SessionWorkerPermissions, TemplatePlanService, Timestamp,
        ToolCaller, ToolChoice, ToolName, ToolSelector, ToolchainRecord, UnavailableVerifyTest,
        Usage, Verifier, VerifyPermissions, WorkerConfig, WorkerDeps, EDIT_SYSTEM, REPAIR_SYSTEM,
    };
    use alloy_tools::mcp::{
        InProcessMcpHost, McpHostConfig, McpPlatform, ToolHandle, ToolHandleToolCaller,
    };
    use alloy_tools::{
        trusted_exec_path, BackendStatus, EditEnginePatchBackend, ExecClass, GitEditEngine,
        GitEditEngineConfig, NativeSandboxBroker, OperatorHomes, PathPolicy, SandboxBackend,
        SandboxBroker, SandboxExecRequest, SandboxProfile,
    };
    use tempfile::TempDir;

    // --- skip gate (mirrors sandbox_rfc0005.rs::landlock_or_skip) ----------

    fn require_landlock() -> bool {
        std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
    }

    /// `true` when Landlock is available on this host. Panics instead of
    /// skipping when `ALLOY_REQUIRE_LANDLOCK=1` — a dishonest green is worse
    /// than a skip, but CI must not silently stop covering this path.
    async fn landlock_or_skip() -> bool {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
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
            return true;
        }
        if require_landlock() {
            panic!("ALLOY_REQUIRE_LANDLOCK=1 but Landlock is Unavailable: {detail}");
        }
        eprintln!("skip: landlock unavailable ({detail}); set ALLOY_REQUIRE_LANDLOCK=1 to fail");
        false
    }

    fn skip_or_panic(reason: &str) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but: {reason}"
        );
        eprintln!("skip: {reason}; set ALLOY_REQUIRE_LANDLOCK=1 to fail");
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

    /// A `CARGO_HOME` containing only a copy of the real `cargo`.
    ///
    /// The jail grants read on `$CARGO_HOME/{registry,git,bin}` but
    /// deliberately *not* on `$CARGO_HOME/config.toml`, and cargo treats an
    /// EACCES on that file as a hard error rather than "absent". So on any
    /// machine whose operator has a `~/.cargo/config.toml` — common; a shared
    /// `build.target-dir` is the usual reason — every sandboxed `cargo check`
    /// fails with `could not load Cargo configuration` before it compiles
    /// anything. Pointing `CARGO_HOME` at a clean directory makes this test
    /// depend only on the code under test. `RUSTUP_HOME` stays real so the
    /// shim still resolves a toolchain, and the fixture crate has no
    /// dependencies, so an empty registry costs nothing.
    fn hermetic_cargo_home(root: &Path, real: &OperatorHomes) -> Option<OperatorHomes> {
        let cargo_bin = root.join("cargo-home/bin");
        std::fs::create_dir_all(&cargo_bin).ok()?;
        std::fs::copy(which_cargo()?, cargo_bin.join("cargo")).ok()?;
        Some(OperatorHomes::new(
            root.join("cargo-home"),
            real.rustup_home.clone(),
        ))
    }

    /// Copy the fixture tree into a unique tempdir so concurrent runs never
    /// share a writable jail (matches `mcp_rfc0006.rs::copy_fixtures_tree`).
    fn copy_fixtures_tree() -> TempDir {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let dir = tempfile::tempdir().unwrap();
        copy_dir_all(&src, dir.path()).expect("copy fixtures");
        dir
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.as_os_str() == "target" {
                continue;
            }
            let to = dst.join(&name);
            if entry.file_type()?.is_dir() {
                copy_dir_all(&entry.path(), &to)?;
            } else if entry.file_type()?.is_file() {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    // --- generation-switching capabilities ----------------------------------

    /// Generation 1 stand-in behind a single scheduler: RFC-0013 Appendix A
    /// starts its worked trace at generation 2, so generation 1 only needs
    /// `analyze`/`edit` to succeed inertly and let the real `cargo_check`
    /// harvest the diagnostics; every later generation dispatches the real
    /// RFC-0013 registry. The switch keys on the input envelope's
    /// generation, which the scheduler stamps at plan time — one scheduler,
    /// one executor, both generations (the production wiring holds one
    /// executor for the whole run).
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

    // --- scripted model -----------------------------------------------------

    const GOAL_TEXT: &str = "fix the type error";

    /// The diff the scripted `edit` completion returns; must apply cleanly
    /// to the fixture's `src/main.rs`.
    const FIX_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    let x: i32 = \"not a number\";\n+    let x: i32 = 42;\n     println!(\"{}\", x);\n }\n";

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
api_key_env = "ALLOY_E2E_UNUSED_KEY"

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

    fn scripted_endpoint() -> ModelEndpoint {
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
            input_usd_per_mtok: Some(2.0),
            output_usd_per_mtok: Some(4.0),
            temperature: None,
        }
    }

    /// Reconstruct the exact `CompletionRequest` a worker will send: the
    /// `NullContextEngine` pack for `capability` with the worker's owned
    /// system instruction prepended (RFC-0013 §6.2), structured output
    /// requested (PR9). `ScriptedProvider` keys on this fingerprint.
    async fn worker_request(capability: &str, system: &'static str) -> CompletionRequest {
        let engine = NullContextEngine::with_goal(GOAL_TEXT);
        let pack = engine
            .assemble(AssembleRequest {
                session: SessionId::new(),
                node: NodeId::new(),
                capability: CapabilityId::new(capability).unwrap(),
                token_budget: 1024, // NullContextEngine output is budget-free.
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
            provider_request_id: Some("scripted-1".into()),
            finish_reason: Some("stop".into()),
        })
    }

    async fn build_scripted_provider() -> Arc<ScriptedProvider> {
        let provider =
            ScriptedProvider::new(ProviderId::new("provider").unwrap(), scripted_endpoint())
                .unwrap();
        provider.insert(
            RequestFingerprint::of(&worker_request("repair", REPAIR_SYSTEM).await),
            scripted_response(serde_json::json!({
                "summary": "the literal is a &str but the binding is typed i32; replace the string with an integer literal",
                "target_files": ["src/main.rs"],
                "steps": [{
                    "file": "src/main.rs",
                    "rationale": "replace the string literal with an i32 literal so the annotation holds",
                    "anchor_line": 2,
                }],
                "needs_replan": false,
                "confidence": 0.9,
            })),
        );
        provider.insert(
            RequestFingerprint::of(&worker_request("edit", EDIT_SYSTEM).await),
            scripted_response(serde_json::json!({
                "patch": FIX_DIFF,
                "summary": "replace the string literal with 42",
                "confidence": 0.85,
            })),
        );
        Arc::new(provider)
    }

    // --- git init in the jail ----------------------------------------------

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

    async fn run_git(broker: &Arc<NativeSandboxBroker>, jail: &Path, args: &[&str]) {
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
            .unwrap();
        assert_eq!(
            result.exit_code,
            Some(0),
            "git {:?} stderr={}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
    }

    /// The `GitEditEngine` checkpoints via git, so the workspace must be a
    /// committed repository before the first `apply_patch` (RFC-0008 §5.6).
    async fn init_git_repo(broker: &Arc<NativeSandboxBroker>, jail: &Path) {
        run_git(broker, jail, &["init"]).await;
        run_git(broker, jail, &["add", "."]).await;
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
        .await;
    }

    // --- fixture ------------------------------------------------------------

    struct Fixture {
        dir: TempDir,
        rt: AlloyRuntime,
        handle: RuntimeHandle,
        storage: Arc<AlloyStorage>,
        plane: SessionPlane,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut rt = AlloyRuntime::new();
            rt.configure(RuntimeConfig {
                data_dir: dir.path().join("runtime"),
                data_dir_rule: "test",
                profile_path: dir.path().join("profiles/default.toml"),
                router_path: dir.path().join("router.toml"),
                env_file_hint: dir.path().join("example.env"),
                retain_full_prompts: false,
                retain_tool_bodies: false,
                run_timeout: Duration::from_secs(300),
                budget_policy: BudgetPolicy::default(),
                context_profile: alloy_runtime::ContextProfile::v2_defaults(),
                capture: Default::default(),
                planner: alloy_runtime::PlannerConfig::new(),
                profile_id: None,
                gates: Default::default(),
                sandbox_echo: None,
                gate_timeout: None,
                max_repair_generations: 2,
            })
            .unwrap();
            let handle = rt.start().await.unwrap();
            let storage = install_sqlite_event_sink(
                &handle,
                Some(StorageOpenOptions::for_data_dir(dir.path().join("storage"))),
            )
            .await
            .unwrap();
            let plane = SessionPlane::new(handle.clone(), Arc::clone(&storage));
            Self {
                dir,
                rt,
                handle,
                storage,
                plane,
            }
        }

        async fn close(self) {
            self.storage.close().await.unwrap();
            self.rt.shutdown().await.unwrap();
        }

        async fn seed_session(&self, workspace_root: PathBuf) -> SessionId {
            let session = Session {
                id: SessionId::new(),
                workspace_root,
                profile: ProfileId::new("default").unwrap(),
                budget: BudgetPolicy::default(),
                language_backends: vec![],
                created_at: Timestamp::now(),
            };
            self.storage
                .sessions()
                .upsert_session(&session, &SessionProvenance::unknown())
                .await
                .unwrap();
            session.id
        }

        async fn seed_run(&self, session_id: SessionId, dag_id: DagId) -> RunId {
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
                goal_json: serde_json::to_value(&goal).unwrap(),
                // `created` so `start` runs the full acceptance path and the
                // lifecycle-triple assertion is meaningful (AM-0003-2).
                state: RunControlState::Created.as_str().into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            };
            self.storage.sessions().upsert_run(&row).await.unwrap();
            run_id
        }
    }

    /// The three `PlanContext` fingerprints (synthetic toolchain — the DAG
    /// cache is off day-1, so they are provenance only).
    fn fingerprints() -> PlanFingerprints {
        let toolchain = ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1 (e2e)".into(),
            cargo_version: "cargo 1.97.1 (e2e)".into(),
        };
        PlanFingerprints {
            policy_hash: policy_hash_digest(
                &ProfileId::new("default").unwrap(),
                &BudgetPolicy::default(),
            ),
            tool_versions: tool_versions_digest(&toolchain),
            compiler_fingerprint: compiler_fingerprint_digest(
                &toolchain,
                "x86_64-unknown-linux-gnu",
            ),
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

    /// Assemble a production `LinearScheduler` (not `new_for_test`) over the
    /// real MCP verify adapter, a shared decision log, and a shared meter
    /// factory (so generation 2's router meter is the run's meter).
    fn build_scheduler(
        fx: &Fixture,
        sched_dir: PathBuf,
        capabilities: &Arc<dyn CapabilityExecutor>,
        verify_compile: &Arc<dyn Verifier>,
        decisions: &Arc<RecordingDecisionLog>,
        cost_meters: &Arc<ProcessCostMeterFactory>,
    ) -> LinearScheduler {
        let mut config = SchedConfig::new(sched_dir);
        config.max_backoff = Duration::from_secs(1);
        LinearScheduler::new(LinearSchedulerDeps {
            dags: fx.storage.dags(),
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: Arc::clone(verify_compile),
            verify_test: Arc::new(UnavailableVerifyTest),
            gate_human: Arc::new(SessionGateHumanAdapter::new(fx.plane.clone()))
                as Arc<dyn GateHumanAdapter>,
            capabilities: Arc::clone(capabilities),
            decisions: Arc::clone(decisions) as _,
            cost_meters: Arc::clone(cost_meters) as _,
            runtime_cancel: tokio_util::sync::CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(300),
            config,
        })
        .unwrap()
    }

    // --- the flow ----------------------------------------------------------

    struct FlowResult {
        analyze_payload: serde_json::Value,
        edit_payload: serde_json::Value,
        meter: CostSnapshot,
        worker_attempts: usize,
        model_calls_recorded: usize,
        fixed_source: String,
        patch_artifact_kind: ArtifactKind,
    }

    /// The full RFC-0013 §15.4 trace. `None` means an environment skip.
    async fn run_repair_flow() -> Option<FlowResult> {
        if !landlock_or_skip().await {
            return None;
        }
        if which_cargo().is_none() {
            skip_or_panic("cargo not on PATH");
            return None;
        }
        let real_homes = match OperatorHomes::resolve() {
            Ok(h) => h,
            Err(e) => {
                skip_or_panic(&format!("operator homes: {e}"));
                return None;
            }
        };

        let fixtures = copy_fixtures_tree();
        // The jail IS the crate workspace: patch paths and cargo both resolve
        // against the same root (RFC-0008 path policy + RFC-0013 EW4).
        let workspace_root = fixtures.path().join("sbx_repair").canonicalize().unwrap();
        let jail = workspace_root.clone();
        assert!(workspace_root.join("Cargo.toml").is_file());

        let homes_root = tempfile::tempdir().unwrap();
        let Some(homes) = hermetic_cargo_home(homes_root.path(), &real_homes) else {
            skip_or_panic("could not stage a hermetic CARGO_HOME");
            return None;
        };

        let mut profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        profile.check_backend = SandboxBackend::Landlock;
        profile.exec_timeout = Duration::from_secs(240);
        let broker =
            match NativeSandboxBroker::with_operator_homes(profile.clone(), homes.clone()).await {
                Ok(b) => b,
                Err(e) => {
                    skip_or_panic(&format!("landlock broker unavailable: {e}"));
                    return None;
                }
            };
        let broker = Arc::new(broker);
        init_git_repo(&broker, &jail).await;

        let fx = Fixture::new().await;

        // Real RFC-0008 edit engine behind the apply_patch builtin.
        let read_only_roots: Vec<PathBuf> = Vec::new();
        let path_policy = PathPolicy::from_profile(&profile, read_only_roots.clone()).unwrap();
        let engine = Arc::new(
            GitEditEngine::new(GitEditEngineConfig::new(
                Arc::clone(&broker) as Arc<dyn SandboxBroker>,
                path_policy,
                trusted_exec_path(&homes),
                fx.storage.artifacts(),
                fx.storage.events(),
            ))
            .unwrap(),
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
            .unwrap(),
        );

        let session_id = fx.seed_session(workspace_root.clone()).await;
        let dag_id = DagId::new();
        let run_id = fx.seed_run(session_id, dag_id).await;

        let sched_dir = fx.dir.path().join("scheduler");
        let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let cost_meters = Arc::new(ProcessCostMeterFactory::new());

        // Verify adapter over its own handle (cargo_check only).
        let verify_tools: Arc<dyn ToolCaller> =
            Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
                Arc::clone(&host) as Arc<dyn McpPlatform>,
                vec![ToolSelector::name(ToolName::new("cargo_check").unwrap())],
            )));
        let verify_perms: Arc<dyn VerifyPermissions> = Arc::new(SessionVerifyPermissions::new(
            fx.storage.sessions(),
            Some("check*".into()),
            None,
        ));
        let verify_compile: Arc<dyn Verifier> = Arc::new(McpVerifyCompileAdapter::new(
            verify_tools,
            verify_perms,
            fx.storage.artifacts(),
        ));

        // --- one executor for both generations (RFC-0017 production path) --
        let worker_tools: Arc<dyn ToolCaller> =
            Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
                Arc::clone(&host) as Arc<dyn McpPlatform>,
                vec![
                    ToolSelector::name(ToolName::new("fs_read").unwrap()),
                    ToolSelector::name(ToolName::new("apply_patch").unwrap()),
                ],
            )));
        let router_config = RouterConfig::from_str("e2e", router_toml()).unwrap();
        let provider = build_scripted_provider().await;
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
                fx.storage.sessions(),
                Some("**".into()),
                Some("**".into()),
            )),
            graph: GraphViewHandle::null(),
            artifacts: fx.storage.artifacts(),
            decisions: Arc::clone(&decisions) as _,
            sessions: fx.storage.sessions(),
            config: WorkerConfig::default(),
        };
        let registry = CapabilityRegistry::mvp(deps).unwrap();
        let real_caps: Arc<dyn CapabilityExecutor> =
            Arc::new(RegistryCapabilityExecutor::new(Arc::new(registry)));
        let caps: Arc<dyn CapabilityExecutor> =
            Arc::new(GenerationSwitchCapabilities { real: real_caps });

        // One production scheduler for the whole run, registered on the
        // runtime handle so the driver's `run_dag_within` reaches it.
        let scheduler = Arc::new(build_scheduler(
            &fx,
            sched_dir,
            &caps,
            &verify_compile,
            &decisions,
            &cost_meters,
        ));
        fx.handle.set_scheduler(scheduler as _).unwrap();

        // Generation 1 is planned by the production plan service (root gets
        // the plain Goal envelope) — no hand-built topology.
        let plans = Arc::new(TemplatePlanService::from_storage(&fx.storage));
        PlanService::plan(&*plans, plan_ctx(session_id, run_id, dag_id))
            .await
            .unwrap();

        // The RFC-0017 GenerationDriver is the run's executor: it dispatches
        // generation 1, admits the genuine VerifyCompile Fail, replans
        // through `PlanService::replan` (which seeds generation 2's root per
        // SD1–SD10), and dispatches generation 2 — all inside one
        // `RunController::start`.
        let driver = Arc::new(GenerationDriver::new(GenerationDriverDeps {
            handle: fx.handle.clone(),
            plans: Arc::clone(&plans) as _,
            runs: fx.plane.runs(),
            dags: fx.storage.dags() as _,
            sessions: fx.storage.sessions() as _,
            events: fx.storage.events() as _,
            decisions: Arc::clone(&decisions) as _,
            cost_meters: Arc::clone(&cost_meters) as _,
            budget_policy: BudgetPolicy::default(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            fingerprints: fingerprints(),
            policy: GenerationPolicy {
                max_repair_generations: 2,
            },
        }));
        fx.plane.set_executor(driver as _);

        let runs = fx.plane.runs();
        let start_runs = Arc::clone(&runs);
        let mut run_task = tokio::spawn(async move { start_runs.start(run_id).await });

        // Poll for `WaitingApproval` before approving (generation 2's gate).
        // Real wall-clock `cargo_check` I/O means this cannot use a paused
        // clock. Generation 1 fails at verify without opening a gate, so the
        // first `WaitingApproval` the blob shows is generation 2's.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        loop {
            if run_task.is_finished() {
                let early = (&mut run_task).await.expect("start task panicked");
                panic!("run returned before the gate opened: {early:?}");
            }
            let dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
            if dag.state == DagState::WaitingApproval {
                assert_eq!(dag.generation, 2, "the gate must belong to generation 2");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gen2 never reached WaitingApproval; last state: {:?}",
                dag.state
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let waiting = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        let gate_id = waiting
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::GateHuman)
            .and_then(|n| n.approval.as_ref())
            .map(|a| a.gate)
            .expect("generation 2 gate node carries an ApprovalSpec");
        runs.approve(run_id, gate_id, Approval::Allow)
            .await
            .unwrap();

        run_task
            .await
            .expect("start task panicked")
            .expect("run failed");
        assert!(
            provider.is_exhausted(),
            "both scripted completions were consumed"
        );

        // §6.3 stayed single-sourced across generations (AC 21b shape): one
        // RunAccepted, one RunCompleted, one RunFinished, terminal row once.
        let run_row = fx
            .storage
            .sessions()
            .get_run(run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run_row.state, "succeeded");
        let runtime_events = fx
            .storage
            .events()
            .list_runtime_events(None, 1000)
            .await
            .unwrap();
        let accepted = runtime_events
            .iter()
            .filter(
                |(_, ev)| matches!(ev, RuntimeEvent::RunAccepted { run_id: r, .. } if *r == run_id),
            )
            .count();
        let finished = runtime_events
            .iter()
            .filter(
                |(_, ev)| matches!(ev, RuntimeEvent::RunFinished { run_id: r, .. } if *r == run_id),
            )
            .count();
        let session_events = fx
            .storage
            .events()
            .list_session_events(session_id, None, 1000)
            .await
            .unwrap();
        let completed = session_events
            .iter()
            .filter(|e| e.type_ == SessionEventType::RunCompleted && e.run_id == Some(run_id))
            .count();
        assert_eq!(
            (accepted, completed, finished),
            (1, 1, 1),
            "one lifecycle triple regardless of generation count"
        );

        // The replan was the production path: two PlanProduced events, the
        // second a seeded replan (AM-0009-2/3).
        let plans_produced: Vec<PlanProducedPayload> = session_events
            .iter()
            .filter(|e| e.type_ == SessionEventType::PlanProduced)
            .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
            .collect();
        assert_eq!(plans_produced.len(), 2);
        assert!(plans_produced[1].replan);
        assert_eq!(plans_produced[1].generation, 2);
        assert_eq!(plans_produced[1].seeded_root, Some(true));

        // Generation 2's root really carries generation 1's failure: the
        // analyze input decodes as `FromPredecessors` whose synthetic pred
        // resolves to the SD3 `replan_seed` envelope with the genuine E0308.
        let final_dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(final_dag.generation, 2);
        let analyze_node = final_dag
            .nodes
            .values()
            .find(|n| n.kind == NodeKind::Analyze)
            .unwrap();
        let root_blob = fx
            .storage
            .artifacts()
            .get(analyze_node.input_ref)
            .await
            .unwrap();
        let root_env: NodeInputEnvelope = serde_json::from_slice(&root_blob.bytes).unwrap();
        let NodeInputPayload::FromPredecessors { preds } = &root_env.payload else {
            panic!("seeded root must be FromPredecessors, got {root_env:?}");
        };
        assert_eq!(preds.len(), 1, "exactly one synthetic seed predecessor");
        assert_eq!(preds[0].kind, NodeKind::VerifyCompile);
        let seed_blob = fx
            .storage
            .artifacts()
            .get(preds[0].output_ref)
            .await
            .unwrap();
        assert_eq!(
            seed_blob
                .meta
                .labels
                .get("alloy.envelope")
                .and_then(|v| v.as_str()),
            Some("replan_seed")
        );
        let seed_env: NodeOutputEnvelope = serde_json::from_slice(&seed_blob.bytes).unwrap();
        assert_eq!(seed_env.generation, 1, "the seed speaks for generation 1");
        assert_eq!(seed_env.payload["ok"], serde_json::json!(false));
        let seed_codes: Vec<&str> = seed_env.payload["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["code"].as_str())
            .collect();
        assert!(
            seed_codes.contains(&"E0308"),
            "the genuine rustc E0308 must ride the seed, got {seed_codes:?}"
        );

        // Every node carries output_ref; decode the two worker payloads.
        let mut payloads: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
        for (name, kind) in [
            ("analyze", NodeKind::Analyze),
            ("edit", NodeKind::Edit),
            ("verify", NodeKind::VerifyCompile),
            ("gate", NodeKind::GateHuman),
        ] {
            let node = final_dag
                .nodes
                .values()
                .find(|n| n.kind == kind)
                .unwrap_or_else(|| panic!("{name} node present"));
            let output_ref = node
                .output_ref
                .unwrap_or_else(|| panic!("{name} MUST carry output_ref at Succeeded"));
            let blob = fx.storage.artifacts().get(output_ref).await.unwrap();
            let envelope: serde_json::Value = serde_json::from_slice(&blob.bytes).unwrap();
            payloads.insert(name, envelope["payload"].clone());
        }

        let edit_payload: CapEditAppliedPayload =
            serde_json::from_value(payloads["edit"].clone()).unwrap();
        let patch_artifact_kind = fx
            .storage
            .artifacts()
            .meta(edit_payload.patch_artifact)
            .await
            .unwrap()
            .kind;

        // The repair really happened on disk, not just in the DAG blob.
        let fixed_source = std::fs::read_to_string(workspace_root.join("src/main.rs")).unwrap();

        let worker_attempts = decisions
            .recorded_decisions()
            .into_iter()
            .filter(|d| d.kind == DecisionKind::Custom("worker_attempt".into()))
            .count();
        let model_calls_recorded = decisions.recorded_model_calls().len();
        let meter = cost_meters.meter_for_snapshot(run_id);

        let result = FlowResult {
            analyze_payload: payloads["analyze"].clone(),
            edit_payload: payloads["edit"].clone(),
            meter,
            worker_attempts,
            model_calls_recorded,
            fixed_source,
            patch_artifact_kind,
        };
        fx.close().await;
        Some(result)
    }

    /// `CostMeterFactory` snapshot helper: the run meter accumulated by both
    /// generations.
    trait MeterSnapshot {
        fn meter_for_snapshot(&self, run: RunId) -> CostSnapshot;
    }

    impl MeterSnapshot for ProcessCostMeterFactory {
        fn meter_for_snapshot(&self, run: RunId) -> CostSnapshot {
            use alloy_runtime::CostMeterFactory;
            self.meter_for(run).snapshot()
        }
    }

    // --- T20 ----------------------------------------------------------------

    #[tokio::test]
    async fn repair_local_diagnostic_e2e_with_scripted_provider() {
        let Some(result) = run_repair_flow().await else {
            return;
        };

        // The analyze payload decodes as a RepairPlanPayload.
        let plan: RepairPlanPayload =
            serde_json::from_value(result.analyze_payload.clone()).unwrap();
        assert_eq!(plan.capability, "repair");
        assert_eq!(plan.target_files, vec!["src/main.rs"]);
        assert!(!plan.diagnostics_addressed.is_empty());

        // The edit payload decodes as the capability EditAppliedPayload with
        // backend-reported paths and a persisted patch artifact.
        let applied: CapEditAppliedPayload =
            serde_json::from_value(result.edit_payload.clone()).unwrap();
        assert_eq!(applied.capability, "edit");
        assert_eq!(applied.files_touched, vec!["src/main.rs"]);
        assert!(applied.transaction_id.is_some());
        assert!(!applied.dry_run);
        assert_eq!(result.patch_artifact_kind, ArtifactKind::Patch);

        // The fixture crate now compiles — the real cargo_check passed in
        // generation 2 (the DAG reached Succeeded) and the fix is on disk.
        assert!(result.fixed_source.contains("let x: i32 = 42;"));

        // Exactly two model calls on the meter (BG2: metered once, by the
        // router), two worker_attempt records, two ModelCall records, no
        // duplicates.
        assert_eq!(result.meter.model_calls, 2);
        assert_eq!(result.worker_attempts, 2);
        assert_eq!(result.model_calls_recorded, 2);
    }

    // --- T21 ----------------------------------------------------------------

    /// Mask run-variant fields (ids and durations) per RFC-0013 T21.
    fn mask(value: &serde_json::Value) -> serde_json::Value {
        let mut value = value.clone();
        if let Some(metrics) = value.get_mut("metrics") {
            metrics["duration_ms"] = serde_json::json!(0);
        }
        for id_field in ["patch_artifact", "transaction_id"] {
            if value.get(id_field).is_some() {
                value[id_field] = serde_json::json!("masked");
            }
        }
        if value.get("artifacts").is_some() {
            value["artifacts"] = serde_json::json!([]);
        }
        value
    }

    #[tokio::test]
    async fn scripted_repair_run_is_deterministic() {
        let Some(first) = run_repair_flow().await else {
            return;
        };
        let Some(second) = run_repair_flow().await else {
            return;
        };
        assert_eq!(
            mask(&first.analyze_payload),
            mask(&second.analyze_payload),
            "analyze payloads must match after masking ids and durations"
        );
        assert_eq!(
            mask(&first.edit_payload),
            mask(&second.edit_payload),
            "edit payloads must match after masking ids and durations"
        );
    }

    // --- T23 ----------------------------------------------------------------

    /// Live-provider smoke (RFC-0013 T23). `#[ignore]`d: run only with
    /// credentials configured — `ALLOY_LIVE_ROUTER_TOML` pointing at a real
    /// `router.toml` whose `api_key_env` is set. Asserts a real completion
    /// parses under PS1/PS2 so scripted and live runs stay reportable
    /// separately (roadmap M7).
    #[tokio::test]
    #[ignore = "requires live provider credentials (ALLOY_LIVE_ROUTER_TOML)"]
    async fn live_provider_smoke() {
        use alloy_runtime::{ModelRouter, RoutingRequest, TomlModelRouter};

        let Some(router_path) = std::env::var_os("ALLOY_LIVE_ROUTER_TOML") else {
            panic!("set ALLOY_LIVE_ROUTER_TOML to run the live smoke");
        };
        let decisions = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let meter = alloy_runtime::SharedCostMeter::new();
        let run = RunId::new();
        let router = TomlModelRouter::from_paths(
            Path::new(&router_path),
            BudgetPolicy::default(),
            Path::new("example.env"),
            decisions,
            meter,
            run,
        )
        .unwrap();

        let request = worker_request("repair", REPAIR_SYSTEM).await;
        let routed = router
            .route(RoutingRequest {
                session: SessionId::new(),
                run: Some(run),
                node: None,
                capability: CapabilityId::new("repair").unwrap(),
                tier_override: None,
                complexity: None,
                budget_remaining: alloy_runtime::BudgetSnapshot {
                    usd_spent: 0.0,
                    tokens_in: 0,
                    tokens_out: 0,
                },
                requires_tools: false,
                requires_structured_output: true,
                response_schema: None,
            })
            .await
            .unwrap();
        let response = router
            .complete(
                &routed,
                alloy_runtime::PromptPack {
                    messages: request.messages,
                    citations: vec![],
                    domains: None,
                },
            )
            .await
            .unwrap();
        // PS1/PS2: structured object, or a fenced/whole-body JSON object.
        let parsed = response
            .structured
            .as_ref()
            .is_some_and(serde_json::Value::is_object)
            || response.text.as_deref().is_some_and(|t| {
                serde_json::from_str::<serde_json::Value>(t.trim())
                    .map(|v| v.is_object())
                    .unwrap_or(false)
                    || t.contains("```json")
            });
        assert!(
            parsed,
            "live completion did not parse under PS1/PS2: {response:?}"
        );
    }
}
