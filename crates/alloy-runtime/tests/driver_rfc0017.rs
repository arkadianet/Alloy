//! RFC-0017 `GenerationDriver` integration suite (§12.3): the bounded
//! repair-generation loop driven through the real `RunController::start`,
//! with a scripted scheduler and the real `TemplatePlanService` replan path.
//!
//! Author: arkadianet

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_runtime::{
    compiler_fingerprint_digest, install_sqlite_event_sink, policy_hash_digest,
    tool_versions_digest, AlloyRuntime, AlloyStorage, ArtifactStore, BudgetPolicy,
    CostMeterFactory, CreateSession, DagId, DagOutcome, DagState, DagStore, DecisionLog,
    DecisionRecord, DiagnosticEvent, DiagnosticId, DiagnosticLevel, Digest, ErrorClass, EventStore,
    FailureIr, GenerationDriver, GenerationDriverDeps, GenerationPolicy, Goal, LanguageId,
    ModelCallRecord, NodeId, NodeKind, ObsError, PlanContext, PlanFingerprints,
    PlanProducedPayload, PlanService, ProcessCostMeterFactory, ProfileId, RunId, RuntimeConfig,
    RuntimeEvent, RuntimeHandle, SchedError, Scheduler, SessionEventType, SessionId, SessionPlane,
    SessionRows, StorageOpenOptions, TemplatePlanService, ToolCallRecord, ToolchainRecord,
};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Which node of the current-generation blob a scripted failure targets.
#[derive(Debug, Clone, Copy)]
enum NodeSel {
    VerifyCompile,
    VerifyTest,
    Edit,
}

/// One scripted `run_within` response.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// `Ok(Succeeded)`.
    Succeed,
    /// `Ok(ReplanRequired)` — the external path (GN9).
    ReplanRequired,
    /// Mark the blob Failed and return a `FailureIr` for the selected node.
    Fail {
        node: NodeSel,
        class: ErrorClass,
        diags: usize,
    },
    /// Sleep (real time — the deadline is a std `Instant`), then `Fail` at
    /// the verify node with compile diagnostics.
    SleepThenFailCompile { ms: u64, diags: usize },
    /// `Err(SchedError::Internal)` — simulates a crash mid-loop (CR4).
    ErrInternal,
    /// Flip the run row to `cancelling` first, then fail at the verify
    /// node — GN6's `control_state` half.
    CancelRowThenFailCompile,
}

struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    remaining_args: Mutex<Vec<Duration>>,
    /// Run-row state observed at each dispatch (AC 21b probe).
    states_at_dispatch: Mutex<Vec<String>>,
    dag_store: Arc<dyn alloy_runtime::DagStore>,
    session_rows: Arc<dyn SessionRows>,
    run: Mutex<Option<RunId>>,
}

impl Scripted {
    fn new(storage: &Arc<AlloyStorage>, steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            remaining_args: Mutex::new(Vec::new()),
            states_at_dispatch: Mutex::new(Vec::new()),
            dag_store: storage.dags() as _,
            session_rows: storage.sessions() as _,
            run: Mutex::new(None),
        })
    }

    fn track(&self, run: RunId) {
        *self.run.lock().unwrap() = Some(run);
    }

    fn remaining(&self) -> Vec<Duration> {
        self.remaining_args.lock().unwrap().clone()
    }

    fn dispatch_states(&self) -> Vec<String> {
        self.states_at_dispatch.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.remaining_args.lock().unwrap().len()
    }

    async fn fail_outcome(
        &self,
        dag_id: DagId,
        sel: NodeSel,
        class: ErrorClass,
        diags: usize,
    ) -> DagOutcome {
        let mut dag = self
            .dag_store
            .get(dag_id)
            .await
            .unwrap()
            .expect("scripted failure needs a stored dag");
        let want = match sel {
            NodeSel::VerifyCompile => NodeKind::VerifyCompile,
            NodeSel::VerifyTest => NodeKind::VerifyTest,
            NodeSel::Edit => NodeKind::Edit,
        };
        let node = dag
            .nodes
            .values()
            .find(|n| n.kind == want)
            .map(|n| n.id)
            .expect("selected node kind present in blob");
        dag.state = DagState::Failed;
        let store = Arc::clone(&self.dag_store);
        store.put(&dag).await.unwrap();
        DagOutcome {
            dag_id,
            generation: dag.generation,
            state: DagState::Failed,
            failed_node: Some(node),
            failure: Some(failure_ir(node, class, diags)),
        }
    }
}

