//! RFC-0007 core integration tests that must pass without the HTTP provider feature.

use std::future::pending;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alloy_runtime::{
    classify_provider_error, classify_router_error, parse_model_call_event, BudgetCheck,
    BudgetPolicy, BudgetSnapshot, CapabilityId, ChatMessage, ChatRole, CompletionRequest,
    DecisionLog, DecisionRecord, ErrorClass, EventSeq, FailureIr, Health, ModelCallRecord,
    ModelEndpoint, ModelProvider, ModelResponse, ModelRouter, ModelUsdSource, NodeId, ObsError,
    PromptPack, ProviderError, ProviderId, RecordingDecisionLog, RecordingModelProvider,
    ResponseFormat, RetentionPolicy, RetryDisposition, RouterConfig, RouterError, RoutingRequest,
    RunId, SessionEvent, SessionEventType, SessionId, SharedCostMeter, Timestamp, TomlModelRouter,
    TomlModelRouterParts, ToolCallRecord, ToolChoice, Usage,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn config_source(base_url: &str) -> String {
    format!(
        r#"
[policy]
default_tier = "standard"
max_in_flight = 2
shutdown_grace_ms = 50

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "{base_url}"
api_key_env = "MODEL_KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "operator-configured"
tiers = ["standard"]
supports_structured_output = true
max_context = 4096
input_usd_per_mtok = 2.0
output_usd_per_mtok = 4.0

[capability_tiers]
repair = "standard"
"#
    )
}

fn config(base_url: &str) -> RouterConfig {
    RouterConfig::from_str("core integration", &config_source(base_url))
        .expect("valid integration config")
}

fn route_request(run: RunId) -> RoutingRequest {
    RoutingRequest {
        session: SessionId::new(),
        run: Some(run),
        node: None,
        capability: CapabilityId::new("repair").expect("capability"),
        complexity: None,
        budget_remaining: BudgetSnapshot {
            usd_spent: 0.0,
            tokens_in: 0,
            tokens_out: 0,
        },
        requires_tools: false,
        requires_structured_output: true,
    }
}

fn prompt() -> PromptPack {
    PromptPack {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "return an object".into(),
        }],
        citations: vec![],
        domains: None,
    }
}

fn response(usage: Usage) -> ModelResponse {
    ModelResponse {
        text: Some("{\"answer\":true}".into()),
        structured: Some(json!({"answer": true})),
        tool_calls: vec![],
        usage,
        provider_request_id: Some("request-1".into()),
        finish_reason: Some("stop".into()),
    }
}

fn build_router(
    config: RouterConfig,
    provider: Arc<dyn ModelProvider>,
    policy: BudgetPolicy,
    log: Arc<RecordingDecisionLog>,
    meter: SharedCostMeter,
    run: RunId,
) -> TomlModelRouter {
    TomlModelRouter::from_parts(TomlModelRouterParts {
        config,
        provider,
        budget_policy: policy,
        decision_log: Some(log),
        cost_meter: Some(meter),
        bound_run: Some(run),
        allow_unmetered: false,
        shutdown_token: None,
    })
    .expect("router")
}

fn recording_dependencies(
    policy: BudgetPolicy,
) -> (
    Arc<RecordingModelProvider>,
    Arc<RecordingDecisionLog>,
    SharedCostMeter,
    RunId,
    TomlModelRouter,
) {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let router = build_router(
        config("https://example.com"),
        provider.clone(),
        policy,
        log.clone(),
        meter.clone(),
        run,
    );
    (provider, log, meter, run, router)
}

#[tokio::test]
async fn budget_denial_no_provider_call() {
    let policy = BudgetPolicy {
        max_tokens_per_run: 0,
        ..BudgetPolicy::default()
    };
    let (provider, _, _, run, router) = recording_dependencies(policy);

    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(provider.recorded().is_empty());
}

#[tokio::test]
async fn double_complete_already_completed() {
    let (provider, _, _, run, router) = recording_dependencies(BudgetPolicy::default());
    provider.push(Ok(response(Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
    })));
    let routed = router.route(route_request(run)).await.expect("route");
    let clone = routed.clone();

    router
        .complete(&routed, prompt())
        .await
        .expect("completion");
    assert!(matches!(
        router.complete(&clone, prompt()).await,
        Err(RouterError::AlreadyCompleted)
    ));
    assert_eq!(provider.recorded().len(), 1);
}

