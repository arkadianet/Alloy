//! Integration tests for the RFC-0003 control plane: restart recovery + concurrency.

use std::path::Path;
use std::sync::Arc;

use alloy_runtime::session::SessionPlane;
use alloy_runtime::storage::{EventStore, SessionRows, StorageOpenOptions};
use alloy_runtime::{
    install_sqlite_event_sink, AlloyRuntime, AlloyStorage, BudgetPolicy, ConfigPaths,
    CreateSession, EventSeq, Goal, LanguageId, ProfileId, RunControlState, RunError, RunRow,
    RuntimeEvent, SessionEvent, SessionEventType, SessionId, SessionService,
};

/// Runtime + storage + plane over `dir`, reusable across simulated restarts.
struct Host {
    rt: AlloyRuntime,
    storage: Arc<AlloyStorage>,
    plane: SessionPlane,
}

impl Host {
    async fn open(dir: &Path) -> Self {
        write_fixtures(dir);
        let cfg = alloy_runtime::RuntimeConfig::load(ConfigPaths {
            profile: dir.join("profiles/default.toml"),
            router: dir.join("router.toml"),
            example_env: dir.join("example.env"),
            data_dir: Some(dir.join("data")),
            workspace_root: Some(dir.to_path_buf()),
        })
        .unwrap();
        let data_dir = cfg.data_dir.clone();
        let mut rt = AlloyRuntime::new();
        rt.configure(cfg).unwrap();
        let handle = rt.start().await.unwrap();
        let storage =
            install_sqlite_event_sink(&handle, Some(StorageOpenOptions::for_data_dir(data_dir)))
                .await
                .unwrap();
        let plane = SessionPlane::new(handle, Arc::clone(&storage));
        Self { rt, storage, plane }
    }

    fn sessions(&self) -> Arc<dyn SessionService> {
        self.plane.sessions()
    }

    async fn run_state(&self, run: alloy_runtime::RunId) -> RunControlState {
        let row = self
            .storage
            .sessions()
            .get_run(run)
            .await
            .unwrap()
            .expect("run row");
        RunControlState::parse(&row.state).expect("known state")
    }

    /// Simulate a crash mid-transition by writing a control state directly.
    async fn force_run_state(&self, run: alloy_runtime::RunId, state: RunControlState) {
        let row = self
            .storage
            .sessions()
            .get_run(run)
            .await
            .unwrap()
            .expect("run row");
        self.storage
            .sessions()
            .upsert_run(&RunRow {
                state: state.as_str().to_owned(),
                ..row
            })
            .await
            .unwrap();
    }

    async fn finished_events(&self, run: alloy_runtime::RunId) -> usize {
        self.storage
            .events()
            .list_runtime_events(None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(
                |(_, ev)| matches!(ev, RuntimeEvent::RunFinished { run_id, .. } if *run_id == run),
            )
            .count()
    }

    async fn accepted_events(&self) -> usize {
        self.storage
            .events()
            .list_runtime_events(None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|(_, ev)| matches!(ev, RuntimeEvent::RunAccepted { .. }))
            .count()
    }

    /// Simulate a clean process exit: drain the runtime, then close the store.
    async fn shutdown(self) {
        self.rt.shutdown().await.unwrap();
        self.storage.close().await.unwrap();
    }
}

fn write_fixtures(dir: &Path) {
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/default.toml"),
        r#"
[profile]
id = "default"
[budgets]
max_usd_per_run = 1.0
max_tokens_per_run = 1000
[observability]
retain_full_prompts = false
retain_tool_bodies = false
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("router.toml"),
        r#"
[policy]
default_tier = "standard"

[[providers]]
id = "openai-compatible-main"
kind = "openai_compatible"
base_url = "https://api.example.com/v1/"
api_key_env = "ALLOY_API_KEY"

[[providers.endpoints]]
id = "team-workhorse"
display_name = "Workhorse"
model = "REPLACE_ME"
tiers = ["standard"]
max_context = 200000
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0

[capability_tiers]
repair = "standard"
"#,
    )
    .unwrap();
    std::fs::write(dir.join("example.env"), "ALLOY_API_KEY=\n").unwrap();
}

fn create_req(dir: &Path) -> CreateSession {
    CreateSession {
        workspace_root: dir.to_path_buf(),
        profile: ProfileId::new("default").unwrap(),
        budget: BudgetPolicy::default(),
        language_backends: vec![LanguageId::new("rust").unwrap()],
    }
}

fn goal(text: &str) -> Goal {
    Goal {
        text: text.to_owned(),
        constraints: vec![],
        attachments: vec![],
    }
}

