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
    tool_versions_digest, AdapterError, AlloyRuntime, AlloyStorage, ArtifactStore, BudgetPolicy,
    CostMeterFactory, CreateSession, DagId, DagOutcome, DagState, DagStore, DecisionLog,
    DecisionRecord, DiagnosticEvent, DiagnosticId, DiagnosticLevel, Digest, EditContext,
    EditEngine, EditError, EditRequest, EditTransaction, EditValidation, ErrorClass, EventSink,
    EventStore, FailureIr, FileChange, FixEvent, GenerationDriver, GenerationDriverDeps,
    GenerationPolicy, Goal, GraphError, GraphQuery, GraphSnapshotId, GraphVersion, GraphView,
    GraphViewHandle, LanguageId, ModelCallRecord, NewSessionEvent, NodeExecContext, NodeExecRef,
    NodeId, NodeKind, ObsError, PermissionToken, PlanContext, PlanFingerprints,
    PlanProducedPayload, PlanService, ProcessCostMeterFactory, ProfileId, ProjectGraph, RunId,
    RuntimeConfig, RuntimeEvent, RuntimeHandle, SchedError, Scheduler, SessionEventType, SessionId,
    SessionPlane, SessionRows, StorageOpenOptions, TemplatePlanService, ToolCallRecord,
    ToolchainRecord, TransactionId, Verdict, VerdictOutcome, Verifier, WorkerPermissions,
    WorkerToolClass,
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
    Analyze,
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
            NodeSel::Analyze => NodeKind::Analyze,
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
        let ok = |generation, state| DagOutcome {
            dag_id,
            generation,
            state,
            failed_node: None,
            failure: None,
        };
        match step {
            Step::Succeed | Step::ReplanRequired => {
                let generation = self
                    .dag_store
                    .get(dag_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|d| d.generation)
                    .unwrap_or(1);
                let state = if matches!(step, Step::Succeed) {
                    DagState::Succeeded
                } else {
                    DagState::ReplanRequired
                };
                Ok(ok(generation, state))
            }
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

// ---- GN13 fakes: enough machinery for `restore_workspace_and_reseed` to
// take its success path (rollback the journaled edit, re-verify against the
// "restored" tree) so the driver observes a replaced seed.

/// Edit engine whose `rollback` always succeeds; apply/validate are never
/// reached by the driver.
struct RollbackOkEditEngine;

#[async_trait]
impl EditEngine for RollbackOkEditEngine {
    async fn validate(
        &self,
        _req: EditRequest,
        _ctx: &EditContext,
    ) -> Result<EditValidation, EditError> {
        unreachable!("driver never validates")
    }

    async fn apply(
        &self,
        _req: EditRequest,
        _ctx: &EditContext,
    ) -> Result<EditTransaction, EditError> {
        unreachable!("driver never applies")
    }

    async fn rollback(&self, _tx: TransactionId, _ctx: &EditContext) -> Result<(), EditError> {
        Ok(())
    }
}

/// Permission source that mints an empty-grant token for any request.
struct GrantAllPerms;

#[async_trait]
impl WorkerPermissions for GrantAllPerms {
    async fn token_for(
        &self,
        ctx: &NodeExecRef,
        _class: WorkerToolClass,
    ) -> Result<PermissionToken, AdapterError> {
        Ok(PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![],
            expires: None,
            run_id: ctx.run_id,
        })
    }
}

/// Post-rollback verifier: fails compile with the RESTORED tree's original
/// diagnostic, i.e. a different failure than the admitted (post-edit) seed.
struct RestoredTreeVerifier;

#[async_trait]
impl Verifier for RestoredTreeVerifier {
    async fn verify(&self, _ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
        Ok(Verdict {
            outcome: VerdictOutcome::Fail,
            diagnostics: vec![diag("original error: missing lifetime specifier")],
            raw_artifact: None,
        })
    }
}

/// Read-only graph fake for the AM-0017-1 graph-channel seed: answers
/// `Diagnostics` with the scripted run-start evidence (what the issue-#53
/// `seed_graph_diagnostics` pass would have recorded), everything else
/// empty; writes are `Disabled` like `NullProjectGraph`'s.
struct SeededGraph {
    diagnostics: Vec<DiagnosticEvent>,
}

