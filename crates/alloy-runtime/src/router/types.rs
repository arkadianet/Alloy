//! Router request, response, endpoint, and sealed-handle types.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::obs::{redact_secrets, truncate_utf8_bytes};
use crate::types::budget::{BudgetSnapshot, ModelTier};
use crate::types::ids::{
    CapabilityId, Digest, EndpointId, EventSeq, NodeId, ProviderId, RunId, SessionId,
};

/// Serde-stable complexity hint. MVP routing ignores this value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ComplexityScore(pub f32);

/// One operator-configured model endpoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelEndpoint {
    /// Endpoint catalog identifier.
    pub id: EndpointId,
    /// Provider that owns this endpoint.
    pub provider: ProviderId,
    /// Human-readable label; never used as the wire model identifier.
    pub display_name: String,
    /// Operator-configured wire model identifier.
    pub model: String,
    /// Tiers for which this endpoint is eligible.
    pub tiers: Vec<ModelTier>,
    /// Whether the endpoint can support tool-enabled work.
    pub supports_tools: bool,
    /// Whether the endpoint can request JSON-object output.
    pub supports_structured_output: bool,
    /// Advisory context-window size.
    pub max_context: u32,
    /// Operator price per one million input tokens.
    pub input_usd_per_mtok: Option<f64>,
    /// Operator price per one million output tokens.
    pub output_usd_per_mtok: Option<f64>,
}

/// Provider health state. RFC-0007 providers always report [`Health::Healthy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Provider is available.
    Healthy,
    /// Provider is partially degraded; reserved for future routing.
    Degraded,
    /// Provider is unavailable; reserved for future routing.
    Unhealthy,
}

/// Role of a message in a chat prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    /// System instruction.
    System,
    /// User input.
    User,
    /// Assistant output.
    Assistant,
    /// Tool output.
    Tool,
}

/// One role-labelled chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role.
    pub role: ChatRole,
    /// UTF-8 message content.
    pub content: String,
}

/// Opaque source attribution attached to a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// Source label, such as a path or artifact identifier.
    pub source: String,
    /// Optional digest of the cited source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

/// Minimal prompt IR accepted by [`super::ModelRouter::complete`].
///
/// This is distinct from the `ArtifactKind::PromptPack` storage classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptPack {
    /// Ordered chat messages sent to the provider.
    pub messages: Vec<ChatMessage>,
    /// Source attributions used for hashing and future prompt assembly.
    #[serde(default)]
    pub citations: Vec<Citation>,
    /// Reserved domain metadata for RFC-0012; ignored by RFC-0007.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<serde_json::Value>,
}

/// Tool-selection policy reserved for future tool schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Do not request tool calls.
    #[default]
    None,
    /// Permit provider-selected tool calls.
    Auto,
}

/// Requested response representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text response.
    #[default]
    Text,
    /// Request a JSON object while preserving the original text.
    JsonObject,
}

/// Provider-neutral completion request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Ordered chat messages.
    pub messages: Vec<ChatMessage>,
    /// Reserved tool schemas; empty in RFC-0007.
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
    /// Tool-selection policy.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Requested response representation.
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Optional output-token ceiling.
    pub max_output_tokens: Option<u32>,
}

/// Token usage returned by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Known input-token count, or `None` when omitted or malformed.
    pub input_tokens: Option<u64>,
    /// Known output-token count, or `None` when omitted or malformed.
    pub output_tokens: Option<u64>,
}

/// Provider-neutral completion response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// Provider response text, when present.
    pub text: Option<String>,
    /// Parsed JSON object for structured requests, when valid.
    pub structured: Option<serde_json::Value>,
    /// Reserved provider tool calls; always empty in RFC-0007.
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
    /// Honest provider usage; missing values remain `None`.
    pub usage: Usage,
    /// Redacted, bounded provider request identifier.
    pub provider_request_id: Option<String>,
    /// Redacted, bounded provider finish reason.
    pub finish_reason: Option<String>,
}

/// Input to model endpoint routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingRequest {
    /// Session receiving route and model-call attribution.
    pub session: SessionId,
    /// Optional run attribution.
    pub run: Option<RunId>,
    /// Optional DAG node attribution.
    pub node: Option<NodeId>,
    /// Capability used to resolve a model tier.
    pub capability: CapabilityId,
    /// Ignored complexity hint retained for wire compatibility.
    pub complexity: Option<ComplexityScore>,
    /// Spent budget counters used only by the unmetered test fallback.
    pub budget_remaining: BudgetSnapshot,
    /// Require an endpoint that supports tools.
    pub requires_tools: bool,
    /// Require an endpoint that supports structured output.
    pub requires_structured_output: bool,
}

#[derive(Clone, Debug)]
struct CompleteTicket {
    used: Arc<AtomicBool>,
}

impl CompleteTicket {
    fn new() -> Self {
        Self {
            used: Arc::new(AtomicBool::new(false)),
        }
    }

