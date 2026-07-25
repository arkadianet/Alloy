//! Unit and integration tests for RFC-0002 storage.

use std::sync::Arc;

use alloy_runtime::events::{EventSink, NewSessionEvent, RuntimeEvent, SessionEventType};
use alloy_runtime::session::{Session, MAX_EVENTS_PAGE};
use alloy_runtime::storage::{
    AlloyStorage, ArtifactKind, ArtifactPut, ArtifactStore, EventStore, SessionRows,
    SqliteSynchronous, StorageOpenOptions, StoreError,
};
use alloy_runtime::types::budget::BudgetPolicy;
use alloy_runtime::types::ids::{
    DagId, Digest, EventSeq, LanguageId, ProfileId, RunId, SessionId, Timestamp,
};
use alloy_runtime::{
    install_sqlite_event_sink, AlloyRuntime, ConfigPaths, DagOutcome, DagState, RuntimeConfig,
    RuntimeHandle, RuntimePhase,
};
use serde_json::json;

fn new_ev(session: SessionId, ty: SessionEventType) -> NewSessionEvent {
    NewSessionEvent {
        session_id: session,
        run_id: None,
        type_: ty,
        payload: json!({"ok": true}),
    }
}

fn run_ev(session: SessionId, run: RunId, ty: SessionEventType) -> NewSessionEvent {
    NewSessionEvent {
        run_id: Some(run),
        ..new_ev(session, ty)
    }
}

async fn open_temp() -> (tempfile::TempDir, AlloyStorage) {
    let dir = tempfile::tempdir().unwrap();
    let opts = StorageOpenOptions::for_data_dir(dir.path());
    let storage = AlloyStorage::open(opts).await.unwrap();
    (dir, storage)
}

async fn test_runtime(dir: &std::path::Path) -> (AlloyRuntime, RuntimeHandle) {
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
    let cfg = RuntimeConfig::load(ConfigPaths {
        profile: dir.join("profiles/default.toml"),
        router: dir.join("router.toml"),
        example_env: dir.join("example.env"),
        data_dir: Some(dir.join("data")),
        workspace_root: Some(dir.to_path_buf()),
    })
    .unwrap();
    let mut rt = AlloyRuntime::new();
    rt.configure(cfg).unwrap();
    let handle = rt.start().await.unwrap();
    (rt, handle)
}

#[tokio::test]
async fn open_creates_layout() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    assert!(dir.path().join("alloy.sqlite").is_file());
    assert!(dir.path().join("artifacts").is_dir());
    assert!(dir.path().join("graph").is_dir());
    assert_eq!(storage.schema_version(), 2);
    storage.close().await.unwrap();
}

#[tokio::test]
async fn migrate_idempotent_and_refuse_newer() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    storage.close().await.unwrap();
    drop(storage);

    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    assert_eq!(storage.schema_version(), 2);
    storage.close().await.unwrap();
    drop(storage);

    // Inject a future schema version.
    {
        let conn = rusqlite::Connection::open(dir.path().join("alloy.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (99, 't')",
            [],
        )
        .unwrap();
    }
    let err = match AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path())).await {
        Ok(_) => panic!("expected migration refuse for newer schema"),
        Err(e) => e,
    };
    assert!(matches!(err, StoreError::Migration(_)));
}

#[tokio::test]
async fn per_session_gapless_seq_interleaved() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let a = SessionId::new();
    let b = SessionId::new();
    assert_eq!(
        events
            .append_session(new_ev(a, SessionEventType::SessionCreated))
            .await
            .unwrap(),
        EventSeq(0)
    );
    assert_eq!(
        events
            .append_session(new_ev(b, SessionEventType::SessionCreated))
            .await
            .unwrap(),
        EventSeq(0)
    );
    assert_eq!(
        events
            .append_session(new_ev(a, SessionEventType::GoalSubmitted))
            .await
            .unwrap(),
        EventSeq(1)
    );
    assert_eq!(
        events
            .append_session(new_ev(b, SessionEventType::Decision))
            .await
            .unwrap(),
        EventSeq(1)
    );
    let listed = events.list_session_events(a, None, 10).await.unwrap();
    assert!(listed.windows(2).all(|w| w[1].seq.0 == w[0].seq.0 + 1));
    storage.close().await.unwrap();
}