#[async_trait]
impl Scheduler for Scripted {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        self.run_within(dag_id, Duration::from_secs(3600)).await
    }

    async fn run_within(
        &self,
        dag_id: DagId,
        remaining: Duration,
    ) -> Result<DagOutcome, SchedError> {
        self.remaining_args.lock().unwrap().push(remaining);
        let tracked = { *self.run.lock().unwrap() };
        if let Some(run) = tracked {
            let row = self.session_rows.get_run(run).await.unwrap().unwrap();
            self.states_at_dispatch.lock().unwrap().push(row.state);
        }
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        let ok = |state| DagOutcome {
            dag_id,
            generation: 1,
            state,
            failed_node: None,
            failure: None,
        };
        match step {
            Step::Succeed => Ok(ok(DagState::Succeeded)),
            Step::ReplanRequired => Ok(ok(DagState::ReplanRequired)),
            Step::Fail { node, class, diags } => {
                Ok(self.fail_outcome(dag_id, node, class, diags).await)
            }
            Step::SleepThenFailCompile { ms, diags } => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(self
                    .fail_outcome(dag_id, NodeSel::VerifyCompile, ErrorClass::Compile, diags)
                    .await)
            }
            Step::ErrInternal => Err(SchedError::Internal("scripted crash".into())),
            Step::CancelRowThenFailCompile => {
                let run = { (*self.run.lock().unwrap()).expect("tracked run") };
                let row = self.session_rows.get_run(run).await.unwrap().unwrap();
                let mut cancelling = row.clone();
                cancelling.state = "cancelling".into();
                self.session_rows.upsert_run(&cancelling).await.unwrap();
                Ok(self
                    .fail_outcome(dag_id, NodeSel::VerifyCompile, ErrorClass::Compile, 1)
                    .await)
            }
        }
    }

    async fn cancel(&self, _dag_id: DagId) -> Result<(), SchedError> {
        Ok(())
    }
}

fn diag(message: &str) -> DiagnosticEvent {
    DiagnosticEvent {
        id: DiagnosticId::new(),
        code: Some("E0308".into()),
        level: DiagnosticLevel::Error,
        message: message.into(),
        spans: vec![],
        children: vec![],
        package: Some("alloy-runtime".into()),
        fingerprint: alloy_runtime::hash_prompt("driver-e2e"),
        raw_json: None,
    }
}

fn failure_ir(node: NodeId, class: ErrorClass, diags: usize) -> FailureIr {
    FailureIr {
        node,
        error_class: class,
        retry: Default::default(),
        diagnostics: (0..diags)
            .map(|i| diag(&format!("mismatched types {i}")))
            .collect(),
        notes: "cargo check failed".into(),
    }
}

fn fingerprints() -> (Digest, Digest, Digest) {
    let toolchain = ToolchainRecord {
        channel: "1.97.1".into(),
        rustc_version: "rustc 1.97.1 (test)".into(),
        cargo_version: "cargo 1.97.1 (test)".into(),
    };
    (
        policy_hash_digest(
            &ProfileId::new("default").unwrap(),
            &BudgetPolicy::default(),
        ),
        tool_versions_digest(&toolchain),
        compiler_fingerprint_digest(&toolchain, "x86_64-unknown-linux-gnu"),
    )
}

/// A decision log that always fails (AC 45 / GN12).
struct FailingDecisionLog;

