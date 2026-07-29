//! Unit tests for the RFC-0003 control plane (§14 unit matrix).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;

use super::goal_record::RunGoalRecord;
use super::plane::SessionPlane;
use super::run_state::RunControlState;
use super::traits::{ReplanReason, RunController, SessionService};
use crate::adapters::Approval;
use crate::config::RuntimeConfig;
use crate::error::{RunError, SchedError, SessionError};
use crate::events::{EventSink, NewSessionEvent, RuntimeEvent, SessionEvent, SessionEventType};
use crate::runtime::{AlloyRuntime, RuntimeHandle, RuntimePhase};
use crate::scheduler::{DagOutcome, DagState, Scheduler};
use crate::storage::{
    install_sqlite_event_sink, AlloyStorage, DagStore, EventStore, RunRow, SessionRows,
    StorageOpenOptions,
};
use crate::types::budget::{BudgetPolicy, BudgetSnapshot, CreateSession, Goal};
use crate::types::ids::{DagId, EventSeq, GateId, LanguageId, ProfileId, RunId, SessionId};

/// Scripted scheduler responses consumed in order by [`MockScheduler`].
#[derive(Debug, Clone, Copy)]
enum Plan {
    /// `Ok(DagOutcome)` with this state.
    State(DagState),
    /// `Err(SchedError::Cancelled)`.
    Cancelled,
    /// `Err(SchedError::DagNotFound)`.
    NotFound,
    /// Signal `entered`, await `release`, then `Ok(DagOutcome)` with this state.
    BlockThen(DagState),
}

struct MockScheduler {
    plans: Mutex<VecDeque<Plan>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    cancels: Mutex<Vec<DagId>>,
    fail_next_cancel: AtomicBool,
    reconciles: Mutex<Vec<(DagId, DagState)>>,
    fail_next_reconcile: AtomicBool,
}

impl MockScheduler {
    fn new(plans: impl IntoIterator<Item = Plan>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(plans.into_iter().collect()),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            cancels: Mutex::new(Vec::new()),
            fail_next_cancel: AtomicBool::new(false),
            reconciles: Mutex::new(Vec::new()),
            fail_next_reconcile: AtomicBool::new(false),
        })
    }

    fn cancelled_dags(&self) -> Vec<DagId> {
        self.cancels.lock().unwrap().clone()
    }

    fn fail_next_cancel(&self) {
        self.fail_next_cancel.store(true, Ordering::SeqCst);
    }

    fn reconciled_calls(&self) -> Vec<(DagId, DagState)> {
        self.reconciles.lock().unwrap().clone()
    }

    fn fail_next_reconcile(&self) {
        self.fail_next_reconcile.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Scheduler for MockScheduler {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        let plan = self.plans.lock().unwrap().pop_front();
        let outcome = |state| DagOutcome {
            dag_id,
            generation: 1,
            state,
            failed_node: None,
            failure: None,
        };
        match plan {
            Some(Plan::State(state)) => Ok(outcome(state)),
            Some(Plan::Cancelled) => Err(SchedError::Cancelled),
            Some(Plan::NotFound) => Err(SchedError::DagNotFound(dag_id)),
            Some(Plan::BlockThen(state)) => {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(outcome(state))
            }
            None => Err(SchedError::Unavailable),
        }
    }

    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError> {
        self.cancels.lock().unwrap().push(dag_id);
        if self.fail_next_cancel.swap(false, Ordering::SeqCst) {
            return Err(SchedError::Internal("injected cancel failure".into()));
        }
        Ok(())
    }

    async fn reconcile_terminal_run(
        &self,
        dag_id: DagId,
        terminal: DagState,
    ) -> Result<(), SchedError> {
        self.reconciles.lock().unwrap().push((dag_id, terminal));
        if self.fail_next_reconcile.swap(false, Ordering::SeqCst) {
            return Err(SchedError::Internal("injected reconcile failure".into()));
        }
        Ok(())
    }
}

/// Runtime + SQLite storage + [`SessionPlane`] over a temp data dir.
struct Harness {
    dir: tempfile::TempDir,
    rt: AlloyRuntime,
    handle: RuntimeHandle,
    storage: Arc<AlloyStorage>,
    plane: SessionPlane,
}

impl Harness {
    async fn new() -> Self {
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
            run_timeout: Duration::from_secs(30),
            budget_policy: crate::types::budget::BudgetPolicy::default(),
            context_profile: crate::context::ContextProfile::v2_defaults(),
            profile_id: Some("default".into()),
            gates: crate::config::GatesConfig::default(),
            sandbox_echo: None,
            gate_timeout: None,
            max_repair_generations: 2,
            capture: Default::default(),
        })
        .unwrap();
        let handle = rt.start().await.unwrap();
        let storage =
            install_sqlite_event_sink(&handle, Some(StorageOpenOptions::for_data_dir(data_dir)))
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

    fn sessions(&self) -> Arc<dyn SessionService> {
        self.plane.sessions()
    }

    fn runs(&self) -> Arc<dyn RunController> {
        self.plane.runs()
    }

    fn install_scheduler(&self, sched: Arc<MockScheduler>) -> Arc<MockScheduler> {
        self.handle.set_scheduler(Arc::clone(&sched) as _).unwrap();
        sched
    }

    async fn create_session(&self) -> SessionId {
        self.sessions()
            .create(CreateSession {
                workspace_root: self.dir.path().to_path_buf(),
                profile: ProfileId::new("default").unwrap(),
                budget: BudgetPolicy::default(),
                language_backends: vec![LanguageId::new("rust").unwrap()],
                provenance: None,
            })
            .await
            .unwrap()
    }

    async fn submit(&self, session: SessionId) -> RunId {
        self.sessions()
            .submit_goal(session, goal("fix the build"))
            .await
            .unwrap()
    }

    async fn run_row(&self, run: RunId) -> RunRow {
        self.storage
            .sessions()
            .get_run(run)
            .await
            .unwrap()
            .expect("run row")
    }

    async fn run_state(&self, run: RunId) -> RunControlState {
        RunControlState::parse(&self.run_row(run).await.state).expect("known state")
    }

    async fn set_run_state(&self, run: RunId, state: RunControlState) {
        let row = self.run_row(run).await;
        self.storage
            .sessions()
            .upsert_run(&RunRow {
                state: state.as_str().to_owned(),
                ..row
            })
            .await
            .unwrap();
    }

    async fn dag_id(&self, run: RunId) -> DagId {
        serde_json::from_value::<RunGoalRecord>(self.run_row(run).await.goal_json)
            .unwrap()
            .dag_id
    }

    /// Seed the minimal `TaskDag` row `approve`/`expire_gate` need to resolve a
    /// generation (amendment A8). Production always has this row by the time a
    /// gate exists (the scheduler's C1 checkpoint writes it before first
    /// dispatch); tests that jump a run straight to `waiting_approval` without
    /// running a real scheduler must seed it explicitly.
    async fn seed_dag(&self, run: RunId) -> DagId {
        let session_id = self.run_row(run).await.session_id;
        let dag_id = self.dag_id(run).await;
        self.storage
            .dags()
            .put(&crate::dag::TaskDag {
                id: dag_id,
                session_id,
                generation: 0,
                nodes: Default::default(),
                edges: Vec::new(),
                state: DagState::WaitingApproval,
            })
            .await
            .unwrap();
        dag_id
    }

    async fn session_events(&self, session: SessionId) -> Vec<SessionEvent> {
        self.storage
            .events()
            .list_session_events(session, None, 1000)
            .await
            .unwrap()
    }

    async fn event_types(&self, session: SessionId) -> Vec<SessionEventType> {
        self.session_events(session)
            .await
            .into_iter()
            .map(|e| e.type_)
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

    async fn count_accepted(&self, run: RunId) -> usize {
        self.runtime_events()
            .await
            .iter()
            .filter(|ev| matches!(ev, RuntimeEvent::RunAccepted { run_id, .. } if *run_id == run))
            .count()
    }

    async fn count_finished(&self, run: RunId) -> usize {
        self.runtime_events()
            .await
            .iter()
            .filter(|ev| matches!(ev, RuntimeEvent::RunFinished { run_id, .. } if *run_id == run))
            .count()
    }

    async fn events_of_type(&self, session: SessionId, ty: SessionEventType) -> Vec<SessionEvent> {
        self.session_events(session)
            .await
            .into_iter()
            .filter(|e| e.type_ == ty)
            .collect()
    }

    async fn close(self) {
        let Self { rt, storage, .. } = self;
        rt.shutdown().await.unwrap();
        storage.close().await.unwrap();
    }
}