#[async_trait]
impl ProjectGraph for SeededGraph {
    async fn rebuild(&self, _root: &std::path::Path) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn apply_incremental(&self, _changes: &[FileChange]) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError> {
        let mut view = GraphView::empty(GraphVersion(1));
        if matches!(q, GraphQuery::Diagnostics { .. }) {
            view.diagnostics = self.diagnostics.clone();
        }
        Ok(view)
    }
    async fn record_diagnostic(&self, _d: DiagnosticEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }
    async fn record_fix(&self, _f: FixEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
        Err(GraphError::Disabled)
    }
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
    /// Wire the GN13 fakes (rollback-ok edit engine, grant-all perms,
    /// restored-tree verifier) so `restore_workspace_and_reseed` replaces
    /// the admitted seed instead of no-opping.
    gn13_fakes: bool,
    /// `Some` wires a [`SeededGraph`] holding these diagnostics as the
    /// run-start graph-channel evidence; `None` wires the null handle
    /// (graph absent — every pre-existing test's shape).
    graph_diagnostics: Option<Vec<DiagnosticEvent>>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_repair_generations: 2,
            run_timeout: Duration::from_secs(30),
            budget_policy: BudgetPolicy::default(),
            failing_decisions: false,
            gn13_fakes: false,
            graph_diagnostics: None,
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
            edit_engine: options
                .gn13_fakes
                .then(|| Arc::new(RollbackOkEditEngine) as _),
            worker_permissions: options.gn13_fakes.then(|| Arc::new(GrantAllPerms) as _),
            verify_compile: options
                .gn13_fakes
                .then(|| Arc::new(RestoredTreeVerifier) as _),
            graph: match options.graph_diagnostics {
                Some(diagnostics) => {
                    GraphViewHandle::new(Arc::new(SeededGraph { diagnostics }) as _)
                }
                None => GraphViewHandle::null(),
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

/// AC 23: GN2 baseline (Edit without lineage / VerifyTest), GN3 (`Tool`),
/// and GN4 (empty diagnostics) never bump; the decision names the rule.
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

/// AC 23b / AM-0017-1: after a verify Fail opens a lineage, an exhausted
/// Edit Model failure may consume a remaining bump; without lineage (or
/// with a non-Model Edit class) the decline stays `kind`.
#[tokio::test]
async fn ac23b_lineage_edit_model_may_bump() {
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
            Step::Fail {
                node: NodeSel::Edit,
                class: ErrorClass::Model,
                diags: 0,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 3, "verify → lineage edit → succeed");
    assert_eq!(h.run_state(run).await, "succeeded");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[0]["seed_source"], json!("outcome"));
    assert_eq!(decisions[1]["admitted"], json!(true));
    assert_eq!(decisions[1]["seed_source"], json!("lineage"));
    assert_eq!(decisions[1]["error_class"], json!("model"));
    assert!(
        decisions[1]["diagnostic_count"].as_u64().unwrap() >= 1,
        "admitted lineage decision reports seed diagnostic_count, not the empty Edit IR"
    );
    h.close().await;

    // Non-Model Edit with lineage present still declines kind.
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
            Step::Fail {
                node: NodeSel::Edit,
                class: ErrorClass::Tool,
                diags: 0,
            },
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 2);
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[1]["admitted"], json!(false));
    assert_eq!(decisions[1]["reason"], json!("kind"));
    h.close().await;

    // Analyze Model with lineage may bump the same way.
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
            Step::Fail {
                node: NodeSel::Analyze,
                class: ErrorClass::Model,
                diags: 0,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();
    assert_eq!(sched.calls(), 3);
    assert_eq!(h.run_state(run).await, "succeeded");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions[1]["seed_source"], json!("lineage"));
    h.close().await;
}

