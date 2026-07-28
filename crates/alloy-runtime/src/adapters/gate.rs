//! [`SessionGateHumanAdapter`] — the `GateHumanAdapter` implementation over
//! [`SessionPlane`] (RFC-0010 §5.7.11, GC1-GC5).
//!
//! Owns registering the waiter and awaiting it; owns nothing else. The
//! scheduler (`scheduler::linear::gate`) owns the deadline, the DAG blob,
//! and every durable write except `ApprovalResolved`/`RunControlState`
//! (`RunController::approve`/`expire_gate` own those).

use async_trait::async_trait;

use super::{Approval, GateHumanAdapter, NodeExecContext};
use crate::error::AdapterError;
use crate::session::SessionPlane;
use crate::types::ids::GateId;

/// `GateHumanAdapter` over the control plane's gate waiter registry.
pub struct SessionGateHumanAdapter {
    plane: SessionPlane,
}

impl SessionGateHumanAdapter {
    /// Construct from an existing control plane handle.
    #[must_use]
    pub fn new(plane: SessionPlane) -> Self {
        Self { plane }
    }
}

impl std::fmt::Debug for SessionGateHumanAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGateHumanAdapter")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl GateHumanAdapter for SessionGateHumanAdapter {
    /// GC1: no timer of its own — the scheduler wraps this call in
    /// `tokio::time::timeout`. GC2: `select!` over exactly two branches.
    /// GC4: never appends events, writes the DAG, or calls
    /// `approve`/`expire_gate`.
    async fn wait_approval(
        &self,
        ctx: &NodeExecContext,
        gate: GateId,
    ) -> Result<Approval, AdapterError> {
        // GC5: registration is adapter-owned; the scheduler MUST NOT call
        // `register_gate_waiter` itself (§5.7.3 GR1).
        let rx = self
            .plane
            .register_gate_waiter(ctx.meta.run_id, gate)
            .await
            .map_err(|e| AdapterError::Internal(format!("gate registration: {e}")))?;

        tokio::select! {
            result = rx => match result {
                Ok(decision) => Ok(decision),
                // GC3: a closed receiver is ambiguous at this layer — the
                // scheduler classifies it via durable RunControlState (§5.7.9).
                Err(_recv_error) => Err(AdapterError::Internal("gate waiter closed".into())),
            },
            () = ctx.cancellation.cancelled() => Err(AdapterError::Cancelled), // GC3
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::adapters::NodeExecRef;
    use crate::config::RuntimeConfig;
    use crate::runtime::AlloyRuntime;
    use crate::session::RunControlState;
    use crate::storage::{
        install_sqlite_event_sink, AlloyStorage, DagStore, RunRow, SessionRows, StorageOpenOptions,
    };
    use crate::types::budget::{BudgetPolicy, CreateSession, Goal};
    use crate::types::ids::{DagId, LanguageId, NodeId, ProfileId, RunId, SessionId};

    struct Fixture {
        dir: tempfile::TempDir,
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
                run_timeout: Duration::from_secs(30),
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
                dir,
                _rt: rt,
                storage,
                plane,
            }
        }

        async fn close(self) {
            self.storage.close().await.unwrap();
            self._rt.shutdown().await.unwrap();
        }

        /// Seed a session + a run via the real `SessionService`, then force
        /// the run row directly into `accepted` (bypassing `start`, which
        /// needs a live scheduler) — `register_gate_waiter` accepts
        /// `Accepted`/`Running`/`WaitingApproval`.
        async fn accepted_run(&self) -> (SessionId, RunId, DagId) {
            let sessions = self.plane.sessions();
            let session = sessions
                .create(CreateSession {
                    workspace_root: self.dir.path().to_path_buf(),
                    profile: ProfileId::new("default").unwrap(),
                    budget: BudgetPolicy::default(),
                    language_backends: vec![LanguageId::new("rust").unwrap()],
                })
                .await
                .unwrap();
            let run = sessions
                .submit_goal(
                    session,
                    Goal {
                        text: "fix".into(),
                        constraints: vec![],
                        attachments: vec![],
                    },
                )
                .await
                .unwrap();
            let row = self.storage.sessions().get_run(run).await.unwrap().unwrap();
            let dag_id =
                serde_json::from_value::<crate::session::RunGoalRecord>(row.goal_json.clone())
                    .unwrap()
                    .dag_id;
            self.storage
                .sessions()
                .upsert_run(&RunRow {
                    state: RunControlState::Accepted.as_str().to_owned(),
                    ..row
                })
                .await
                .unwrap();
            // `approve`/`expire_gate` resolve the DAG generation (amendment A8);
            // production always has this row via the scheduler's C1 checkpoint
            // before a gate can exist.
            self.storage
                .dags()
                .put(&crate::dag::TaskDag {
                    id: dag_id,
                    session_id: session,
                    generation: 0,
                    nodes: Default::default(),
                    edges: Vec::new(),
                    state: crate::scheduler::DagState::WaitingApproval,
                })
                .await
                .unwrap();
            (session, run, dag_id)
        }

        async fn set_run_state(&self, run: RunId, state: RunControlState) {
            let row = self.storage.sessions().get_run(run).await.unwrap().unwrap();
            self.storage
                .sessions()
                .upsert_run(&RunRow {
                    state: state.as_str().to_owned(),
                    ..row
                })
                .await
                .unwrap();
        }

        fn exec_ctx(&self, session: SessionId, run: RunId, dag: DagId) -> NodeExecContext {
            NodeExecContext {
                meta: NodeExecRef {
                    session_id: session,
                    run_id: run,
                    dag_id: dag,
                    node_id: NodeId::new(),
                    workspace_root: self.dir.path().to_path_buf(),
                    attempt: 0,
                },
                cancellation: CancellationToken::new(),
            }
        }
    }

    #[tokio::test]
    async fn wait_approval_resolves_on_approve() {
        let fx = Fixture::new().await;
        let (session, run, dag) = fx.accepted_run().await;
        let adapter = SessionGateHumanAdapter::new(fx.plane.clone());
        let gate = GateId::new();
        let ctx = fx.exec_ctx(session, run, dag);

        let runs = fx.plane.runs();
        let wait = tokio::spawn(async move { adapter.wait_approval(&ctx, gate).await });
        tokio::time::sleep(Duration::from_millis(20)).await; // let register_gate_waiter land
        runs.approve(run, gate, Approval::Allow).await.unwrap();

        let decision = wait.await.unwrap().unwrap();
        assert_eq!(decision, Approval::Allow);
        fx.close().await;
    }

    #[tokio::test]
    async fn wait_approval_resolves_on_deny() {
        let fx = Fixture::new().await;
        let (session, run, dag) = fx.accepted_run().await;
        let adapter = SessionGateHumanAdapter::new(fx.plane.clone());
        let gate = GateId::new();
        let ctx = fx.exec_ctx(session, run, dag);

        let runs = fx.plane.runs();
        let wait = tokio::spawn(async move { adapter.wait_approval(&ctx, gate).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        runs.approve(run, gate, Approval::Deny).await.unwrap();

        let decision = wait.await.unwrap().unwrap();
        assert_eq!(decision, Approval::Deny);
        fx.close().await;
    }

    #[tokio::test]
    async fn wait_approval_cancels_via_context_token() {
        let fx = Fixture::new().await;
        let (session, run, dag) = fx.accepted_run().await;
        let adapter = SessionGateHumanAdapter::new(fx.plane.clone());
        let gate = GateId::new();
        let ctx = fx.exec_ctx(session, run, dag);
        let token = ctx.cancellation.clone();

        let wait = tokio::spawn(async move { adapter.wait_approval(&ctx, gate).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();

        let err = wait.await.unwrap().unwrap_err();
        assert!(matches!(err, AdapterError::Cancelled));
        fx.close().await;
    }

    #[tokio::test]
    async fn registration_failure_maps_to_internal() {
        let fx = Fixture::new().await;
        let (session, run, dag) = fx.accepted_run().await;
        // Terminal run row: registration must fail with InvalidPhase, mapped
        // to AdapterError::Internal (GC5).
        fx.set_run_state(run, RunControlState::Succeeded).await;

        let adapter = SessionGateHumanAdapter::new(fx.plane.clone());
        let gate = GateId::new();
        let ctx = fx.exec_ctx(session, run, dag);
        let err = adapter.wait_approval(&ctx, gate).await.unwrap_err();
        assert!(matches!(err, AdapterError::Internal(m) if m.contains("gate registration")));
        fx.close().await;
    }
}