#[tokio::test]
async fn wrong_router_rejected() {
    let (_, _, _, run, first) = recording_dependencies(BudgetPolicy::default());
    let (_, _, _, _, second) = recording_dependencies(BudgetPolicy::default());
    let routed = first.route(route_request(run)).await.expect("route");

    assert!(matches!(
        second.complete(&routed, prompt()).await,
        Err(RouterError::WrongRouter)
    ));
}

struct PendingProvider {
    calls: AtomicUsize,
    started: Notify,
}

#[async_trait]
impl ModelProvider for PendingProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("provider").expect("provider")
    }

    async fn complete(
        &self,
        _endpoint: &ModelEndpoint,
        _request: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        pending().await
    }

    async fn health(&self) -> Health {
        Health::Healthy
    }
}

#[tokio::test]
async fn host_cancel_returns_cancelled() {
    let provider = Arc::new(PendingProvider {
        calls: AtomicUsize::new(0),
        started: Notify::new(),
    });
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let cancellation = CancellationToken::new();
    let router = TomlModelRouter::from_parts(TomlModelRouterParts {
        config: config("https://example.com"),
        provider: provider.clone(),
        budget_policy: BudgetPolicy::default(),
        decision_log: Some(log),
        cost_meter: Some(meter),
        bound_run: Some(run),
        allow_unmetered: false,
        shutdown_token: Some(cancellation.clone()),
    })
    .expect("router");
    let routed = router.route(route_request(run)).await.expect("route");

    cancellation.cancel();
    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::Cancelled)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

struct BlockingDecisionLog {
    model_started: Notify,
    release_model: Notify,
    model_done: Notify,
    model_calls: Mutex<Vec<ModelCallRecord>>,
}

impl BlockingDecisionLog {
    fn new() -> Self {
        Self {
            model_started: Notify::new(),
            release_model: Notify::new(),
            model_done: Notify::new(),
            model_calls: Mutex::new(Vec::new()),
        }
    }

    fn recorded_model_calls(&self) -> Vec<ModelCallRecord> {
        self.model_calls.lock().expect("model calls lock").clone()
    }
}

#[async_trait]
impl DecisionLog for BlockingDecisionLog {
    async fn record(&self, _record: DecisionRecord) -> Result<EventSeq, ObsError> {
        Ok(EventSeq(0))
    }

    async fn record_model_call(&self, record: ModelCallRecord) -> Result<EventSeq, ObsError> {
        self.model_started.notify_one();
        self.release_model.notified().await;
        self.model_calls
            .lock()
            .expect("model calls lock")
            .push(record);
        self.model_done.notify_one();
        Ok(EventSeq(1))
    }

    async fn record_tool_call(&self, _record: ToolCallRecord) -> Result<EventSeq, ObsError> {
        Ok(EventSeq(2))
    }
}

#[tokio::test]
async fn drop_complete_before_provider_no_obs() {
    let provider = Arc::new(PendingProvider {
        calls: AtomicUsize::new(0),
        started: Notify::new(),
    });
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let router = Arc::new(build_router(
        config("https://example.com"),
        provider.clone(),
        BudgetPolicy::default(),
        log.clone(),
        meter.clone(),
        run,
    ));
    let routed = router.route(route_request(run)).await.expect("route");
    let provider_started = provider.started.notified();
    tokio::pin!(provider_started);
    let task = {
        let router = router.clone();
        tokio::spawn(async move { router.complete(&routed, prompt()).await })
    };

    provider_started.await;
    task.abort();
    assert!(task
        .await
        .expect_err("completion task aborted")
        .is_cancelled());
    tokio::task::yield_now().await;
    assert!(log.recorded_model_calls().is_empty());
    assert_eq!(meter.snapshot().model_calls, 0);
}