/// Live defect (E2 bucket A): a generation-1 `Edit(Model)` failure is the
/// AM-0017-1 near-miss — admissible kind, admissible class, no in-run
/// lineage seed — and its decline is deliberate (AC 23/23b). What must not
/// happen is the measured silent exit: the run burned its whole edit
/// budget, no repair generation was ever admitted, and nothing durable
/// distinguished that from a repair that fought and lost. The §9.2 decline
/// record must name the absent lineage and the unspent repair budget, and
/// the terminal outcome's `FailureIr` must say no repair generation ran.
#[tokio::test]
async fn gen1_edit_model_decline_reports_absent_lineage_and_unspent_budget() {
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::Edit,
            class: ErrorClass::Model,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 1, "the decline admits nothing (AC 23)");
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["admitted"], json!(false));
    // AC 23b's reason vocabulary is retained; the near-miss is additive.
    assert_eq!(decisions[0]["reason"], json!("kind"));
    assert_eq!(decisions[0]["lineage"], json!("absent"));
    assert_eq!(decisions[0]["bumps_used"], json!(0));
    assert_eq!(decisions[0]["max_repair_generations"], json!(2));

    // The durable terminal outcome names the declined repair instead of
    // exiting shaped like an ordinary model failure — scheduler notes kept.
    let notes = run_finished_failure_notes(&h, run).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains("no repair generation was admitted"),
        "terminal FailureIr must carry the decline note, got: {}",
        notes[0]
    );
    assert!(
        notes[0].contains("cargo check failed"),
        "scheduler-authored notes must be preserved, got: {}",
        notes[0]
    );
    h.close().await;
}

/// The near-miss annotation stays narrow. An exhausted lineage decline
/// (GN5) keeps its `FailureIr` intact (GN11) and reports no `lineage`
/// field; a generation-1 `Edit(Tool)` decline (wrong class, not a
/// near-miss) reports neither — while every decline still carries the
/// budget accounting.
#[tokio::test]
async fn non_near_miss_declines_keep_failure_ir_intact() {
    // Bound 1: the verify Fail admits the only bump, then the lineage
    // Edit(Model) failure declines "exhausted" with lineage present.
    let h = Harness::new(HarnessOptions {
        max_repair_generations: 1,
        ..Default::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::VerifyCompile,
                class: ErrorClass::Compile,
                diags: 1,
            },
            Step::Fail {
                node: NodeSel::Edit,
                class: ErrorClass::Model,
                diags: 1,
            },
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();

    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[1]["admitted"], json!(false));
    assert_eq!(decisions[1]["reason"], json!("exhausted"));
    assert!(
        decisions[1].get("lineage").is_none_or(|v| v.is_null()),
        "an exhausted decline with lineage present is not the near-miss"
    );
    assert_eq!(decisions[1]["bumps_used"], json!(1));
    assert_eq!(decisions[1]["max_repair_generations"], json!(1));
    let notes = run_finished_failure_notes(&h, run).await;
    assert_eq!(
        notes,
        vec!["cargo check failed".to_owned()],
        "GN11: an exhausted decline returns the FailureIr intact"
    );
    h.close().await;

    // Generation-1 Edit(Tool): declines "kind" but is not the near-miss —
    // no annotation, no lineage field.
    let h = Harness::new(HarnessOptions::default()).await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::Edit,
            class: ErrorClass::Tool,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();

    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["reason"], json!("kind"));
    assert!(
        decisions[0].get("lineage").is_none_or(|v| v.is_null()),
        "a wrong-class decline is not the near-miss"
    );
    let notes = run_finished_failure_notes(&h, run).await;
    assert_eq!(notes, vec!["cargo check failed".to_owned()]);
    h.close().await;
}

/// Fix B (E2 bucket A): a generation-1 `Edit(Model)` failure with run-start
/// compile evidence in the graph channel (the issue-#53 seed pass) must
/// replan from that evidence instead of dying with the whole repair budget
/// unspent. The admission rides the normal GN3–GN7 rules; the decision
/// records `seed_source: "graph"`, and the replanned generation's root is
/// seeded (SD1–SD10) so generation 2 actually sees the compile errors.
#[tokio::test]
async fn gen1_edit_model_admits_replan_seeded_from_graph_channel() {
    let h = Harness::new(HarnessOptions {
        graph_diagnostics: Some(vec![diag("run-start: mismatched types")]),
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::Fail {
                node: NodeSel::Edit,
                class: ErrorClass::Model,
                diags: 1,
            },
            Step::Succeed,
        ],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));

    h.plane.runs().start(run).await.unwrap();

    assert_eq!(
        sched.calls(),
        2,
        "graph-seeded replan dispatches generation 2"
    );
    assert_eq!(h.run_state(run).await, "succeeded");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[0]["seed_source"], json!("graph"));
    assert_eq!(decisions[0]["to_generation"], json!(2));
    assert!(
        decisions[0]["diagnostic_count"].as_u64().unwrap() >= 1,
        "the admitted decision reports the graph seed's diagnostics, not the empty Edit IR"
    );
    // The evidence rode the replan: generation 2's root is seeded.
    let plans: Vec<PlanProducedPayload> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::PlanProduced)
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect();
    assert_eq!(plans.len(), 2);
    assert!(plans[1].replan);
    assert_eq!(plans[1].seeded_root, Some(true));
    let m = h.driver.metrics();
    assert_eq!(m.replans_admitted, 1);
    assert_eq!(m.generations_run, 2);
    h.close().await;
}

