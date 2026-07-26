//! OpenAI-compatible, non-streaming chat-completions provider.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::Instrument;
use url::Url;
use zeroize::Zeroize;

use crate::types::ids::ProviderId;

use super::config::validate_base_url;
use super::error::{map_reqwest_error, ProviderError};
use super::http_client::ValidatedHttpClient;
use super::secret::SecretString;
use super::traits::ModelProvider;
use super::types::{
    redact_and_truncate, ChatMessage, CompletionRequest, Health, ModelEndpoint, ModelResponse,
    ResponseFormat, Usage,
};

const RESPONSE_BODY_MAX_BYTES: usize = 1024 * 1024;

/// Construction parameters for an OpenAI-compatible provider.
pub struct OpenAiCompatibleSpec {
    /// Provider catalog identifier.
    pub id: ProviderId,
    /// API base URL; HTTPS or loopback HTTP only.
    pub base_url: String,
    /// API key used to build a sensitive authorization header.
    pub api_key: SecretString,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Total request timeout.
    pub request_timeout: Duration,
}

/// Non-streaming OpenAI-compatible chat-completions provider.
pub struct OpenAiCompatibleProvider {
    id: ProviderId,
    base_url: Url,
    api_key: SecretString,
    authorization: HeaderValue,
    client: ValidatedHttpClient,
}

impl OpenAiCompatibleProvider {
    /// Validate the URL and authorization header, then build a policy-constrained client.
    pub fn new(spec: OpenAiCompatibleSpec) -> Result<Self, ProviderError> {
        let mut base_url = validate_base_url(&spec.base_url)
            .map_err(|message| ProviderError::Internal(redact_and_truncate(&message, 512)))?;
        if !base_url.path().ends_with('/') {
            let normalized = format!("{}/", base_url.path());
            base_url.set_path(&normalized);
        }

        let mut bearer = format!("Bearer {}", spec.api_key.expose());
        let mut authorization = HeaderValue::from_str(&bearer).map_err(|_| {
            bearer.zeroize();
            ProviderError::Internal("invalid authorization header".into())
        })?;
        bearer.zeroize();
        authorization.set_sensitive(true);

        let client = ValidatedHttpClient::build(spec.connect_timeout, spec.request_timeout)?;
        Ok(Self {
            id: spec.id,
            base_url,
            api_key: spec.api_key,
            authorization,
            client,
        })
    }

    fn completion_url(&self) -> Result<Url, ProviderError> {
        self.base_url
            .join("chat/completions")
            .map_err(|error| ProviderError::Internal(redact_and_truncate(&error.to_string(), 512)))
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    async fn complete(
        &self,
        endpoint: &ModelEndpoint,
        request: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let url = self.completion_url()?;
        let body = WireRequest::new(endpoint, &request);
        let span = tracing::debug_span!(
            "alloy.router.provider_http",
            provider_id = %self.id,
            endpoint_id = %endpoint.id,
            status = tracing::field::Empty
        );
        let response = async {
            self.client
                .inner()
                .post(url)
                .header(AUTHORIZATION, self.authorization.clone())
                .header(ACCEPT, "application/json")
                .json(&body)
                .send()
                .await
        }
        .instrument(span.clone())
        .await
        .map_err(map_reqwest_error)?;
        let status = response.status();
        span.record("status", status.as_u16());

        let bytes = match read_capped(response).await {
            Ok(bytes) => bytes,
            Err(BodyReadError::TooLarge) if status.is_success() => {
                return Err(ProviderError::MalformedResponse(
                    "response body too large".into(),
                ));
            }
            Err(BodyReadError::TooLarge) => {
                return Err(ProviderError::HttpStatus {
                    status: status.as_u16(),
                    message: "response body too large".into(),
                });
            }
            Err(BodyReadError::Request(error)) => return Err(map_reqwest_error(error)),
        };

        if !status.is_success() {
            return Err(map_status(status.as_u16(), &bytes, self.api_key.expose()));
        }
        map_success(&bytes, &request.response_format)
    }

    async fn health(&self) -> Health {
        Health::Healthy
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<WireResponseFormat>,
}

impl<'a> WireRequest<'a> {
    fn new(endpoint: &'a ModelEndpoint, request: &'a CompletionRequest) -> Self {
        Self {
            model: &endpoint.model,
            messages: &request.messages,
            stream: false,
            temperature: request.temperature,
            max_tokens: request.max_output_tokens,
            response_format: matches!(request.response_format, ResponseFormat::JsonObject)
                .then_some(WireResponseFormat {
                    kind: "json_object",
                }),
        }
    }
}

#[derive(Serialize)]
struct WireResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
pub(crate) struct WireResponse {
    id: Option<String>,
    choices: Option<Vec<WireChoice>>,
    #[serde(default, deserialize_with = "deserialize_optional_usage")]
    usage: Option<WireUsage>,
    error: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct WireChoice {
    message: Option<WireMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WireMessage {
    content: Option<WireContent>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WireContentPart>),
    Other(#[allow(dead_code)] Value),
}

#[derive(Deserialize)]
struct WireContentPart {
    text: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    prompt_tokens: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireErrorEnvelope {
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    code: Option<String>,
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)
        .ok()
        .and_then(|value| value.as_u64()))
}

/// Malformed / wrong-typed `usage` MUST NOT fail an otherwise valid completion (§5.7).
fn deserialize_optional_usage<'de, D>(deserializer: D) -> Result<Option<WireUsage>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| {
        // Reject sequences: serde would otherwise map `[prompt, completion]` positionally.
        if !value.is_object() {
            return None;
        }
        serde_json::from_value(value).ok()
    }))
}