#[tokio::test]
async fn drop_complete_after_provider_keeps_obs() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    provider.push(Ok(response(Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
    })));
    let log = Arc::new(BlockingDecisionLog::new());
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let router = Arc::new(
        TomlModelRouter::from_parts(TomlModelRouterParts {
            config: config("https://example.com"),
            provider,
            budget_policy: BudgetPolicy::default(),
            decision_log: Some(log.clone()),
            cost_meter: Some(meter.clone()),
            bound_run: Some(run),
            allow_unmetered: false,
            shutdown_token: None,
        })
        .expect("router"),
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let append_started = log.model_started.notified();
    tokio::pin!(append_started);
    let task = {
        let router = router.clone();
        tokio::spawn(async move { router.complete(&routed, prompt()).await })
    };

    append_started.await;
    assert_eq!(meter.snapshot().model_calls, 1);
    task.abort();
    assert!(task
        .await
        .expect_err("completion task aborted")
        .is_cancelled());

    let append_done = log.model_done.notified();
    tokio::pin!(append_done);
    log.release_model.notify_one();
    append_done.await;
    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].input_tokens, Some(3));
    assert_eq!(calls[0].output_tokens, Some(2));
}

#[tokio::test]
async fn zero_usd_ceiling_denies_with_unknown_spend() {
    let policy = BudgetPolicy {
        max_usd_per_run: 0.0,
        ..BudgetPolicy::default()
    };
    let (provider, _, meter, run, router) = recording_dependencies(policy);
    assert_eq!(meter.snapshot().usd_spent, None);

    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::BudgetDenied(BudgetCheck::UsdExhausted))
    ));
    assert!(provider.recorded().is_empty());
}

#[test]
fn from_parts_requires_meter() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();

    for (decision_log, cost_meter, bound_run) in [
        (None, Some(SharedCostMeter::new()), Some(run)),
        (Some(log.clone() as Arc<_>), None, Some(run)),
        (
            Some(log.clone() as Arc<_>),
            Some(SharedCostMeter::new()),
            None,
        ),
    ] {
        let result = TomlModelRouter::from_parts(TomlModelRouterParts {
            config: config("https://example.com"),
            provider: provider.clone(),
            budget_policy: BudgetPolicy::default(),
            decision_log,
            cost_meter,
            bound_run,
            allow_unmetered: false,
            shutdown_token: None,
        });
        assert!(matches!(result, Err(RouterError::Config(_))));
    }
}

#[test]
fn usd_budget_requires_prices() {
    let mut without_prices = config("https://example.com");
    without_prices.providers[0].endpoints[0].input_usd_per_mtok = None;
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));

    let result = TomlModelRouter::from_parts(TomlModelRouterParts {
        config: without_prices,
        provider,
        budget_policy: BudgetPolicy::default(),
        decision_log: Some(Arc::new(RecordingDecisionLog::new(
            RetentionPolicy::defaults(),
        ))),
        cost_meter: Some(SharedCostMeter::new()),
        bound_run: Some(RunId::new()),
        allow_unmetered: false,
        shutdown_token: None,
    });
    assert!(matches!(result, Err(RouterError::Config(_))));
}

#[test]
fn classify_retry_disposition_table() {
    let retryable = [
        ProviderError::RateLimit,
        ProviderError::Timeout,
        ProviderError::Transport("connection reset".into()),
        ProviderError::HttpStatus {
            status: 503,
            message: "unavailable".into(),
        },
    ];
    for error in retryable {
        assert_eq!(
            classify_provider_error(&error).retry,
            RetryDisposition::Retryable
        );
    }

    let non_retryable = [
        ProviderError::Auth,
        ProviderError::ContextLength,
        ProviderError::MalformedResponse("bad shape".into()),
        ProviderError::HttpStatus {
            status: 400,
            message: "bad request".into(),
        },
        ProviderError::Tls("certificate".into()),
        ProviderError::Internal("invariant".into()),
    ];
    for error in non_retryable {
        assert_eq!(
            classify_provider_error(&error).retry,
            RetryDisposition::NonRetryable
        );
    }
    assert_eq!(
        classify_router_error(&RouterError::BudgetDenied(BudgetCheck::UsdExhausted)),
        alloy_runtime::ClassifiedRouterFailure {
            class: ErrorClass::Budget,
            retry: RetryDisposition::NonRetryable,
        }
    );
    assert_eq!(
        classify_router_error(&RouterError::Cancelled).class,
        ErrorClass::Cancelled
    );
}