#[tokio::test]
async fn exclusive_cursor_and_clamp() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let s = SessionId::new();
    for _ in 0..5 {
        events
            .append_session(new_ev(s, SessionEventType::NodeState))
            .await
            .unwrap();
    }
    let page = events
        .list_session_events(s, Some(EventSeq(1)), 2)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].seq, EventSeq(2));
    assert_eq!(page[1].seq, EventSeq(3));

    let clamped = events.list_session_events(s, None, 0).await.unwrap();
    assert_eq!(clamped.len(), 1); // clamp to 1

    let huge = events
        .list_session_events(s, None, MAX_EVENTS_PAGE + 50)
        .await
        .unwrap();
    assert!(huge.len() <= MAX_EVENTS_PAGE);
    storage.close().await.unwrap();
}

#[tokio::test]
async fn appendix_a_types_round_trip() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let s = SessionId::new();
    let types = [
        SessionEventType::SessionCreated,
        SessionEventType::GoalSubmitted,
        SessionEventType::PlanProduced,
        SessionEventType::NodeState,
        SessionEventType::Decision,
        SessionEventType::ModelCall,
        SessionEventType::ToolCall,
        SessionEventType::EditApplied,
        SessionEventType::ApprovalRequested,
        SessionEventType::ApprovalResolved,
        SessionEventType::BudgetWarning,
        SessionEventType::ReplanRequested,
        SessionEventType::RunCompleted,
        SessionEventType::Error,
    ];
    for ty in types {
        events.append_session(new_ev(s, ty)).await.unwrap();
    }
    let listed = events
        .list_session_events(s, None, types.len())
        .await
        .unwrap();
    assert_eq!(listed.len(), types.len());
    for (ev, ty) in listed.iter().zip(types.iter()) {
        assert_eq!(ev.type_, *ty);
    }
    storage.close().await.unwrap();
}

/// Existence probes used by the RFC-0003 control plane for resume/cancel idempotency.
/// Each is a `LIMIT 1` lookup, so duplicates stay `true` and every id/type component of
/// the predicate has to matter.
#[tokio::test]
async fn event_existence_probes_are_scoped_and_limit_one() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let session = SessionId::new();
    let other_session = SessionId::new();
    let run = RunId::new();
    let other_run = RunId::new();
    let dag = DagId::new();

    assert!(!events
        .has_session_event_for_run(session, run, SessionEventType::RunCompleted)
        .await
        .unwrap());
    assert!(!events.has_run_accepted_event(run).await.unwrap());
    assert!(!events.has_run_finished_event(run).await.unwrap());

    for _ in 0..2 {
        events
            .append_session(run_ev(session, run, SessionEventType::RunCompleted))
            .await
            .unwrap();
    }
    events
        .append_session(new_ev(session, SessionEventType::SessionCreated))
        .await
        .unwrap();
    events
        .append_session(run_ev(
            other_session,
            other_run,
            SessionEventType::RunCompleted,
        ))
        .await
        .unwrap();

    assert!(events
        .has_session_event_for_run(session, run, SessionEventType::RunCompleted)
        .await
        .unwrap());
    assert!(!events
        .has_session_event_for_run(session, run, SessionEventType::ApprovalResolved)
        .await
        .unwrap());
    assert!(!events
        .has_session_event_for_run(session, other_run, SessionEventType::RunCompleted)
        .await
        .unwrap());
    assert!(!events
        .has_session_event_for_run(other_session, run, SessionEventType::RunCompleted)
        .await
        .unwrap());
    // Session-scoped rows carry `run_id = NULL` and must never satisfy a run probe.
    assert!(!events
        .has_session_event_for_run(session, run, SessionEventType::SessionCreated)
        .await
        .unwrap());

    events
        .append_runtime(RuntimeEvent::RunAccepted {
            run_id: run,
            dag_id: dag,
        })
        .await
        .unwrap();
    assert!(events.has_run_accepted_event(run).await.unwrap());
    assert!(!events.has_run_accepted_event(other_run).await.unwrap());
    // `RunAccepted` must not be mistaken for a finish.
    assert!(!events.has_run_finished_event(run).await.unwrap());

    for _ in 0..2 {
        events
            .append_runtime(RuntimeEvent::RunFinished {
                run_id: run,
                outcome: DagOutcome {
                    dag_id: dag,
                    generation: 0,
                    state: DagState::Failed,
                    failed_node: None,
                    failure: None,
                },
            })
            .await
            .unwrap();
    }
    assert!(events.has_run_finished_event(run).await.unwrap());
    assert!(!events.has_run_finished_event(other_run).await.unwrap());
    // Host events carry no session id, so the probe keys on `run_id` alone.
    assert!(events.has_run_accepted_event(run).await.unwrap());

    storage.close().await.unwrap();
}