fn goal(text: &str) -> Goal {
    Goal {
        text: text.to_owned(),
        constraints: vec![],
        attachments: vec![],
    }
}

// ---------------------------------------------------------------- SessionService

#[tokio::test]
async fn session_create_persists_row_and_event() {
    let h = Harness::new().await;
    let id = h.create_session().await;

    let stored = h.storage.sessions().get_session(id).await.unwrap().unwrap();
    assert_eq!(stored.id, id);
    assert_eq!(stored.profile.as_str(), "default");

    let events = h.session_events(id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, EventSeq(0));
    assert_eq!(events[0].type_, SessionEventType::SessionCreated);
    assert_eq!(events[0].payload["profile"], json!("default"));
    assert_eq!(events[0].payload["language_backends"], json!(["rust"]));
    assert!(events[0].payload["budget"]["max_usd_per_run"].is_number());
    assert_eq!(h.plane.metrics().sessions_created, 1);
    h.close().await;
}

#[tokio::test]
async fn session_reject_unknown_profile() {
    let h = Harness::new().await;
    let err = h
        .sessions()
        .create(CreateSession {
            workspace_root: h.dir.path().to_path_buf(),
            profile: ProfileId::new("wat").unwrap(),
            budget: BudgetPolicy::default(),
            language_backends: vec![LanguageId::new("rust").unwrap()],
            provenance: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::Invalid(m) if m.contains("unsupported profile")));
    h.close().await;
}

#[tokio::test]
async fn session_create_rejects_relative_root_and_empty_backends() {
    let h = Harness::new().await;
    let base = CreateSession {
        workspace_root: h.dir.path().to_path_buf(),
        profile: ProfileId::new("default").unwrap(),
        budget: BudgetPolicy::default(),
        language_backends: vec![LanguageId::new("rust").unwrap()],
        provenance: None,
    };

    let relative = CreateSession {
        workspace_root: std::path::PathBuf::from("relative/ws"),
        ..base.clone()
    };
    assert!(matches!(
        h.sessions().create(relative).await.unwrap_err(),
        SessionError::Invalid(m) if m.contains("absolute")
    ));

    let no_backends = CreateSession {
        language_backends: vec![],
        ..base
    };
    assert!(matches!(
        h.sessions().create(no_backends).await.unwrap_err(),
        SessionError::Invalid(m) if m.contains("language_backends")
    ));
    h.close().await;
}

#[tokio::test]
async fn session_submit_goal_creates_run() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let row = h.run_row(run).await;
    assert_eq!(row.state, RunControlState::Created.as_str());
    assert_eq!(row.session_id, session);
    let record: RunGoalRecord = serde_json::from_value(row.goal_json).unwrap();
    assert_eq!(record.goal.text, "fix the build");

    let events = h.session_events(session).await;
    assert_eq!(events[1].type_, SessionEventType::GoalSubmitted);
    assert_eq!(events[1].run_id, Some(run));
    assert_eq!(
        events[1].payload["dag_id"],
        json!(record.dag_id.to_string())
    );
    assert!(events[1].payload["budget"]["max_tokens_per_run"].is_number());
    assert_eq!(h.plane.metrics().goals_submitted, 1);
    h.close().await;
}

#[tokio::test]
async fn session_submit_goal_rejects_empty_text() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let err = h
        .sessions()
        .submit_goal(session, goal("   \n"))
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::Invalid(_)));

    let missing = h
        .sessions()
        .submit_goal(SessionId::new(), goal("x"))
        .await
        .unwrap_err();
    assert!(matches!(missing, SessionError::NotFound(_)));
    h.close().await;
}

#[tokio::test]
async fn session_events_pagination_exclusive() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    h.submit(session).await;
    h.submit(session).await;
    h.submit(session).await;

    let first = h.sessions().events(session, None, 2).await.unwrap();
    assert_eq!(
        first.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![EventSeq(0), EventSeq(1)]
    );

    let rest = h
        .sessions()
        .events(session, Some(EventSeq(1)), 10)
        .await
        .unwrap();
    assert_eq!(
        rest.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![EventSeq(2), EventSeq(3)]
    );

    // limit 0 clamps up to 1; oversized limits clamp down to MAX_EVENTS_PAGE.
    assert_eq!(
        h.sessions().events(session, None, 0).await.unwrap().len(),
        1
    );
    assert_eq!(
        h.sessions()
            .events(session, None, usize::MAX)
            .await
            .unwrap()
            .len(),
        4
    );
    h.close().await;
}