#[test]
fn failure_ir_carries_retry() {
    let classified = classify_provider_error(&ProviderError::RateLimit);
    let failure = FailureIr {
        node: NodeId::new(),
        error_class: classified.class,
        retry: classified.retry,
        diagnostics: vec![],
        notes: "provider rate limit".into(),
    };
    let encoded = serde_json::to_value(&failure).expect("serialize");
    assert_eq!(encoded["retry"], "retryable");
    assert_eq!(
        serde_json::from_value::<FailureIr>(encoded)
            .expect("deserialize")
            .retry,
        RetryDisposition::Retryable
    );

    let legacy = json!({
        "node": NodeId::new(),
        "error_class": "model",
        "diagnostics": [],
        "notes": "pre-RFC-0007"
    });
    assert_eq!(
        serde_json::from_value::<FailureIr>(legacy)
            .expect("legacy failure")
            .retry,
        RetryDisposition::NonRetryable
    );
}

#[tokio::test]
async fn model_call_has_endpoint_model_route_seq() {
    let (provider, log, _, run, router) = recording_dependencies(BudgetPolicy::default());
    provider.push(Ok(response(Usage {
        input_tokens: Some(20),
        output_tokens: Some(5),
    })));

    let routed = router.route(route_request(run)).await.expect("route");
    let route_event_seq = routed.route_event_seq().expect("recorded route");
    router
        .complete(&routed, prompt())
        .await
        .expect("completion");

    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].endpoint_id.as_ref().map(|id| id.as_str()),
        Some("endpoint")
    );
    assert_eq!(calls[0].model.as_deref(), Some("operator-configured"));
    assert_eq!(calls[0].route_event_seq, Some(route_event_seq));
    assert_eq!(
        calls[0].usd_source,
        Some(ModelUsdSource::OperatorPriceTable)
    );
}

#[test]
fn usage_unknown_roundtrip_query() {
    let session = SessionId::new();
    let event = SessionEvent {
        seq: EventSeq(4),
        ts: Timestamp::now(),
        session_id: session,
        run_id: Some(RunId::new()),
        type_: SessionEventType::ModelCall,
        payload: json!({
            "provider_id": "provider",
            "model_tier": "standard",
            "input_tokens": null,
            "output_tokens": 7,
            "usage_unknown": true,
            "usd": null,
            "duration_ms": 5,
            "confidence": null,
            "error_class": null,
            "content_hash": null,
            "prompt_body": null,
            "endpoint_id": "endpoint",
            "model": "operator-configured",
            "route_event_seq": 3,
            "usd_source": null,
            "finish_reason": "stop",
            "provider_request_id": "request-1"
        }),
    };

    let parsed = parse_model_call_event(&event).expect("parse model call");
    assert_eq!(parsed.input_tokens, None);
    assert_eq!(parsed.output_tokens, Some(7));
    assert_eq!(
        parsed.endpoint_id.as_ref().map(|id| id.as_str()),
        Some("endpoint")
    );
    assert_eq!(parsed.route_event_seq, Some(EventSeq(3)));
}

#[test]
fn model_call_pre_amendment_event_parses() {
    let event = SessionEvent {
        seq: EventSeq(1),
        ts: Timestamp::now(),
        session_id: SessionId::new(),
        run_id: None,
        type_: SessionEventType::ModelCall,
        payload: json!({
            "provider_id": "provider",
            "model_tier": "standard",
            "input_tokens": 2,
            "output_tokens": 3,
            "usage_unknown": false,
            "usd": null,
            "duration_ms": null,
            "confidence": null,
            "error_class": null,
            "content_hash": null,
            "prompt_body": null
        }),
    };

    let parsed = parse_model_call_event(&event).expect("parse pre-amendment model call");
    assert_eq!(parsed.endpoint_id, None);
    assert_eq!(parsed.model, None);
    assert_eq!(parsed.route_event_seq, None);
    assert_eq!(parsed.usd_source, None);
    assert_eq!(parsed.finish_reason, None);
    assert_eq!(parsed.provider_request_id, None);
}

#[test]
fn toml_parse_v2_example() {
    let parsed = RouterConfig::from_str(
        "router.toml.example",
        include_str!("../../../router.toml.example"),
    )
    .expect("shipped example must parse");
    assert_eq!(parsed.providers.len(), 1);
    assert_eq!(parsed.providers[0].endpoints.len(), 2);
}

