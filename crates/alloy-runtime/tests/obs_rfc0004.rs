//! Integration tests for RFC-0004 observability & cost metering.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::obs::{
    list_decision_events, maybe_signal_budget_warning, parse_decision_event,
    parse_model_call_event, parse_tool_call_event, reaccumulate_cost_from_events, DecisionKind,
    DecisionLog, DecisionRecord, EventDecisionLog, ModelCallRecord, ObsError, RetentionPolicy,
    SharedCostMeter, ToolCallRecord,
};
use alloy_runtime::session::SessionPlane;
use alloy_runtime::storage::{EventStore, StorageOpenOptions};
use alloy_runtime::{
    install_sqlite_event_sink, AlloyRuntime, AlloyStorage, BudgetPolicy, ConfigPaths,
    CreateSession, Goal, LanguageId, ModelTier, ProfileId, ProviderId, RuntimePhase,
    SessionEventType, SessionId, SessionService,
};

struct Host {
    _tmp: Option<tempfile::TempDir>,
    rt: AlloyRuntime,
    handle: alloy_runtime::RuntimeHandle,
    storage: Arc<AlloyStorage>,
    plane: SessionPlane,
    dir: PathBuf,
}

impl Host {
    async fn open(retain_prompts: bool, retain_tools: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let mut host = Self::open_kept(dir, retain_prompts, retain_tools).await;
        host._tmp = Some(tmp);
        host
    }

    async fn open_kept(dir: PathBuf, retain_prompts: bool, retain_tools: bool) -> Self {
        write_fixtures(&dir, retain_prompts, retain_tools);
        let cfg = alloy_runtime::RuntimeConfig::load(ConfigPaths {
            profile: dir.join("profiles/default.toml"),
            router: dir.join("router.toml"),
            example_env: dir.join("example.env"),
            data_dir: Some(dir.join("data")),
            workspace_root: Some(dir.clone()),
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
        let plane = SessionPlane::new(handle.clone(), Arc::clone(&storage));
        Self {
            _tmp: None,
            rt,
            handle,
            storage,
            plane,
            dir,
        }
    }

    fn log(&self) -> EventDecisionLog {
        EventDecisionLog::from_handle(self.handle.clone(), Arc::clone(&self.storage)).unwrap()
    }

    async fn create_session(&self) -> SessionId {
        let sessions: Arc<dyn SessionService> = self.plane.sessions();
        sessions.create(create_req(&self.dir)).await.unwrap()
    }

    async fn shutdown(self) {
        self.rt.shutdown().await.unwrap();
        self.storage.close().await.unwrap();
    }
}

fn write_fixtures(dir: &Path, retain_prompts: bool, retain_tools: bool) {
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/default.toml"),
        format!(
            r#"
[profile]
id = "default"
[budgets]
max_usd_per_run = 1.0
max_tokens_per_run = 1000
[observability]
retain_full_prompts = {retain_prompts}
retain_tool_bodies = {retain_tools}
"#
        ),
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
}

fn create_req(dir: &Path) -> CreateSession {
    CreateSession {
        workspace_root: dir.to_path_buf(),
        profile: ProfileId::new("default").unwrap(),
        budget: BudgetPolicy::default(),
        language_backends: vec![LanguageId::new("rust").unwrap()],
    }
}

#[tokio::test]
async fn decision_append_and_list() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    let seq = log
        .record(DecisionRecord {
            session,
            run: None,
            node: None,
            kind: DecisionKind::ModelRoute,
            metadata: serde_json::json!({"route": "std"}),
            content_hash: None,
            prompt_body: Some("prompt api_key=sk-12345678".into()),
        })
        .await
        .unwrap();
    assert!(seq.0 >= 1);

    let page = list_decision_events(h.storage.events().as_ref(), session, None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].type_, SessionEventType::Decision);
    let rec = parse_decision_event(&page.events[0]).unwrap();
    assert!(rec.prompt_body.is_none());
    assert!(rec.content_hash.is_some());
    assert_eq!(rec.kind, DecisionKind::ModelRoute);
    h.shutdown().await;
}