/// What still stops a replan loop on hopeless errors, with graph evidence
/// permanently available: (a) GN5 — the bump bound caps total generations
/// at 1 + max_repair_generations even when EVERY generation fails
/// Edit(Model) and the graph channel would admit again; (b) the kind/class
/// gate — a non-Model Edit failure never consults the graph and still
/// declines `kind` on the first generation.
#[tokio::test]
async fn graph_channel_admission_keeps_bound_and_kind_gate() {
    // (a) Three Edit(Model) failures, bound 2: exactly three dispatches,
    // then an "exhausted" decline — never a fourth generation.
    let h = Harness::new(HarnessOptions {
        graph_diagnostics: Some(vec![diag("run-start: mismatched types")]),
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let fail_edit = Step::Fail {
        node: NodeSel::Edit,
        class: ErrorClass::Model,
        diags: 1,
    };
    let sched = Scripted::new(&h.storage, [fail_edit, fail_edit, fail_edit]);
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 3, "GN5 bounds graph-fed Model failures");
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 3);
    assert_eq!(decisions[0]["admitted"], json!(true));
    assert_eq!(decisions[0]["seed_source"], json!("graph"));
    assert_eq!(decisions[1]["admitted"], json!(true));
    assert_eq!(
        decisions[1]["seed_source"],
        json!("lineage"),
        "after the graph admission the in-run lineage carries the seed"
    );
    assert_eq!(decisions[2]["admitted"], json!(false));
    assert_eq!(decisions[2]["reason"], json!("exhausted"));
    h.close().await;

    // (b) Wrong class: Edit(Tool) with graph evidence present declines
    // `kind` without dispatching a second generation.
    let h = Harness::new(HarnessOptions {
        graph_diagnostics: Some(vec![diag("run-start: mismatched types")]),
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::Edit,
            class: ErrorClass::Tool,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();

    assert_eq!(
        sched.calls(),
        1,
        "graph evidence never admits a non-Model class"
    );
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["admitted"], json!(false));
    assert_eq!(decisions[0]["reason"], json!("kind"));
    h.close().await;
}

/// A run that genuinely cannot proceed still fails honestly: when the
/// run-start check left no error-level evidence (clean or warning-only
/// workspace), the generation-1 Edit(Model) decline keeps the AM-0017-1
/// near-miss shape — reason `kind`, `lineage: "absent"`, annotated terminal
/// `FailureIr` — and dispatches nothing.
#[tokio::test]
async fn gen1_edit_model_without_run_start_errors_still_declines_honestly() {
    let warning = {
        let mut d = diag("unused variable: `x`");
        d.level = DiagnosticLevel::Warning;
        d
    };
    let h = Harness::new(HarnessOptions {
        graph_diagnostics: Some(vec![warning]),
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [Step::Fail {
            node: NodeSel::Edit,
            class: ErrorClass::Model,
            diags: 1,
        }],
    );
    sched.track(run);
    h.install(Arc::clone(&sched));
    h.plane.runs().start(run).await.unwrap();

    assert_eq!(sched.calls(), 1, "warning-only evidence admits nothing");
    assert_eq!(h.run_state(run).await, "failed");
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["admitted"], json!(false));
    assert_eq!(decisions[0]["reason"], json!("kind"));
    assert_eq!(decisions[0]["lineage"], json!("absent"));
    assert_eq!(decisions[0]["bumps_used"], json!(0));
    let notes = run_finished_failure_notes(&h, run).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains("no repair generation was admitted"),
        "terminal FailureIr must carry the decline note, got: {}",
        notes[0]
    );
    h.close().await;
}