#[async_trait]
impl DecisionLog for FailingDecisionLog {
    async fn record(&self, _rec: DecisionRecord) -> Result<alloy_runtime::EventSeq, ObsError> {
        Err(ObsError::Invalid("injected decision failure".into()))
    }
    async fn record_model_call(
        &self,
        _rec: ModelCallRecord,
    ) -> Result<alloy_runtime::EventSeq, ObsError> {
        Err(ObsError::Invalid("injected decision failure".into()))
    }
    async fn record_tool_call(
        &self,
        _rec: ToolCallRecord,
    ) -> Result<alloy_runtime::EventSeq, ObsError> {
        Err(ObsError::Invalid("injected decision failure".into()))
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    rt: AlloyRuntime,
    handle: RuntimeHandle,
    storage: Arc<AlloyStorage>,
    plane: SessionPlane,
    plans: Arc<TemplatePlanService>,
    driver: Arc<GenerationDriver>,
    cancellation: CancellationToken,
    cost_meters: Arc<ProcessCostMeterFactory>,
    budget_policy: BudgetPolicy,
}

struct HarnessOptions {
    max_repair_generations: u32,
    run_timeout: Duration,
    budget_policy: BudgetPolicy,
    failing_decisions: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_repair_generations: 2,
            run_timeout: Duration::from_secs(30),
            budget_policy: BudgetPolicy::default(),
            failing_decisions: false,
        }
    }
}

impl Harness {
    async fn new(options: HarnessOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let mut rt = AlloyRuntime::new();
        rt.configure(RuntimeConfig {
            data_dir: data_dir.clone(),
            data_dir_rule: "test",
            profile_path: dir.path().join("profiles/default.toml"),
            router_path: dir.path().join("router.toml"),
            env_file_hint: dir.path().join("example.env"),
            retain_full_prompts: false,
            retain_tool_bodies: false,
            run_timeout: options.run_timeout,
            budget_policy: options.budget_policy.clone(),
            context_profile: alloy_runtime::ContextProfile::v2_defaults(),
            profile_id: Some("default".into()),
            gates: Default::default(),
            sandbox_echo: None,
            gate_timeout: None,
            max_repair_generations: options.max_repair_generations,
            capture: Default::default(),
            planner: alloy_runtime::PlannerConfig::new(),
        })
        .unwrap();
        let handle = rt.start().await.unwrap();
        let storage =
            install_sqlite_event_sink(&handle, Some(StorageOpenOptions::for_data_dir(data_dir)))
                .await
                .unwrap();
        let plane = SessionPlane::new(handle.clone(), Arc::clone(&storage));
        let plans = Arc::new(TemplatePlanService::from_storage(&storage));
        let cancellation = CancellationToken::new();
        let cost_meters = Arc::new(ProcessCostMeterFactory::new());
        let decisions: Arc<dyn DecisionLog> = if options.failing_decisions {
            Arc::new(FailingDecisionLog)
        } else {
            Arc::new(alloy_runtime::EventDecisionLog::new(
                handle.clone(),
                Arc::clone(&storage),
                alloy_runtime::RetentionPolicy::defaults(),
            ))
        };
        let (policy_hash, tool_versions, compiler_fingerprint) = fingerprints();
        let driver = Arc::new(GenerationDriver::new(GenerationDriverDeps {
            handle: handle.clone(),
            plans: Arc::clone(&plans) as _,
            runs: plane.runs(),
            dags: storage.dags() as _,
            sessions: storage.sessions() as _,
            events: storage.events() as _,
            decisions,
            cost_meters: Arc::clone(&cost_meters) as _,
            budget_policy: options.budget_policy.clone(),
            cancellation: cancellation.clone(),
            fingerprints: PlanFingerprints {
                policy_hash,
                tool_versions,
                compiler_fingerprint,
            },
            policy: GenerationPolicy {
                max_repair_generations: options.max_repair_generations,
            },
        }));
        plane.set_executor(Arc::clone(&driver) as _);
        Self {
            _dir: dir,
            rt,
            handle,
            storage,
            plane,
            plans,
            driver,
            cancellation,
            cost_meters,
            budget_policy: options.budget_policy,
        }
    }

    fn install(&self, sched: Arc<Scripted>) {
        self.handle.set_scheduler(sched as _).unwrap();
    }

    /// Create a session, submit a goal, and plan generation 1.
    async fn planned_run(&self) -> (SessionId, RunId, DagId) {
        let session = self
            .plane
            .sessions()
            .create(CreateSession {
                workspace_root: self._dir.path().to_path_buf(),
                profile: ProfileId::new("default").unwrap(),
                budget: BudgetPolicy::default(),
                language_backends: vec![LanguageId::new("rust").unwrap()],
                provenance: None,
            })
            .await
            .unwrap();
        let run = self
            .plane
            .sessions()
            .submit_goal(
                session,
                Goal {
                    text: "fix the build".into(),
                    constraints: vec![],
                    attachments: vec![],
                },
            )
            .await
            .unwrap();
        let row = self.storage.sessions().get_run(run).await.unwrap().unwrap();
        let record: alloy_runtime::RunGoalRecord = serde_json::from_value(row.goal_json).unwrap();
        let dag_id = record.dag_id;
        self.plans
            .plan(self.plan_ctx(session, run, dag_id))
            .await
            .unwrap();
        (session, run, dag_id)
    }