#[test]
fn toml_rejects_non_loopback_http() {
    assert!(RouterConfig::from_str("non-loopback", &config_source("http://example.com")).is_err());
}

#[test]
fn toml_accepts_loopback_http() {
    for base_url in ["http://127.0.0.1:8080/v1", "http://localhost:8080/v1"] {
        RouterConfig::from_str("loopback", &config_source(base_url))
            .expect("loopback HTTP accepted");
    }
}

#[test]
fn toml_rejects_base_url_userinfo() {
    assert!(
        RouterConfig::from_str("userinfo", &config_source("https://user@example.com")).is_err()
    );
}

#[test]
fn toml_rejects_base_url_query_or_fragment() {
    for base_url in ["https://example.com?v=1", "https://example.com/#fragment"] {
        assert!(RouterConfig::from_str("suffix", &config_source(base_url)).is_err());
    }
}

#[test]
fn toml_rejects_two_providers() {
    let second_provider = r#"
[[providers]]
id = "other"
kind = "openai_compatible"
base_url = "https://other.example.com"
api_key_env = "OTHER_KEY"

[[providers.endpoints]]
id = "other-endpoint"
display_name = "Other"
model = "other-configured"
tiers = ["standard"]
max_context = 1024
input_usd_per_mtok = 1.0
output_usd_per_mtok = 1.0
"#;
    let source = format!(
        "{}\n{second_provider}",
        config_source("https://example.com")
    );
    assert!(RouterConfig::from_str("two providers", &source).is_err());
}

#[test]
fn toml_rejects_unknown_root_key() {
    let source = format!(
        "unknown_root_key = true\n{}",
        config_source("https://example.com")
    );
    assert!(RouterConfig::from_str("unknown root", &source).is_err());
}

#[tokio::test]
async fn scoring_weights_ignored() {
    let base = config_source("https://example.com");
    let weighted = format!(
        "{base}\n\
         [policy.scoring]\n\
         complexity_weight = 1000.0\n\
         budget_weight = -999.0\n\
         latency_weight = 42.0\n"
    );
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let run = RunId::new();
    let plain = build_router(
        RouterConfig::from_str("plain", &base).expect("plain"),
        provider.clone(),
        BudgetPolicy::default(),
        Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
        SharedCostMeter::new(),
        run,
    );
    let weighted = build_router(
        RouterConfig::from_str("weighted", &weighted).expect("weighted"),
        provider,
        BudgetPolicy::default(),
        Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
        SharedCostMeter::new(),
        run,
    );

    let plain_route = plain.route(route_request(run)).await.expect("plain route");
    let weighted_route = weighted
        .route(route_request(run))
        .await
        .expect("weighted route");
    assert_eq!(plain_route.endpoint(), weighted_route.endpoint());
}

#[test]
fn completion_request_defaults_remain_non_streaming_surface() {
    let request = CompletionRequest {
        messages: vec![],
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: ResponseFormat::Text,
        temperature: None,
        max_output_tokens: None,
    };
    assert!(request.tools.is_empty());
}

#[test]
fn production_rejects_unmetered_escape_hatch() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let result = TomlModelRouter::from_parts(TomlModelRouterParts {
        config: config("https://example.com"),
        provider,
        budget_policy: BudgetPolicy::default(),
        decision_log: None,
        cost_meter: None,
        bound_run: None,
        allow_unmetered: true,
        shutdown_token: None,
    });
    assert!(matches!(result, Err(RouterError::Config(_))));
}

#[test]
fn router_core_contains_no_hardcoded_vendor_model_ids() {
    let router_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/router");
    let denied = [
        "gpt-4",
        "gpt-3.5",
        "claude-3",
        "claude-opus",
        "gemini-",
        "o1-",
        "o3-",
    ];
    for entry in std::fs::read_dir(router_dir).expect("router source directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("rs")
            || path.file_name().and_then(|value| value.to_str()) == Some("recording.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("router source");
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source.as_str(), |(prefix, _)| prefix)
            .to_ascii_lowercase();
        for pattern in denied {
            assert!(
                !production.contains(pattern),
                "{} contains forbidden model-id pattern {pattern}",
                path.display()
            );
        }
    }
}
