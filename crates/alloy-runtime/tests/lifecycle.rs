//! Integration tests for AlloyRuntime lifecycle (RFC-0001).

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    AlloyRuntime, ConfigPaths, DagId, EventSink, InMemoryEventSink, NewSessionEvent, NullScheduler,
    RuntimeConfig, RuntimeError, RuntimeEvent, RuntimePhase, SchedError, Scheduler,
    SessionEventType, SessionId,
};
use async_trait::async_trait;
use serde_json::json;

fn test_config(dir: &std::path::Path) -> RuntimeConfig {
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
[provider.default]
kind = "openai_compatible"
"#,
    )
    .unwrap();
    std::fs::write(dir.join("example.env"), "ALLOY_API_KEY=\n").unwrap();

    RuntimeConfig::load(ConfigPaths {
        profile: dir.join("profiles/default.toml"),
        router: dir.join("router.toml"),
        example_env: dir.join("example.env"),
        data_dir: None,
        workspace_root: Some(dir.to_path_buf()),
    })
    .unwrap()
}

#[tokio::test]
async fn lifecycle_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    assert_eq!(rt.handle().phase(), RuntimePhase::Created);
    rt.configure(test_config(dir.path())).unwrap();
    assert_eq!(rt.handle().phase(), RuntimePhase::Configured);
    let handle = rt.start().await.unwrap();
    assert_eq!(handle.phase(), RuntimePhase::Running);
    rt.drain(Duration::from_millis(20)).await.unwrap();
    assert_eq!(rt.handle().phase(), RuntimePhase::Draining);
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_from_created() {
    let rt = AlloyRuntime::new();
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_phase_run_before_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let err = rt.run(DagId::new()).await.unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidPhase { op: "run", .. }));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn double_configure_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let err = rt
        .configure(test_config(dir.path()))
        .err()
        .expect("second configure must fail");
    assert!(matches!(
        err,
        RuntimeError::InvalidPhase {
            op: "configure",
            ..
        }
    ));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn null_scheduler_maps_to_scheduler_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let _ = rt.start().await.unwrap();
    let err = rt.run(DagId::new()).await.unwrap_err();
    assert!(matches!(err, RuntimeError::SchedulerUnavailable));
    // Must not be Scheduler(Unavailable)
    assert!(!matches!(
        err,
        RuntimeError::Scheduler(SchedError::Unavailable)
    ));
    rt.shutdown().await.unwrap();
}

struct GateScheduler {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl Scheduler for GateScheduler {
    async fn run(&self, dag_id: DagId) -> Result<alloy_runtime::DagOutcome, SchedError> {
        self.entered.notify_waiters();
        self.release.notified().await;
        Ok(alloy_runtime::DagOutcome {
            dag_id,
            generation: 0,
            state: alloy_runtime::DagState::Succeeded,
            failed_node: None,
            failure: None,
        })
    }
    async fn cancel(&self, _dag_id: DagId) -> Result<(), SchedError> {
        self.release.notify_waiters();
        Ok(())
    }
}

#[tokio::test]
async fn single_flight_busy() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    let sched = Arc::new(GateScheduler {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    handle.set_scheduler(sched.clone()).await.unwrap();

    {
        let entered = sched.entered.notified();
        let mut run_fut = std::pin::pin!(rt.run(DagId::new()));
        tokio::select! {
            _ = &mut run_fut => panic!("run finished before release"),
            _ = entered => {}
        }
        let busy = rt.run(DagId::new()).await.unwrap_err();
        assert!(matches!(busy, RuntimeError::SchedulerBusy));
        sched.release.notify_waiters();
        let outcome = run_fut.await.unwrap();
        assert_eq!(outcome.state, alloy_runtime::DagState::Succeeded);
    }
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_token_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    let token = handle.cancellation();
    assert!(!token.is_cancelled());
    rt.shutdown().await.unwrap();
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn drop_without_shutdown_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let _ = rt.start().await.unwrap();
    drop(rt);
}

#[tokio::test]
async fn config_never_writes_dotenv() {
    let dir = tempfile::tempdir().unwrap();
    let _ = test_config(dir.path());
    assert!(!dir.path().join(".env").exists());
}

#[tokio::test]
async fn event_seq_interleaved_sessions_via_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    let a = SessionId::new();
    let b = SessionId::new();
    let mk = |session_id| NewSessionEvent {
        session_id,
        run_id: None,
        type_: SessionEventType::GoalSubmitted,
        payload: json!({}),
    };
    assert_eq!(handle.append_session(mk(a)).await.unwrap().0, 0);
    assert_eq!(handle.append_session(mk(b)).await.unwrap().0, 0);
    assert_eq!(handle.append_session(mk(a)).await.unwrap().0, 1);
    let events = handle.memory_sink().session_events(a);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq.0, 0);
    assert_eq!(events[1].seq.0, 1);
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_event_sink_refuses_non_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    handle.emit(RuntimeEvent::Started).await.unwrap();
    let err = handle
        .set_event_sink(Arc::new(InMemoryEventSink::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Internal(_)));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn workspace_has_five_members() {
    let manifest = include_str!("../../../Cargo.toml");
    let members: Vec<_> = manifest
        .lines()
        .filter(|l| l.trim().starts_with("\"crates/"))
        .collect();
    assert_eq!(members.len(), 5, "{members:?}");
}

#[tokio::test]
async fn null_scheduler_type_is_default() {
    let _ = NullScheduler;
    let sink: Arc<dyn EventSink> = Arc::new(InMemoryEventSink::new());
    assert_eq!(sink.buffered_len(), 0);
}
