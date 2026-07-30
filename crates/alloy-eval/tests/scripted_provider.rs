//! Scripted provider integration coverage.

use std::sync::Arc;

use alloy_eval::{RequestFingerprint, ScriptOutcome, ScriptedProvider, ScriptedProviderError};
use alloy_runtime::{
    CompletionRequest, EndpointId, ModelEndpoint, ModelProvider, ModelResponse, ModelTier,
    ProviderError, ProviderId, ResponseFormat, ToolChoice, Usage,
};

fn endpoint(provider: ProviderId) -> ModelEndpoint {
    ModelEndpoint {
        id: EndpointId::new("eval-script").unwrap(),
        provider,
        display_name: "eval-script".into(),
        model: "scripted".into(),
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: false,
        supports_json_schema: false,
        json_schema_strict: false,
        max_context: 8192,
        input_usd_per_mtok: None,
        output_usd_per_mtok: None,
        temperature: None,
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![],
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: ResponseFormat::Text,
        temperature: None,
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn scripted_provider_fixture() {
    let id = ProviderId::new("eval-script").unwrap();
    let provider = Arc::new(ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap());
    let object: Arc<dyn ModelProvider> = provider.clone();
    provider.push(
        &request(),
        ScriptOutcome::Response(ModelResponse {
            text: Some("ok".into()),
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
            },
            provider_request_id: None,
            finish_reason: None,
        }),
    );
    let response = object.complete(&endpoint(id), request()).await.unwrap();
    assert_eq!(response.text.as_deref(), Some("ok"));
    assert!(provider.is_exhausted());
}

#[tokio::test]
async fn scripted_provider_error_mapping() {
    let id = ProviderId::new("eval-script").unwrap();
    let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
    provider.push(
        &request(),
        ScriptOutcome::Error(ScriptedProviderError::RateLimit),
    );
    assert!(matches!(
        provider.complete(&endpoint(id), request()).await,
        Err(ProviderError::RateLimit)
    ));
}

#[test]
fn fingerprint_stable_across_crate_boundary() {
    let fp = RequestFingerprint::of(&request());
    assert_eq!(
        fp.as_hex(),
        "71ab8ab13b7cb4a68d7727e6268d8793fc4f41506cac57ef15cb7c1931ef7d36"
    );
}