    fn plan_ctx(&self, session: SessionId, run: RunId, dag: DagId) -> PlanContext {
        let (policy_hash, tool_versions, compiler_fingerprint) = fingerprints();
        PlanContext {
            session_id: session,
            run_id: run,
            dag_id: dag,
            goal: Goal {
                text: "fix the build".into(),
                constraints: vec![],
                attachments: vec![],
            },
            template_override: None,
            policy_hash,
            tool_versions,
            compiler_fingerprint,
            prior_source: None,
            prior_proposal_artifact: None,
        }
    }

    async fn run_state(&self, run: RunId) -> String {
        self.storage
            .sessions()
            .get_run(run)
            .await
            .unwrap()
            .unwrap()
            .state
    }

    async fn session_events(&self, session: SessionId) -> Vec<alloy_runtime::SessionEvent> {
        self.storage
            .events()
            .list_session_events(session, None, 1000)
            .await
            .unwrap()
    }

    /// Parsed `Replan` decision metadata objects, in append order.
    async fn replan_decisions(&self, session: SessionId) -> Vec<serde_json::Value> {
        self.session_events(session)
            .await
            .into_iter()
            .filter(|e| e.type_ == SessionEventType::Decision)
            .filter_map(|e| {
                (e.payload.get("kind") == Some(&json!("replan")))
                    .then(|| e.payload.get("metadata").cloned().unwrap_or_default())
            })
            .collect()
    }

    async fn runtime_events(&self) -> Vec<RuntimeEvent> {
        self.storage
            .events()
            .list_runtime_events(None, 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|(_rowid, ev)| ev)
            .collect()
    }

    async fn lifecycle_counts(&self, run: RunId, session: SessionId) -> (usize, usize, usize) {
        let accepted = self
            .runtime_events()
            .await
            .iter()
            .filter(|ev| matches!(ev, RuntimeEvent::RunAccepted { run_id, .. } if *run_id == run))
            .count();
        let finished = self
            .runtime_events()
            .await
            .iter()
            .filter(|ev| matches!(ev, RuntimeEvent::RunFinished { run_id, .. } if *run_id == run))
            .count();
        let completed = self
            .session_events(session)
            .await
            .iter()
            .filter(|e| e.type_ == SessionEventType::RunCompleted && e.run_id == Some(run))
            .count();
        (accepted, completed, finished)
    }

    async fn close(self) {
        let Self { rt, storage, .. } = self;
        rt.shutdown().await.unwrap();
        storage.close().await.unwrap();
    }
}

/// AC 21: scripted Fail(Compile, diags) then Succeed → one bump, final
/// `Succeeded`, decisions `Replan{admitted:true}` then none.
#[tokio::test]
async fn ac21_fail_then_succeed_bumps_once() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 2,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 2, "exactly two generations dispatched");
    assert_eq!(h.run_state(run).await, "succeeded");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1, "one admission decision only");
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[0]["to_generation"], json!(2));
    let m = h.driver.metrics();
    assert_eq!(m.replans_admitted, 1);
    assert_eq!(m.generations_run, 2);
    h.close().await;
}

