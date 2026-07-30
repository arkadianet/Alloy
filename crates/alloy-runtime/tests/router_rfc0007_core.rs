//! RFC-0007 core integration tests that must pass without the HTTP provider feature.

use std::future::pending;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_runtime::{
    classify_provider_error, classify_router_error, parse_model_call_event, BudgetCheck,
    BudgetPolicy, BudgetSnapshot, CapabilityId, ChatMessage, ChatRole, CompletionRequest,
    DecisionKind, DecisionLog, DecisionRecord, EndpointId, ErrorClass, EventSeq, FailureIr, Health,
    ModelCallRecord, ModelEndpoint, ModelProvider, ModelResponse, ModelRouter, ModelTier,
    ModelUsdSource, NodeId, ObsError, PromptPack, ProviderError, ProviderId, RecordingDecisionLog,
    RecordingModelProvider, RetentionPolicy, RetryDisposition, RouterConfig, RouterError,
    RoutingRequest, RunId, SessionEvent, SessionEventType, SessionId, SharedCostMeter, Timestamp,
    TomlModelRouter, TomlModelRouterParts, ToolCallRecord, Usage,
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
        tier_override: None,
        complexity: None,
        budget_remaining: BudgetSnapshot {
            usd_spent: 0.0,
            tokens_in: 0,
            tokens_out: 0,
        },
        requires_tools: false,
        requires_structured_output: true,
        response_schema: None,
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
    TomlModelRouter::from_parts(TomlModelRouterParts::new(
        config,
        provider,
        policy,
        Some(log),
        Some(meter),
        Some(run),
    ))
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
    let (provider, log, _, run, router) = recording_dependencies(policy);

    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(provider.recorded().is_empty());
    let decisions = log.recorded_decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].kind, DecisionKind::Budget);
    assert_eq!(decisions[0].metadata["budget_check"], "tokens_exhausted");
    assert_eq!(decisions[0].metadata["budget_source"], "meter");
    assert_eq!(decisions[0].metadata["in_flight_at_route"], 1);
    let metrics = router.metrics();
    assert_eq!(metrics.routes_budget_denied, 1);
    assert_eq!(metrics.completes_err, 0);
}

#[tokio::test]
async fn complete_budget_recheck_denies() {
    let policy = BudgetPolicy {
        max_tokens_per_run: 10,
        ..BudgetPolicy::default()
    };
    let (provider, log, meter, run, router) = recording_dependencies(policy);
    let routed = router.route(route_request(run)).await.expect("route");
    let duplicate = routed.clone();
    meter.add_model_usage(ModelTier::Standard, Some(10), Some(0), None);

    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(matches!(
        router.complete(&duplicate, prompt()).await,
        Err(RouterError::AlreadyCompleted)
    ));
    assert!(provider.recorded().is_empty());
    let decisions = log.recorded_decisions();
    let budget = decisions.last().expect("completion budget decision");
    assert_eq!(budget.kind, DecisionKind::Budget);
    assert_eq!(budget.metadata["budget_check"], "tokens_exhausted");
    assert_eq!(budget.metadata["budget_source"], "meter");
    assert_eq!(budget.metadata["in_flight_at_route"], 1);
    let metrics = router.metrics();
    assert_eq!(metrics.routes_ok, 1);
    // BudgetDenied + AlreadyCompleted both return Err from complete (§9.3).
    assert_eq!(metrics.completes_err, 2);
    assert_eq!(metrics.in_flight, 0);
}