enum BodyReadError {
    TooLarge,
    Request(reqwest::Error),
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, BodyReadError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyReadError::Request)? {
        if body.len().saturating_add(chunk.len()) > RESPONSE_BODY_MAX_BYTES {
            return Err(BodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn map_status(status: u16, body: &[u8], api_key: &str) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimit,
        400 if context_length_signal(body) => ProviderError::ContextLength,
        _ => {
            // Bound work before scrubbing; final message is ≤512 UTF-8 bytes.
            let prefix = utf8_prefix(body, 4 * 1024);
            let body = String::from_utf8_lossy(prefix);
            let scrubbed = if api_key.is_empty() {
                body.into_owned()
            } else {
                body.replace(api_key, "[REDACTED]")
            };
            ProviderError::HttpStatus {
                status,
                message: redact_and_truncate(&scrubbed, 512),
            }
        }
    }
}

fn utf8_prefix(bytes: &[u8], max_bytes: usize) -> &[u8] {
    let end = max_bytes.min(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => &bytes[..end],
        Err(error) => &bytes[..error.valid_up_to()],
    }
}

fn context_length_signal(body: &[u8]) -> bool {
    if serde_json::from_slice::<WireErrorEnvelope>(body)
        .ok()
        .and_then(|root| root.error)
        .and_then(|error| error.code)
        .is_some_and(|code| code.eq_ignore_ascii_case("context_length_exceeded"))
    {
        return true;
    }
    let scan_len = body.len().min(8 * 1024);
    let lowercase = String::from_utf8_lossy(&body[..scan_len]).to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "maximum tokens exceeded",
        "too many tokens",
        "prompt is too long",
    ]
    .iter()
    .any(|signal| lowercase.contains(signal))
}

fn map_success(body: &[u8], format: &ResponseFormat) -> Result<ModelResponse, ProviderError> {
    let root_value: Value = serde_json::from_slice(body).map_err(|error| {
        ProviderError::MalformedResponse(redact_and_truncate(&error.to_string(), 512))
    })?;
    if !root_value.is_object() {
        return Err(ProviderError::MalformedResponse(
            "response root must be a JSON object".into(),
        ));
    }
    let root: WireResponse = serde_json::from_value(root_value).map_err(|error| {
        ProviderError::MalformedResponse(redact_and_truncate(&error.to_string(), 512))
    })?;
    if root.error.is_some_and(|value| value.is_object()) {
        return Err(ProviderError::MalformedResponse(
            "successful response contains an error object".into(),
        ));
    }
    let choice = root
        .choices
        .as_ref()
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::MalformedResponse("missing response choice".into()))?;
    let message = choice
        .message
        .as_ref()
        .ok_or_else(|| ProviderError::MalformedResponse("missing response message".into()))?;

    let text = map_content(message.content.as_ref())?;
    let finish_reason = if text.is_none() && message.refusal.is_some() {
        Some("refusal".to_owned())
    } else {
        choice
            .finish_reason
            .as_deref()
            .map(|value| redact_and_truncate(value, 128))
    };
    let structured = if matches!(format, ResponseFormat::JsonObject) {
        text.as_deref()
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .filter(Value::is_object)
    } else {
        None
    };
    let input_tokens = root.usage.as_ref().and_then(|usage| usage.prompt_tokens);
    let output_tokens = root
        .usage
        .as_ref()
        .and_then(|usage| usage.completion_tokens);

    Ok(ModelResponse {
        text,
        structured,
        tool_calls: vec![],
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        provider_request_id: root
            .id
            .as_deref()
            .map(|value| redact_and_truncate(value, 256)),
        finish_reason,
    })
}

