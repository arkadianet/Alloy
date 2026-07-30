//! Pure operator-price-table accounting.

use super::types::{ModelEndpoint, Usage};

pub(crate) fn derive_usd(endpoint: &ModelEndpoint, usage: &Usage) -> Option<f64> {
    let (input_tokens, output_tokens) = (usage.input_tokens?, usage.output_tokens?);
    let (input_price, output_price) = (endpoint.input_usd_per_mtok?, endpoint.output_usd_per_mtok?);
    if !input_price.is_finite()
        || !output_price.is_finite()
        || input_price < 0.0
        || output_price < 0.0
    {
        return None;
    }
    let usd = (input_tokens as f64 / 1_000_000.0) * input_price
        + (output_tokens as f64 / 1_000_000.0) * output_price;
    (usd.is_finite() && usd >= 0.0).then_some(usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::ModelTier;
    use crate::types::ids::{EndpointId, ProviderId};

    fn endpoint() -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("endpoint").unwrap(),
            provider: ProviderId::new("provider").unwrap(),
            display_name: "Endpoint".into(),
            model: "configured".into(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            supports_json_schema: false,
            json_schema_strict: false,
            max_context: 1,
            input_usd_per_mtok: Some(2.0),
            output_usd_per_mtok: Some(4.0),
            temperature: None,
        }
    }

    #[test]
    fn derives_only_from_complete_known_inputs() {
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
        };
        assert_eq!(derive_usd(&endpoint(), &usage), Some(4.0));

        let mut missing = endpoint();
        missing.input_usd_per_mtok = None;
        assert_eq!(derive_usd(&missing, &usage), None);
        assert_eq!(
            derive_usd(
                &endpoint(),
                &Usage {
                    input_tokens: None,
                    output_tokens: Some(1),
                }
            ),
            None
        );
    }
}
