//! Integration tests for AlloyRuntime lifecycle (RFC-0001).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    AlloyRuntime, ConfigPaths, CreateSession, DagId, DagOutcome, DagState, DiagnosticEvent,
    DiagnosticLevel, Digest, FailureIr, Glob, Grant, InMemoryEventSink, NewSessionEvent,
    NullScheduler, ProfileId, RuntimeConfig, RuntimeError, RuntimeEvent, RuntimePhase, SchedError,
    Scheduler, SessionEventType, SessionId, Timestamp,
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
api_key_env = "ALLOY_API_KEY"
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
async fn start_before_configure_rejected() {
    let mut rt = AlloyRuntime::new();
    let err = match rt.start().await {
        Ok(_) => panic!("start before configure must fail"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        RuntimeError::InvalidPhase {
            op: "start",
            current: RuntimePhase::Created,
            ..
        }
    ));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn double_start_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let _ = rt.start().await.unwrap();
    let err = match rt.start().await {
        Ok(_) => panic!("second start must fail"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        RuntimeError::InvalidPhase {
            op: "start",
            current: RuntimePhase::Running,
            ..
        }
    ));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn data_dir_create_failure_is_io_and_failed_phase() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    // Place a regular file where the data dir should be created.
    let blocked = dir.path().join("blocked-data-dir");
    std::fs::write(&blocked, b"not a directory").unwrap();
    cfg.data_dir = blocked;
    cfg.data_dir_rule = "test";

    let mut rt = AlloyRuntime::new();
    rt.configure(cfg).unwrap();
    let err = match rt.start().await {
        Ok(_) => panic!("start with blocked data_dir must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, RuntimeError::Io(_)), "expected Io, got {err}");
    assert_eq!(rt.handle().phase(), RuntimePhase::Failed);
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
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        self.entered.notify_waiters();
        self.release.notified().await;
        Ok(DagOutcome {
            dag_id,
            generation: 0,
            state: DagState::Succeeded,
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
    handle.set_scheduler(sched.clone()).unwrap();

    {
        let entered = sched.entered.notified();
        let mut run_fut = std::pin::pin!(rt.run(DagId::new()));
        tokio::select! {
            _ = &mut run_fut => panic!("run finished before release"),
            _ = entered => {}
        }
        let busy = rt.run(DagId::new()).await.unwrap_err();
        assert!(matches!(busy, RuntimeError::SchedulerBusy));
        let replace = handle.set_scheduler(Arc::new(NullScheduler));
        assert!(matches!(replace, Err(RuntimeError::SchedulerBusy)));
        sched.release.notify_waiters();
        let outcome = run_fut.await.unwrap();
        assert_eq!(outcome.state, DagState::Succeeded);
    }
    rt.shutdown().await.unwrap();
}

struct RecordingScheduler {
    cancelled: Arc<AtomicUsize>,
    last_cancel: Arc<std::sync::Mutex<Option<DagId>>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl Scheduler for RecordingScheduler {
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
        self.entered.notify_waiters();
        self.release.notified().await;
        Ok(DagOutcome {
            dag_id,
            generation: 0,
            state: DagState::Cancelled,
            failed_node: None,
            failure: None,
        })
    }
    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError> {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        *self
            .last_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dag_id);
        self.release.notify_waiters();
        Ok(())
    }
}

#[tokio::test]
async fn drain_cancels_active_dag_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    let dag = DagId::new();
    let sched = Arc::new(RecordingScheduler {
        cancelled: Arc::new(AtomicUsize::new(0)),
        last_cancel: Arc::new(std::sync::Mutex::new(None)),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    handle.set_scheduler(sched.clone()).unwrap();

    {
        let entered = sched.entered.notified();
        let mut run_fut = std::pin::pin!(rt.run(dag));
        tokio::select! {
            _ = &mut run_fut => panic!("finished early"),
            _ = entered => {}
        }
        rt.drain(Duration::from_millis(200)).await.unwrap();
        let _ = run_fut.await;
    }
    assert_eq!(sched.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        *sched
            .last_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some(dag)
    );
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
    let dotenv = dir.path().join(".env");
    std::fs::write(&dotenv, "SENTINEL=keep\n").unwrap();
    let _ = test_config(dir.path());
    assert_eq!(std::fs::read_to_string(&dotenv).unwrap(), "SENTINEL=keep\n");
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
async fn set_event_sink_refuses_non_empty_with_busy() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    // start already emitted Configured/Started into memory sink
    let err = handle
        .set_event_sink(Arc::new(InMemoryEventSink::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::EventSinkBusy));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_scheduler_queues_registered_event_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    handle.set_scheduler(Arc::new(NullScheduler)).unwrap();
    // Immediately after sync set_scheduler the event is queued, not yet in the sink.
    assert!(!handle
        .memory_sink()
        .runtime_events()
        .iter()
        .any(|e| matches!(e, RuntimeEvent::SchedulerRegistered)));
    // Next async lifecycle path flushes pending events before appending.
    rt.drain(Duration::from_millis(10)).await.unwrap();
    let events = handle.memory_sink().runtime_events();
    let started = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::Started))
        .expect("Started");
    let registered = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::SchedulerRegistered))
        .expect("SchedulerRegistered");
    let drained = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::DrainStarted { .. }))
        .expect("DrainStarted");
    assert!(started < registered);
    assert!(registered < drained);
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_does_not_emit_run_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(test_config(dir.path())).unwrap();
    let handle = rt.start().await.unwrap();
    let _ = rt.run(DagId::new()).await;
    let events = handle.memory_sink().runtime_events();
    assert!(events
        .iter()
        .all(|e| !matches!(e, RuntimeEvent::RunAccepted { .. })));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn core_ir_serde_round_trips() {
    let profile = ProfileId::new("default").unwrap();
    let cs = CreateSession {
        workspace_root: std::path::PathBuf::from("/tmp/ws"),
        profile: profile.clone(),
        budget: alloy_runtime::BudgetPolicy::default(),
        language_backends: vec![alloy_runtime::LanguageId::new("rust").unwrap()],
    };
    let json = serde_json::to_string(&cs).unwrap();
    let back: CreateSession = serde_json::from_str(&json).unwrap();
    assert_eq!(back.profile.as_str(), "default");

    let dig = Digest::sha256(b"x");
    let diag = DiagnosticEvent {
        id: alloy_runtime::DiagnosticId::new(),
        code: Some("E0502".into()),
        level: DiagnosticLevel::Error,
        message: "borrow".into(),
        spans: vec![],
        children: vec![],
        package: None,
        fingerprint: dig.clone(),
        raw_json: None,
    };
    let fail = FailureIr {
        node: alloy_runtime::NodeId::new(),
        error_class: alloy_runtime::ErrorClass::Compile,
        diagnostics: vec![diag],
        notes: "n".into(),
    };
    let round: FailureIr = serde_json::from_str(&serde_json::to_string(&fail).unwrap()).unwrap();
    assert_eq!(round.notes, "n");

    let grant = Grant::FsRead(Glob("**/*.rs".into()));
    let _: Grant = serde_json::from_str(&serde_json::to_string(&grant).unwrap()).unwrap();

    let ts = Timestamp::now();
    let wire = serde_json::to_string(&ts).unwrap();
    assert!(wire.contains('T') || wire.contains('Z') || wire.contains('+'));
    let _: Timestamp = serde_json::from_str(&wire).unwrap();

    let ty = SessionEventType::SessionCreated;
    let s = serde_json::to_string(&ty).unwrap();
    assert_eq!(s, "\"session_created\"");
}

#[tokio::test]
async fn workspace_has_five_members() {
    let manifest = include_str!("../../../Cargo.toml");
    let mut in_members = false;
    let mut count = 0usize;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with("members") {
            in_members = true;
            continue;
        }
        if in_members {
            if t.starts_with(']') {
                break;
            }
            if t.starts_with('"') {
                count += 1;
            }
        }
    }
    assert_eq!(count, 5);
}

#[tokio::test]
async fn null_scheduler_type_is_default() {
    let _ = NullScheduler;
    assert_eq!(InMemoryEventSink::new().buffered_len(), 0);
}
