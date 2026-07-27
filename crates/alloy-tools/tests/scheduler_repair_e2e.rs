//! RFC-0010 §11.3 / AC69 — cross-subsystem end-to-end test.
//!
//! Real SQLite storage, a real [`LinearScheduler`], a real MCP host running
//! `cargo_check` inside a real Landlock jail over a tiny fixture crate with a
//! deliberate type error, a stub [`CapabilityExecutor`] standing in for the
//! RFC-0013 worker bodies (applies a fixed patch once it has diagnostics to
//! work from), and a gate approved through the real [`RunController`].
//!
//! Traces Appendix K (`repair_local_diagnostic`) including the
//! replan/second-generation step: generation 1's `verify` soft-fails against
//! the broken fixture; the test plays the role of RFC-0009's (not-yet-built)
//! auto-replan by bumping the DAG to generation 2 with a fresh node set whose
//! root (`analyze`) input carries generation 1's `FailureIr` as a synthetic
//! predecessor envelope; generation 2's `edit` step applies the fix; `verify`
//! passes; the gate opens and is approved; the DAG reaches `Succeeded` with
//! every node carrying `output_ref`.
//!
//! This MUST live here, not in `alloy-runtime` (§11.3): only this crate owns
//! `ToolHandle`/`InProcessMcpHost`/`NativeSandboxBroker`.
//!
//! Skip policy: mirrors `sandbox_rfc0005.rs`'s `landlock_or_skip` — absent a
//! real, working Landlock jail this test skips (not fails) unless
//! `ALLOY_REQUIRE_LANDLOCK=1`.
//!
//! Author: arkadianet

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::adapters::SessionGateHumanAdapter;
use alloy_runtime::runtime::AlloyRuntime;
use alloy_runtime::session::SessionPlane;
use alloy_runtime::storage::{
    install_sqlite_event_sink, AlloyStorage, ArtifactKind, ArtifactPut, ArtifactStore, DagStore,
    SessionRows, StorageOpenOptions,
};
use alloy_runtime::types::ids::{ArtifactId, DagId, NodeId, ProfileId, RunId, SessionId};
use alloy_runtime::{
    allocate_ids, build_topology, Approval, BudgetPolicy, BuildTopology, CapabilityExecContext,
    CapabilityExecError, CapabilityExecutor, CapabilityOutcome, DagOutcome, DagState, Goal,
    LinearScheduler, LinearSchedulerDeps, McpVerifyCompileAdapter, NodeInputEnvelope,
    NodeInputPayload, NodeKind, PredecessorOutput, ProcessCostMeterFactory, RecordingDecisionLog,
    RetentionPolicy, RunControlState, RunGoalRecord, RunRow, RuntimeConfig, SchedConfig, Scheduler,
    Session, SessionVerifyPermissions, TemplateCatalog, TemplateId, Timestamp,
    UnavailableVerifyTest,
};
use alloy_tools::mcp::{
    InProcessMcpHost, McpHostConfig, StubPatchApplyBackend, ToolHandle, ToolHandleToolCaller,
};
use alloy_tools::{
    NativeSandboxBroker, OperatorHomes, SandboxBackend, SandboxBroker, SandboxProfile,
};
use tempfile::TempDir;

// --- skip gate (mirrors sandbox_rfc0005.rs::landlock_or_skip) --------------

fn require_landlock() -> bool {
    std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
}

