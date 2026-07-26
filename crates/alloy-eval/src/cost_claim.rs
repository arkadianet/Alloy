//! Cost claim envelope and uncalibrated USD derivation.

use alloy_runtime::{ModelEndpoint, Usage};
use serde::{Deserialize, Serialize};

use crate::metrics::MetricField;

/// Grade for a cost claim carried by an eval report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClaimGrade {
    /// Internal operator-price-table estimate only; must not be marketed.
    UncalibratedInternal,
    /// Reserved for post-calibration publishes. Day-1 must not emit this.
    CalibratedHoldout,
}

/// Cost claim envelope for the control or sole eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostClaimEnvelope {
    /// Cost claim grade.
    pub grade: CostClaimGrade,
    /// Present only for calibrated claims, unreachable in Day-1.
    pub marketing_usd_p50: Option<f64>,
    /// Internal p50 computed from complete finite fixture costs.
    pub internal_cost_usd_p50: MetricField<f64>,
    /// Constant disclaimer string.
    #[serde(default = "default_cost_disclaimer")]
    pub disclaimer: String,
}

/// Exact Day-1 uncalibrated-cost disclaimer.
pub const COST_DISCLAIMER: &str =
    "internal operator-price-table estimate only; not a calibrated marketing claim (V2 §18 / ADR F-08)";

fn default_cost_disclaimer() -> String {
    COST_DISCLAIMER.to_string()
}

impl CostClaimEnvelope {
    /// Construct a Day-1 uncalibrated cost envelope.
    #[must_use]
    pub fn uncalibrated(internal_cost_usd_p50: MetricField<f64>) -> Self {
        Self {
            grade: CostClaimGrade::UncalibratedInternal,
            marketing_usd_p50: None,
            internal_cost_usd_p50,
            disclaimer: COST_DISCLAIMER.to_string(),
        }
    }
}

/// Derive uncalibrated USD from endpoint prices and provider usage.
#[must_use]
pub(crate) fn derive_eval_usd(endpoint: &ModelEndpoint, usage: &Usage) -> Option<f64> {
    let input_price = endpoint.input_usd_per_mtok?;
    let output_price = endpoint.output_usd_per_mtok?;
    let input_tokens = usage.input_tokens?;
    let output_tokens = usage.output_tokens?;

    if !input_price.is_finite()
        || !output_price.is_finite()
        || input_price < 0.0
        || output_price < 0.0
    {
        return None;
    }

    let usd = (input_tokens as f64 / 1_000_000.0) * input_price
        + (output_tokens as f64 / 1_000_000.0) * output_price;
    if usd.is_finite() && usd >= 0.0 {
        Some(usd)
    } else {
        tracing::debug!("eval USD derivation produced a non-finite or negative value");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::UnmeasuredReason;
    use alloy_runtime::{EndpointId, ModelTier, ProviderId};

    fn endpoint(input: Option<f64>, output: Option<f64>) -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("eval-script").unwrap(),
            provider: ProviderId::new("eval-script").unwrap(),
            display_name: "eval".to_owned(),
            model: "scripted".to_owned(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            max_context: 8192,
            input_usd_per_mtok: input,
            output_usd_per_mtok: output,
        }
    }

    fn usage(input: Option<u64>, output: Option<u64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
        }
    }

    #[test]
    fn eval_usd_matches_runtime_price_semantics() {
        assert_eq!(
            derive_eval_usd(
                &endpoint(Some(2.0), Some(4.0)),
                &usage(Some(1_000_000), Some(500_000))
            ),
            Some(4.0)
        );
        assert_eq!(
            derive_eval_usd(
                &endpoint(None, Some(4.0)),
                &usage(Some(1_000_000), Some(500_000))
            ),
            None
        );
        assert_eq!(
            derive_eval_usd(&endpoint(Some(2.0), Some(4.0)), &usage(Some(1), None)),
            None
        );
        assert_eq!(
            derive_eval_usd(&endpoint(Some(-1.0), Some(4.0)), &usage(Some(1), Some(1))),
            None
        );
        assert_eq!(
            derive_eval_usd(
                &endpoint(Some(f64::INFINITY), Some(4.0)),
                &usage(Some(1), Some(1))
            ),
            None
        );
    }

    #[test]
    fn cost_disclaimer_default_and_constructor() {
        let envelope = CostClaimEnvelope::uncalibrated(MetricField::Unmeasured {
            reason: UnmeasuredReason::CostInputsIncomplete,
        });
        assert_eq!(envelope.grade, CostClaimGrade::UncalibratedInternal);
        assert_eq!(envelope.marketing_usd_p50, None);
        assert_eq!(envelope.disclaimer, COST_DISCLAIMER);

        let json = r#"{"grade":"uncalibrated_internal","marketing_usd_p50":null,"internal_cost_usd_p50":{"state":"unmeasured","value":{"reason":"cost_inputs_incomplete"}}}"#;
        let decoded: CostClaimEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.disclaimer, COST_DISCLAIMER);
    }
}