/// AC 21b (blocker 1): a 2-generation run through `RunController::start`
/// emits exactly one `RunAccepted` / `RunCompleted` / `RunFinished` and
/// writes a terminal row once; the run row reads `running` at the second
/// dispatch — never `created`, never `replan_requested`. 1- and
/// 3-generation runs emit the same lifecycle multiset.
#[tokio::test]
async fn ac21b_lifecycle_single_sourced_across_generations() {
    for steps in [
        vec![Step::Succeed],
        vec![
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Succeed,
        ],
        vec![
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Succeed,
        ],
    ] {
        let generations = steps.len();
        let h = Harness::new(HarnessOptions::default()).await;
        let (session, run, _dag) = h.planned_run().await;
        let sched = Scripted::new(&h.storage, steps);
        sched.track(run);
        h.install(Arc::clone(&sched));

        h.plane.runs().start(run).await.unwrap();

        assert_eq!(sched.calls(), generations);
        let (accepted, completed, finished) = h.lifecycle_counts(run, session).await;
        assert_eq!(
            (accepted, completed, finished),
            (1, 1, 1),
            "{generations}-generation run must emit the single lifecycle triple"
        );
        assert_eq!(h.run_state(run).await, "succeeded");
        let states = sched.dispatch_states();
        for state in &states[1..] {
            assert_eq!(
                state, "running",
                "row must read running between generations (RC1)"
            );
        }
        assert!(states
            .iter()
            .all(|s| s != "created" && s != "replan_requested"));
        h.close().await;
    }
}

/// AC 23: GN2 (Edit / VerifyTest failures) and GN3 (`Tool` class) and GN4
/// (empty diagnostics) never bump; the decision names the rule.
#[tokio::test]
async fn ac23_kind_class_and_empty_diagnostics_never_bump() {
    for (step, reason) in [
        (
            Step::Fail {
                node: NodeSel::Edit,
                class: ErrorClass::Model,
                diags: 1,
            },
            "kind",
        ),
        (
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Tool,
                diags: 1,
            },
            "class",
        ),
        (
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 0,
            },
            "no_diagnostics",
        ),
    ] {
        let h = Harness::new(HarnessOptions::default()).await;
        let (session, run, _dag) = h.planned_run().await;
        let sched = Scripted::new(&h.storage, [step]);
        sched.track(run);
        h.install(Arc::clone(&sched));

        h.plane.runs().start(run).await.unwrap();

        assert_eq!(sched.calls(), 1, "no second generation for {reason}");
        assert_eq!(h.run_state(run).await, "failed");
        let decisions = h.replan_decisions(session).await;
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["admitted"], json!(false));
        assert_eq!(decisions[0]["reason"], json!(reason));
        h.close().await;
    }

    // GN2's VerifyTest exclusion needs a VerifyTest node; the repair
    // template has none, so flip the stored verify node's kind first.
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, dag_id) = h.planned_run().await;
    let mut dag = h.storage.dags().get(dag_id).await.unwrap().unwrap();
    let verify = dag
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::VerifyCompile)
        .map(|n| n.id)
        .unwrap();
    dag.nodes.get_mut(&verify).unwrap().kind = NodeKind::VerifyTest;
    h.storage.dags().put(&dag).await.unwrap();
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::VerifyTest,
            class: ErrorClass::Compile,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 1, "a VerifyTest failure never bumps (day-1)");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["reason"], json!("kind"));
    h.close().await;
}

/// AC 24 / GN5 / GN11: with the default bound of 2, the third verify Fail
/// returns the final Failed outcome with its `FailureIr` intact and records
/// `Replan{admitted:false, reason:"exhausted"}`.
#[tokio::test]
async fn ac24_exhaustion_is_an_outcome() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let fail = Step::Fail {
        node: NodeSel::VerifyCompile,
        class: ErrorClass::Compile,
        diags: 1,
    };
    let sched = Scripted::new(&h.storage, [fail, fail, fail]);
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 3, "three generations, then exhaustion");
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 3);
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[1]["admitted"], json!(true));
    assert_eq!(decisions[2]["admitted"], json!(false));
    assert_eq!(decisions[2]["reason"], json!("exhausted"));
    // The final outcome kept its FailureIr: RunFinished carries it.
    let finished = h
        .runtime_events()
        .await
        .into_iter()
        .find_map(|ev| match ev {
            RuntimeEvent::RunFinished { run_id, outcome } if run_id == run => Some(outcome),
            _ => None,
        })
        .unwrap();
    assert_eq!(finished.state, DagState::Failed);
    assert!(finished.failure.is_some(), "FailureIr intact on exhaustion");
    let (accepted, completed, finished_n) = h.lifecycle_counts(run, session).await;
    assert_eq!((accepted, completed, finished_n), (1, 1, 1));
    h.close().await;
}