#[tokio::test]
async fn route_uses_meter_before_snapshot() {
    let policy = BudgetPolicy {
        max_tokens_per_run: 5,
        ..BudgetPolicy::default()
    };
    let (provider, _, meter, run, router) = recording_dependencies(policy);
    meter.add_model_usage(ModelTier::Standard, Some(5), Some(0), None);
    let request = route_request(run);
    assert_eq!(request.budget_remaining.tokens_in, 0);

    assert!(matches!(
        router.route(request).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(provider.recorded().is_empty());
}

#[tokio::test]
async fn budget_denied_no_escalation() {
    let mut router_config = config("https://example.com");
    router_config
        .capability_tiers
        .insert("repair".into(), ModelTier::Premium);
    router_config.providers[0].endpoints[0].tiers = vec![ModelTier::Premium];
    let mut economy = router_config.providers[0].endpoints[0].clone();
    economy.id = EndpointId::new("economy-endpoint").expect("endpoint");
    economy.model = "operator-economy".into();
    economy.tiers = vec![ModelTier::Economy];
    router_config.providers[0].endpoints.push(economy);
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();
    let router = build_router(
        router_config,
        provider.clone(),
        BudgetPolicy {
            max_tokens_per_run: 0,
            ..BudgetPolicy::default()
        },
        log.clone(),
        SharedCostMeter::new(),
        run,
    );

    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(provider.recorded().is_empty());
    let decision = log.recorded_decisions().pop().expect("budget decision");
    assert_eq!(decision.metadata["tier"], "premium");
}

#[tokio::test]
async fn route_decision_metadata_is_normative() {
    let (_, log, _, run, router) = recording_dependencies(BudgetPolicy::default());
    router.route(route_request(run)).await.expect("route");

    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.kind, DecisionKind::ModelRoute);
    assert_eq!(decision.metadata["capability"], "repair");
    assert_eq!(decision.metadata["capability_mapped"], true);
    assert_eq!(decision.metadata["tier"], "standard");
    assert_eq!(decision.metadata["tier_source"], "capability_map");
    assert_eq!(decision.metadata["provider_id"], "provider");
    assert_eq!(decision.metadata["endpoint_id"], "endpoint");
    assert_eq!(decision.metadata["model"], "operator-configured");
    assert_eq!(decision.metadata["requires_tools"], false);
    assert_eq!(decision.metadata["requires_structured_output"], true);
    assert_eq!(decision.metadata["in_flight_at_route"], 1);
}

#[tokio::test]
async fn no_endpoint_records_model_route() {
    let (provider, log, _, run, router) = recording_dependencies(BudgetPolicy::default());
    let mut request = route_request(run);
    request.requires_tools = true;

    assert!(matches!(
        router.route(request).await,
        Err(RouterError::NoEndpoint {
            requires_tools: true,
            ..
        })
    ));
    assert!(provider.recorded().is_empty());
    let decision = log
        .recorded_decisions()
        .pop()
        .expect("no endpoint decision");
    assert_eq!(decision.kind, DecisionKind::ModelRoute);
    assert_eq!(decision.metadata["error"], "no_endpoint");
    assert_eq!(decision.metadata["capability"], "repair");
    assert_eq!(decision.metadata["capability_mapped"], true);
    assert_eq!(decision.metadata["tier"], "standard");
    assert_eq!(decision.metadata["tier_source"], "capability_map");
    assert_eq!(decision.metadata["provider_id"], "provider");
    assert_eq!(decision.metadata["requires_tools"], true);
    assert_eq!(decision.metadata["requires_structured_output"], true);
    assert_eq!(decision.metadata["in_flight_at_route"], 1);
    assert!(decision.metadata.get("endpoint_id").is_none());
    assert!(decision.metadata.get("model").is_none());
    assert_eq!(router.metrics().routes_no_endpoint, 1);
}

/// Config with a `premium`-only endpoint alongside the `standard` one, so a
/// routed tier is observable in `endpoint_id` rather than inferred.
fn escalation_config_source() -> String {
    r#"
[policy]
default_tier = "standard"
max_in_flight = 2
shutdown_grace_ms = 50

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "MODEL_KEY"

[[providers.endpoints]]
id = "standard-endpoint"
display_name = "Standard"
model = "operator-standard"
tiers = ["standard"]
supports_structured_output = true
max_context = 4096
input_usd_per_mtok = 2.0
output_usd_per_mtok = 4.0

[[providers.endpoints]]
id = "premium-endpoint"
display_name = "Premium"
model = "operator-premium"
tiers = ["premium"]
supports_structured_output = true
max_context = 4096
input_usd_per_mtok = 8.0
output_usd_per_mtok = 16.0

[capability_tiers]
repair = "standard"
"#
    .to_owned()
}

fn escalation_router(source: &str) -> (Arc<RecordingDecisionLog>, RunId, TomlModelRouter) {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();
    let router = build_router(
        RouterConfig::from_str("escalation", source).expect("valid escalation config"),
        provider,
        BudgetPolicy::default(),
        log.clone(),
        SharedCostMeter::new(),
        run,
    );
    (log, run, router)
}

/// RFC-0010 §5.11.4 ES1/ES3 + RFC-0013 MR2: an escalated attempt carries
/// `CapabilityExecContext.effective_tier = Premium`, and that tier MUST reach
/// endpoint selection — otherwise escalation is cosmetic and the retry runs
/// on exactly the model that just failed.
#[tokio::test]
async fn tier_override_routes_to_the_escalated_endpoint() {
    let (log, run, router) = escalation_router(&escalation_config_source());
    let mut request = route_request(run);
    request.tier_override = Some(ModelTier::Premium);

    let routed = router.route(request).await.expect("premium endpoint");
    assert_eq!(routed.tier(), ModelTier::Premium);
    assert_eq!(routed.endpoint().id.as_str(), "premium-endpoint");
    assert_eq!(routed.endpoint().model, "operator-premium");

    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.metadata["tier"], "premium");
    assert_eq!(decision.metadata["tier_source"], "escalation");
    assert_eq!(decision.metadata["requested_tier"], "premium");
    assert_eq!(decision.metadata["endpoint_id"], "premium-endpoint");
    // The capability→tier map still resolved, it was merely raised.
    assert_eq!(decision.metadata["capability_mapped"], true);
}