#[tokio::test]
async fn model_call_and_tool_call_round_trip() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    log.record_model_call(ModelCallRecord {
        session,
        run: None,
        node: None,
        provider_id: ProviderId::new("default").unwrap(),
        model_tier: ModelTier::Standard,
        input_tokens: Some(10),
        output_tokens: Some(5),
        usd: Some(0.02),
        duration_ms: Some(12),
        confidence: None,
        error_class: None,
        content_hash: None,
        prompt_body: Some("hi".into()),
    })
    .await
    .unwrap();
    log.record_tool_call(ToolCallRecord {
        session,
        run: None,
        node: None,
        tool_name: "read".into(),
        tool_server: Some("builtin".into()),
        latency_ms: Some(3),
        denied: false,
        content_hash: None,
        body: Some("body".into()),
    })
    .await
    .unwrap();

    let page = list_decision_events(h.storage.events().as_ref(), session, None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    let m = parse_model_call_event(&page.events[0]).unwrap();
    assert_eq!(m.input_tokens, Some(10));
    assert!(m.prompt_body.is_none());
    let t = parse_tool_call_event(&page.events[1]).unwrap();
    assert_eq!(t.tool_name, "read");
    assert!(t.body.is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn list_decision_events_cursor() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    for i in 0..5 {
        log.record(DecisionRecord {
            session,
            run: None,
            node: None,
            kind: DecisionKind::Retry,
            metadata: serde_json::json!({"i": i}),
            content_hash: None,
            prompt_body: None,
        })
        .await
        .unwrap();
    }
    // Interleave a non-decision event via plane? SessionCreated already exists.
    // Append NodeState is owned by 0010 — skip; list filters Decision/Model/Tool only.

    let p1 = list_decision_events(h.storage.events().as_ref(), session, None, 2)
        .await
        .unwrap();
    assert_eq!(p1.events.len(), 2);
    let after = p1.next_after;
    assert!(after.is_some());
    let p2 = list_decision_events(h.storage.events().as_ref(), session, after, 10)
        .await
        .unwrap();
    assert_eq!(p2.events.len(), 3);
    let mut seqs: Vec<_> = p1
        .events
        .iter()
        .chain(p2.events.iter())
        .map(|e| e.seq.0)
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted);
    seqs.dedup();
    assert_eq!(seqs.len(), 5);
    h.shutdown().await;
}

#[tokio::test]
async fn replay_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (session, seq, payload) = {
        let h = Host::open_kept(dir.clone(), false, false).await;
        let session = h.create_session().await;
        let log = h.log();
        let seq = log
            .record(DecisionRecord {
                session,
                run: None,
                node: None,
                kind: DecisionKind::Gate,
                metadata: serde_json::json!({"k": 1}),
                content_hash: None,
                prompt_body: Some("x".into()),
            })
            .await
            .unwrap();
        let page = list_decision_events(h.storage.events().as_ref(), session, None, 1)
            .await
            .unwrap();
        let payload = page.events[0].payload.clone();
        let type_ = page.events[0].type_;
        assert_eq!(type_, SessionEventType::Decision);
        h.shutdown().await;
        (session, seq, payload)
    };

    let h2 = Host::open_kept(dir, false, false).await;
    let page = list_decision_events(h2.storage.events().as_ref(), session, None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].seq, seq);
    assert_eq!(page.events[0].payload, payload);
    h2.shutdown().await;
}

#[tokio::test]
async fn opt_in_prompt_retention() {
    let h = Host::open(true, true).await;
    let session = h.create_session().await;
    let log = h.log();
    log.record(DecisionRecord {
        session,
        run: None,
        node: None,
        kind: DecisionKind::ContextInclusion,
        metadata: serde_json::json!({}),
        content_hash: None,
        prompt_body: Some("hello api_key=sk-12345678".into()),
    })
    .await
    .unwrap();
    let page = list_decision_events(h.storage.events().as_ref(), session, None, 1)
        .await
        .unwrap();
    let rec = parse_decision_event(&page.events[0]).unwrap();
    let body = rec.prompt_body.unwrap();
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains("sk-12345678"));
    h.shutdown().await;
}

