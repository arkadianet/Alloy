//! Model routing and OpenAI-compatible provider support (RFC-0007).
//!
//! The router selects operator-configured endpoints, enforces run budgets, and
//! records every completed provider attempt. It deliberately does not retry,
//! fail over, stream, or score endpoints; those behaviours belong to later
//! RFCs.

mod config;
mod decision_bridge;
mod error;
#[cfg(feature = "http-provider")]
mod http_client;
mod meter_bridge;
mod metrics;
#[cfg(feature = "http-provider")]
mod openai;
mod price;
mod recording;
mod secret;
mod select;
mod toml_router;
mod traits;
mod types;

pub use config::{
    EndpointConfig, ProviderConfig, ProviderKind, RouterConfig, RouterPolicy, ScoringWeights,
};
pub use error::{
    classify_provider_error, classify_router_error, ClassifiedRouterFailure, ProviderError,
    RouterError,
};
pub use metrics::RouterMetricsSnapshot;
#[cfg(feature = "http-provider")]
pub use openai::{OpenAiCompatibleProvider, OpenAiCompatibleSpec};
pub use recording::RecordingModelProvider;
pub use secret::SecretString;
pub use toml_router::{RouterShutdownReport, TomlModelRouter, TomlModelRouterParts};
pub use traits::{ModelProvider, ModelRouter};
pub use types::{
    ChatMessage, ChatRole, Citation, CompletionRequest, ComplexityScore, Health, JsonSchemaSpec,
    ModelEndpoint, ModelResponse, PromptPack, ResponseFormat, RoutedModel, RoutingRequest,
    ToolChoice, Usage,
};