#[tokio::test]
async fn reopen_deterministic_replay() {
    let dir = tempfile::tempdir().unwrap();
    let s = SessionId::new();
    let before = {
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let events = storage.events();
        events.append_runtime(RuntimeEvent::Started).await.unwrap();
        for i in 0..3 {
            events
                .append_session(NewSessionEvent {
                    session_id: s,
                    run_id: None,
                    type_: SessionEventType::Decision,
                    payload: json!({"i": i}),
                })
                .await
                .unwrap();
        }
        let listed = events.list_session_events(s, None, 10).await.unwrap();
        let runtime = events.list_runtime_events(None, 10).await.unwrap();
        storage.checkpoint().await.unwrap();
        storage.close().await.unwrap();
        (listed, runtime)
    };

    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let events = storage.events();
    let after = events.list_session_events(s, None, 10).await.unwrap();
    assert_eq!(before.0.len(), after.len());
    for (a, b) in before.0.iter().zip(after.iter()) {
        assert_eq!(a.seq, b.seq);
        assert_eq!(a.ts, b.ts);
        assert_eq!(a.type_, b.type_);
        assert_eq!(a.payload, b.payload);
    }
    let runtime_after = events.list_runtime_events(None, 10).await.unwrap();
    assert!(!runtime_after.is_empty());
    assert_eq!(before.1.len(), runtime_after.len());

    let mut seen = Vec::new();
    let last = events
        .replay_session(s, |ev| {
            seen.push(ev.seq);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(last, Some(EventSeq(2)));
    assert_eq!(seen, vec![EventSeq(0), EventSeq(1), EventSeq(2)]);

    let empty = SessionId::new();
    let none = events
        .replay_session(empty, |_| panic!("no callbacks"))
        .await
        .unwrap();
    assert!(none.is_none());
    storage.close().await.unwrap();
}

#[tokio::test]
async fn artifact_put_get_digest_soft_delete() {
    let (_dir, storage) = open_temp().await;
    let arts = storage.artifacts();
    let bytes = b"hello-alloy-artifact".to_vec();
    let digest = Digest::sha256(&bytes);
    let id1 = arts
        .put(ArtifactPut {
            bytes: bytes.clone(),
            kind: ArtifactKind::Blob,
            content_type: Some("text/plain".into()),
            session_id: None,
            run_id: None,
            labels: Default::default(),
        })
        .await
        .unwrap();
    let id2 = arts
        .put(ArtifactPut {
            bytes: bytes.clone(),
            kind: ArtifactKind::Log,
            content_type: None,
            session_id: None,
            run_id: None,
            labels: Default::default(),
        })
        .await
        .unwrap();
    assert_ne!(id1, id2);
    let oldest = arts.get_by_digest(&digest).await.unwrap().unwrap();
    assert_eq!(oldest, id1);

    let blob = arts.get(id1).await.unwrap();
    assert_eq!(blob.bytes, bytes);
    assert_eq!(blob.meta.digest, digest);

    // Tamper CAS file.
    let path = storage.layout().cas_path(digest.as_hex()).unwrap();
    std::fs::write(&path, b"tampered").unwrap();
    let err = arts.get(id1).await.unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch));
    // Restore for soft-delete test.
    std::fs::write(&path, &bytes).unwrap();

    arts.delete(id1).await.unwrap();
    assert!(matches!(
        arts.get(id1).await.unwrap_err(),
        StoreError::NotFound(_)
    ));
    // Oldest non-deleted is now id2.
    assert_eq!(arts.get_by_digest(&digest).await.unwrap(), Some(id2));
    storage.close().await.unwrap();
}

