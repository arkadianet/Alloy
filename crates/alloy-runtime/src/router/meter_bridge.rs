//! Mapping provider outcomes into RFC-0004 model-call records.

use std::time::Duration;

use crate::obs::{hash_prompt, ModelCallRecord, ModelUsdSource, MODEL_PROMPT_BODY_MAX_BYTES};
use crate::types::diagnostic::ErrorClass;
use crate::types::ids::ProviderId;

use super::error::{classify_provider_error, ProviderError};
use super::price::derive_usd;
use super::types::{ModelResponse, PromptPack, RoutedModel};

pub(crate) struct ModelCallBuild {
    pub(crate) record: ModelCallRecord,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) usd: Option<f64>,
    pub(crate) prompt_body_oversize: bool,
    pub(crate) canonical_len: usize,
}

pub(crate) fn build_model_call_record(
    routed: &RoutedModel,
    provider_id: ProviderId,
    prompt: &PromptPack,
    duration: Duration,
    result: &Result<ModelResponse, ProviderError>,
) -> ModelCallBuild {
    let canonical = serde_json::to_string(&prompt.messages).unwrap_or_else(|error| {
        tracing::error!(%error, "canonical prompt serialization unexpectedly failed");
        "[]".to_owned()
    });
    let canonical_len = canonical.len();
    let prompt_body_oversize = canonical_len > MODEL_PROMPT_BODY_MAX_BYTES;
    let content_hash = Some(hash_prompt(&canonical));
    let prompt_body = (!prompt_body_oversize).then_some(canonical);
    let duration_ms = Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));

    let (input_tokens, output_tokens, usd, error_class, finish_reason, request_id) = match result {
        Ok(response) => {
            let usd = derive_usd(routed.endpoint(), &response.usage);
            (
                response.usage.input_tokens,
                response.usage.output_tokens,
                usd,
                None,
                response.finish_reason.clone(),
                response.provider_request_id.clone(),
            )
        }
        Err(error) => (
            None,
            None,
            None,
            Some(classify_provider_error(error).class),
            None,
            None,
        ),
    };

    let mut record = ModelCallRecord::new(routed.session(), provider_id, routed.tier())
        .tokens(input_tokens, output_tokens)
        .usd(usd)
        .duration_ms(duration_ms)
        .confidence(None)
        .error_class(error_class)
        .content_hash(content_hash)
        .prompt_body(prompt_body)
        .endpoint_id(Some(routed.endpoint().id.clone()))
        .model(Some(routed.endpoint().model.clone()))
        .route_event_seq(routed.route_event_seq())
        .usd_source(usd.map(|_| ModelUsdSource::OperatorPriceTable))
        .finish_reason(finish_reason)
        .provider_request_id(request_id);
    if let Some(run) = routed.run() {
        record = record.run(run);
    }
    if let Some(node) = routed.node() {
        record = record.node(node);
    }

    ModelCallBuild {
        record,
        input_tokens,
        output_tokens,
        usd,
        prompt_body_oversize,
        canonical_len,
    }
}