fn as_json(events: &[SessionEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

#[tokio::test]
async fn session_resume_after_restart_is_bit_identical() {
    let dir = tempfile::tempdir().unwrap();

    let host = Host::open(dir.path()).await;
    let session = host
        .sessions()
        .create(create_req(dir.path()))
        .await
        .unwrap();
    let run = host
        .sessions()
        .submit_goal(session, goal("ship rfc-0003"))
        .await
        .unwrap();
    let before = host.sessions().events(session, None, 100).await.unwrap();
    assert_eq!(before.len(), 2);
    host.shutdown().await;

    let host = Host::open(dir.path()).await;
    let resumed = host.sessions().resume(session).await.unwrap();
    assert_eq!(resumed.id, session);
    assert_eq!(resumed.workspace_root, dir.path());
    assert_eq!(resumed.profile.as_str(), "default");
    assert_eq!(resumed.language_backends[0].as_str(), "rust");

    let after = host.sessions().events(session, None, 100).await.unwrap();
    assert_eq!(
        as_json(&after),
        as_json(&before),
        "seq/ts/payload preserved"
    );
    assert_eq!(host.run_state(run).await, RunControlState::Created);
    host.shutdown().await;
}

#[tokio::test]
async fn session_sqlite_cursor_after_restart() {
    let dir = tempfile::tempdir().unwrap();

    let host = Host::open(dir.path()).await;
    let session = host
        .sessions()
        .create(create_req(dir.path()))
        .await
        .unwrap();
    for i in 0..3 {
        host.sessions()
            .submit_goal(session, goal(&format!("goal {i}")))
            .await
            .unwrap();
    }
    let page = host.sessions().events(session, None, 2).await.unwrap();
    let cursor = page.last().unwrap().seq;
    assert_eq!(cursor, EventSeq(1));
    host.shutdown().await;

    let host = Host::open(dir.path()).await;
    let rest = host
        .sessions()
        .events(session, Some(cursor), 100)
        .await
        .unwrap();
    assert_eq!(
        rest.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![EventSeq(2), EventSeq(3)]
    );

    // New appends continue the same gapless sequence across the restart.
    host.sessions()
        .submit_goal(session, goal("after restart"))
        .await
        .unwrap();
    let tail = host
        .sessions()
        .events(session, Some(EventSeq(3)), 100)
        .await
        .unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, EventSeq(4));
    host.shutdown().await;
}

#[tokio::test]
async fn run_accepted_survives_restart_and_redispatch() {
    let dir = tempfile::tempdir().unwrap();

    let host = Host::open(dir.path()).await;
    let session = host
        .sessions()
        .create(create_req(dir.path()))
        .await
        .unwrap();
    let run = host
        .sessions()
        .submit_goal(session, goal("needs a scheduler"))
        .await
        .unwrap();
    assert!(matches!(
        host.plane.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(host.run_state(run).await, RunControlState::Accepted);
    assert_eq!(host.accepted_events().await, 1);
    host.shutdown().await;

    let host = Host::open(dir.path()).await;
    host.sessions().resume(session).await.unwrap();
    assert_eq!(host.run_state(run).await, RunControlState::Accepted);

    // Re-dispatchable with the MVP NullScheduler, and acceptance is announced once.
    assert!(matches!(
        host.plane.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    assert_eq!(host.run_state(run).await, RunControlState::Accepted);
    assert_eq!(host.accepted_events().await, 1);
    assert_eq!(host.plane.metrics().runs_start_unavailable, 1);
    host.shutdown().await;
}

#[tokio::test]
async fn cancelling_run_is_finalized_after_restart() {
    let dir = tempfile::tempdir().unwrap();

    let host = Host::open(dir.path()).await;
    let session = host
        .sessions()
        .create(create_req(dir.path()))
        .await
        .unwrap();
    let run = host
        .sessions()
        .submit_goal(session, goal("cancel me"))
        .await
        .unwrap();
    // Accept once so RunAccepted is durable, then crash mid-cancel.
    assert!(matches!(
        host.plane.runs().start(run).await.unwrap_err(),
        RunError::SchedulerUnavailable
    ));
    host.force_run_state(run, RunControlState::Cancelling).await;
    host.shutdown().await;

    let host = Host::open(dir.path()).await;
    host.sessions().resume(session).await.unwrap();

    assert_eq!(host.run_state(run).await, RunControlState::Cancelled);
    let completed: Vec<_> = host
        .sessions()
        .events(session, None, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.type_ == SessionEventType::RunCompleted)
        .collect();
    assert_eq!(completed.len(), 1, "resume owes exactly one RunCompleted");
    assert_eq!(completed[0].run_id, Some(run));
    assert_eq!(host.finished_events(run).await, 1);

    // Finalization is durable: a second resume is a no-op.
    host.sessions().resume(session).await.unwrap();
    assert_eq!(host.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(host.finished_events(run).await, 1);
    host.shutdown().await;
}

#[tokio::test]
async fn created_cancelling_resume_skips_run_finished() {
    let dir = tempfile::tempdir().unwrap();

    let host = Host::open(dir.path()).await;
    let session = host
        .sessions()
        .create(create_req(dir.path()))
        .await
        .unwrap();
    let run = host
        .sessions()
        .submit_goal(session, goal("never started"))
        .await
        .unwrap();
    // Historical shape: created → cancelling without RunAccepted.
    host.force_run_state(run, RunControlState::Cancelling).await;
    host.shutdown().await;

    let host = Host::open(dir.path()).await;
    host.sessions().resume(session).await.unwrap();
    assert_eq!(host.run_state(run).await, RunControlState::Cancelled);
    assert_eq!(host.finished_events(run).await, 0);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_and_submitters_are_gapless() {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::open(dir.path()).await;

    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.push(
            host.sessions()
                .create(create_req(dir.path()))
                .await
                .unwrap(),
        );
    }

    let mut tasks = Vec::new();
    for session in sessions.clone() {
        let svc = host.sessions();
        tasks.push(tokio::spawn(async move {
            for i in 0..5 {
                svc.submit_goal(session, goal(&format!("goal {i}")))
                    .await
                    .unwrap();
            }
        }));
        let reader = host.sessions();
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                reader.events(session, None, 100).await.unwrap();
                tokio::task::yield_now().await;
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    for session in sessions {
        let events = host.sessions().events(session, None, 100).await.unwrap();
        assert_eq!(events.len(), 6, "SessionCreated + 5 GoalSubmitted");
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.seq, EventSeq(i as u64));
            assert_eq!(ev.session_id, session);
        }
    }
    host.shutdown().await;
}

#[tokio::test]
async fn resume_of_unknown_session_after_restart_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::open(dir.path()).await;
    let missing = SessionId::new();
    assert!(matches!(
        host.sessions().resume(missing).await.unwrap_err(),
        alloy_runtime::SessionError::NotFound(got) if got == missing
    ));
    host.shutdown().await;
}