#[tokio::test]
async fn session_resume_not_found() {
    let h = Harness::new().await;
    let id = SessionId::new();
    assert!(matches!(
        h.sessions().resume(id).await.unwrap_err(),
        SessionError::NotFound(got) if got == id
    ));
    assert!(matches!(
        h.sessions().events(id, None, 10).await.unwrap_err(),
        SessionError::NotFound(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn session_resume_rearms_crash_recovery_states() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let running = h.submit(session).await;
    let waiting = h.submit(session).await;
    let cancelling = h.submit(session).await;
    let created = h.submit(session).await;
    h.set_run_state(running, RunControlState::Running).await;
    h.set_run_state(waiting, RunControlState::WaitingApproval)
        .await;
    h.set_run_state(cancelling, RunControlState::Cancelling)
        .await;

    let before = h.session_events(session).await.len();
    let resumed = h.sessions().resume(session).await.unwrap();
    assert_eq!(resumed.id, session);

    assert_eq!(h.run_state(running).await, RunControlState::Accepted);
    assert_eq!(h.run_state(waiting).await, RunControlState::Accepted);
    assert_eq!(h.run_state(cancelling).await, RunControlState::Cancelled);
    assert_eq!(h.run_state(created).await, RunControlState::Created);
    // Re-arming invents no events; finalizing the abandoned cancel owes exactly one
    // `RunCompleted`, because no other writer will ever produce it (§5.3).
    let events = h.session_events(session).await;
    assert_eq!(events.len(), before + 1);
    assert_eq!(events.last().unwrap().type_, SessionEventType::RunCompleted);
    assert_eq!(events.last().unwrap().run_id, Some(cancelling));
    assert_eq!(h.plane.metrics().sessions_resumed, 1);
    h.close().await;
}

#[tokio::test]
async fn session_resume_calls_reconcile_terminal_run_for_every_terminal_row_a6() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([]));
    let session = h.create_session().await;
    let failed = h.submit(session).await;
    let succeeded = h.submit(session).await;
    let cancelled = h.submit(session).await;
    let running = h.submit(session).await; // non-terminal: must NOT be reconciled
    h.set_run_state(failed, RunControlState::Failed).await;
    h.set_run_state(succeeded, RunControlState::Succeeded).await;
    h.set_run_state(cancelled, RunControlState::Cancelled).await;
    h.set_run_state(running, RunControlState::Running).await;

    h.sessions().resume(session).await.unwrap();

    let mut calls = sched.reconciled_calls();
    calls.sort_by_key(|(dag, _)| *dag);
    let mut expected = vec![
        (h.dag_id(failed).await, DagState::Failed),
        (h.dag_id(succeeded).await, DagState::Succeeded),
        (h.dag_id(cancelled).await, DagState::Cancelled),
    ];
    expected.sort_by_key(|(dag, _)| *dag);
    assert_eq!(calls, expected);
    h.close().await;
}

#[tokio::test]
async fn session_resume_reconcile_failure_is_best_effort_and_does_not_abort() {
    // A6: "best effort, warn on error, never abort resume" — a failing
    // reconcile must not stop the rest of resume's work (metrics bump,
    // other rows' re-arming).
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([]));
    sched.fail_next_reconcile();
    let session = h.create_session().await;
    let failed = h.submit(session).await;
    let running = h.submit(session).await;
    h.set_run_state(failed, RunControlState::Failed).await;
    h.set_run_state(running, RunControlState::Running).await;

    let resumed = h.sessions().resume(session).await.unwrap();
    assert_eq!(resumed.id, session);
    assert_eq!(sched.reconciled_calls().len(), 1); // still attempted
    assert_eq!(h.run_state(running).await, RunControlState::Accepted); // still re-armed
    assert_eq!(h.plane.metrics().sessions_resumed, 1);
    h.close().await;
}

#[tokio::test]
async fn session_resume_skips_corrupt_goal_json() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let corrupt = h.submit(session).await;
    let healthy = h.submit(session).await;
    h.set_run_state(healthy, RunControlState::Running).await;

    let row = h.run_row(corrupt).await;
    h.storage
        .sessions()
        .upsert_run(&RunRow {
            goal_json: json!({ "not": "a goal record" }),
            state: RunControlState::Running.as_str().to_owned(),
            ..row
        })
        .await
        .unwrap();

    h.sessions().resume(session).await.unwrap();

    // Corrupt row stays listable and undispatched; the healthy run is re-armed.
    assert_eq!(h.run_state(corrupt).await, RunControlState::Running);
    assert_eq!(h.run_state(healthy).await, RunControlState::Accepted);

    // `start` guards on state before it parses the envelope, so reach the parse by
    // putting the corrupt row into a dispatchable state by hand.
    assert!(matches!(
        h.runs().start(corrupt).await.unwrap_err(),
        RunError::AlreadyStarted(_)
    ));
    h.set_run_state(corrupt, RunControlState::Accepted).await;
    assert!(matches!(
        h.runs().start(corrupt).await.unwrap_err(),
        RunError::Internal(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn session_resume_finalizes_cancelling_with_run_completed() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    // A crash between `cancelling` and `cancelled` leaves nobody to finish the cancel.
    // This fixture never accepted the run, so resume must not invent RunFinished.
    h.set_run_state(run, RunControlState::Cancelling).await;

    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    let completed = h
        .events_of_type(session, SessionEventType::RunCompleted)
        .await;
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].run_id, Some(run));
    assert_eq!(completed[0].payload["dag_state"], json!("cancelled"));
    assert_eq!(
        completed[0].payload["reason"],
        json!("resume_finalized_cancel")
    );
    assert_eq!(h.count_finished(run).await, 0);
    assert_eq!(h.plane.metrics().runs_cancelled, 1);
    h.close().await;
}

#[tokio::test]
async fn session_resume_finalizes_accepted_cancelling_emits_finished() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    // Announce acceptance, then crash mid-cancel.
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(h.count_accepted(run).await, 1);
    h.set_run_state(run, RunControlState::Cancelling).await;

    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn session_resume_cancel_events_precede_cancelled_upsert() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let cancelling = h.submit(session).await;
    let running = h.submit(session).await;
    assert!(matches!(
        h.runs().start(cancelling).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    h.set_run_state(cancelling, RunControlState::Cancelling)
        .await;
    h.set_run_state(running, RunControlState::Running).await;

    // Injected upsert failure after terminal events: row must stay `cancelling`, not
    // `cancelled` without events. The sibling run still re-arms and resume completes.
    h.plane.fail_next_run_upsert();
    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(cancelling).await, RunControlState::Cancelling);
    assert_eq!(h.run_state(running).await, RunControlState::Accepted);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(cancelling).await, 1);
    assert_eq!(h.plane.metrics().sessions_resumed, 1);

    // Retry is idempotent: no duplicate terminal events, then durable Cancelled.
    h.sessions().resume(session).await.unwrap();
    assert_eq!(h.run_state(cancelling).await, RunControlState::Cancelled);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(cancelling).await, 1);
    assert_eq!(h.plane.metrics().runs_cancelled, 1);
    assert_eq!(h.plane.metrics().sessions_resumed, 2);
    h.close().await;
}

