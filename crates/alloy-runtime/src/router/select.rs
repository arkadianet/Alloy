//! Pure tier resolution, endpoint selection, and budget arithmetic.

use crate::obs::BudgetCheck;
use crate::types::budget::{BudgetPolicy, BudgetSnapshot, ModelTier};
use crate::types::ids::CapabilityId;

use super::config::RouterConfig;
use super::types::ModelEndpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TierSource {
    CapabilityMap,
    Default,
    /// The configured tier was raised by `RoutingRequest.tier_override`
    /// (RFC-0010 §5.11.4 escalation).
    Escalation,
}

impl TierSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityMap => "capability_map",
            Self::Default => "default",
            Self::Escalation => "escalation",
        }
    }
}

pub(crate) fn resolve_tier(
    config: &RouterConfig,
    capability: &CapabilityId,
) -> (ModelTier, TierSource) {
    let key = capability.as_str().to_ascii_lowercase();
    match config.capability_tiers.get(&key) {
        Some(tier) => (*tier, TierSource::CapabilityMap),
        None => (config.policy.default_tier, TierSource::Default),
    }
}

/// Capability ordering used **only** to enforce RFC-0007's no-downgrade rule.
///
/// `ModelTier` is deliberately not `Ord`: it is a serde-stable operator-facing
/// label, and a total order on it would invite cost/quality comparisons the
/// RFC does not define. This local rank answers exactly one question — "is the
/// requested tier above the configured one?" — and `Local` sits at the bottom
/// because an offline endpoint is the least-capable target a caller can ask
/// escalation to land on.
const fn capability_rank(tier: ModelTier) -> u8 {
    match tier {
        ModelTier::Local => 0,
        ModelTier::Economy => 1,
        ModelTier::Standard => 2,
        ModelTier::Premium => 3,
    }
}

/// Raise `(tier, source)` to `requested` when — and only when — the request
/// asks for a strictly more capable tier (RFC-0007 §5.2.1, RFC-0010 ES6).
pub(crate) fn apply_tier_override(
    resolved: (ModelTier, TierSource),
    requested: Option<ModelTier>,
) -> (ModelTier, TierSource) {
    let (tier, source) = resolved;
    match requested {
        Some(requested) if capability_rank(requested) > capability_rank(tier) => {
            (requested, TierSource::Escalation)
        }
        _ => (tier, source),
    }
}

pub(crate) fn select_endpoint(
    config: &RouterConfig,
    tier: ModelTier,
    requires_tools: bool,
    requires_structured_output: bool,
) -> Option<ModelEndpoint> {
    let provider = config.providers.first()?;
    provider
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.tiers.contains(&tier)
                && (!requires_tools || endpoint.supports_tools)
                && (!requires_structured_output || endpoint.supports_structured_output)
        })
        .map(|endpoint| endpoint.to_endpoint(provider.id.clone()))
}

/// Apply budget arithmetic to spent counters without mutating a meter.
pub(crate) fn check_budget_snapshot(spent: &BudgetSnapshot, policy: &BudgetPolicy) -> BudgetCheck {
    let tokens_exhausted =
        spent.tokens_in.saturating_add(spent.tokens_out) >= policy.max_tokens_per_run;
    let usd_exhausted = !policy.max_usd_per_run.is_finite()
        || policy.max_usd_per_run < 0.0
        || spent.usd_spent >= policy.max_usd_per_run;
    budget_check(tokens_exhausted, usd_exhausted)
}

pub(crate) fn apply_usd_ceiling_overlay(check: BudgetCheck, policy: &BudgetPolicy) -> BudgetCheck {
    let tokens_exhausted = matches!(
        check,
        BudgetCheck::TokensExhausted | BudgetCheck::TokensAndUsdExhausted
    );
    let usd_exhausted = matches!(
        check,
        BudgetCheck::UsdExhausted | BudgetCheck::TokensAndUsdExhausted
    ) || !policy.max_usd_per_run.is_finite()
        || policy.max_usd_per_run <= 0.0;
    budget_check(tokens_exhausted, usd_exhausted)
}

