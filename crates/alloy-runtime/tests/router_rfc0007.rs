//! RFC-0007 HTTP-provider integration tests. Cargo gates this target on `http-provider`.

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    BudgetCheck, BudgetPolicy, BudgetSnapshot, CapabilityId, ChatMessage, ChatRole,
    CompletionRequest, ModelEndpoint, ModelProvider, ModelRouter, ModelTier, ModelUsdSource,
    OpenAiCompatibleProvider, OpenAiCompatibleSpec, PromptPack, ProviderError, ProviderId,
    RecordingDecisionLog, ResponseFormat, RetentionPolicy, RouterConfig, RouterError,
    RoutingRequest, RunId, SecretString, SessionId, SharedCostMeter, TomlModelRouter,
    TomlModelRouterParts, ToolChoice,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(base_url: &str, max_in_flight: u32) -> RouterConfig {
    RouterConfig::from_str(
        "HTTP integration",
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

fn endpoint() -> ModelEndpoint {
    ModelEndpoint {
        id: alloy_runtime::EndpointId::new("endpoint").expect("endpoint"),
        provider: ProviderId::new("provider").expect("provider"),
        display_name: "Endpoint".into(),
        model: "operator-configured".into(),
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: true,
        max_context: 4096,
        input_usd_per_mtok: Some(2.0),
        output_usd_per_mtok: Some(4.0),
    }
}

fn completion_request(format: ResponseFormat) -> CompletionRequest {
    CompletionRequest {
        messages: prompt().messages,
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: format,
        temperature: None,
        max_output_tokens: None,
    }
}

fn http_provider(base_url: String, request_timeout: Duration) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
        id: ProviderId::new("provider").expect("provider"),
        base_url,
        api_key: SecretString::new("integration-secret"),
        connect_timeout: Duration::from_secs(1),
        request_timeout,
    })
    .expect("HTTP provider")
}

fn router(
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

#[tokio::test]
async fn openai_complete_wiremock_ok() {
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
    let provider = Arc::new(http_provider(base_url, Duration::from_secs(2)));
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
    assert_eq!(response.usage.input_tokens, Some(20));
    assert_eq!(response.usage.output_tokens, Some(5));

    let meter_snapshot = meter.snapshot();
    assert_eq!(meter_snapshot.model_calls, 1);
    assert_eq!(meter_snapshot.tokens_in, 20);
    assert_eq!(meter_snapshot.tokens_out, 5);
    let calls = log.recorded_model_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].endpoint_id.as_ref().map(|id| id.as_str()),
        Some("endpoint")
    );
    assert_eq!(calls[0].model.as_deref(), Some("operator-configured"));
    assert_eq!(calls[0].route_event_seq, routed.route_event_seq());
    assert_eq!(
        calls[0].usd_source,
        Some(ModelUsdSource::OperatorPriceTable)
    );
    assert_eq!(calls[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(calls[0].provider_request_id.as_deref(), Some("request-1"));
}

#[tokio::test]
async fn openai_auth_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid bearer token"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::Auth)
    ));
}

#[tokio::test]
async fn openai_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::RateLimit)
    ));
}

#[tokio::test]
async fn openai_context_length() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"code": "context_length_exceeded", "message": "prompt is too long"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::ContextLength)
    ));
}

#[tokio::test]
async fn openai_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_json(json!({
                    "choices": [{"message": {"content": "late"}, "finish_reason": "stop"}]
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_millis(20));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::Timeout)
    ));
}

#[tokio::test]
async fn openai_malformed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{not-json"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::MalformedResponse(_))
    ));
}

#[tokio::test]
async fn openai_200_error_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"message": "provider encoded failure as success"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::MalformedResponse(_))
    ));
}

#[tokio::test]
async fn openai_finish_reason_length() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"content": "{\"partial\":true}"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 8}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let base_url = format!("{}/v1", server.uri());
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let run = RunId::new();
    let router = router(
        config(&base_url, 1),
        Arc::new(http_provider(base_url, Duration::from_secs(1))),
        BudgetPolicy::default(),
        log.clone(),
        SharedCostMeter::new(),
        run,
    );
    let routed = router.route(route_request(run)).await.expect("route");

    let response = router
        .complete(&routed, prompt())
        .await
        .expect("completion");
    assert_eq!(response.finish_reason.as_deref(), Some("length"));
    assert_eq!(
        log.recorded_model_calls()[0].finish_reason.as_deref(),
        Some("length")
    );
}

#[tokio::test]
async fn openai_content_parts_concat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "hello "},
                        {"type": "image", "url": "ignored"},
                        {"type": "text", "text": "world"}
                    ]
                },
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    let response = provider
        .complete(&endpoint(), completion_request(ResponseFormat::Text))
        .await
        .expect("completion");
    assert_eq!(response.text.as_deref(), Some("hello world"));
}