#[tokio::test]
async fn budget_warning_hook_integration() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let meter = SharedCostMeter::new();
    meter.add_model_usage(ModelTier::Standard, Some(100), Some(100), Some(2.0));
    let policy = BudgetPolicy {
        max_usd_per_run: 1.0,
        max_tokens_per_run: 50,
        ..BudgetPolicy::default()
    };
    let seq = maybe_signal_budget_warning(&h.plane, session, None, &meter, &policy)
        .await
        .unwrap();
    assert!(seq.is_some());
    let events = h
        .storage
        .events()
        .list_session_events(session, None, 100)
        .await
        .unwrap();
    assert!(events
        .iter()
        .any(|e| e.type_ == SessionEventType::BudgetWarning));
    h.shutdown().await;
}

#[tokio::test]
async fn budget_warning_fails_when_draining() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let meter = SharedCostMeter::new();
    meter.add_model_usage(ModelTier::Standard, Some(1000), Some(1000), None);
    let policy = BudgetPolicy {
        max_tokens_per_run: 1,
        ..BudgetPolicy::default()
    };
    h.rt.drain(Duration::from_millis(20)).await.unwrap();
    assert_eq!(h.handle.phase(), RuntimePhase::Draining);
    let err = maybe_signal_budget_warning(&h.plane, session, None, &meter, &policy)
        .await
        .unwrap_err();
    assert!(matches!(err, ObsError::Session(_)));
    h.shutdown().await;
}

#[tokio::test]
async fn reaccumulate_cost_from_events_test() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let run_a = h
        .plane
        .sessions()
        .submit_goal(
            session,
            Goal {
                text: "a".into(),
                constraints: vec![],
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    let run_b = h
        .plane
        .sessions()
        .submit_goal(
            session,
            Goal {
                text: "b".into(),
                constraints: vec![],
                attachments: vec![],
            },
        )
        .await
        .unwrap();

    let log = h.log();
    log.record_model_call(ModelCallRecord {
        session,
        run: Some(run_a),
        node: None,
        provider_id: ProviderId::new("p").unwrap(),
        model_tier: ModelTier::Premium,
        input_tokens: Some(10),
        output_tokens: Some(2),
        usd: Some(0.1),
        duration_ms: None,
        confidence: None,
        error_class: None,
        content_hash: None,
        prompt_body: None,
    })
    .await
    .unwrap();
    log.record_model_call(ModelCallRecord {
        session,
        run: Some(run_b),
        node: None,
        provider_id: ProviderId::new("p").unwrap(),
        model_tier: ModelTier::Standard,
        input_tokens: Some(7),
        output_tokens: Some(3),
        usd: Some(0.05),
        duration_ms: None,
        confidence: None,
        error_class: None,
        content_hash: None,
        prompt_body: None,
    })
    .await
    .unwrap();

    let all = reaccumulate_cost_from_events(h.storage.events().as_ref(), session, None)
        .await
        .unwrap()
        .snapshot();
    assert_eq!(all.tokens_in, 17);
    assert_eq!(all.tokens_out, 5);
    assert!((all.usd_spent.unwrap() - 0.15).abs() < 1e-9);
    assert_eq!(all.model_calls, 2);

    let only_a = reaccumulate_cost_from_events(h.storage.events().as_ref(), session, Some(run_a))
        .await
        .unwrap()
        .snapshot();
    assert_eq!(only_a.tokens_in, 10);
    assert_eq!(only_a.model_calls, 1);
    h.shutdown().await;
}

#[tokio::test]
async fn session_missing_rejects_record() {
    let h = Host::open(false, false).await;
    let log = h.log();
    let missing = SessionId::new();
    let err = log
        .record(DecisionRecord {
            session: missing,
            run: None,
            node: None,
            kind: DecisionKind::Budget,
            metadata: serde_json::json!({}),
            content_hash: None,
            prompt_body: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ObsError::Session(_)));
    h.shutdown().await;
}

#[tokio::test]
async fn append_failure_surfaces_obs_error() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    h.storage.close().await.unwrap();
    let err = log
        .record(DecisionRecord {
            session,
            run: None,
            node: None,
            kind: DecisionKind::Retry,
            metadata: serde_json::json!({}),
            content_hash: None,
            prompt_body: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ObsError::Append(_) | ObsError::Store(_)));
    // runtime still needs shutdown; storage already closed
    h.rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn never_writes_dotenv() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "SENTINEL=1\n").unwrap();
    let before = std::fs::read(&env_path).unwrap();
    let h = Host::open_kept(dir.clone(), false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    log.record(DecisionRecord {
        session,
        run: None,
        node: None,
        kind: DecisionKind::ToolGrant,
        metadata: serde_json::json!({}),
        content_hash: None,
        prompt_body: Some("touch .env".into()),
    })
    .await
    .unwrap();
    h.shutdown().await;
    let after = std::fs::read(&env_path).unwrap();
    assert_eq!(before, after);
}