/// Mid-flight host cancel: durable attempt with unknown spend and `Cancelled`.
pub(crate) fn build_cancelled_model_call_record(
    routed: &RoutedModel,
    provider_id: ProviderId,
    prompt: &PromptPack,
    duration: Duration,
) -> ModelCallBuild {
    let canonical = serde_json::to_string(&prompt.messages).unwrap_or_else(|error| {
        tracing::error!(%error, "canonical prompt serialization unexpectedly failed");
        "[]".to_owned()
    });
    let canonical_len = canonical.len();
    let prompt_body_oversize = canonical_len > MODEL_PROMPT_BODY_MAX_BYTES;
    let content_hash = Some(hash_prompt(&canonical));
    let prompt_body = (!prompt_body_oversize).then_some(canonical);
    let duration_ms = Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));

    let mut record = ModelCallRecord::new(routed.session(), provider_id, routed.tier())
        .tokens(None, None)
        .usd(None)
        .duration_ms(duration_ms)
        .confidence(None)
        .error_class(Some(ErrorClass::Cancelled))
        .content_hash(content_hash)
        .prompt_body(prompt_body)
        .endpoint_id(Some(routed.endpoint().id.clone()))
        .model(Some(routed.endpoint().model.clone()))
        .route_event_seq(routed.route_event_seq())
        .usd_source(None)
        .finish_reason(None)
        .provider_request_id(None);
    if let Some(run) = routed.run() {
        record = record.run(run);
    }
    if let Some(node) = routed.node() {
        record = record.node(node);
    }

    ModelCallBuild {
        record,
        input_tokens: None,
        output_tokens: None,
        usd: None,
        prompt_body_oversize,
        canonical_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{ChatMessage, ChatRole, ModelEndpoint, RoutingRequest, Usage};
    use crate::types::budget::{BudgetSnapshot, ModelTier};
    use crate::types::ids::{CapabilityId, EndpointId, SessionId};

    fn routed() -> RoutedModel {
        let request = RoutingRequest {
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
            requires_structured_output: false,
        };
        RoutedModel::mint(
            ModelEndpoint {
                id: EndpointId::new("endpoint").unwrap(),
                provider: ProviderId::new("provider").unwrap(),
                display_name: "Endpoint".into(),
                model: "configured".into(),
                tiers: vec![ModelTier::Standard],
                supports_tools: false,
                supports_structured_output: false,
                max_context: 1,
                input_usd_per_mtok: Some(1.0),
                output_usd_per_mtok: Some(2.0),
            },
            ModelTier::Standard,
            &request,
            true,
            None,
            1,
        )
    }

    #[test]
    fn maps_success_and_oversize_prompt_honestly() {
        let prompt = PromptPack {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "x".repeat(MODEL_PROMPT_BODY_MAX_BYTES + 1),
            }],
            citations: vec![],
            domains: None,
        };
        let result = Ok(ModelResponse {
            text: Some("ok".into()),
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            },
            provider_request_id: Some("request".into()),
            finish_reason: Some("stop".into()),
        });
        let built = build_model_call_record(
            &routed(),
            ProviderId::new("provider").unwrap(),
            &prompt,
            Duration::from_millis(4),
            &result,
        );
        assert!(built.prompt_body_oversize);
        assert!(built.record.prompt_body.is_none());
        assert!(built.record.content_hash.is_some());
        assert!(built.usd.is_some());
    }

    #[test]
    fn maps_provider_error_to_unknown_usage() {
        let prompt = PromptPack {
            messages: vec![],
            citations: vec![],
            domains: None,
        };
        let built = build_model_call_record(
            &routed(),
            ProviderId::new("provider").unwrap(),
            &prompt,
            Duration::ZERO,
            &Err(ProviderError::Timeout),
        );
        assert_eq!(built.input_tokens, None);
        assert_eq!(built.output_tokens, None);
        assert_eq!(built.record.error_class, Some(crate::ErrorClass::Timeout));
    }

    #[test]
    fn maps_cancelled_attempt_to_unknown_usage() {
        let prompt = PromptPack {
            messages: vec![],
            citations: vec![],
            domains: None,
        };
        let built = build_cancelled_model_call_record(
            &routed(),
            ProviderId::new("provider").unwrap(),
            &prompt,
            Duration::from_millis(3),
        );
        assert_eq!(built.input_tokens, None);
        assert_eq!(built.output_tokens, None);
        assert_eq!(built.usd, None);
        assert_eq!(built.record.error_class, Some(crate::ErrorClass::Cancelled));
        assert_eq!(built.record.duration_ms, Some(3));
        assert_eq!(
            built.record.endpoint_id.as_ref().map(|id| id.as_str()),
            Some("endpoint")
        );
        assert_eq!(built.record.model.as_deref(), Some("configured"));
    }
}
