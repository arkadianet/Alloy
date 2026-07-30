//! Deterministic in-memory provider for tests and offline composition.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::types::ids::ProviderId;

use super::error::ProviderError;
use super::traits::ModelProvider;
use super::types::{CompletionRequest, Health, ModelEndpoint, ModelResponse};

/// FIFO scripted outcomes plus recorded invocations. Performs no network I/O.
pub struct RecordingModelProvider {
    id: ProviderId,
    state: Mutex<RecordingState>,
}

struct RecordingState {
    outcomes: VecDeque<Result<ModelResponse, ProviderError>>,
    invocations: Vec<(ModelEndpoint, CompletionRequest)>,
}

impl RecordingModelProvider {
    /// Create an empty recording provider.
    #[must_use]
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            state: Mutex::new(RecordingState {
                outcomes: VecDeque::new(),
                invocations: Vec::new(),
            }),
        }
    }

    /// Append one scripted outcome to the FIFO.
    pub fn push(&self, outcome: Result<ModelResponse, ProviderError>) {
        Self::lock(&self.state).outcomes.push_back(outcome);
    }

    /// Return a snapshot of invocations in call order.
    #[must_use]
    pub fn recorded(&self) -> Vec<(ModelEndpoint, CompletionRequest)> {
        Self::lock(&self.state).invocations.clone()
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!("recording model provider mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }
}

#[async_trait]
impl ModelProvider for RecordingModelProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    async fn complete(
        &self,
        endpoint: &ModelEndpoint,
        request: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let mut state = Self::lock(&self.state);
        state.invocations.push((endpoint.clone(), request));
        state
            .outcomes
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Internal("recording exhausted".into())))
    }

    async fn health(&self) -> Health {
        Health::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{ResponseFormat, ToolChoice, Usage};
    use crate::types::budget::ModelTier;
    use crate::types::ids::EndpointId;
    use std::sync::Arc;

    fn endpoint(provider: ProviderId) -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("endpoint").unwrap(),
            provider,
            display_name: "Endpoint".into(),
            model: "configured".into(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            supports_json_schema: false,
            json_schema_strict: false,
            max_context: 1,
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
    async fn records_and_returns_fifo_outcomes() {
        let id = ProviderId::new("provider").unwrap();
        let provider = Arc::new(RecordingModelProvider::new(id.clone()));
        let object: Arc<dyn ModelProvider> = provider.clone();
        provider.push(Ok(ModelResponse {
            text: Some("first".into()),
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
            },
            provider_request_id: None,
            finish_reason: None,
        }));
        provider.push(Err(ProviderError::RateLimit));

        assert_eq!(
            object
                .complete(&endpoint(id.clone()), request())
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("first")
        );
        assert!(matches!(
            object.complete(&endpoint(id), request()).await,
            Err(ProviderError::RateLimit)
        ));
        assert_eq!(provider.recorded().len(), 2);
        assert_eq!(provider.health().await, Health::Healthy);
    }
}