/// Returns `true` when Landlock is available on this host. Panics instead of
/// skipping when `ALLOY_REQUIRE_LANDLOCK=1` — a dishonest green is worse than
/// a skip, but CI must not silently stop covering this path.
#[cfg(target_os = "linux")]
async fn landlock_or_skip() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let mut profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    let (available, detail) = match NativeSandboxBroker::new(profile).await {
        Ok(b) => match &b.capabilities().landlock {
            alloy_tools::BackendStatus::Available { detail } => (true, detail.clone()),
            alloy_tools::BackendStatus::Unavailable { reason } => (false, reason.clone()),
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

#[cfg(not(target_os = "linux"))]
async fn landlock_or_skip() -> bool {
    if require_landlock() {
        panic!("ALLOY_REQUIRE_LANDLOCK=1 but this OS has no Landlock backend");
    }
    eprintln!("skip: scheduler_repair_e2e is Linux/Landlock-only");
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

/// Copy the fixture tree into a unique tempdir so concurrent runs never share
/// a writable jail (matches `mcp_rfc0006.rs::copy_fixtures_tree`).
fn copy_fixtures_tree() -> TempDir {
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

    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    copy_dir_all(&src, dir.path()).expect("copy fixtures");
    dir
}

/// The fixed source this test's stub "edit" capability writes once it has
/// seen generation 1's diagnostics.
const FIXED_MAIN_RS: &str = "fn main() {\n    let x: i32 = 42;\n    println!(\"{}\", x);\n}\n";

// --- stub CapabilityExecutor (RFC-0013 worker bodies not landed) -----------

/// Test-only `analyze`/`edit` worker.
///
/// `analyze`: a blind first pass (`NodeInputPayload::Goal`) reports no fix
/// available; once it is handed generation 1's `FailureIr` through a
/// `FromPredecessors` envelope it reports a fix is available.
///
/// `edit`: reads its own `FromPredecessors` input (rewritten by the
/// scheduler's C5 to `analyze`'s real output) and, only when `analyze`
/// reported a fix available, writes [`FIXED_MAIN_RS`] into
/// `ctx.meta.workspace_root` — standing in for a real edit worker without
/// touching `EditEngine` (RFC-0008/AC83 is scheduler-only; this is
/// test-harness code in `alloy-tools`, outside that boundary).
struct StubRepairCapabilities {
    artifacts: Arc<dyn ArtifactStore>,
}

#[async_trait::async_trait]
impl CapabilityExecutor for StubRepairCapabilities {
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        match ctx.kind {
            NodeKind::Analyze => {
                let fix_available = match &ctx.input.payload {
                    NodeInputPayload::Goal(_) => false,
                    NodeInputPayload::FromPredecessors { preds } => {
                        let mut saw_diagnostics = false;
                        for pred in preds {
                            let blob = self
                                .artifacts
                                .get(pred.output_ref)
                                .await
                                .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                            let v: serde_json::Value = serde_json::from_slice(&blob.bytes)
                                .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                            if v.get("diagnostics")
                                .and_then(|d| d.as_array())
                                .is_some_and(|d| !d.is_empty())
                            {
                                saw_diagnostics = true;
                            }
                        }
                        saw_diagnostics
                    }
                };
                Ok(CapabilityOutcome::Succeeded {
                    payload: serde_json::json!({ "fix_available": fix_available }),
                })
            }
            NodeKind::Edit => {
                let NodeInputPayload::FromPredecessors { preds } = &ctx.input.payload else {
                    return Err(CapabilityExecError::Internal(
                        "edit node MUST have a FromPredecessors input".into(),
                    ));
                };
                let mut fix_available = false;
                for pred in preds {
                    let blob = self
                        .artifacts
                        .get(pred.output_ref)
                        .await
                        .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                    let v: serde_json::Value = serde_json::from_slice(&blob.bytes)
                        .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                    if v.get("fix_available").and_then(serde_json::Value::as_bool) == Some(true) {
                        fix_available = true;
                    }
                }
                if fix_available {
                    std::fs::write(ctx.meta.workspace_root.join("src/main.rs"), FIXED_MAIN_RS)
                        .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                }
                Ok(CapabilityOutcome::Succeeded {
                    payload: serde_json::json!({ "patched": fix_available }),
                })
            }
            other => Err(CapabilityExecError::Internal(format!(
                "StubRepairCapabilities has no worker for {other:?}"
            ))),
        }
    }
}

// --- fixture ------------------------------------------------------------

struct Fixture {
    _dir: TempDir,
    _rt: AlloyRuntime,
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
            run_timeout: Duration::from_secs(120),
            budget_policy: BudgetPolicy::default(),
        })
        .unwrap();
        let handle = rt.start().await.unwrap();
        let storage = install_sqlite_event_sink(
            &handle,
            Some(StorageOpenOptions::for_data_dir(dir.path().join("storage"))),
        )
        .await
        .unwrap();
        let plane = SessionPlane::new(handle, Arc::clone(&storage));
        Self {
            _dir: dir,
            _rt: rt,
            storage,
            plane,
        }
    }

    async fn close(self) {
        self.storage.close().await.unwrap();
        self._rt.shutdown().await.unwrap();
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
            .upsert_session(&session)
            .await
            .unwrap();
        session.id
    }

    async fn seed_run(&self, session_id: SessionId, dag_id: DagId) -> RunId {
        let run_id = RunId::new();
        let goal = RunGoalRecord {
            goal: Goal {
                text: "fix the type error".into(),
                constraints: vec![],
                attachments: vec![],
            },
            dag_id,
        };
        let row = RunRow {
            id: run_id,
            session_id,
            goal_json: serde_json::to_value(&goal).unwrap(),
            state: RunControlState::Accepted.as_str().into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        self.storage.sessions().upsert_run(&row).await.unwrap();
        run_id
    }

    async fn put_json_artifact(&self, value: &serde_json::Value) -> ArtifactId {
        self.storage
            .artifacts()
            .put(ArtifactPut {
                bytes: serde_json::to_vec(value).unwrap(),
                kind: ArtifactKind::Blob,
                content_type: Some("application/json".into()),
                session_id: None,
                run_id: None,
                labels: serde_json::Map::new(),
            })
            .await
            .unwrap()
    }
}

