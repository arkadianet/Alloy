//! RFC-0010 §11.3 / AC69 — cross-subsystem end-to-end test.
//!
//! Real SQLite storage, a real `LinearScheduler` built through the
//! production `LinearScheduler::new`, a real MCP host running `cargo_check`
//! inside a real Landlock jail over a tiny fixture crate with a deliberate
//! type error, a stub `CapabilityExecutor` standing in for the RFC-0013
//! worker bodies, and a gate approved through the real `RunController`.
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
//! Everything below is Linux-only and lives inside one `cfg`-gated module so
//! the macOS/Seatbelt job does not see a file full of unused imports under
//! `-D warnings`.
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

    use alloy_runtime::adapters::SessionGateHumanAdapter;
    use alloy_runtime::runtime::AlloyRuntime;
    use alloy_runtime::session::SessionPlane;
    use alloy_runtime::storage::{
        install_sqlite_event_sink, AlloyStorage, ArtifactKind, ArtifactPut, ArtifactStore,
        DagStore, SessionRows, StorageOpenOptions,
    };
    use alloy_runtime::types::ids::{ArtifactId, DagId, NodeId, ProfileId, RunId, SessionId};
    use alloy_runtime::SessionProvenance;
    use alloy_runtime::{
        allocate_ids, build_topology, Approval, BudgetPolicy, BuildTopology, CapabilityExecContext,
        CapabilityExecError, CapabilityExecutor, CapabilityOutcome, DagState, GateHumanAdapter,
        Goal, LinearScheduler, LinearSchedulerDeps, McpVerifyCompileAdapter, NodeInputEnvelope,
        NodeInputPayload, NodeKind, PredecessorOutput, ProcessCostMeterFactory,
        RecordingDecisionLog, RetentionPolicy, RunControlState, RunGoalRecord, RunRow,
        RuntimeConfig, SchedConfig, Scheduler, Session, SessionVerifyPermissions, TaskDag,
        TemplateCatalog, TemplateId, Timestamp, ToolCaller, ToolName, ToolSelector,
        UnavailableVerifyTest, Verifier, VerifyPermissions,
    };
    use alloy_tools::mcp::{
        InProcessMcpHost, McpHostConfig, McpPlatform, StubPatchApplyBackend, ToolHandle,
        ToolHandleToolCaller,
    };
    use alloy_tools::{
        BackendStatus, NativeSandboxBroker, OperatorHomes, SandboxBackend, SandboxBroker,
        SandboxProfile,
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

    /// The fixed source the stub `edit` capability writes once it has seen
    /// generation 1's diagnostics.
    const FIXED_MAIN_RS: &str = "fn main() {\n    let x: i32 = 42;\n    println!(\"{}\", x);\n}\n";

    // --- stub CapabilityExecutor (RFC-0013 worker bodies not landed) -------

    /// Test-only `analyze`/`edit` worker.
    ///
    /// `analyze`: a blind first pass (`NodeInputPayload::Goal`) reports no fix
    /// available; once handed generation 1's diagnostics through a
    /// `FromPredecessors` envelope it reports a fix is available.
    ///
    /// `edit`: reads its own input (rewritten by the scheduler's C5 to
    /// `analyze`'s real `output_ref`) and, only when `analyze` reported a fix,
    /// writes [`FIXED_MAIN_RS`] into `ctx.meta.workspace_root`.
    struct StubRepairCapabilities {
        artifacts: Arc<dyn ArtifactStore>,
    }

    impl StubRepairCapabilities {
        /// Decode every predecessor output envelope this node was handed.
        async fn pred_payloads(
            &self,
            preds: &[PredecessorOutput],
        ) -> Result<Vec<serde_json::Value>, CapabilityExecError> {
            let mut out = Vec::with_capacity(preds.len());
            for pred in preds {
                let blob = self
                    .artifacts
                    .get(pred.output_ref)
                    .await
                    .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
                out.push(
                    serde_json::from_slice(&blob.bytes)
                        .map_err(|e| CapabilityExecError::Internal(e.to_string()))?,
                );
            }
            Ok(out)
        }
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
                            self.pred_payloads(preds).await?.iter().any(|v| {
                                v.get("diagnostics")
                                    .and_then(|d| d.as_array())
                                    .is_some_and(|d| !d.is_empty())
                            })
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
                    let fix_available = self.pred_payloads(preds).await?.iter().any(|v| {
                        v.get("payload")
                            .and_then(|p| p.get("fix_available"))
                            .or_else(|| v.get("fix_available"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                    });
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

    // --- fixture ----------------------------------------------------------

    struct Fixture {
        dir: TempDir,
        rt: AlloyRuntime,
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
                capture: Default::default(),
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
                dir,
                rt,
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
                    text: "fix the type error".into(),
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
    /// `analyze_diagnostics`: `None` for a blind first attempt (the root gets
    /// the plain `Goal`); `Some(_)` to synthesize a replan whose root carries
    /// a `FromPredecessors` envelope holding those diagnostics — standing in
    /// for RFC-0009's not-yet-built auto-replan (Appendix K steps 15/16).
    async fn build_generation(
        fx: &Fixture,
        dag_id: DagId,
        session_id: SessionId,
        generation: u64,
        analyze_diagnostics: Option<&serde_json::Value>,
    ) -> (TaskDag, alloy_runtime::TemplateIdMap) {
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
                            // Synthetic: generation 1's verify node, which is
                            // not part of this generation's node set.
                            node_id: NodeId::new(),
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

        // Non-root nodes are C5-rewritten before dispatch; the placeholder
        // only has to exist.
        for name in ["edit", "verify", "gate"] {
            let placeholder =
                serde_json::json!({ "schema_version": 1, "alloy.envelope": "pending_pred" });
            input_refs.insert(ids.nodes[name], fx.put_json_artifact(&placeholder).await);
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

    /// Assemble a production `LinearScheduler` (not `new_for_test`) over the
    /// real MCP verify adapter.
    fn build_scheduler(
        fx: &Fixture,
        sched_dir: PathBuf,
        capabilities: &Arc<dyn CapabilityExecutor>,
        verify_compile: &Arc<dyn Verifier>,
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
            decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
            cost_meters: Arc::new(ProcessCostMeterFactory::new()),
            runtime_cancel: tokio_util::sync::CancellationToken::new(),
            budget_policy: BudgetPolicy::default(),
            run_timeout: Duration::from_secs(300),
            config,
        })
        .unwrap()
    }

    // --- the test ---------------------------------------------------------

    #[tokio::test]
    async fn repair_local_diagnostic_e2e_replans_and_succeeds() {
        if !landlock_or_skip().await {
            return;
        }
        if which_cargo().is_none() {
            skip_or_panic("cargo not on PATH");
            return;
        }
        let real_homes = match OperatorHomes::resolve() {
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

        let homes_root = tempfile::tempdir().unwrap();
        let Some(homes) = hermetic_cargo_home(homes_root.path(), &real_homes) else {
            skip_or_panic("could not stage a hermetic CARGO_HOME");
            return;
        };

        let mut profile = SandboxProfile::default_for_jail(jail).unwrap();
        profile.check_backend = SandboxBackend::Landlock;
        profile.exec_timeout = Duration::from_secs(240);
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

        let sched_dir = fx.dir.path().join("scheduler");
        let capabilities: Arc<dyn CapabilityExecutor> = Arc::new(StubRepairCapabilities {
            artifacts: fx.storage.artifacts(),
        });
        let tools: Arc<dyn ToolCaller> = Arc::new(ToolHandleToolCaller::new(ToolHandle::new(
            Arc::clone(&host) as Arc<dyn McpPlatform>,
            vec![ToolSelector::name(ToolName::new("cargo_check").unwrap())],
        )));
        let perms: Arc<dyn VerifyPermissions> = Arc::new(SessionVerifyPermissions::new(
            fx.storage.sessions(),
            Some("check*".into()),
            None,
        ));
        let verify_compile: Arc<dyn Verifier> = Arc::new(McpVerifyCompileAdapter::new(
            tools,
            perms,
            fx.storage.artifacts(),
        ));

        // --- generation 1: blind attempt, verify soft-fails ---------------
        //
        // Scoped so the scheduler — and with it the `scheduler.lock` file
        // `OwnershipLock` holds for the process lifetime — is dropped before
        // generation 2 constructs a second scheduler over the same
        // `data_dir`. Without this the gen-2 build fails `Ownership` and none
        // of the assertions below ever run.
        let (diagnostics_json, verify_id_gen1) = {
            let (dag1, ids1) = build_generation(&fx, dag_id, session_id, 1, None).await;
            fx.storage.dags().put(&dag1).await.unwrap();

            let scheduler = build_scheduler(&fx, sched_dir.clone(), &capabilities, &verify_compile);
            let outcome1 = scheduler.run(dag_id).await.unwrap();

            let verify_id = ids1.nodes["verify"];
            assert_eq!(
                outcome1.state,
                DagState::Failed,
                "gen1 outcome: {outcome1:?}"
            );
            assert_eq!(outcome1.failed_node, Some(verify_id));
            let failure = outcome1
                .failure
                .expect("FO6: a Failed DAG with an attributed node carries a failure");
            assert!(
                !failure.diagnostics.is_empty(),
                "gen1 verify MUST capture rustc diagnostics: {failure:?}"
            );
            assert!(
                failure
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some("E0308")),
                "expected the fixture's type error, got {:?}",
                failure.diagnostics
            );

            (
                serde_json::json!({
                    "diagnostics": failure.diagnostics,
                    "notes": failure.notes,
                }),
                verify_id,
            )
        };

        // --- generation 2: informed attempt, verify passes, gate opens ----
        let (dag2, ids2) =
            build_generation(&fx, dag_id, session_id, 2, Some(&diagnostics_json)).await;
        assert_ne!(
            ids2.nodes["verify"], verify_id_gen1,
            "a replan mints a fresh node set"
        );
        fx.storage.dags().put(&dag2).await.unwrap();

        let scheduler2 = Arc::new(build_scheduler(
            &fx,
            sched_dir,
            &capabilities,
            &verify_compile,
        ));
        let gate_id = ids2.gates["gate"];
        let runs = fx.plane.runs();
        let sched_for_run = Arc::clone(&scheduler2);
        let mut run_task = tokio::spawn(async move { sched_for_run.run(dag_id).await });

        // Poll for `WaitingApproval` before approving. Real wall-clock
        // `cargo_check` I/O means this cannot use a paused clock (§11.4 TD1
        // governs the deterministic in-memory suite, not this one). Checking
        // `run_task` each turn makes an early scheduler exit surface at once
        // instead of after the full deadline.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        loop {
            if run_task.is_finished() {
                let early = (&mut run_task).await.expect("scheduler task panicked");
                panic!("gen2 run returned before the gate opened: {early:?}");
            }
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
            .expect("scheduler task panicked")
            .expect("gen2 run failed");
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
        // The repair really happened on disk, not just in the DAG blob.
        let fixed = std::fs::read_to_string(workspace_root.join("src/main.rs")).unwrap();
        assert_eq!(fixed, FIXED_MAIN_RS);

        fx.close().await;
    }
}
