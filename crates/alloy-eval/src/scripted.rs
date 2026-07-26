//! Endpoint-bound, request-keyed FIFO scripted [`alloy_runtime::ModelProvider`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use alloy_runtime::{
    CompletionRequest, Health, ModelEndpoint, ModelProvider, ModelResponse, ProviderError,
    ProviderId,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::fingerprint::RequestFingerprint;
use crate::manifest::ScriptTurnOutcome;

/// Keyed scripted [`ModelProvider`] for offline eval. Performs no network I/O.
///
/// Ownership: process-local; share across tasks only via [`std::sync::Arc`].
/// Sync: interior `std::sync::Mutex` (same pattern as `RecordingModelProvider`).
#[derive(Debug)]
pub struct ScriptedProvider {
    id: ProviderId,
    endpoint: ModelEndpoint,
    state: Mutex<ScriptedState>,
}

#[derive(Debug)]
struct ScriptedState {
    queues: HashMap<RequestFingerprint, VecDeque<ScriptOutcome>>,
    invocations: Vec<ScriptedInvocation>,
}

/// One scripted complete outcome.
#[derive(Debug, Clone)]
pub enum ScriptOutcome {
    /// Successful model response.
    Response(ModelResponse),
    /// Provider-level failure returned to the caller.
    Error(ScriptedProviderError),
}

impl From<ScriptTurnOutcome> for ScriptOutcome {
    fn from(value: ScriptTurnOutcome) -> Self {
        match value {
            ScriptTurnOutcome::Response {
                text,
                structured,
                usage,
                provider_request_id,
                finish_reason,
            } => Self::Response(ModelResponse {
                text,
                structured,
                tool_calls: vec![],
                usage,
                provider_request_id,
                finish_reason,
            }),
            ScriptTurnOutcome::Error { error } => Self::Error(error),
        }
    }
}

/// Cloneable subset of [`ProviderError`] for fixture manifests.
///
/// Mapped to [`ProviderError`] at `complete` time. Does not add variants to
/// the merged `ProviderError` enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScriptedProviderError {
    /// Authentication failure.
    Auth,
    /// Rate limit.
    RateLimit,
    /// Context length exceeded.
    ContextLength,
    /// Timeout.
    Timeout,
    /// Malformed provider response.
    MalformedResponse {
        /// Error message.
        message: String,
    },
    /// HTTP status failure.
    HttpStatus {
        /// Status code.
        status: u16,
        /// Error message.
        message: String,
    },
    /// TLS failure.
    Tls {
        /// Error message.
        message: String,
    },
    /// Transport failure.
    Transport {
        /// Error message.
        message: String,
    },
    /// Internal provider failure.
    Internal {
        /// Error message.
        message: String,
    },
}

impl From<ScriptedProviderError> for ProviderError {
    fn from(value: ScriptedProviderError) -> Self {
        match value {
            ScriptedProviderError::Auth => Self::Auth,
            ScriptedProviderError::RateLimit => Self::RateLimit,
            ScriptedProviderError::ContextLength => Self::ContextLength,
            ScriptedProviderError::Timeout => Self::Timeout,
            ScriptedProviderError::MalformedResponse { message } => {
                Self::MalformedResponse(message)
            }
            ScriptedProviderError::HttpStatus { status, message } => {
                Self::HttpStatus { status, message }
            }
            ScriptedProviderError::Tls { message } => Self::Tls(message),
            ScriptedProviderError::Transport { message } => Self::Transport(message),
            ScriptedProviderError::Internal { message } => Self::Internal(message),
        }
    }
}

/// One recorded scripted invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptedInvocation {
    /// Bound endpoint used for the call.
    pub endpoint: ModelEndpoint,
    /// Request that was completed.
    pub request: CompletionRequest,
    /// Fingerprint of the request.
    pub fingerprint: RequestFingerprint,
}

impl ScriptedProvider {
    /// Empty provider bound to exactly one endpoint.
    ///
    /// Failure: [`EvalError::Manifest`] when `endpoint.provider != id`.
    pub fn new(id: ProviderId, endpoint: ModelEndpoint) -> Result<Self, EvalError> {
        if endpoint.provider != id {
            return Err(EvalError::Manifest(
                "scripted provider id must match endpoint.provider".into(),
            ));
        }
        Ok(Self {
            id,
            endpoint,
            state: Mutex::new(ScriptedState {
                queues: HashMap::new(),
                invocations: Vec::new(),
            }),
        })
    }

    /// Append one outcome to the FIFO queue for `key`.
    pub fn insert(&self, key: RequestFingerprint, outcome: ScriptOutcome) {
        let mut state = Self::lock(&self.state);
        state.queues.entry(key).or_default().push_back(outcome);
    }