#[test]
fn obs_module_not_imported_by_session_storage_runtime() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let roots = [
        manifest.join("session"),
        manifest.join("storage"),
        manifest.join("runtime"),
    ];
    for root in roots {
        for entry in walkdir_rs(&root) {
            let text = std::fs::read_to_string(&entry).unwrap();
            assert!(
                !text.contains("crate::obs") && !text.contains("alloy_runtime::obs"),
                "{} must not import obs",
                entry.display()
            );
        }
    }
}

fn walkdir_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir_rs(&p));
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn retention_policy_from_config_and_from_handle() {
    let h = Host::open(true, false).await;
    let cfg = h.handle.config().unwrap();
    let from_cfg = RetentionPolicy::from(cfg.as_ref());
    assert!(from_cfg.retain_full_prompts);
    assert!(!from_cfg.retain_tool_bodies);
    assert!(!RetentionPolicy::defaults().retain_full_prompts);
    assert!(!RetentionPolicy::defaults().retain_tool_bodies);
    // from_handle uses the same mapping — exercise via opt-in record body.
    let session = h.create_session().await;
    let log = h.log();
    log.record(DecisionRecord {
        session,
        run: None,
        node: None,
        kind: DecisionKind::Retry,
        metadata: serde_json::json!({}),
        content_hash: None,
        prompt_body: Some("kept".into()),
    })
    .await
    .unwrap();
    let page = list_decision_events(h.storage.events().as_ref(), session, None, 1)
        .await
        .unwrap();
    assert_eq!(
        parse_decision_event(&page.events[0])
            .unwrap()
            .prompt_body
            .as_deref(),
        Some("kept")
    );
    h.shutdown().await;
}

#[tokio::test]
async fn list_decision_events_limit_zero_is_one() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    for _ in 0..3 {
        log.record(DecisionRecord {
            session,
            run: None,
            node: None,
            kind: DecisionKind::Retry,
            metadata: serde_json::json!({}),
            content_hash: None,
            prompt_body: None,
        })
        .await
        .unwrap();
    }
    let page = list_decision_events(h.storage.events().as_ref(), session, None, 0)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(page.next_after.is_some());
    h.shutdown().await;
}

#[tokio::test]
async fn metadata_secrets_masked_on_wire() {
    let h = Host::open(false, false).await;
    let session = h.create_session().await;
    let log = h.log();
    log.record(DecisionRecord {
        session,
        run: None,
        node: None,
        kind: DecisionKind::ModelRoute,
        metadata: serde_json::json!({"api_key": "sk-abcdefghij", "ok": 1}),
        content_hash: None,
        prompt_body: None,
    })
    .await
    .unwrap();
    let page = list_decision_events(h.storage.events().as_ref(), session, None, 1)
        .await
        .unwrap();
    let rec = parse_decision_event(&page.events[0]).unwrap();
    assert_eq!(rec.metadata["api_key"], "[REDACTED]");
    assert_eq!(rec.metadata["ok"], 1);
    h.shutdown().await;
}