/// The absent override is the identity: base routing is untouched.
#[tokio::test]
async fn absent_tier_override_routes_at_the_capability_tier() {
    let (log, run, router) = escalation_router(&escalation_config_source());
    let routed = router.route(route_request(run)).await.expect("standard");
    assert_eq!(routed.tier(), ModelTier::Standard);
    assert_eq!(routed.endpoint().id.as_str(), "standard-endpoint");
    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.metadata["tier_source"], "capability_map");
    assert!(decision.metadata.get("requested_tier").is_none());
}

/// RFC-0007 §5.2.1: nothing downgrades a configured tier. An override *below*
/// the capability tier is inert — escalation raises, it never lowers.
#[tokio::test]
async fn tier_override_never_downgrades_the_capability_tier() {
    let (log, run, router) = escalation_router(&escalation_config_source());
    let mut request = route_request(run);
    request.tier_override = Some(ModelTier::Economy);

    let routed = router.route(request).await.expect("standard endpoint");
    assert_eq!(routed.tier(), ModelTier::Standard);
    assert_eq!(routed.endpoint().id.as_str(), "standard-endpoint");
    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.metadata["tier"], "standard");
    assert_eq!(decision.metadata["tier_source"], "capability_map");
    assert_eq!(decision.metadata["requested_tier"], "economy");
    assert_eq!(decision.metadata["escalation_unserved"], false);
}

/// A single-endpoint config (what both shipped examples ship by default)
/// escalating to an unserved `premium` MUST degrade to the configured tier,
/// not hard-fail the retry with `NoEndpoint`. The fall back is recorded, so
/// "escalation had no target" is observable rather than silent.
#[tokio::test]
async fn unserved_escalation_falls_back_to_the_capability_tier() {
    let (log, run, router) = escalation_router(&config_source("https://example.com"));
    let mut request = route_request(run);
    request.tier_override = Some(ModelTier::Premium);

    let routed = router.route(request).await.expect("degrades to standard");
    assert_eq!(routed.tier(), ModelTier::Standard);
    assert_eq!(routed.endpoint().id.as_str(), "endpoint");

    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.metadata["tier"], "standard");
    assert_eq!(decision.metadata["tier_source"], "capability_map");
    assert_eq!(decision.metadata["requested_tier"], "premium");
    assert_eq!(decision.metadata["escalation_unserved"], true);
    assert_eq!(router.metrics().routes_escalation_unserved, 1);
    assert_eq!(router.metrics().routes_no_endpoint, 0);
}