/// AC 25 / GN6: a cancelling run row, a fired token, and an exhausted
/// budget each decline the bump with the named reason.
#[tokio::test]
async fn ac25_cancel_and_budget_decline() {
    // control_state == Cancelling.
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(&h.storage, [Step::CancelRowThenFailCompile]);
    sched.track(run);
    h.install(Arc::clone(&sched));
    // The cancelling row wins the §6.3 step-9 merge; start still returns Ok.
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 1);
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["reason"], json!("cancelled"));
    h.close().await;

    // Fired cancellation token.
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    h.cancellation.cancel();
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::VerifyCompile,
            class: ErrorClass::Compile,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 1);
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["reason"], json!("cancelled"));
    h.close().await;

    // Budget exhausted (GN6's second half).
    let policy = BudgetPolicy {
        max_tokens_per_run: 10,
        ..BudgetPolicy::default()
    };
    let h = Harness::new(HarnessOptions {
        budget_policy: policy,
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let meter = h.cost_meters.meter_for(run);
    meter.add_model_usage(
        alloy_runtime::ModelTier::Standard,
        Some(100),
        Some(100),
        None,
    );
    assert_ne!(
        meter.check_budget(&h.budget_policy),
        alloy_runtime::BudgetCheck::Ok
    );
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::VerifyCompile,
            class: ErrorClass::Compile,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 1);
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["reason"], json!("budget"));
    h.close().await;
}

/// AC 25b / GN7: the absolute deadline shrinks across generations — the
/// second dispatch receives strictly less than the first, and once the
/// budget is spent the next bump is refused with reason `deadline`. Total
/// generations never restart the clock.
#[tokio::test]
async fn ac25b_absolute_deadline_shrinks_and_refuses() {
    let t = Duration::from_millis(1000);
    let h = Harness::new(HarnessOptions {
        run_timeout: t,
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::SleepThenFailCompile { ms: 600, diags: 1 },
            Step::SleepThenFailCompile { ms: 500, diags: 1 },
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    let started = std::time::Instant::now();
    h.plane.runs().start(run).await.unwrap();
    let wall = started.elapsed();

    let remaining = sched.remaining();
    assert_eq!(remaining.len(), 2, "third dispatch refused");
    assert!(
        remaining[1] < remaining[0],
        "remaining must strictly decrease: {remaining:?}"
    );
    assert!(
        remaining[1] <= Duration::from_millis(450),
        "second generation gets the remainder, not a fresh run_timeout: {remaining:?}"
    );
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.last().unwrap()["reason"], json!("deadline"));
    // ≤ T + scheduling slack: nowhere near 2×run_timeout.
    assert!(
        wall < t + Duration::from_millis(700),
        "wall clock bounded by the absolute deadline, got {wall:?}"
    );
    h.close().await;
}

/// AC 26 / GN10: template-sourced runs replan with the prior template and
/// the decision records `provenance: "preserved"`.
#[tokio::test]
async fn ac26_template_provenance_preserved() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    let plans: Vec<PlanProducedPayload> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::PlanProduced)
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[1].template_id, plans[0].template_id);
    assert!(plans[1].replan);
    assert_eq!(plans[1].seeded_root, Some(true));
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["provenance"], json!("preserved"));
    h.close().await;
}

/// AC 27 / GN8: the event log shows `ReplanRequested` →
/// `PlanProduced{replan:true}` → `ReplanResumed`, then the next generation,
/// with the run row `running` throughout the between-generation window.
#[tokio::test]
async fn ac27_replan_event_ordering() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    let events = h.session_events(session).await;
    let pos = |ty: SessionEventType, replan: Option<bool>| {
        events
            .iter()
            .position(|e| {
                e.type_ == ty
                    && replan.is_none_or(|want| e.payload.get("replan") == Some(&json!(want)))
            })
            .unwrap_or_else(|| panic!("missing {ty:?}"))
    };
    let requested = pos(SessionEventType::ReplanRequested, None);
    let produced = pos(SessionEventType::PlanProduced, Some(true));
    let resumed = pos(SessionEventType::ReplanResumed, None);
    assert!(
        requested < produced && produced < resumed,
        "GN8 ordering violated: requested={requested} produced={produced} resumed={resumed}"
    );
    assert_eq!(sched.dispatch_states()[1], "running");
    h.close().await;
}

