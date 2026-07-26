//! Provider and router abstraction traits.

use async_trait::async_trait;

use crate::types::ids::ProviderId;

use super::error::{ProviderError, RouterError};
use super::types::{
    CompletionRequest, Health, ModelEndpoint, ModelResponse, PromptPack, RoutedModel,
    RoutingRequest,
};

/// A provider capable of executing one non-streaming model completion.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider catalog identifier.
    fn id(&self) -> ProviderId;

    /// Execute exactly one completion against `endpoint`.
    async fn complete(
        &self,
        endpoint: &ModelEndpoint,
        req: CompletionRequest,
    ) -> Result<ModelResponse, ProviderError>;

    /// Return provider health. RFC-0007 implementations always return healthy.
    async fn health(&self) -> Health;
}

/// Selects sealed model endpoints and completes prompts through them.
#[async_trait]
pub trait ModelRouter: Send + Sync {
    /// Select an endpoint after lifecycle, capability, and budget checks.
    async fn route(&self, req: RoutingRequest) -> Result<RoutedModel, RouterError>;

    /// Complete a prompt once through a handle issued by this router.
    async fn complete(
        &self,
        routed: &RoutedModel,
        prompt: PromptPack,
    ) -> Result<ModelResponse, RouterError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::RecordingModelProvider;
    use std::sync::Arc;

    #[tokio::test]
    async fn provider_trait_is_object_safe_and_shareable() {
        let provider: Arc<dyn ModelProvider> = Arc::new(RecordingModelProvider::new(
            ProviderId::new("provider").unwrap(),
        ));
        assert_eq!(provider.health().await, Health::Healthy);
    }
}