#[tokio::test]
async fn session_resume_keeps_cancelling_when_append_fails() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Cancelling).await;

    h.plane.fail_next_append();
    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelling);
    assert!(h
        .events_of_type(session, SessionEventType::RunCompleted)
        .await
        .is_empty());
    assert_eq!(h.count_finished(run).await, 0);
    assert_eq!(h.plane.metrics().sessions_resumed, 1);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_resume_does_not_clobber_concurrent_cancel() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    h.set_run_state(run, RunControlState::Cancelling).await;

    // Resume and a live `cancel` both want to finalize this row. They serialize on the
    // per-run mutex and each re-reads the row under it, so exactly one of them writes the
    // terminal state and its events — whichever order the executor picks.
    let runs = h.runs();
    let sessions = h.sessions();
    let cancelling = tokio::spawn(async move { runs.cancel(run).await });
    let resuming = tokio::spawn(async move { sessions.resume(session).await });
    cancelling.await.unwrap().unwrap();
    resuming.await.unwrap().unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn session_events_allowed_while_draining() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    h.submit(session).await;

    h.rt.drain(Duration::from_millis(10)).await.unwrap();
    assert_eq!(h.handle.phase(), RuntimePhase::Draining);

    h.sessions().resume(session).await.unwrap();
    assert_eq!(
        h.sessions().events(session, None, 10).await.unwrap().len(),
        2
    );
    assert!(matches!(
        h.sessions()
            .submit_goal(session, goal("x"))
            .await
            .unwrap_err(),
        SessionError::Invalid(_)
    ));
    h.close().await;
}

// ---------------------------------------------------------------- RunController

#[tokio::test]
async fn run_start_null_scheduler_unavailable() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let err = h.runs().start(run).await.unwrap_err();
    assert!(matches!(err, RunError::SchedulerUnavailable));
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    assert_eq!(h.count_accepted(run).await, 1);
    assert_eq!(h.count_finished(run).await, 0);

    let error_events: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::Error)
        .collect();
    assert_eq!(error_events.len(), 1);
    assert_eq!(
        error_events[0].payload["class"],
        json!("scheduler_unavailable")
    );
    assert_eq!(h.plane.metrics().runs_start_unavailable, 1);
    h.close().await;
}

#[tokio::test]
async fn run_start_redispatch_after_unavailable() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));

    // Accepted must stay re-dispatchable and must not re-announce acceptance.
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    assert_eq!(h.count_accepted(run).await, 1);
    assert_eq!(h.plane.metrics().runs_started, 2);
    h.close().await;
}

#[tokio::test]
async fn run_start_terminal_success_emits_finished() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::State(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    h.runs().start(run).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Succeeded);
    assert_eq!(h.count_finished(run).await, 1);
    let completed: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::RunCompleted)
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].payload["dag_state"], json!("succeeded"));

    // Terminal runs are not restartable.
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "terminal"
    ));
    h.close().await;
}

#[tokio::test]
async fn run_start_scheduler_cancelled_emits_finished() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::Cancelled]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    h.runs().start(run).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(h.count_finished(run).await, 1);
    let types = h.event_types(session).await;
    assert!(types.contains(&SessionEventType::RunCompleted));
    assert!(!types.contains(&SessionEventType::Error));
    h.close().await;
}

#[tokio::test]
async fn run_start_dag_not_found_keeps_accepted() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::NotFound]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::InvalidPhase(m) if m.starts_with("dag not found")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    let error_events: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::Error)
        .collect();
    assert_eq!(error_events[0].payload["class"], json!("dag_not_found"));
    h.close().await;
}

#[tokio::test]
async fn run_start_pending_outcome_is_internal() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::State(DagState::Pending)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::Internal(m) if m.contains("pending")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    assert_eq!(h.count_finished(run).await, 0);
    h.close().await;
}

#[tokio::test]
async fn run_running_outcome_not_redispatchable() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::State(DagState::Running)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    h.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Running);
    assert_eq!(h.count_finished(run).await, 0);
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::AlreadyStarted(got) if got == run
    ));

    // §5.3 crash recovery is the only way back to a dispatchable state.
    h.sessions().resume(session).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    h.close().await;
}

#[tokio::test]
async fn run_start_missing_run_is_not_found() {
    let h = Harness::new().await;
    let run = RunId::new();
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::NotFound(got) if got == run
    ));
    h.close().await;
}

#[tokio::test]
async fn start_lock_not_held_across_run_dag() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(
        DagState::WaitingApproval,
    )]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    // Per-run mutex is free while `run_dag` is awaited: gate + approve both proceed.
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.plane.approve(run, gate, Approval::Allow).await.unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Allow);

    sched.release.notify_one();
    started.await.unwrap().unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    // A concurrent `start` is rejected while the execution lease is held, and the
    // lease is released once the outcome is durable.
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::AlreadyStarted(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn run_double_start_while_live_already_started() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::AlreadyStarted(got) if got == run
    ));

    sched.release.notify_one();
    started.await.unwrap().unwrap();
    assert_eq!(h.count_accepted(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_start_abort_clears_lease() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;
    started.abort();
    assert!(started.await.unwrap_err().is_cancelled());

    // Dropping the `start` future releases the execution lease, so the run is
    // re-dispatchable instead of stuck behind `AlreadyStarted` for the process lifetime.
    let err = h.runs().start(run).await.unwrap_err();
    assert!(
        matches!(err, RunError::SchedulerUnavailable),
        "expected a fresh dispatch attempt, got {err}"
    );
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    assert_eq!(h.count_accepted(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn lock_maps_evict_after_drop() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    // `submit_goal` took the per-session mutex and gave it back.
    assert_eq!(h.plane.session_lock_map_len(), 0);
    assert_eq!(h.plane.run_lock_map_len(), 0);

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;
    // The ticket held across `run_dag` keeps the entry alive so `relock` re-acquires the
    // same mutex rather than a fresh one.
    assert_eq!(h.plane.run_lock_map_len(), 1);

    sched.release.notify_one();
    started.await.unwrap().unwrap();
    assert_eq!(h.plane.run_lock_map_len(), 0);
    h.close().await;
}

#[tokio::test]
async fn run_request_replan_not_overwritten_by_late_start() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    h.runs()
        .request_replan(run, ReplanReason::UserRequested)
        .await
        .unwrap();

    sched.release.notify_one();
    started.await.unwrap().unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::ReplanRequested);
    assert_eq!(h.count_finished(run).await, 0);
    h.close().await;
}

#[tokio::test]
async fn run_cancel_during_start_is_not_clobbered() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    h.runs().cancel(run).await.unwrap();
    sched.release.notify_one();
    started.await.unwrap().unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(sched.cancelled_dags(), vec![h.dag_id(run).await]);
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_cancel_idempotent_and_records_run_completed() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Accepted).await;

    h.runs().cancel(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    h.runs().cancel(run).await.unwrap();

    let completed: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::RunCompleted)
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].payload["dag_state"], json!("cancelled"));
    assert!(!h
        .event_types(session)
        .await
        .contains(&SessionEventType::Error));
    // Durable state had left `Created`, so RunFinished is emitted once.
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_cancel_from_created_skips_run_finished() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    h.runs().cancel(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(h.count_finished(run).await, 0);
    h.close().await;
}