/// Degradation is confined to the override: when the *configured* tier has no
/// endpoint either, `NoEndpoint` still names the configured tier (RFC-0007's
/// "no failover to another tier" is intact).
#[tokio::test]
async fn unserved_escalation_over_an_unserved_base_still_fails_closed() {
    let (log, run, router) = escalation_router(&escalation_config_source());
    let mut request = route_request(run);
    request.tier_override = Some(ModelTier::Premium);
    request.requires_tools = true; // no endpoint in this config supports tools

    assert!(matches!(
        router.route(request).await,
        Err(RouterError::NoEndpoint {
            tier: ModelTier::Standard,
            requires_tools: true,
            ..
        })
    ));
    let decision = log.recorded_decisions().pop().expect("route decision");
    assert_eq!(decision.metadata["error"], "no_endpoint");
    assert_eq!(decision.metadata["tier"], "standard");
    assert_eq!(decision.metadata["requested_tier"], "premium");
    assert_eq!(decision.metadata["escalation_unserved"], true);
    assert_eq!(router.metrics().routes_no_endpoint, 1);
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
async fn concurrent_completes() {
    const COMPLETIONS: usize = 8;
    let (provider, log, _, run, router) = recording_dependencies(BudgetPolicy::default());
    for _ in 0..COMPLETIONS {
        provider.push(Ok(response(Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
        })));
    }
    let router = Arc::new(router);
    let mut completions = Vec::new();
    for _ in 0..COMPLETIONS {
        let routed = router.route(route_request(run)).await.expect("route");
        let router = Arc::clone(&router);
        completions.push(tokio::spawn(async move {
            router.complete(&routed, prompt()).await
        }));
    }
    for completion in completions {
        completion
            .await
            .expect("completion task")
            .expect("successful completion");
    }

    assert_eq!(provider.recorded().len(), COMPLETIONS);
    assert_eq!(log.recorded_model_calls().len(), COMPLETIONS);
    let metrics = router.metrics();
    assert_eq!(metrics.routes_ok, COMPLETIONS as u64);
    assert_eq!(metrics.completes_ok, COMPLETIONS as u64);
    assert_eq!(metrics.completes_err, 0);
    assert_eq!(metrics.in_flight, 0);
}

#[tokio::test]
async fn max_in_flight_bounds_admission() {
    let provider = Arc::new(PendingProvider {
        calls: AtomicUsize::new(0),
        started: Notify::new(),
    });
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();
    let mut router_config = config("https://example.com");
    router_config.policy.max_in_flight = 1;
    let router = Arc::new(build_router(
        router_config,
        provider.clone(),
        BudgetPolicy::default(),
        log,
        SharedCostMeter::new(),
        run,
    ));
    let routed = router.route(route_request(run)).await.expect("route");
    let started = provider.started.notified();
    tokio::pin!(started);
    let first = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.complete(&routed, prompt()).await })
    };
    started.await;

    let waiting_route = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.route(route_request(run)).await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiting_route.is_finished());

    let shutdown = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiting_route)
            .await
            .expect("waiting route released")
            .expect("route task"),
        Err(RouterError::ShuttingDown)
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("first completion cancelled")
            .expect("completion task"),
        Err(RouterError::Cancelled)
    ));
    let report = shutdown.await.expect("shutdown task");
    assert!(report.cancelled_in_flight);
    assert_eq!(report.remaining_in_flight, 0);
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
    let router = TomlModelRouter::from_parts(
        TomlModelRouterParts::new(
            config("https://example.com"),
            provider.clone(),
            BudgetPolicy::default(),
            Some(log),
            Some(meter),
            Some(run),
        )
        .shutdown_token(cancellation.clone()),
    )
    .expect("router");
    let routed = router.route(route_request(run)).await.expect("route");

    cancellation.cancel();
    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::Cancelled)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    // Pre-cancel rejects before ticket consume, so a second attempt is still
    // Cancelled (not AlreadyCompleted).
    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::Cancelled)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mid_flight_host_cancel_returns_cancelled() {
    let provider = Arc::new(PendingProvider {
        calls: AtomicUsize::new(0),
        started: Notify::new(),
    });
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let cancellation = CancellationToken::new();
    let router = Arc::new(
        TomlModelRouter::from_parts(
            TomlModelRouterParts::new(
                config("https://example.com"),
                provider.clone(),
                BudgetPolicy::default(),
                Some(log.clone()),
                Some(meter.clone()),
                Some(run),
            )
            .shutdown_token(cancellation.clone()),
        )
        .expect("router"),
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let routed_for_complete = routed.clone();
    let started = provider.started.notified();
    tokio::pin!(started);
    let completion = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.complete(&routed_for_complete, prompt()).await })
    };
    started.await;
    cancellation.cancel();

    assert!(matches!(
        completion.await.expect("completion task"),
        Err(RouterError::Cancelled)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(meter.snapshot().model_calls, 1);
    assert_eq!(meter.snapshot().unknown_token_events, 1);
    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].error_class, Some(ErrorClass::Cancelled));
    assert!(calls[0].input_tokens.is_none());
    assert!(calls[0].output_tokens.is_none());
    // Host token remains cancelled → §5.4.1 Cancelled precedes AlreadyCompleted.
    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::Cancelled)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bound_run_mismatch_rejects_without_decision() {
    let (provider, log, _, run, router) = recording_dependencies(BudgetPolicy::default());
    let mut request = route_request(run);
    request.run = Some(RunId::new());

    assert!(matches!(
        router.route(request).await,
        Err(RouterError::Config(_))
    ));
    assert!(provider.recorded().is_empty());
    assert!(log.recorded_decisions().is_empty());
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
        TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config("https://example.com"),
            provider,
            BudgetPolicy::default(),
            Some(log.clone()),
            Some(meter.clone()),
            Some(run),
        ))
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
async fn shutdown_drains_appends() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    provider.push(Ok(response(Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
    })));
    let log = Arc::new(BlockingDecisionLog::new());
    let run = RunId::new();
    let router = Arc::new(
        TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config("https://example.com"),
            provider,
            BudgetPolicy::default(),
            Some(log.clone()),
            Some(SharedCostMeter::new()),
            Some(run),
        ))
        .expect("router"),
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let append_started = log.model_started.notified();
    tokio::pin!(append_started);
    let completion = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.complete(&routed, prompt()).await })
    };
    append_started.await;

    let first_shutdown = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    let second_shutdown = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    log.release_model.notify_one();

    completion
        .await
        .expect("completion task")
        .expect("completion");
    let first_report = first_shutdown.await.expect("first shutdown");
    let second_report = second_shutdown.await.expect("second shutdown");
    assert_eq!(first_report, second_report);
    assert_eq!(first_report.remaining_appends, 0);
    assert_eq!(log.recorded_model_calls().len(), 1);
}