#[tokio::test]
async fn session_and_run_rows() {
    let (_dir, storage) = open_temp().await;
    let rows = storage.sessions();
    let session = Session {
        id: SessionId::new(),
        workspace_root: "/tmp/ws".into(),
        profile: ProfileId::new("default").unwrap(),
        budget: BudgetPolicy::default(),
        language_backends: vec![LanguageId::new("rust").unwrap()],
        created_at: Timestamp::now(),
    };
    rows.upsert_session(&session).await.unwrap();
    let loaded = rows.get_session(session.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.profile.as_str(), "default");

    let run = alloy_runtime::RunRow {
        id: RunId::new(),
        session_id: session.id,
        goal_json: json!({"text": "fix"}),
        state: "pending".into(),
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    };
    rows.upsert_run(&run).await.unwrap();
    let got = rows.get_run(run.id).await.unwrap().unwrap();
    assert_eq!(got.state, "pending");
    let list = rows.list_runs(session.id).await.unwrap();
    assert_eq!(list.len(), 1);
    storage.close().await.unwrap();
}

#[tokio::test]
async fn handoff_lossless_and_continues_seq() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, handle) = test_runtime(dir.path()).await;
    assert_eq!(handle.phase(), RuntimePhase::Running);

    let session = SessionId::new();
    handle
        .append_session(new_ev(session, SessionEventType::SessionCreated))
        .await
        .unwrap();
    handle
        .append_session(new_ev(session, SessionEventType::GoalSubmitted))
        .await
        .unwrap();
    handle.emit(RuntimeEvent::Started).await.unwrap();
    assert!(handle.memory_sink().buffered_len() >= 2);

    let storage = install_sqlite_event_sink(&handle, None).await.unwrap();
    assert_eq!(handle.memory_sink().buffered_len(), 0);

    let events = storage.events();
    let listed = events.list_session_events(session, None, 10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].seq, EventSeq(0));
    assert_eq!(listed[1].seq, EventSeq(1));

    // Post-swap append continues gapless on SQLite only.
    let next = handle
        .append_session(new_ev(session, SessionEventType::Decision))
        .await
        .unwrap();
    assert_eq!(next, EventSeq(2));
    assert_eq!(handle.memory_sink().buffered_len(), 0);
    let listed = events.list_session_events(session, None, 10).await.unwrap();
    assert_eq!(listed.len(), 3);

    storage.checkpoint().await.unwrap();
    storage.close().await.unwrap();
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn handoff_failure_restores_memory() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, handle) = test_runtime(dir.path()).await;
    let session = SessionId::new();
    handle
        .append_session(new_ev(session, SessionEventType::SessionCreated))
        .await
        .unwrap();
    let before = handle.memory_sink().buffered_len();
    assert!(before > 0);

    let err = handle
        .handoff_event_sink(handle.memory_sink().clone(), |_snap| async {
            Err(StoreError::Corrupt("forced".into()))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, alloy_runtime::RuntimeError::Internal(_)));
    assert_eq!(handle.memory_sink().buffered_len(), before);

    // Memory still the live sink.
    let seq = handle
        .append_session(new_ev(session, SessionEventType::Decision))
        .await
        .unwrap();
    assert_eq!(seq, EventSeq(1));
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_event_sink_refuses_nonempty_memory() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, handle) = test_runtime(dir.path()).await;
    handle
        .append_session(new_ev(SessionId::new(), SessionEventType::SessionCreated))
        .await
        .unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path().join("data")))
        .await
        .unwrap();
    let err = handle.set_event_sink(storage.events()).await.unwrap_err();
    assert!(matches!(err, alloy_runtime::RuntimeError::EventSinkBusy));
    storage.close().await.unwrap();
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn sqlite_event_store_as_dyn_event_sink() {
    let (_dir, storage) = open_temp().await;
    let sink: Arc<dyn EventSink> = storage.events();
    let s = SessionId::new();
    let seq = sink
        .append_session(new_ev(s, SessionEventType::ToolCall))
        .await
        .unwrap();
    assert_eq!(seq, EventSeq(0));
    storage.close().await.unwrap();
}

#[tokio::test]
async fn never_writes_dotenv() {
    let dir = tempfile::tempdir().unwrap();
    let env_path = dir.path().join(".env");
    assert!(!env_path.exists());
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path().join("data")))
        .await
        .unwrap();
    let _ = StorageOpenOptions::from_env(dir.path().join("data")).unwrap();
    storage.close().await.unwrap();
    assert!(!env_path.exists(), ".env must never be written");
}