#[tokio::test]
async fn run_cancel_retry_after_cancel_dag_failure_skips_unpaired_finished() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    // Force the historical created→cancelling shape so a retry must consult RunAccepted.
    h.set_run_state(run, RunControlState::Cancelling).await;
    sched.fail_next_cancel();

    assert!(matches!(
        h.runs().cancel(run).await.unwrap_err(),
        RunError::Internal(m) if m.contains("injected cancel failure")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Cancelling);

    h.runs().cancel(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(h.count_finished(run).await, 0);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn run_cancel_retry_after_cancel_dag_failure_emits_finished_when_accepted() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(h.count_accepted(run).await, 1);

    sched.fail_next_cancel();
    assert!(matches!(
        h.runs().cancel(run).await.unwrap_err(),
        RunError::Internal(m) if m.contains("injected cancel failure")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Cancelling);

    h.runs().cancel(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_cancel_corrupt_goal_skips_run_finished() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    let row = h.run_row(run).await;
    h.storage
        .sessions()
        .upsert_run(&RunRow {
            goal_json: json!({ "broken": true }),
            state: RunControlState::Accepted.as_str().to_owned(),
            ..row
        })
        .await
        .unwrap();

    h.runs().cancel(run).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(h.count_finished(run).await, 0);
    assert!(sched.cancelled_dags().is_empty());
    assert!(h
        .event_types(session)
        .await
        .contains(&SessionEventType::RunCompleted));
    h.close().await;
}

#[tokio::test]
async fn run_cancel_clears_waiters() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();

    h.runs().cancel(run).await.unwrap();

    assert!(rx.await.is_err(), "waiter sender must be dropped on cancel");
    assert!(matches!(
        h.plane.approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "terminal"
    ));
    h.close().await;
}

#[tokio::test]
async fn run_approve_with_waiter() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    h.runs()
        .approve(run, gate, Approval::AllowOnce)
        .await
        .unwrap();
    assert_eq!(rx.await.unwrap(), Approval::AllowOnce);
    assert_eq!(h.run_state(run).await, RunControlState::Running);

    let resolved: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::ApprovalResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["decision"], json!("allow_once"));
    assert_eq!(resolved[0].payload["gate_id"], json!(gate.to_string()));
    // A8: without `generation`, RFC-0010 §5.7.2's resume scan cannot filter a
    // GateId reused across a replan, and would apply a stale resolution to
    // the new generation's gate.
    assert_eq!(
        resolved[0].payload["generation"],
        json!(0),
        "A8 requires ApprovalResolved to carry the DAG generation"
    );

    // Second approve for the same gate finds no waiter.
    h.set_run_state(run, RunControlState::WaitingApproval).await;
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::UnknownGate(got) if got == gate
    ));
    assert_eq!(h.plane.metrics().approvals_resolved, 1);
    h.close().await;
}

#[tokio::test]
async fn run_expire_gate_resolves_with_expired_decision_and_generation() {
    // A4/A8: `expire_gate` mirrors `approve(Deny)` with `decision: "expired"`
    // and MUST carry `generation` for the same §5.7.2 scan-filtering reason
    // `approve` does. Nothing pinned that on the expiry path.
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    h.runs().expire_gate(run, gate).await.unwrap();

    // The waiter is released (dropped, not delivered an Approval): §5.7.8
    // terminalizes rather than resuming the fold.
    assert!(rx.await.is_err(), "expiry must not deliver an Approval");
    assert_eq!(h.run_state(run).await, RunControlState::Failed);

    let resolved: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::ApprovalResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["decision"], json!("expired"));
    assert_eq!(resolved[0].payload["gate_id"], json!(gate.to_string()));
    assert_eq!(
        resolved[0].payload["generation"],
        json!(0),
        "A8 applies to expire_gate exactly as it does to approve"
    );
    h.close().await;
}

#[tokio::test]
async fn run_expire_gate_without_a_waiter_is_not_an_error() {
    // A7: idempotent with respect to a missing waiter — the scheduler retries
    // expiry (§5.7.8 EXPIRE_RETRY_MAX) and must not see a hard error just
    // because the waiter already went away.
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::WaitingApproval).await;

    h.runs()
        .expire_gate(run, GateId::new())
        .await
        .expect("A7: no registered waiter is not an error");
    h.close().await;
}

#[tokio::test]
async fn run_approve_deny_during_run_dag_joins_cleanly() {
    let h = Harness::new().await;
    // Scheduler blocks inside run_dag until released, then reports Failed — the shape a
    // real gate-failed DAG returns after GateHumanAdapter observes Deny.
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Failed)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.runs().approve(run, gate, Approval::Deny).await.unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Deny);
    assert_eq!(h.run_state(run).await, RunControlState::Failed);

    sched.release.notify_one();
    // Agreeing Ok(Failed) over durable `failed` is the expected join, not InvalidPhase.
    started.await.unwrap().unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_terminal_outcome_overrides_stale_waiting_approval_a5() {
    // Amendment A5: unlike the sibling test above (where `approve` flips the
    // row to `failed` *before* the scheduler returns, so `apply_start_outcome`
    // takes the already-terminal-durable join path), here nothing ever calls
    // `approve`/`expire_gate` — durable state is still `waiting_approval`
    // when a terminal `Ok(Failed)` outcome comes back. The old code merged
    // this away (`is_control_protected` unconditionally won for
    // `waiting_approval`), stranding the run non-terminal forever: nothing
    // else ever revisits a row once a live run has moved past it.
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Failed)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    let gate = GateId::new();
    let _rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    sched.release.notify_one();
    started.await.unwrap().unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_non_terminal_outcome_still_merges_under_waiting_approval() {
    // A5 explicitly keeps this case merging: a `Running`/`WaitingApproval`/
    // `ReplanRequired` outcome under a `waiting_approval` durable row is not
    // a conflict to resolve, just the normal "gate still pending" shape.
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(
        DagState::WaitingApproval,
    )]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let started = tokio::spawn(async move { runs.start(run).await });
    sched.entered.notified().await;

    let gate = GateId::new();
    let _rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    sched.release.notify_one();
    started.await.unwrap().unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);
    assert_eq!(h.count_finished(run).await, 0); // merged, not finalized
    h.close().await;
}

#[tokio::test]
async fn run_approve_deny_fails_run() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([Plan::State(DagState::WaitingApproval)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.runs().approve(run, gate, Approval::Deny).await.unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Deny);

    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert_eq!(h.count_finished(run).await, 1);
    let completed: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::RunCompleted)
        .collect();
    assert_eq!(completed[0].payload["dag_state"], json!("failed"));
    assert_eq!(completed[0].payload["reason"], json!("approval_denied"));
    h.close().await;
}