    fn try_consume(&self) -> bool {
        self.used
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// Sealed endpoint selection issued by a model router.
///
/// Clones share one completion ticket: exactly one clone may be completed.
#[derive(Debug)]
pub struct RoutedModel {
    endpoint: ModelEndpoint,
    tier: ModelTier,
    session: SessionId,
    run: Option<RunId>,
    node: Option<NodeId>,
    capability: CapabilityId,
    capability_mapped: bool,
    requires_structured_output: bool,
    route_event_seq: Option<EventSeq>,
    router_instance_id: u64,
    complete_ticket: CompleteTicket,
}

impl RoutedModel {
    /// Selected endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &ModelEndpoint {
        &self.endpoint
    }

    /// Resolved model tier.
    #[must_use]
    pub fn tier(&self) -> ModelTier {
        self.tier
    }

    /// Attributed session.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Attributed run, if any.
    #[must_use]
    pub fn run(&self) -> Option<RunId> {
        self.run
    }

    /// Attributed DAG node, if any.
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// Whether completion must request a JSON object.
    #[must_use]
    pub fn requires_structured_output(&self) -> bool {
        self.requires_structured_output
    }

    /// Sequence of the successfully recorded route decision, if any.
    #[must_use]
    pub fn route_event_seq(&self) -> Option<EventSeq> {
        self.route_event_seq
    }

    pub(crate) fn mint(
        endpoint: ModelEndpoint,
        tier: ModelTier,
        req: &RoutingRequest,
        capability_mapped: bool,
        route_event_seq: Option<EventSeq>,
        router_instance_id: u64,
    ) -> Self {
        Self {
            endpoint,
            tier,
            session: req.session,
            run: req.run,
            node: req.node,
            capability: req.capability.clone(),
            capability_mapped,
            requires_structured_output: req.requires_structured_output,
            route_event_seq,
            router_instance_id,
            complete_ticket: CompleteTicket::new(),
        }
    }

    pub(crate) fn router_instance_id(&self) -> u64 {
        self.router_instance_id
    }

    pub(crate) fn try_consume(&self) -> bool {
        self.complete_ticket.try_consume()
    }

    pub(crate) fn capability(&self) -> &CapabilityId {
        &self.capability
    }

    pub(crate) fn capability_mapped(&self) -> bool {
        self.capability_mapped
    }
}

impl Clone for RoutedModel {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            tier: self.tier,
            session: self.session,
            run: self.run,
            node: self.node,
            capability: self.capability.clone(),
            capability_mapped: self.capability_mapped,
            requires_structured_output: self.requires_structured_output,
            route_event_seq: self.route_event_seq,
            router_instance_id: self.router_instance_id,
            complete_ticket: self.complete_ticket.clone(),
        }
    }
}

impl Serialize for RoutedModel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RoutedModel", 7)?;
        state.serialize_field("endpoint", &self.endpoint)?;
        state.serialize_field("tier", &self.tier)?;
        state.serialize_field("session", &self.session)?;
        state.serialize_field("run", &self.run)?;
        state.serialize_field("node", &self.node)?;
        state.serialize_field(
            "requires_structured_output",
            &self.requires_structured_output,
        )?;
        state.serialize_field("route_event_seq", &self.route_event_seq)?;
        state.end()
    }
}

pub(crate) fn redact_and_truncate(value: &str, max_bytes: usize) -> String {
    truncate_utf8_bytes(&redact_secrets(value), max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("endpoint").unwrap(),
            provider: ProviderId::new("provider").unwrap(),
            display_name: "Endpoint".into(),
            model: "configured-model".into(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: true,
            max_context: 1,
            input_usd_per_mtok: Some(0.0),
            output_usd_per_mtok: Some(0.0),
        }
    }

    fn request() -> RoutingRequest {
        RoutingRequest {
            session: SessionId::new(),
            run: None,
            node: None,
            capability: CapabilityId::new("repair").unwrap(),
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

    #[test]
    fn cloned_routed_model_shares_ticket_and_hides_seals() {
        let routed = RoutedModel::mint(endpoint(), ModelTier::Standard, &request(), true, None, 42);
        let clone = routed.clone();
        assert!(routed.try_consume());
        assert!(!clone.try_consume());

        let json = serde_json::to_value(routed).unwrap();
        assert!(json.get("router_instance_id").is_none());
        assert!(json.get("complete_ticket").is_none());
    }

    #[test]
    fn truncation_preserves_utf8_boundaries_and_redacts_first() {
        assert_eq!(crate::obs::truncate_utf8_bytes("éé", 3), "é");
        let value = redact_and_truncate("api_key=sk-abcdefghij", 32);
        assert!(!value.contains("sk-"));
    }

    #[test]
    fn prompt_pack_round_trips() {
        let prompt = PromptPack {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
            }],
            citations: vec![],
            domains: None,
        };
        let json = serde_json::to_string(&prompt).unwrap();
        assert_eq!(serde_json::from_str::<PromptPack>(&json).unwrap(), prompt);
    }
}