#[tokio::test]
async fn close_idempotent_and_ops_fail() {
    let (_dir, storage) = open_temp().await;
    storage.close().await.unwrap();
    storage.close().await.unwrap();
    let err = storage
        .events()
        .append_session(new_ev(SessionId::new(), SessionEventType::Error))
        .await
        .unwrap_err();
    assert!(matches!(err, alloy_runtime::EventSinkError::Internal(_)));
}

#[tokio::test]
async fn concurrent_appends_different_sessions() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let sessions: Vec<_> = (0..8).map(|_| SessionId::new()).collect();
    let mut handles = Vec::new();
    for s in sessions.clone() {
        let events = Arc::clone(&events);
        handles.push(tokio::spawn(async move {
            for _ in 0..20 {
                events
                    .append_session(new_ev(s, SessionEventType::NodeState))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    for s in sessions {
        let listed = events.list_session_events(s, None, 100).await.unwrap();
        assert_eq!(listed.len(), 20);
        assert!(listed.windows(2).all(|w| w[1].seq.0 == w[0].seq.0 + 1));
        assert_eq!(listed[0].seq, EventSeq(0));
        assert_eq!(listed[19].seq, EventSeq(19));
    }
    storage.close().await.unwrap();
}

#[tokio::test]
async fn synchronous_parse_and_open_options_env() {
    assert!(SqliteSynchronous::parse("bogus").is_err());
    let dir = tempfile::tempdir().unwrap();
    // Safety: scoped env for this test only; restore after.
    let prev = std::env::var("ALLOY_SQLITE_SYNCHRONOUS").ok();
    std::env::set_var("ALLOY_SQLITE_SYNCHRONOUS", "bogus");
    let err = StorageOpenOptions::from_env(dir.path()).unwrap_err();
    match prev {
        Some(v) => std::env::set_var("ALLOY_SQLITE_SYNCHRONOUS", v),
        None => std::env::remove_var("ALLOY_SQLITE_SYNCHRONOUS"),
    }
    assert!(
        matches!(err, StoreError::Io(_)),
        "from_env must reject malformed ALLOY_SQLITE_SYNCHRONOUS: {err:?}"
    );
}

#[tokio::test]
async fn replay_callback_error_aborts() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let s = SessionId::new();
    for _ in 0..3 {
        events
            .append_session(new_ev(s, SessionEventType::Decision))
            .await
            .unwrap();
    }
    let mut n = 0;
    let err = events
        .replay_session(s, |_| {
            n += 1;
            if n == 2 {
                Err(StoreError::Internal("stop".into()))
            } else {
                Ok(())
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Internal(_)));
    assert_eq!(n, 2);
    storage.close().await.unwrap();
}

#[tokio::test]
async fn close_is_barrier_rejects_new_ops() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let s = SessionId::new();
    events
        .append_session(new_ev(s, SessionEventType::SessionCreated))
        .await
        .unwrap();
    storage.close().await.unwrap();
    let err = events
        .append_session(new_ev(s, SessionEventType::Decision))
        .await
        .unwrap_err();
    assert!(matches!(err, alloy_runtime::EventSinkError::Internal(_)));
    // Second close is a no-op.
    storage.close().await.unwrap();
}

#[tokio::test]
async fn handoff_verify_failure_leaves_no_durable_residue() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let events = storage.events();
    let session = SessionId::new();

    // Build a snapshot with inconsistent next_seq (claims next=7 but only seq 0 present).
    let mut snap = alloy_runtime::HandoffSnapshot::default();
    snap.sessions.insert(
        session,
        vec![alloy_runtime::SessionEvent {
            seq: EventSeq(0),
            ts: Timestamp::now(),
            session_id: session,
            run_id: None,
            type_: SessionEventType::SessionCreated,
            payload: json!({}),
        }],
    );
    snap.next_seq.insert(session, 7);

    let err = events.import_handoff_snapshot(snap).await.unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));

    assert!(events
        .list_session_events(session, None, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(events
        .list_runtime_events(None, 10)
        .await
        .unwrap()
        .is_empty());
    storage.close().await.unwrap();
}

#[tokio::test]
async fn seq_mismatch_refuses_ready_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let s = SessionId::new();
        storage
            .events()
            .append_session(new_ev(s, SessionEventType::Decision))
            .await
            .unwrap();
        storage.close().await.unwrap();
    }
    {
        let conn = rusqlite::Connection::open(dir.path().join("alloy.sqlite")).unwrap();
        conn.execute("UPDATE session_seq SET next_seq = 99", [])
            .unwrap();
    }
    let err = match AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path())).await {
        Ok(_) => panic!("expected corrupt seq mismatch to refuse ready"),
        Err(e) => e,
    };
    assert!(matches!(err, StoreError::Corrupt(_)));
}