#[tokio::test]
async fn run_approve_persists_before_notify() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let mut rx = h.plane.register_gate_waiter(run, gate).await.unwrap();

    // A failed row write must leave the gate unresolved and the waiter untouched.
    h.plane.fail_next_run_upsert();
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::Internal(m) if m.contains("injected upsert failure")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    // A failed event append lands after the row write: state advances, but the waiter
    // must not observe a decision it cannot audit. The sender is dropped (not restored)
    // so a Deny path cannot permanently strand the receiver behind a terminal row.
    h.plane.fail_next_append();
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::Internal(m) if m.contains("injected append failure")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Running);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    assert!(h
        .events_of_type(session, SessionEventType::ApprovalResolved)
        .await
        .is_empty());

    // Re-register after the closed waiter and resolve exactly once.
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.runs().approve(run, gate, Approval::Allow).await.unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Allow);
    assert_eq!(
        h.events_of_type(session, SessionEventType::ApprovalResolved)
            .await
            .len(),
        1
    );
    assert_eq!(h.plane.metrics().approvals_resolved, 1);
    h.close().await;
}

#[tokio::test]
async fn run_approve_deny_drops_waiter_when_append_fails() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let mut rx = h.plane.register_gate_waiter(run, gate).await.unwrap();

    h.plane.fail_next_append();
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Deny).await.unwrap_err(),
        RunError::Internal(m) if m.contains("injected append failure")
    ));
    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    // Terminal + closed: no production path can release a restored sender.
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Deny).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "terminal"
    ));
    h.close().await;
}

#[tokio::test]
async fn session_resume_repairs_failed_approval_without_terminal_events() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    // Acceptance is durable so resume owes RunFinished after repairing the Deny window.
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(h.count_accepted(run).await, 1);
    // Crash after Failed upsert, before ApprovalResolved / RunCompleted / RunFinished.
    h.set_run_state(run, RunControlState::Failed).await;

    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    let resolved = h
        .events_of_type(session, SessionEventType::ApprovalResolved)
        .await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["decision"], json!("deny"));
    assert_eq!(
        resolved[0].payload["reason"],
        json!("resume_finalized_approval_denied")
    );
    let completed = h
        .events_of_type(session, SessionEventType::RunCompleted)
        .await;
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].payload["dag_state"], json!("failed"));
    assert_eq!(completed[0].payload["reason"], json!("approval_denied"));
    assert_eq!(h.count_finished(run).await, 1);

    // Second resume is idempotent.
    h.sessions().resume(session).await.unwrap();
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn session_resume_repairs_missing_run_finished_when_run_completed_exists() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(h.count_accepted(run).await, 1);

    // Crash after Failed + ApprovalResolved + RunCompleted, before RunFinished.
    h.set_run_state(run, RunControlState::Failed).await;
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_: SessionEventType::ApprovalResolved,
            payload: json!({
                "decision": "deny",
                "reason": "approval_denied",
            }),
        })
        .await
        .unwrap();
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_: SessionEventType::RunCompleted,
            payload: json!({
                "dag_state": "failed",
                "reason": "approval_denied",
            }),
        })
        .await
        .unwrap();
    assert_eq!(h.count_finished(run).await, 0);

    h.sessions().resume(session).await.unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert_eq!(
        h.events_of_type(session, SessionEventType::ApprovalResolved)
            .await
            .len(),
        1
    );
    assert_eq!(
        h.events_of_type(session, SessionEventType::RunCompleted)
            .await
            .len(),
        1
    );
    assert_eq!(h.count_finished(run).await, 1);

    h.sessions().resume(session).await.unwrap();
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

#[tokio::test]
async fn run_approve_deny_emits_run_finished_after_redispatch() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    // First dispatch announces acceptance, then fails to find a scheduler.
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(h.count_accepted(run).await, 1);

    h.install_scheduler(MockScheduler::new([Plan::State(DagState::WaitingApproval)]));
    h.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::WaitingApproval);

    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.runs().approve(run, gate, Approval::Deny).await.unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Deny);
    assert_eq!(h.run_state(run).await, RunControlState::Failed);
    assert_eq!(h.count_finished(run).await, 1);
    assert_eq!(h.count_accepted(run).await, 1);

    // A run this process never dispatched still gets its terminal event: durable state
    // having left `created` is what proves acceptance was already announced.
    let restored = h.submit(session).await;
    h.seed_dag(restored).await;
    h.set_run_state(restored, RunControlState::WaitingApproval)
        .await;
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(restored, gate).await.unwrap();
    h.runs()
        .approve(restored, gate, Approval::Deny)
        .await
        .unwrap();
    assert_eq!(rx.await.unwrap(), Approval::Deny);
    assert_eq!(h.run_state(restored).await, RunControlState::Failed);
    assert_eq!(h.count_finished(restored).await, 1);
    h.close().await;
}

/// Dogfood CI race (2026-07-29): the scheduler persists
/// `DagState::WaitingApproval` before the run row flips to
/// `waiting_approval`, so an approver acting on the published DAG state
/// could land in the gap and be rejected — even though the durable
/// `ApprovalRequested` already exists and SQ9 resolution would be picked
/// up by the scheduler's durable scan. A durable gate request now
/// satisfies the phase guard for a non-terminal run.
#[tokio::test]
async fn run_approve_accepts_durable_gate_request_before_state_flip() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    let gate = GateId::new();
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_: SessionEventType::ApprovalRequested,
            payload: json!({ "gate_id": gate.to_string() }),
        })
        .await
        .unwrap();
    h.set_run_state(run, RunControlState::Running).await;
    h.runs().approve(run, gate, Approval::Allow).await.unwrap();
    // Same guard on the deny side.
    let run2 = h.submit(session).await;
    h.seed_dag(run2).await;
    let gate2 = GateId::new();
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run2),
            type_: SessionEventType::ApprovalRequested,
            payload: json!({ "gate_id": gate2.to_string() }),
        })
        .await
        .unwrap();
    h.set_run_state(run2, RunControlState::Running).await;
    h.runs().approve(run2, gate2, Approval::Deny).await.unwrap();
    h.close().await;
}