const fn budget_check(tokens_exhausted: bool, usd_exhausted: bool) -> BudgetCheck {
    match (tokens_exhausted, usd_exhausted) {
        (false, false) => BudgetCheck::Ok,
        (true, false) => BudgetCheck::TokensExhausted,
        (false, true) => BudgetCheck::UsdExhausted,
        (true, true) => BudgetCheck::TokensAndUsdExhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::SharedCostMeter;
    use crate::router::RouterConfig;

    fn config() -> RouterConfig {
        RouterConfig::from_str(
            "test",
            r#"
[policy]
default_tier = "economy"

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[[providers.endpoints]]
id = "first"
display_name = "First"
model = "configured-a"
tiers = ["standard"]
supports_tools = false
supports_structured_output = true
max_context = 1

[[providers.endpoints]]
id = "second"
display_name = "Second"
model = "configured-b"
tiers = ["standard"]
supports_tools = true
supports_structured_output = true
max_context = 1

[capability_tiers]
repair = "standard"
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_normalized_capability_and_default() {
        let config = config();
        assert_eq!(
            resolve_tier(&config, &CapabilityId::new("Repair").unwrap()),
            (ModelTier::Standard, TierSource::CapabilityMap)
        );
        assert_eq!(
            resolve_tier(&config, &CapabilityId::new("unknown").unwrap()),
            (ModelTier::Economy, TierSource::Default)
        );
    }

    #[test]
    fn tier_override_raises_and_never_lowers() {
        let mapped = (ModelTier::Standard, TierSource::CapabilityMap);
        // Absent override is the identity.
        assert_eq!(apply_tier_override(mapped, None), mapped);
        // Strictly more capable → applied, and the source says so.
        assert_eq!(
            apply_tier_override(mapped, Some(ModelTier::Premium)),
            (ModelTier::Premium, TierSource::Escalation)
        );
        // Equal or less capable → inert, source untouched.
        assert_eq!(
            apply_tier_override(mapped, Some(ModelTier::Standard)),
            mapped
        );
        assert_eq!(
            apply_tier_override(mapped, Some(ModelTier::Economy)),
            mapped
        );
        assert_eq!(apply_tier_override(mapped, Some(ModelTier::Local)), mapped);
        // The rule is about capability rank, not about the map: a defaulted
        // tier is raised the same way and keeps reporting how it got there.
        let defaulted = (ModelTier::Local, TierSource::Default);
        assert_eq!(
            apply_tier_override(defaulted, Some(ModelTier::Economy)),
            (ModelTier::Economy, TierSource::Escalation)
        );
    }

    #[test]
    fn first_matching_endpoint_wins_after_filters() {
        let config = config();
        assert_eq!(
            select_endpoint(&config, ModelTier::Standard, false, true)
                .unwrap()
                .id
                .as_str(),
            "first"
        );
        assert_eq!(
            select_endpoint(&config, ModelTier::Standard, true, true)
                .unwrap()
                .id
                .as_str(),
            "second"
        );
    }

    #[test]
    fn snapshot_and_overlay_are_fail_closed() {
        let policy = BudgetPolicy {
            max_tokens_per_run: 10,
            max_usd_per_run: 2.0,
            ..BudgetPolicy::default()
        };
        assert_eq!(
            check_budget_snapshot(
                &BudgetSnapshot {
                    usd_spent: 0.0,
                    tokens_in: 5,
                    tokens_out: 5,
                },
                &policy
            ),
            BudgetCheck::TokensExhausted
        );
        let zero_usd = BudgetPolicy {
            max_usd_per_run: 0.0,
            ..policy
        };
        assert_eq!(
            apply_usd_ceiling_overlay(BudgetCheck::Ok, &zero_usd),
            BudgetCheck::UsdExhausted
        );
    }

    #[test]
    fn check_budget_snapshot_zero_usd_diff() {
        let policy = BudgetPolicy {
            max_usd_per_run: 0.0,
            ..BudgetPolicy::default()
        };
        let meter = SharedCostMeter::new();
        assert_eq!(meter.snapshot().usd_spent, None);
        assert_eq!(meter.check_budget(&policy), BudgetCheck::Ok);
        assert_eq!(
            check_budget_snapshot(
                &BudgetSnapshot {
                    usd_spent: 0.0,
                    tokens_in: 0,
                    tokens_out: 0,
                },
                &policy,
            ),
            BudgetCheck::UsdExhausted
        );
    }
}
