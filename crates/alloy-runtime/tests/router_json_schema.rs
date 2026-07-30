//! Schema-constrained decoding (RFC-0007 amendment A-0007-2): the request
//! body carries the caller's JSON Schema when — and only when — the routed
//! endpoint opted in with `supports_json_schema = true`; otherwise the
//! router degrades honestly to plain `json_object`.
//!
//! Author: arkadianet

use std::sync::Arc;

use alloy_runtime::{
    BudgetPolicy, BudgetSnapshot, CapabilityId, ChatMessage, ChatRole, JsonSchemaSpec,
    ModelResponse, ModelRouter, PromptPack, ProviderId, RecordingDecisionLog,
    RecordingModelProvider, ResponseFormat, RetentionPolicy, RouterConfig, RoutingRequest, RunId,
    SessionId, SharedCostMeter, TomlModelRouter, TomlModelRouterParts, Usage,
};
use serde_json::json;

fn config_source(supports_json_schema: bool) -> String {
    format!(
        r#"
[policy]
default_tier = "standard"
max_in_flight = 2
shutdown_grace_ms = 50

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "http://127.0.0.1:1"
api_key_env = "ALLOY_TEST_KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "operator-configured"
tiers = ["standard"]
supports_structured_output = true
supports_json_schema = {supports_json_schema}
max_context = 4096
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0

[capability_tiers]
repair = "standard"
"#
    )
}

fn router(supports_json_schema: bool) -> (Arc<RecordingModelProvider>, TomlModelRouter, RunId) {
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider id"),
    ));
    let config = RouterConfig::from_str("json-schema tests", &config_source(supports_json_schema))
        .expect("valid config");
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();
    let router = TomlModelRouter::from_parts(TomlModelRouterParts::new(
        config,
        Arc::clone(&provider) as _,
        BudgetPolicy::default(),
        Some(log),
        Some(SharedCostMeter::new()),
        Some(run),
    ))
    .expect("router");
    (provider, router, run)
}

fn schema_spec() -> JsonSchemaSpec {
    JsonSchemaSpec {
        name: "repair_plan".into(),
        schema: json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"],
            "additionalProperties": false,
        }),
    }
}

fn request(run: RunId, schema: Option<JsonSchemaSpec>) -> RoutingRequest {
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
        response_schema: schema,
    }
}

fn prompt() -> PromptPack {
    PromptPack {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".into(),
        }],
        citations: vec![],
        domains: None,
    }
}

fn scripted_response() -> ModelResponse {
    ModelResponse {
        text: Some("{\"summary\":\"ok\"}".into()),
        structured: None,
        tool_calls: vec![],
        usage: Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
        },
        provider_request_id: None,
        finish_reason: Some("stop".into()),
    }
}

/// The completion request carries the caller's schema verbatim when the
/// endpoint opted in.
#[tokio::test]
async fn request_body_carries_schema_when_endpoint_supports_it() {
    let (provider, router, run) = router(true);
    provider.push(Ok(scripted_response()));

    let routed = router
        .route(request(run, Some(schema_spec())))
        .await
        .unwrap();
    router.complete(&routed, prompt()).await.unwrap();

    let recorded = provider.recorded();
    assert_eq!(recorded.len(), 1);
    let (endpoint, completion) = &recorded[0];
    assert!(endpoint.supports_json_schema);
    let ResponseFormat::JsonSchema { name, schema } = &completion.response_format else {
        panic!(
            "expected JsonSchema response format, got {:?}",
            completion.response_format
        );
    };
    assert_eq!(name, "repair_plan");
    assert_eq!(schema, &schema_spec().schema);
}

/// An endpoint that did not opt in degrades honestly to `json_object`
/// rather than sending a body the server may reject.
#[tokio::test]
async fn schema_degrades_to_json_object_when_endpoint_lacks_support() {
    let (provider, router, run) = router(false);
    provider.push(Ok(scripted_response()));

    let routed = router
        .route(request(run, Some(schema_spec())))
        .await
        .unwrap();
    router.complete(&routed, prompt()).await.unwrap();

    let recorded = provider.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1.response_format, ResponseFormat::JsonObject);
}

/// No schema and no structured requirement keep the pre-amendment shapes.
#[tokio::test]
async fn absent_schema_keeps_json_object_and_text_formats() {
    let (provider, router, run) = router(true);
    provider.push(Ok(scripted_response()));
    provider.push(Ok(scripted_response()));

    let routed = router.route(request(run, None)).await.unwrap();
    router.complete(&routed, prompt()).await.unwrap();

    let mut plain = request(run, None);
    plain.requires_structured_output = false;
    let routed = router.route(plain).await.unwrap();
    router.complete(&routed, prompt()).await.unwrap();

    let recorded = provider.recorded();
    assert_eq!(recorded[0].1.response_format, ResponseFormat::JsonObject);
    assert_eq!(recorded[1].1.response_format, ResponseFormat::Text);
}

/// `supports_json_schema` is optional in `router.toml` and defaults false,
/// so every existing config keeps parsing (and keeps its behaviour).
#[test]
fn endpoint_flag_defaults_false_in_config() {
    let source = config_source(true).replace("supports_json_schema = true\n", "");
    let config = RouterConfig::from_str("default flag", &source).expect("valid config");
    assert!(!config.providers[0].endpoints[0].supports_json_schema);
}

/// Serde: `RoutingRequest` without the new field still deserializes
/// (`response_schema` defaults to `None`), keeping older payloads valid.
#[test]
fn routing_request_without_schema_field_deserializes() {
    let mut value = serde_json::to_value(request(RunId::new(), None)).unwrap();
    let map = value.as_object_mut().unwrap();
    map.remove("response_schema");
    let parsed: RoutingRequest = serde_json::from_value(value).unwrap();
    assert!(parsed.response_schema.is_none());
    // And a present schema round-trips.
    let with = request(RunId::new(), Some(schema_spec()));
    let round: RoutingRequest =
        serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
    assert_eq!(round.response_schema, Some(schema_spec()));
}