fn map_content(content: Option<&WireContent>) -> Result<Option<String>, ProviderError> {
    match content {
        None => Ok(None),
        Some(WireContent::Text(text)) => Ok(Some(text.clone())),
        Some(WireContent::Parts(parts)) => {
            let mut combined = String::new();
            let mut found = false;
            for text in parts.iter().filter_map(|part| part.text.as_deref()) {
                found = true;
                combined.push_str(text);
            }
            if found {
                Ok(Some(combined))
            } else {
                Err(ProviderError::MalformedResponse(
                    "content parts contain no text".into(),
                ))
            }
        }
        Some(WireContent::Other(_)) => Err(ProviderError::MalformedResponse(
            "response content has an invalid type".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(base_url: &str, key: &str) -> OpenAiCompatibleSpec {
        OpenAiCompatibleSpec {
            id: ProviderId::new("provider").unwrap(),
            base_url: base_url.into(),
            api_key: SecretString::new(key),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn normalizes_base_path_and_marks_authorization_sensitive() {
        let provider =
            OpenAiCompatibleProvider::new(spec("https://example.com/v1", "secret")).unwrap();
        assert_eq!(
            provider.completion_url().unwrap().as_str(),
            "https://example.com/v1/chat/completions"
        );
        let trailing =
            OpenAiCompatibleProvider::new(spec("https://example.com/v1/", "secret")).unwrap();
        assert_eq!(
            trailing.completion_url().unwrap().as_str(),
            "https://example.com/v1/chat/completions"
        );
        assert!(provider.authorization.is_sensitive());
        assert!(OpenAiCompatibleProvider::new(spec("https://example.com", "bad\nkey")).is_err());
    }

    #[test]
    fn maps_content_parts_refusal_and_malformed_usage() {
        let parts = br#"{
            "id":"request",
            "choices":[{"message":{"content":[{"text":"a"},{"kind":"ignored"},{"text":"b"}]},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":-1,"completion_tokens":1.5}
        }"#;
        let response = map_success(parts, &ResponseFormat::Text).unwrap();
        assert_eq!(response.text.as_deref(), Some("ab"));
        assert_eq!(response.usage.input_tokens, None);
        assert_eq!(response.usage.output_tokens, None);

        let bad_usage_container = br#"{
            "choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],
            "usage":"not-an-object"
        }"#;
        let response = map_success(bad_usage_container, &ResponseFormat::Text).unwrap();
        assert_eq!(response.text.as_deref(), Some("ok"));
        assert_eq!(response.usage.input_tokens, None);
        assert_eq!(response.usage.output_tokens, None);

        let usage_array = br#"{
            "choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],
            "usage":[1,2]
        }"#;
        let response = map_success(usage_array, &ResponseFormat::Text).unwrap();
        assert_eq!(response.usage.input_tokens, None);

        let refusal =
            br#"{"choices":[{"message":{"content":null,"refusal":"no"},"finish_reason":"stop"}]}"#;
        let response = map_success(refusal, &ResponseFormat::Text).unwrap();
        assert_eq!(response.text, None);
        assert_eq!(response.finish_reason.as_deref(), Some("refusal"));

        let invalid_content =
            br#"{"choices":[{"message":{"content":123},"finish_reason":"stop"}]}"#;
        assert!(matches!(
            map_success(invalid_content, &ResponseFormat::Text),
            Err(ProviderError::MalformedResponse(_))
        ));

        let array_root =
            br#"[null,[{"message":{"content":"x"},"finish_reason":"stop"}],null,null]"#;
        assert!(matches!(
            map_success(array_root, &ResponseFormat::Text),
            Err(ProviderError::MalformedResponse(_))
        ));
    }

    #[test]
    fn maps_status_table_and_structured_object() {
        assert!(matches!(
            map_status(401, b"secret", "key"),
            ProviderError::Auth
        ));
        assert!(matches!(
            map_status(403, b"forbidden", "key"),
            ProviderError::Auth
        ));
        assert!(matches!(
            map_status(429, b"wait", "key"),
            ProviderError::RateLimit
        ));
        assert!(matches!(
            map_status(400, b"maximum context exceeded", "key"),
            ProviderError::ContextLength
        ));
        let body =
            br#"{"choices":[{"message":{"content":"{\"ok\":true}"},"finish_reason":"stop"}]}"#;
        assert!(map_success(body, &ResponseFormat::JsonObject)
            .unwrap()
            .structured
            .is_some());
    }

    #[test]
    fn status_body_scrubs_exact_api_key() {
        let error = map_status(
            500,
            b"upstream echoed exact-secret-value",
            "exact-secret-value",
        );
        let ProviderError::HttpStatus { message, .. } = error else {
            panic!("unexpected status mapping");
        };
        assert!(!message.contains("exact-secret-value"));
        assert!(message.contains("[REDACTED]"));
    }
}