    /// Fingerprint `request` and append one outcome to its FIFO queue.
    pub fn push(&self, request: &CompletionRequest, outcome: ScriptOutcome) {
        self.insert(RequestFingerprint::of(request), outcome);
    }

    /// Append entries in iterator order, preserving FIFO order within each key.
    pub fn extend(&self, entries: impl IntoIterator<Item = (RequestFingerprint, ScriptOutcome)>) {
        let mut state = Self::lock(&self.state);
        for (key, outcome) in entries {
            state.queues.entry(key).or_default().push_back(outcome);
        }
    }

    /// One entry per non-empty queue, sorted by fingerprint hex.
    #[must_use]
    pub fn remaining_keys(&self) -> Vec<RequestFingerprint> {
        let state = Self::lock(&self.state);
        let sorted: BTreeMap<_, _> = state
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(k, _)| (k.as_hex().to_owned(), k.clone()))
            .collect();
        sorted.into_values().collect()
    }

    /// Total number of unconsumed outcomes across all queues.
    #[must_use]
    pub fn remaining_outcomes(&self) -> usize {
        Self::lock(&self.state)
            .queues
            .values()
            .map(VecDeque::len)
            .sum()
    }

    /// Invocations in call order: `(endpoint, request, fingerprint)`.
    #[must_use]
    pub fn recorded(&self) -> Vec<ScriptedInvocation> {
        Self::lock(&self.state).invocations.clone()
    }

    /// True when no outcomes remain.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.remaining_outcomes() == 0
    }

    /// Borrow the bound endpoint.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn bound_endpoint(&self) -> &ModelEndpoint {
        &self.endpoint
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!("scripted provider mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    async fn complete(
        &self,
        endpoint: &ModelEndpoint,
        request: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let _span = tracing::info_span!(
            "alloy_eval.scripted_complete",
            fingerprint = tracing::field::Empty,
            hit = tracing::field::Empty
        )
        .entered();

        if endpoint.id != self.endpoint.id {
            return Err(ProviderError::Internal("scripted wrong endpoint".into()));
        }

        let fp = RequestFingerprint::of(&request);
        _span.record("fingerprint", fp.as_hex());

        let mut state = Self::lock(&self.state);
        state.invocations.push(ScriptedInvocation {
            endpoint: self.endpoint.clone(),
            request,
            fingerprint: fp.clone(),
        });

        let outcome = match state.queues.get_mut(&fp) {
            Some(queue) => {
                let outcome = queue.pop_front();
                if queue.is_empty() {
                    state.queues.remove(&fp);
                }
                outcome
            }
            None => None,
        };

        match outcome {
            Some(ScriptOutcome::Response(response)) => {
                _span.record("hit", true);
                Ok(response)
            }
            Some(ScriptOutcome::Error(error)) => {
                _span.record("hit", true);
                Err(ProviderError::from(error))
            }
            None => {
                _span.record("hit", false);
                Err(ProviderError::Internal(format!(
                    "scripted miss: {}",
                    fp.as_hex()
                )))
            }
        }
    }

    async fn health(&self) -> Health {
        Health::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{ResponseFormat, ToolChoice, Usage};
    use std::sync::Arc;

    fn endpoint(provider: ProviderId) -> ModelEndpoint {
        ModelEndpoint {
            id: alloy_runtime::EndpointId::new("eval-script").unwrap(),
            provider,
            display_name: "eval-script".into(),
            model: "scripted".into(),
            tiers: vec![alloy_runtime::ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            max_context: 8192,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
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

    fn response(text: &str) -> ModelResponse {
        ModelResponse {
            text: Some(text.into()),
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
            },
            provider_request_id: None,
            finish_reason: None,
        }
    }

    #[tokio::test]
    async fn scripted_provider_implements_trait() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = Arc::new(ScriptedProvider::new(id.clone(), endpoint(id)).unwrap());
        let object: Arc<dyn ModelProvider> = provider.clone();
        assert_eq!(object.health().await, Health::Healthy);
    }

    #[tokio::test]
    async fn scripted_keyed_hit_miss() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
        let req = request();
        provider.push(&req, ScriptOutcome::Response(response("ok")));
        assert_eq!(
            provider
                .complete(&endpoint(id.clone()), req.clone())
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("ok")
        );
        assert!(provider.is_exhausted());
        let err = provider.complete(&endpoint(id), req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Internal(msg) if msg.starts_with("scripted miss:")));
        assert_eq!(provider.recorded().len(), 2);
    }

    #[tokio::test]
    async fn scripted_per_key_fifo_retries() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
        let req = request();
        let fp = RequestFingerprint::of(&req);
        provider.insert(fp.clone(), ScriptOutcome::Response(response("first")));
        provider.insert(fp, ScriptOutcome::Response(response("second")));
        assert_eq!(
            provider
                .complete(&endpoint(id.clone()), req.clone())
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            provider
                .complete(&endpoint(id), req)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("second")
        );
    }

    #[tokio::test]
    async fn scripted_extend_preserves_per_key_order() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
        let mut req_a = request();
        req_a.max_output_tokens = Some(1);
        let mut req_b = request();
        req_b.max_output_tokens = Some(2);
        let fp_a = RequestFingerprint::of(&req_a);
        let fp_b = RequestFingerprint::of(&req_b);
        provider.extend([
            (fp_a.clone(), ScriptOutcome::Response(response("a1"))),
            (fp_b.clone(), ScriptOutcome::Response(response("b1"))),
            (fp_a, ScriptOutcome::Response(response("a2"))),
            (fp_b, ScriptOutcome::Response(response("b2"))),
        ]);
        assert_eq!(
            provider
                .complete(&endpoint(id.clone()), req_b.clone())
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("b1")
        );
        assert_eq!(
            provider
                .complete(&endpoint(id.clone()), req_a.clone())
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("a1")
        );
        assert_eq!(
            provider
                .complete(&endpoint(id.clone()), req_a)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("a2")
        );
        assert_eq!(
            provider
                .complete(&endpoint(id), req_b)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("b2")
        );
    }

    #[tokio::test]
    async fn scripted_wrong_endpoint_rejected() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
        let req = request();
        provider.push(&req, ScriptOutcome::Response(response("ok")));
        let mut other = endpoint(id.clone());
        other.id = alloy_runtime::EndpointId::new("other").unwrap();
        let err = provider.complete(&other, req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Internal(msg) if msg == "scripted wrong endpoint"));
        assert!(provider.recorded().is_empty());
        assert_eq!(provider.remaining_outcomes(), 1);
    }

    #[test]
    fn scripted_constructor_provider_match() {
        let id = ProviderId::new("eval-script").unwrap();
        assert!(ScriptedProvider::new(id.clone(), endpoint(id)).is_ok());
        let other = ProviderId::new("other").unwrap();
        let mut ep = endpoint(other);
        ep.provider = ProviderId::new("mismatch").unwrap();
        let err = ScriptedProvider::new(ProviderId::new("eval-script").unwrap(), ep).unwrap_err();
        assert!(matches!(
            err,
            EvalError::Manifest(msg) if msg == "scripted provider id must match endpoint.provider"
        ));
    }

    #[tokio::test]
    async fn scripted_same_endpoint_hit() {
        let id = ProviderId::new("eval-script").unwrap();
        let ep = endpoint(id.clone());
        let provider = ScriptedProvider::new(id, ep.clone()).unwrap();
        let req = request();
        provider.push(&req, ScriptOutcome::Response(response("ok")));
        let _ = provider.complete(&ep, req.clone()).await.unwrap();
        let recorded = provider.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].endpoint, ep);
        assert_eq!(recorded[0].request, req);
        assert_eq!(recorded[0].fingerprint, RequestFingerprint::of(&req));
    }

    #[test]
    fn script_turn_outcome_conversion() {
        let outcome = ScriptTurnOutcome::Response {
            text: Some("t".into()),
            structured: None,
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
            },
            provider_request_id: None,
            finish_reason: None,
        };
        match ScriptOutcome::from(outcome) {
            ScriptOutcome::Response(r) => {
                assert!(r.tool_calls.is_empty());
                assert_eq!(r.text.as_deref(), Some("t"));
            }
            ScriptOutcome::Error(_) => panic!("expected response"),
        }
        let err = ScriptTurnOutcome::Error {
            error: ScriptedProviderError::RateLimit,
        };
        match ScriptOutcome::from(err) {
            ScriptOutcome::Error(ScriptedProviderError::RateLimit) => {}
            _ => panic!("expected rate limit"),
        }
    }

    #[tokio::test]
    async fn scripted_health_healthy() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id)).unwrap();
        assert_eq!(provider.health().await, Health::Healthy);
    }

    #[tokio::test]
    async fn scripted_no_http() {
        let id = ProviderId::new("eval-script").unwrap();
        let provider = ScriptedProvider::new(id.clone(), endpoint(id.clone())).unwrap();
        let req = request();
        provider.push(&req, ScriptOutcome::Response(response("offline")));
        let out = provider.complete(&endpoint(id), req).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("offline"));
    }
}