#[tokio::test]
async fn foreign_key_maps_to_conflict_not_io() {
    let (_dir, storage) = open_temp().await;
    let rows = storage.sessions();
    let orphan = alloy_runtime::RunRow {
        id: RunId::new(),
        session_id: SessionId::new(),
        goal_json: json!({}),
        state: "pending".into(),
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    };
    let err = rows.upsert_run(&orphan).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
    storage.close().await.unwrap();
}

#[tokio::test]
async fn ordered_shutdown_drain_checkpoint_close() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, handle) = test_runtime(dir.path()).await;
    let session = SessionId::new();
    handle
        .append_session(new_ev(session, SessionEventType::SessionCreated))
        .await
        .unwrap();
    let storage = install_sqlite_event_sink(&handle, None).await.unwrap();

    // Ordered shutdown when storage is installed: drain → checkpoint → close → runtime shutdown.
    rt.drain(std::time::Duration::from_millis(50))
        .await
        .unwrap();
    storage.checkpoint().await.unwrap();
    storage.close().await.unwrap();
    rt.shutdown().await.unwrap();

    // Reopen sees durable events.
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path().join("data")))
        .await
        .unwrap();
    let listed = storage
        .events()
        .list_session_events(session, None, 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    storage.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_same_session_gapless() {
    let (_dir, storage) = open_temp().await;
    let events = storage.events();
    let s = SessionId::new();
    let mut handles = Vec::new();
    for _ in 0..32 {
        let events = Arc::clone(&events);
        handles.push(tokio::spawn(async move {
            events
                .append_session(new_ev(s, SessionEventType::NodeState))
                .await
                .unwrap()
        }));
    }
    let mut seqs = Vec::new();
    for h in handles {
        seqs.push(h.await.unwrap());
    }
    seqs.sort();
    assert_eq!(seqs.len(), 32);
    for (i, seq) in seqs.iter().enumerate() {
        assert_eq!(*seq, EventSeq(i as u64));
    }
    storage.close().await.unwrap();
}

#[tokio::test]
async fn crash_after_commit_reopen_sees_event() {
    use std::process::{Command, Stdio};

    if let Ok(data) = std::env::var("ALLOY_STORAGE_CRASH_DIR") {
        let data = std::path::PathBuf::from(data);
        let sid_path = data.parent().unwrap().join("sid");
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
            .await
            .unwrap();
        let s = SessionId::new();
        std::fs::write(&sid_path, s.to_string()).unwrap();
        storage
            .events()
            .append_session(new_ev(s, SessionEventType::Decision))
            .await
            .unwrap();
        storage.checkpoint().await.unwrap();
        // Simulate hard crash after durable commit+checkpoint (no close).
        std::process::abort();
    }

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let sid_path = dir.path().join("sid");

    let exe = std::env::current_exe().unwrap();
    let output = Command::new(&exe)
        .env("ALLOY_STORAGE_CRASH_DIR", &data)
        .env("RUST_BACKTRACE", "0")
        .args([
            "--exact",
            "crash_after_commit_reopen_sees_event",
            "--nocapture",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let status = output.status;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!status.success(), "child should abort; stderr:\n{stderr}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // SIGABRT == 6 on Linux/macOS.
        assert_eq!(
            status.signal(),
            Some(6),
            "child should abort with SIGABRT; status={status:?}; stderr:\n{stderr}"
        );
    }

    let sid_str = std::fs::read_to_string(&sid_path).expect("child should write session id");
    let s = SessionId::parse(sid_str.trim()).expect("valid session id");

    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    let listed = storage
        .events()
        .list_session_events(s, None, 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].seq, EventSeq(0));
    storage.close().await.unwrap();
}