/// Build one generation of the `repair_local_diagnostic` template.
///
/// `analyze_input`: `None` for a blind first attempt (root gets the plain
/// `Goal`); `Some(diagnostics)` to synthesize a replan whose root carries a
/// `FromPredecessors` envelope pointing at an artifact holding `diagnostics`
/// (standing in for RFC-0009's not-yet-built auto-replan, per Appendix K
/// step 15/16).
#[allow(clippy::too_many_arguments)]
async fn build_generation(
    fx: &Fixture,
    dag_id: DagId,
    session_id: SessionId,
    generation: u64,
    analyze_diagnostics: Option<&serde_json::Value>,
) -> (alloy_runtime::TaskDag, alloy_runtime::dag::TemplateIdMap) {
    let manifest = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
    let ids = allocate_ids(manifest);

    let mut input_refs: BTreeMap<NodeId, ArtifactId> = BTreeMap::new();
    let analyze_id = ids.nodes["analyze"];
    let analyze_input_ref = match analyze_diagnostics {
        None => {
            let env = NodeInputEnvelope::new(
                dag_id,
                analyze_id,
                NodeKind::Analyze,
                generation,
                NodeInputPayload::Goal(Goal {
                    text: "fix the type error".into(),
                    constraints: vec![],
                    attachments: vec![],
                }),
            );
            fx.put_json_artifact(&serde_json::to_value(&env).unwrap())
                .await
        }
        Some(diagnostics) => {
            let pred_artifact = fx.put_json_artifact(diagnostics).await;
            let env = NodeInputEnvelope::new(
                dag_id,
                analyze_id,
                NodeKind::Analyze,
                generation,
                NodeInputPayload::FromPredecessors {
                    preds: vec![PredecessorOutput {
                        node_id: NodeId::new(), // synthetic: gen-1 verify, not in this dag
                        kind: NodeKind::VerifyCompile,
                        output_ref: pred_artifact,
                    }],
                },
            );
            fx.put_json_artifact(&serde_json::to_value(&env).unwrap())
                .await
        }
    };
    input_refs.insert(analyze_id, analyze_input_ref);

    // Non-root nodes are C5-rewritten before dispatch; the placeholder just
    // has to exist (matches loop_.rs Fixture::put_pending_placeholder_artifact).
    for name in ["edit", "verify", "gate"] {
        let id = ids.nodes[name];
        let placeholder =
            serde_json::json!({ "schema_version": 1, "alloy.envelope": "pending_pred" });
        input_refs.insert(id, fx.put_json_artifact(&placeholder).await);
    }

    let dag = build_topology(BuildTopology {
        manifest,
        dag_id,
        session_id,
        generation,
        ids: &ids,
        input_refs: &input_refs,
    });
    (dag, ids)
}

// --- the test -------------------------------------------------------------