/// A durable `ApprovalRequested` only satisfies the phase guard while the
/// gate is still OPEN: once a durable `ApprovalResolved` exists for it, a
/// later approval on a non-waiting run must be rejected (external review
/// finding on #54 — the first fallback re-approved resolved gates).
#[tokio::test]
async fn run_approve_rejects_resolved_durable_gate() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    let gate = GateId::new();
    for (type_, payload) in [
        (
            SessionEventType::ApprovalRequested,
            json!({ "gate_id": gate.to_string() }),
        ),
        (
            SessionEventType::ApprovalResolved,
            json!({ "gate_id": gate.to_string(), "decision": "allow", "generation": 1 }),
        ),
    ] {
        h.storage
            .events()
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: Some(run),
                type_,
                payload,
            })
            .await
            .unwrap();
    }
    h.set_run_state(run, RunControlState::Running).await;
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "not waiting approval"
    ));
    // A RE-request after the resolution reopens the gate (GR3 re-emission).
    h.storage
        .events()
        .append_session(NewSessionEvent {
            session_id: session,
            run_id: Some(run),
            type_: SessionEventType::ApprovalRequested,
            payload: json!({ "gate_id": gate.to_string() }),
        })
        .await
        .unwrap();
    h.runs().approve(run, gate, Approval::Allow).await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn run_approve_requires_waiting_approval() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    let gate = GateId::new();

    // Waiter present but state is not `waiting_approval`: state guard wins.
    h.set_run_state(run, RunControlState::Accepted).await;
    let _rx = h.plane.register_gate_waiter(run, gate).await.unwrap();
    h.set_run_state(run, RunControlState::Running).await;
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "not waiting approval"
    ));

    h.set_run_state(run, RunControlState::Cancelling).await;
    assert!(matches!(
        h.runs().approve(run, gate, Approval::Allow).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "cancelling"
    ));

    // Waiting with no waiter is the only `UnknownGate` case.
    h.set_run_state(run, RunControlState::WaitingApproval).await;
    assert!(matches!(
        h.runs()
            .approve(run, GateId::new(), Approval::Allow)
            .await
            .unwrap_err(),
        RunError::UnknownGate(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn run_request_replan_records_event_and_clears_waiters() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();
    let rx = h.plane.register_gate_waiter(run, gate).await.unwrap();

    h.runs()
        .request_replan(run, ReplanReason::BudgetPolicy)
        .await
        .unwrap();

    assert_eq!(h.run_state(run).await, RunControlState::ReplanRequested);
    assert!(rx.await.is_err());
    let replans: Vec<_> = h
        .session_events(session)
        .await
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::ReplanRequested)
        .collect();
    assert_eq!(replans.len(), 1);
    assert_eq!(replans[0].payload["reason"], json!("budget_policy"));

    // Idempotent, and a replan-pending run is not startable.
    h.runs()
        .request_replan(run, ReplanReason::UserRequested)
        .await
        .unwrap();
    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "replan pending"
    ));
    assert_eq!(h.plane.metrics().replans_requested, 1);
    h.close().await;
}

#[tokio::test]
async fn run_request_replan_rejects_created_and_terminal() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    assert!(matches!(
        h.runs()
            .request_replan(run, ReplanReason::UserRequested)
            .await
            .unwrap_err(),
        RunError::InvalidPhase(m) if m == "not started"
    ));

    h.set_run_state(run, RunControlState::Succeeded).await;
    assert!(matches!(
        h.runs()
            .request_replan(run, ReplanReason::UserRequested)
            .await
            .unwrap_err(),
        RunError::InvalidPhase(m) if m == "terminal"
    ));
    h.close().await;
}

#[tokio::test]
async fn run_unknown_state_string_is_invalid_phase() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    let row = h.run_row(run).await;
    h.storage
        .sessions()
        .upsert_run(&RunRow {
            state: "from_the_future".to_owned(),
            ..row
        })
        .await
        .unwrap();

    assert!(matches!(
        h.runs().start(run).await.unwrap_err(),
        RunError::InvalidPhase(m) if m.contains("unknown run state")
    ));
    // The session itself still resumes and lists the row.
    h.sessions().resume(session).await.unwrap();
    h.close().await;
}

// ---------------------------------------------------------------- SessionPlane

#[tokio::test]
async fn budget_warning_hook_appends_event() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    let seq = h
        .plane
        .signal_budget_warning(
            session,
            Some(run),
            BudgetSnapshot {
                usd_spent: 4.5,
                tokens_in: 10,
                tokens_out: 20,
            },
            "approaching run budget",
        )
        .await
        .unwrap();
    assert_eq!(seq, EventSeq(2));

    let events = h.session_events(session).await;
    let warning = events.last().unwrap();
    assert_eq!(warning.type_, SessionEventType::BudgetWarning);
    assert_eq!(warning.run_id, Some(run));
    assert_eq!(warning.payload["message"], json!("approaching run budget"));
    assert_eq!(warning.payload["snapshot"]["tokens_in"], json!(10));
    assert_eq!(h.plane.metrics().budget_warnings, 1);

    assert!(matches!(
        h.plane
            .signal_budget_warning(
                SessionId::new(),
                None,
                BudgetSnapshot {
                    usd_spent: 0.0,
                    tokens_in: 0,
                    tokens_out: 0,
                },
                "no session",
            )
            .await
            .unwrap_err(),
        SessionError::NotFound(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn register_gate_waiter_rejects_created_and_terminal() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    let gate = GateId::new();

    assert!(matches!(
        h.plane.register_gate_waiter(run, gate).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "not started"
    ));

    h.set_run_state(run, RunControlState::Failed).await;
    assert!(matches!(
        h.plane.register_gate_waiter(run, gate).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "terminal"
    ));

    assert!(matches!(
        h.plane
            .register_gate_waiter(RunId::new(), gate)
            .await
            .unwrap_err(),
        RunError::NotFound(_)
    ));
    h.close().await;
}

#[tokio::test]
async fn register_gate_waiter_rejects_replan_requested() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    h.runs()
        .request_replan(run, ReplanReason::UserRequested)
        .await
        .unwrap();

    assert!(matches!(
        h.plane.register_gate_waiter(run, GateId::new()).await.unwrap_err(),
        RunError::InvalidPhase(m) if m == "replan pending"
    ));
    // Registering must not rewrite a pending replan back to `waiting_approval`.
    assert_eq!(h.run_state(run).await, RunControlState::ReplanRequested);
    h.close().await;
}

#[tokio::test]
async fn register_gate_waiter_replaces_prior_waiter() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;
    h.set_run_state(run, RunControlState::Accepted).await;
    let gate = GateId::new();

    let first = h.plane.register_gate_waiter(run, gate).await.unwrap();
    let second = h.plane.register_gate_waiter(run, gate).await.unwrap();

    h.plane.approve(run, gate, Approval::Allow).await.unwrap();
    assert!(first.await.is_err(), "prior receiver errs on replacement");
    assert_eq!(second.await.unwrap(), Approval::Allow);
    h.close().await;
}

// -------------------------------------------------- RFC-0017 AM-0003-1/2/3

