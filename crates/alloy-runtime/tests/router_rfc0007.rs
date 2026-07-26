//! RFC-0007 integration tests: HTTP, recording, budgets, lifecycle, and BYOM policy.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    BudgetCheck, BudgetPolicy, BudgetSnapshot, CapabilityId, ChatMessage, ChatRole, ModelProvider,
    ModelResponse, ModelRouter, OpenAiCompatibleProvider, OpenAiCompatibleSpec, PromptPack,
    ProviderId, RecordingDecisionLog, RecordingModelProvider, ResponseFormat, RetentionPolicy,
    RouterConfig, RouterError, RoutingRequest, RunId, SecretString, SessionId, SharedCostMeter,
    TomlModelRouter, TomlModelRouterParts, ToolChoice, Usage,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(base_url: &str, max_in_flight: u32) -> RouterConfig {
    RouterConfig::from_str(
        "integration",
        &format!(
            r#"
[policy]
default_tier = "standard"
max_in_flight = {max_in_flight}
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
        ),
    )
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

fn router(
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

#[tokio::test]
async fn wiremock_completion_records_usage_and_attribution() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer integration-secret"))
        .and(body_partial_json(json!({
            "model": "operator-configured",
            "stream": false,
            "response_format": {"type": "json_object"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "request-1",
            "choices": [{
                "message": {"content": "{\"answer\":true}"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 5}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let base_url = format!("{}/v1", server.uri());
    let config = config(&base_url, 2);
    let provider = Arc::new(
        OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
            id: ProviderId::new("provider").expect("provider"),
            base_url,
            api_key: SecretString::new("integration-secret"),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
        })
        .expect("HTTP provider"),
    );
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy {
        retain_full_prompts: true,
        retain_tool_bodies: false,
    }));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let router = router(
        config,
        provider,
        BudgetPolicy::default(),
        log.clone(),
        meter.clone(),
        run,
    );

    let routed = router.route(route_request(run)).await.expect("route");
    let response = router
        .complete(&routed, prompt())
        .await
        .expect("completion");
    assert_eq!(response.text.as_deref(), Some("{\"answer\":true}"));
    assert_eq!(response.structured, Some(json!({"answer": true})));
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: Some(20),
            output_tokens: Some(5),
        }
    );

    let meter_snapshot = meter.snapshot();
    assert_eq!(meter_snapshot.model_calls, 1);
    assert_eq!(meter_snapshot.tokens_in, 20);
    assert_eq!(meter_snapshot.tokens_out, 5);
    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].endpoint_id.as_ref().unwrap().as_str(), "endpoint");
    assert_eq!(calls[0].model.as_deref(), Some("operator-configured"));
    assert!(calls[0].route_event_seq.is_some());
    assert_eq!(calls[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(calls[0].provider_request_id.as_deref(), Some("request-1"));
}

#[tokio::test]
async fn recording_provider_budget_recheck_consumes_ticket_without_calling_provider() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    provider.push(Ok(ModelResponse {
        text: Some("unused".into()),
        structured: None,
        tool_calls: vec![],
        usage: Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
        },
        provider_request_id: None,
        finish_reason: None,
    }));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let policy = BudgetPolicy {
        max_tokens_per_run: 10,
        ..BudgetPolicy::default()
    };
    let router = router(
        config("https://example.com", 1),
        provider.clone(),
        policy,
        log,
        meter.clone(),
        run,
    );
    let routed = router.route(route_request(run)).await.expect("route");
    meter.add_model_usage(
        alloy_runtime::ModelTier::Standard,
        Some(10),
        Some(0),
        Some(0.0),
    );

    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
    assert!(matches!(
        router.complete(&routed, prompt()).await,
        Err(RouterError::AlreadyCompleted)
    ));
    assert!(provider.recorded().is_empty());
}

#[tokio::test]
async fn shutdown_report_is_shared_and_new_work_is_rejected() {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let meter = SharedCostMeter::new();
    let run = RunId::new();
    let router = router(
        config("https://example.com", 2),
        provider,
        BudgetPolicy::default(),
        log,
        meter,
        run,
    );

    let (first, second) = tokio::join!(router.shutdown(), router.shutdown());
    assert_eq!(first, second);
    assert_eq!(first.remaining_in_flight, 0);
    assert_eq!(first.remaining_appends, 0);
    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::ShuttingDown)
    ));
}

#[test]
fn completion_request_defaults_remain_non_streaming_surface() {
    let request = alloy_runtime::CompletionRequest {
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
        config: config("https://example.com", 1),
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