#[cfg(target_os = "linux")]
#[tokio::test]
async fn repair_local_diagnostic_e2e_replans_and_succeeds() {
    if !landlock_or_skip().await {
        return;
    }
    if which_cargo().is_none() {
        skip_or_panic("cargo not on PATH");
        return;
    }
    let homes = match OperatorHomes::resolve() {
        Ok(h) => h,
        Err(e) => {
            skip_or_panic(&format!("operator homes: {e}"));
            return;
        }
    };

    let fixtures = copy_fixtures_tree();
    let jail = fixtures.path().canonicalize().unwrap();
    let workspace_root = jail.join("sbx_repair");
    assert!(workspace_root.join("Cargo.toml").is_file());

    let mut profile = SandboxProfile::default_for_jail(jail).unwrap();
    profile.check_backend = SandboxBackend::Landlock;
    profile.exec_timeout = Duration::from_secs(120);
    let broker = match NativeSandboxBroker::with_operator_homes(profile, homes.clone()).await {
        Ok(b) => b,
        Err(e) => {
            skip_or_panic(&format!("landlock broker unavailable: {e}"));
            return;
        }
    };
    let host = Arc::new(
        InProcessMcpHost::new(
            Arc::new(broker) as Arc<dyn SandboxBroker>,
            homes,
            Vec::new(),
            Arc::new(StubPatchApplyBackend),
            McpHostConfig::new(),
        )
        .unwrap(),
    );

    let fx = Fixture::new().await;
    let session_id = fx.seed_session(workspace_root.clone()).await;
    let dag_id = DagId::new();
    let run_id = fx.seed_run(session_id, dag_id).await;

    let sched_dir = fx._dir.path().join("scheduler");
    let capabilities: Arc<dyn CapabilityExecutor> = Arc::new(StubRepairCapabilities {
        artifacts: fx.storage.artifacts(),
    });
    let tools: Arc<dyn alloy_runtime::ToolCaller> =
        Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
            host.clone() as Arc<dyn alloy_tools::mcp::McpPlatform>,
            vec![alloy_runtime::ToolSelector::name(
                alloy_runtime::ToolName::new("cargo_check").unwrap(),
            )],
        )));
    let perms: Arc<dyn alloy_runtime::VerifyPermissions> = Arc::new(SessionVerifyPermissions::new(
        fx.storage.sessions(),
        Some("check*".into()),
        None,
    ));
    let verify_compile = Arc::new(McpVerifyCompileAdapter::new(
        Arc::clone(&tools),
        perms,
        fx.storage.artifacts(),
    ));
    let gate_human = Arc::new(SessionGateHumanAdapter::new(fx.plane.clone()));

    let build_scheduler = |sched_dir: PathBuf| {
        let mut config = SchedConfig::new(sched_dir);
        config.max_backoff = Duration::from_secs(1);
        LinearScheduler::new(LinearSchedulerDeps {
            dags: fx.storage.dags(),
            artifacts: fx.storage.artifacts(),
            events: fx.storage.events(),
            sessions: fx.storage.sessions(),
            session_plane: fx.plane.clone(),
            runs: fx.plane.runs(),
            verify_compile: verify_compile.clone() as Arc<dyn alloy_runtime::VerifyCompileAdapter>,
            verify_test: Arc::new(UnavailableVerifyTest),
            gate_human: gate_human.clone() as Arc<dyn alloy_runtime::GateHumanAdapter>,
            capabilities: Arc::clone(&capabilities),
            decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
            cost_meters: Arc::new(ProcessCostMeterFactory::new()),
            runtime_cancel: tokio_util::sync::CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(120),
            config,
        })
        .unwrap()
    };

    // --- generation 1: blind attempt, verify soft-fails ---------------
    {
        let (dag1, _ids1) = build_generation(&fx, dag_id, session_id, 1, None).await;
        fx.storage.dags().put(&dag1).await.unwrap();

        let scheduler = build_scheduler(sched_dir.clone());
        let outcome1: DagOutcome = scheduler.run(dag_id).await.unwrap();

        assert_eq!(
            outcome1.state,
            DagState::Failed,
            "gen1 outcome: {outcome1:?}"
        );
        let verify_id = _ids1.nodes["verify"];
        assert_eq!(outcome1.failed_node, Some(verify_id));
        let failure = outcome1
            .failure
            .expect("gen1 verify failure MUST carry a FailureIr");
        assert!(
            !failure.diagnostics.is_empty(),
            "gen1 verify MUST have captured rustc diagnostics: {failure:?}"
        );

        // Generation 2 ("replan"): synthesize the diagnostics artifact the
        // real RFC-0009 auto-replan would have produced.
        let diagnostics_json = serde_json::json!({
            "diagnostics": failure.diagnostics,
            "notes": failure.notes,
        });

        // --- generation 2: informed attempt, verify passes, gate opens --
        let (dag2, ids2) =
            build_generation(&fx, dag_id, session_id, 2, Some(&diagnostics_json)).await;
        fx.storage.dags().put(&dag2).await.unwrap();

        let scheduler2 = Arc::new(build_scheduler(sched_dir.clone()));
        let gate_id = ids2.gates["gate"];
        let runs = fx.plane.runs();
        let sched_for_run = Arc::clone(&scheduler2);
        let run_task = tokio::spawn(async move { sched_for_run.run(dag_id).await });

        // Poll for the DAG to reach WaitingApproval before approving —
        // real wall-clock cargo_check I/O means this can't use paused time
        // (§11.4 TD1 is for the deterministic in-memory suite, not this one).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
            if dag.state == DagState::WaitingApproval {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gen2 never reached WaitingApproval; last state: {:?}",
                dag.state
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        runs.approve(run_id, gate_id, Approval::Allow)
            .await
            .unwrap();

        let outcome2 = run_task
            .await
            .expect("scheduler.run task panicked")
            .unwrap();
        assert_eq!(
            outcome2.state,
            DagState::Succeeded,
            "gen2 outcome: {outcome2:?}"
        );
        assert_eq!(outcome2.failed_node, None);
        assert!(outcome2.failure.is_none());

        let final_dag = fx.storage.dags().get(dag_id).await.unwrap().unwrap();
        for name in ["analyze", "edit", "verify", "gate"] {
            let node = &final_dag.nodes[&ids2.nodes[name]];
            assert!(
                node.output_ref.is_some(),
                "{name} MUST carry output_ref at Succeeded: {node:?}"
            );
        }
    }

    fx.close().await;
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn repair_local_diagnostic_e2e_replans_and_succeeds() {
    landlock_or_skip().await;
}