/// AC 29: `resume_after_replan` re-arms `ReplanRequested → Accepted`,
/// appends `ReplanResumed`, is idempotent from `Accepted` (no second
/// event), and rejects every other state.
#[tokio::test]
async fn ac29_resume_after_replan_state_machine() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await; // dag row (non-Running) with generation 0

    // Not resumable before a replan was requested.
    assert!(matches!(
        h.runs().resume_after_replan(run).await.unwrap_err(),
        RunError::InvalidPhase(_)
    ));

    h.set_run_state(run, RunControlState::ReplanRequested).await;
    h.runs().resume_after_replan(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);
    let resumed = h
        .events_of_type(session, SessionEventType::ReplanResumed)
        .await;
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].payload["generation"], 0);
    assert_eq!(resumed[0].run_id, Some(run));

    // Idempotent from Accepted: Ok, no second event.
    h.runs().resume_after_replan(run).await.unwrap();
    assert_eq!(
        h.events_of_type(session, SessionEventType::ReplanResumed)
            .await
            .len(),
        1
    );

    // Every other state is InvalidPhase.
    for state in [
        RunControlState::Running,
        RunControlState::WaitingApproval,
        RunControlState::Cancelling,
        RunControlState::Cancelled,
        RunControlState::Succeeded,
        RunControlState::Failed,
    ] {
        h.set_run_state(run, state).await;
        assert!(
            matches!(
                h.runs().resume_after_replan(run).await.unwrap_err(),
                RunError::InvalidPhase(_)
            ),
            "resume_after_replan must reject {state:?}"
        );
    }
    h.close().await;
}

/// AC 29: `resume_after_replan` refuses while an execution lease is held,
/// even from `ReplanRequested`.
#[tokio::test]
async fn ac29_resume_after_replan_rejects_held_lease() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let start = tokio::spawn({
        let runs = Arc::clone(&runs);
        async move { runs.start(run).await }
    });
    sched.entered.notified().await;

    // Lease held: a foreign replan_requested write plus resume must not race
    // the in-flight dispatch.
    h.set_run_state(run, RunControlState::ReplanRequested).await;
    assert!(matches!(
        h.runs().resume_after_replan(run).await.unwrap_err(),
        RunError::InvalidPhase(_)
    ));

    sched.release.notify_one();
    start.await.unwrap().unwrap();
    h.close().await;
}

/// AC 29: after an external replan cycle, a following `start` emits no
/// second `RunAccepted`.
#[tokio::test]
async fn ac29_resume_then_start_emits_no_second_run_accepted() {
    let h = Harness::new().await;
    h.install_scheduler(MockScheduler::new([
        Plan::State(DagState::ReplanRequired),
        Plan::State(DagState::Succeeded),
    ]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    h.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::ReplanRequested);
    assert_eq!(h.count_accepted(run).await, 1);

    h.runs().resume_after_replan(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Accepted);

    h.runs().start(run).await.unwrap();
    assert_eq!(h.run_state(run).await, RunControlState::Succeeded);
    assert_eq!(h.count_accepted(run).await, 1, "exactly one RunAccepted");
    assert_eq!(h.count_finished(run).await, 1);
    h.close().await;
}

/// AC 29b: the in-run generation methods are lease-gated — without a live
/// dispatch both return `InvalidPhase` and write nothing.
#[tokio::test]
async fn ac29b_repair_generation_methods_require_lease() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.set_run_state(run, RunControlState::Running).await;

    assert!(matches!(
        h.runs()
            .begin_repair_generation(run, &ReplanReason::UserRequested)
            .await
            .unwrap_err(),
        RunError::InvalidPhase(_)
    ));
    assert!(matches!(
        h.runs()
            .complete_repair_generation(run, 2)
            .await
            .unwrap_err(),
        RunError::InvalidPhase(_)
    ));
    assert!(h
        .events_of_type(session, SessionEventType::ReplanRequested)
        .await
        .is_empty());
    assert!(h
        .events_of_type(session, SessionEventType::ReplanResumed)
        .await
        .is_empty());
    assert_eq!(h.run_state(run).await, RunControlState::Running);
    h.close().await;
}

/// AC 29b: inside a live dispatch, `begin_repair_generation` drops gate
/// waiters and appends `ReplanRequested` **without** writing
/// `replan_requested` to the row; `complete_repair_generation` appends
/// `ReplanResumed`; the row state is never touched by either.
#[tokio::test]
async fn ac29b_begin_complete_repair_generation_inside_dispatch() {
    let h = Harness::new().await;
    let sched = h.install_scheduler(MockScheduler::new([Plan::BlockThen(DagState::Succeeded)]));
    let session = h.create_session().await;
    let run = h.submit(session).await;
    h.seed_dag(run).await;

    let runs = h.runs();
    let start = tokio::spawn({
        let runs = Arc::clone(&runs);
        async move { runs.start(run).await }
    });
    sched.entered.notified().await;

    // A waiter registered mid-dispatch (as a gate adapter would).
    let gate = GateId::new();
    let waiter = h.plane.register_gate_waiter(run, gate).await.unwrap();

    h.runs()
        .begin_repair_generation(run, &ReplanReason::UserRequested)
        .await
        .unwrap();
    assert!(waiter.await.is_err(), "waiters dropped (SEC9b)");
    let requested = h
        .events_of_type(session, SessionEventType::ReplanRequested)
        .await;
    assert_eq!(requested.len(), 1);
    assert_eq!(
        requested[0].payload["reason"],
        serde_json::json!("user_requested")
    );
    // RC1: the row reads `running` for the loop — never `replan_requested`.
    assert_eq!(h.run_state(run).await, RunControlState::Running);

    h.runs().complete_repair_generation(run, 2).await.unwrap();
    let resumed = h
        .events_of_type(session, SessionEventType::ReplanResumed)
        .await;
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].payload["generation"], 2);
    assert_eq!(h.run_state(run).await, RunControlState::Running);

    sched.release.notify_one();
    // The row is `waiting_approval` (the waiter registration wrote it), which
    // control-protects against the Succeeded outcome — the merge is §6.3
    // step 9(a)'s business, not this test's. What matters here: neither
    // AM-0003-3 method wrote `replan_requested` at any point.
    start.await.unwrap().unwrap();
    assert_ne!(h.run_state(run).await, RunControlState::ReplanRequested);
    h.close().await;
}

/// AC 29b: `control_state` reads the durable state without writing.
#[tokio::test]
async fn ac29b_control_state_reads_without_writing() {
    let h = Harness::new().await;
    let session = h.create_session().await;
    let run = h.submit(session).await;

    assert_eq!(
        h.runs().control_state(run).await.unwrap(),
        RunControlState::Created
    );
    let row_before = h.run_row(run).await;
    h.set_run_state(run, RunControlState::Cancelling).await;
    assert_eq!(
        h.runs().control_state(run).await.unwrap(),
        RunControlState::Cancelling
    );
    // No writes: only our own set_run_state touched the row.
    let row_after = h.run_row(run).await;
    assert_eq!(row_after.state, "cancelling");
    assert_eq!(row_before.goal_json, row_after.goal_json);
    assert!(matches!(
        h.runs().control_state(RunId::new()).await.unwrap_err(),
        RunError::NotFound(_)
    ));
    let _ = session;
    h.close().await;
}