/// AC 28 / GN9: an externally requested replan surfacing as
/// `ReplanRequired` passes through `execute` unconverted; §6.3 step 10 maps
/// it to `replan_requested`.
#[tokio::test]
async fn ac28_replan_required_passes_through() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(&h.storage, [Step::ReplanRequired]);
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 1);
    assert_eq!(h.run_state(run).await, "replan_requested");
    assert!(h.replan_decisions(session).await.is_empty());
    h.close().await;
}

/// AC 30 / CR2 / CR4: a run "crashed" after `replan` +
/// `complete_repair_generation` but before the next dispatch leaves a
/// coherent durable state — row `running`, DAG Pending at generation 2 with
/// a seeded root. Resume rearms to `accepted`; `start` re-dispatches
/// generation 2 with its seed intact; no `replan_requested` row was ever
/// written.
#[tokio::test]
async fn ac30_crash_after_replan_resumes_seeded_generation() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, dag_id) = h.planned_run().await;
    let fail = Step::Fail {
        node: NodeSel::VerifyCompile,
        class: ErrorClass::Compile,
        diags: 1,
    };
    // Generation 1 fails and is admitted; the replan lands; the next
    // dispatch "crashes" the process (scripted scheduler error).
    let sched = Scripted::new(&h.storage, [fail, Step::ErrInternal]);
    sched.track(run);
    h.install(Arc::clone(&sched));
    let err = h.plane.runs().start(run).await.unwrap_err();
    let _ = err; // infrastructure error surfaced; durable state is the point.

    // CR4's durable shape: row running, DAG Pending at generation 2.
    assert_eq!(h.run_state(run).await, "running");
    let dag = h.storage.dags().get(dag_id).await.unwrap().unwrap();
    assert_eq!(dag.generation, 2);
    assert_eq!(dag.state, DagState::Pending);
    // The seeded root survived: its input envelope is FromPredecessors.
    let root = dag
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Analyze)
        .unwrap();
    let blob = h.storage.artifacts().get(root.input_ref).await.unwrap();
    let env: alloy_runtime::NodeInputEnvelope = serde_json::from_slice(&blob.bytes).unwrap();
    assert!(matches!(
        env.payload,
        alloy_runtime::NodeInputPayload::FromPredecessors { .. }
    ));

    // Resume rearm (running → accepted), then start re-dispatches
    // generation 2 — bumps restarts at 0, seed intact, single lifecycle.
    h.plane.sessions().resume(session).await.unwrap();
    assert_eq!(h.run_state(run).await, "accepted");
    let sched2 = Scripted::new(&h.storage, [Step::Succeed]);
    sched2.track(run);
    h.install(Arc::clone(&sched2));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, "succeeded");
    let (accepted, completed, finished) = h.lifecycle_counts(run, session).await;
    assert_eq!((accepted, completed, finished), (1, 1, 1));
    // The states observed at every dispatch never included replan_requested.
    assert!(sched
        .dispatch_states()
        .iter()
        .all(|s| s != "replan_requested"));
    assert!(sched2
        .dispatch_states()
        .iter()
        .all(|s| s != "replan_requested"));
    h.close().await;
}

/// AC 31 (driver half) / RX4: `max_repair_generations = 0` executes exactly
/// one generation — the driver short-circuits, no executor swap.
#[tokio::test]
async fn ac31_zero_bound_single_generation() {
    let h = Harness::new(HarnessOptions {
        max_repair_generations: 0,
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::VerifyCompile,
            class: ErrorClass::Compile,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 1, "bound 0 disables auto-replan");
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[0]["reason"], json!("exhausted"));
    h.close().await;
}

/// AC 45 (driver half) / GN12: a decision log that always errors never
/// fails a plan or a generation; the §9.4 counters still increment.
#[tokio::test]
async fn ac45_failing_decision_log_never_fails_a_generation() {
    let h = Harness::new(HarnessOptions {
        failing_decisions: true,
        ..HarnessOptions::default()
    })
    .await;
    let (_session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(h.run_state(run).await, "succeeded");
    let m = h.driver.metrics();
    assert_eq!(m.replans_admitted, 1);
    assert_eq!(m.generations_run, 2);
    h.close().await;
}