#[tokio::test]
async fn shutdown_idempotent_concurrent() {
    let (_, _, _, _, router) = recording_dependencies(BudgetPolicy::default());
    let router = Arc::new(router);
    let first = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    let second = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    let first_report = first.await.expect("first shutdown");
    let second_report = second.await.expect("second shutdown");
    assert_eq!(first_report, second_report);
    assert_eq!(router.shutdown().await, first_report);
}

#[tokio::test]
async fn shutdown_leadership_is_cancel_safe() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    provider.push(Ok(response(Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
    })));
    let log = Arc::new(BlockingDecisionLog::new());
    let run = RunId::new();
    let router = Arc::new(
        TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config("https://example.com"),
            provider,
            BudgetPolicy::default(),
            Some(log.clone()),
            Some(SharedCostMeter::new()),
            Some(run),
        ))
        .expect("router"),
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let append_started = log.model_started.notified();
    tokio::pin!(append_started);
    let completion = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.complete(&routed, prompt()).await })
    };
    append_started.await;

    let leader = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.shutdown().await })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match router.route(route_request(run)).await {
                Err(RouterError::ShuttingDown) => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected route result: {error}"),
            }
        }
    })
    .await
    .expect("route loop observed ShuttingDown before timeout");
    leader.abort();
    assert!(leader
        .await
        .expect_err("shutdown caller aborted")
        .is_cancelled());
    log.release_model.notify_one();
    completion
        .await
        .expect("completion task")
        .expect("completion");

    let report = tokio::time::timeout(Duration::from_secs(1), router.shutdown())
        .await
        .expect("runtime-owned shutdown completed");
    assert_eq!(report.remaining_appends, 0);
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
        let result = TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config("https://example.com"),
            provider.clone(),
            BudgetPolicy::default(),
            decision_log,
            cost_meter,
            bound_run,
        ));
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

    let result = TomlModelRouter::from_parts(TomlModelRouterParts::new(
        without_prices,
        provider,
        BudgetPolicy::default(),
        Some(Arc::new(RecordingDecisionLog::new(
            RetentionPolicy::defaults(),
        ))),
        Some(SharedCostMeter::new()),
        Some(RunId::new()),
    ));
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