/// Terminal `RunFinished` failure notes for `run`, in emission order.
async fn run_finished_failure_notes(h: &Harness, run: RunId) -> Vec<String> {
    h.runtime_events()
        .await
        .into_iter()
        .filter_map(|ev| match ev {
            RuntimeEvent::RunFinished { run_id, outcome } if run_id == run => {
                outcome.failure.map(|f| f.notes)
            }
            _ => None,
        })
        .collect()
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
    // Generous wall budget so CI load cannot flake the monotonic/decision
    // assertions; each sleep still burns > half the timeout so two
    // generations exhaust the absolute deadline.
    let t = Duration::from_millis(5_000);
    let sleep = 2_800;
    let h = Harness::new(HarnessOptions {
        run_timeout: t,
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;
    let sched = Scripted::new(
        &h.storage,
        [
            Step::SleepThenFailCompile {
                ms: sleep,
                diags: 1,
            },
            Step::SleepThenFailCompile {
                ms: sleep,
                diags: 1,
            },
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
    // Second generation must see a shrunken remainder, not a fresh timeout.
    assert!(
        remaining[1] < t,
        "second generation gets the remainder, not a fresh run_timeout: {remaining:?}"
    );
    let decisions = h.replan_decisions(session).await;
    assert_eq!(decisions.last().unwrap()["reason"], json!("deadline"));
    // Absolute deadline — nowhere near 2×run_timeout, with CI slack.
    assert!(
        wall < t + Duration::from_secs(5),
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

/// E2 dev-loop fix: when GN13 replaces the admitted (post-edit) seed with
/// the restored tree's verdict, the informative failure — what the
/// rolled-back edit actually broke — must not be erased. The driver must
/// record it as an `error` event with class `rollback` on the run, whose
/// message names the discarded diagnostics and states the workspace was
/// restored, so the context engine can promote it into generation N+1's
/// `conversation:prior_failure` section. Measured motivation: 16/16
/// two-plus-edit dev-loop runs never saw a post-edit diagnostic and 7/16
/// re-emitted a byte-identical patch every generation.
#[tokio::test]
async fn gn13_reseed_records_the_discarded_post_edit_failure_as_a_rollback_note() {
    let h = Harness::new(HarnessOptions {
        gn13_fakes: true,
        ..HarnessOptions::default()
    })
    .await;
    let (session, run, _dag) = h.planned_run().await;

    // Journal one applied edit so GN13 has a checkpoint to restore.
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_: SessionEventType::EditApplied,
            payload: json!({
                "transaction_id": TransactionId::new().to_string(),
                "files_touched": ["src/lib.rs"],
            }),
        })
        .await
        .unwrap();

    let sched = Scripted::new(
        &h.storage,
        [
            // Generation 1 fails verify on the POST-EDIT tree ("mismatched
            // types 0"). GN13 then rolls back and re-verifies; the fake
            // verifier reports the restored tree's ORIGINAL diagnostic.
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

    let notes: Vec<alloy_runtime::SessionEvent> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| {
            e.type_ == SessionEventType::Error && e.payload.get("class") == Some(&json!("rollback"))
        })
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "exactly one rollback note for the one reseeded generation"
    );
    let note = &notes[0];
    assert_eq!(note.run_id, Some(run), "the note must be run-attributed");
    let message = note.payload["message"].as_str().expect("message present");
    assert!(
        message.contains("mismatched types 0"),
        "the DISCARDED post-edit diagnostic must be quoted: {message}"
    );
    assert!(
        message.contains("rolled back"),
        "the note must state the workspace was rolled back: {message}"
    );
    assert!(
        !message.contains("original error: missing lifetime"),
        "the restored tree's diagnostic rides the replan seed, not the note: {message}"
    );
    h.close().await;
}

/// Control for the note above: without GN13 wiring the seed is never
/// replaced, so no rollback note may appear — the note must never become
/// ambient noise on ordinary replans.
#[tokio::test]
async fn no_rollback_note_when_gn13_leaves_the_seed_alone() {
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
    assert_eq!(h.run_state(run).await, "succeeded");

    let rollbacks = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| {
            e.type_ == SessionEventType::Error && e.payload.get("class") == Some(&json!("rollback"))
        })
        .count();
    assert_eq!(rollbacks, 0, "an untouched seed must record no note");
    h.close().await;
}