#[tokio::test]
async fn openai_refusal_no_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"content": null, "refusal": "cannot comply"},
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    let response = provider
        .complete(&endpoint(), completion_request(ResponseFormat::Text))
        .await
        .expect("refusal remains a response");
    assert_eq!(response.text, None);
    assert_eq!(response.finish_reason.as_deref(), Some("refusal"));
}

#[tokio::test]
async fn openai_body_over_cap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024 * 1024 + 1]))
        .expect(1)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(2));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::MalformedResponse(message))
            if message == "response body too large"
    ));
}

#[tokio::test]
async fn openai_redirect_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/redirect-target", server.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/redirect-target"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "must not be reached"}}]
        })))
        .expect(0)
        .mount(&server)
        .await;
    let provider = http_provider(format!("{}/v1", server.uri()), Duration::from_secs(1));

    assert!(matches!(
        provider
            .complete(&endpoint(), completion_request(ResponseFormat::Text))
            .await,
        Err(ProviderError::HttpStatus { status: 302, .. })
    ));
}

#[tokio::test]
async fn missing_api_key_fail_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let router_path = temp.path().join("router.toml");
    let missing_key = format!("ALLOY_RFC0007_MISSING_{}", RunId::new());
    assert!(std::env::var_os(&missing_key).is_none());
    let source = config("https://example.com", 1);
    let source = format!(
        r#"
[policy]
default_tier = "standard"

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "{}"
api_key_env = "{missing_key}"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "{}"
tiers = ["standard"]
max_context = 4096
input_usd_per_mtok = 2.0
output_usd_per_mtok = 4.0
"#,
        source.providers[0].base_url, source.providers[0].endpoints[0].model
    );
    std::fs::write(&router_path, source).expect("write router config");
    let hint = temp.path().join("example.env");

    let result = TomlModelRouter::from_paths(
        &router_path,
        BudgetPolicy::default(),
        &hint,
        Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
        SharedCostMeter::new(),
        RunId::new(),
    );
    let Err(RouterError::Config(message)) = result else {
        panic!("missing API key must fail closed");
    };
    assert!(message.contains(&missing_key));
    assert!(message.contains("example.env"));
    assert!(!temp.path().join(".env").exists());
}

#[tokio::test]
async fn budget_denial_no_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let base_url = format!("{}/v1", server.uri());
    let run = RunId::new();
    let router = router(
        config(&base_url, 1),
        Arc::new(http_provider(base_url, Duration::from_secs(1))),
        BudgetPolicy {
            max_tokens_per_run: 0,
            ..BudgetPolicy::default()
        },
        Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
        SharedCostMeter::new(),
        run,
    );

    assert!(matches!(
        router.route(route_request(run)).await,
        Err(RouterError::BudgetDenied(BudgetCheck::TokensExhausted))
    ));
}

#[tokio::test]
async fn double_complete_already_completed_no_second_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"content": "{\"ok\":true}"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base_url = format!("{}/v1", server.uri());
    let run = RunId::new();
    let router = router(
        config(&base_url, 1),
        Arc::new(http_provider(base_url, Duration::from_secs(1))),
        BudgetPolicy::default(),
        Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
        SharedCostMeter::new(),
        run,
    );
    let routed = router.route(route_request(run)).await.expect("route");
    let clone = routed.clone();

    router.complete(&routed, prompt()).await.expect("first");
    assert!(matches!(
        router.complete(&clone, prompt()).await,
        Err(RouterError::AlreadyCompleted)
    ));
}

/// Self-signed TLS server: client built with platform verifier (no invalid-cert
/// bypass) MUST classify the failure as [`ProviderError::Tls`], not Transport.
#[tokio::test]
async fn openai_tls_classified() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::ServerConfig;
    use std::net::SocketAddr;
    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .expect("self-signed cert");
    let cert_der = CertificateDer::from(certified.cert);
    let key_der =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server config");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(StdArc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        // Client rejects the untrusted cert; accept may fail — that is expected.
        let _ = acceptor.accept(tcp).await;
    });

    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
        id: ProviderId::new("provider").expect("provider"),
        base_url: format!("https://127.0.0.1:{}/v1/", addr.port()),
        api_key: SecretString::new("test-key"),
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
    })
    .expect("provider");

    let err = provider
        .complete(&endpoint(), completion_request(ResponseFormat::Text))
        .await
        .expect_err("untrusted cert must fail");
    assert!(
        matches!(err, ProviderError::Tls(_)),
        "expected Tls, got {err:?}"
    );
}