#[tokio::test]
async fn oversize_prompt_body_hash_only() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    provider.push(Ok(response(Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
    })));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy {
        retain_full_prompts: true,
        retain_tool_bodies: false,
    }));
    let run = RunId::new();
    let router = build_router(
        config("https://example.com"),
        provider,
        BudgetPolicy::default(),
        log.clone(),
        SharedCostMeter::new(),
        run,
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let oversized = PromptPack {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "x".repeat(300 * 1024),
        }],
        citations: vec![],
        domains: None,
    };

    router
        .complete(&routed, oversized)
        .await
        .expect("completion");
    let call = log.recorded_model_calls().pop().expect("model call");
    assert!(call.content_hash.is_some());
    assert!(call.prompt_body.is_none());
    assert_eq!(router.metrics().model_call_prompt_body_oversize, 1);
}

#[tokio::test]
async fn provider_error_records_unknown_usage_and_model_call() {
    let (provider, log, meter, run, router) = recording_dependencies(BudgetPolicy::default());
    provider.push(Err(ProviderError::RateLimit));
    let routed = router.route(route_request(run)).await.expect("route");

    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::Provider(ProviderError::RateLimit))
    ));

    let snap = meter.snapshot();
    assert_eq!(snap.model_calls, 1);
    assert_eq!(snap.unknown_token_events, 1);
    assert_eq!(snap.tokens_in, 0);
    assert_eq!(snap.tokens_out, 0);
    assert_eq!(snap.usd_spent, None);

    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.input_tokens, None);
    assert_eq!(call.output_tokens, None);
    assert_eq!(call.usd, None);
    assert_eq!(call.error_class, Some(ErrorClass::Model));
    assert_eq!(router.metrics().completes_err, 1);
}

#[tokio::test]
async fn metrics_public_api_counters() {
    let (provider, _, _, run, router) = recording_dependencies(BudgetPolicy::default());
    provider.push(Ok(response(Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
    })));
    provider.push(Err(ProviderError::RateLimit));

    let first = router.route(route_request(run)).await.expect("first route");
    router
        .complete(&first, prompt())
        .await
        .expect("first completion");

    let mut default_request = route_request(run);
    default_request.capability = CapabilityId::new("unmapped").expect("capability");
    let second = router.route(default_request).await.expect("second route");
    assert!(matches!(
        router.complete(&second, prompt()).await,
        Err(RouterError::Provider(ProviderError::RateLimit))
    ));

    let mut no_endpoint = route_request(run);
    no_endpoint.requires_tools = true;
    assert!(matches!(
        router.route(no_endpoint).await,
        Err(RouterError::NoEndpoint { .. })
    ));

    let metrics = router.metrics();
    assert_eq!(metrics.routes_ok, 2);
    assert_eq!(metrics.routes_no_endpoint, 1);
    assert_eq!(metrics.routes_default_tier, 1);
    assert_eq!(metrics.completes_ok, 1);
    assert_eq!(metrics.completes_err, 1);
    assert_eq!(metrics.in_flight, 0);
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
fn router_and_obs_contain_no_hardcoded_vendor_model_ids() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let denied = [
        "gpt-4",
        "gpt-3.5",
        "claude-3",
        "claude-opus",
        "gemini-",
        "o1-",
        "o3-",
    ];
    // RFC-0007 §11.6: no hardcoded vendor model IDs in router core or obs surfaces
    // that record route / complete metadata.
    let mut directories = vec![manifest.join("src/router"), manifest.join("src/obs")];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("source directory {}: {error}", directory.display()))
        {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                directories.push(path);
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("source file");
            let production = production_source_prefix(&source).to_ascii_lowercase();
            for pattern in denied {
                assert!(
                    !production.contains(pattern),
                    "{} contains forbidden model-id pattern {pattern}",
                    path.display()
                );
            }
        }
    }
}

/// Strip the trailing unit-test module so `#[cfg(test)]` field attributes do not
/// truncate production scans (e.g. `allow_unmetered` early in `toml_router.rs`).
fn production_source_prefix(source: &str) -> &str {
    const MARKERS: &[&str] = &["\n#[cfg(test)]\nmod tests", "\nmod tests {"];
    let mut cut = source.len();
    for marker in MARKERS {
        if let Some(idx) = source.find(marker) {
            cut = cut.min(idx);
        }
    }
    &source[..cut]
}
