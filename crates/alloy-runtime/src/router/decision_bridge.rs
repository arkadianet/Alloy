//! Construction of route and budget decision metadata.

use crate::obs::{BudgetCheck, CostSnapshot, DecisionKind, DecisionRecord};
use crate::types::budget::{BudgetSnapshot, ModelTier};
use crate::types::ids::{NodeId, ProviderId, RunId, SessionId};

use super::select::TierSource;
use super::types::{ModelEndpoint, RoutedModel, RoutingRequest};

pub(crate) struct BudgetCounters {
    pub(crate) tokens_in: u64,
    pub(crate) tokens_out: u64,
    pub(crate) usd_spent: Option<f64>,
}

impl From<&CostSnapshot> for BudgetCounters {
    fn from(snapshot: &CostSnapshot) -> Self {
        Self {
            tokens_in: snapshot.tokens_in,
            tokens_out: snapshot.tokens_out,
            usd_spent: snapshot.usd_spent,
        }
    }
}

impl From<&BudgetSnapshot> for BudgetCounters {
    fn from(snapshot: &BudgetSnapshot) -> Self {
        Self {
            tokens_in: snapshot.tokens_in,
            tokens_out: snapshot.tokens_out,
            usd_spent: Some(snapshot.usd_spent),
        }
    }
}

/// Packed inputs for a `DecisionKind::Budget` record.
struct BudgetDecisionInput<'a> {
    session: SessionId,
    run: Option<RunId>,
    node: Option<NodeId>,
    capability: &'a str,
    tier: ModelTier,
    source: TierSource,
    check: BudgetCheck,
    counters: BudgetCounters,
    budget_source: &'static str,
    /// Gauge sampled at the decision site; metadata key remains `in_flight_at_route`
    /// even when this value is taken at complete-time recheck (§5.10).
    in_flight: usize,
}

pub(crate) fn route_decision(
    request: &RoutingRequest,
    tier: ModelTier,
    source: TierSource,
    provider_id: &ProviderId,
    endpoint: Option<&ModelEndpoint>,
    in_flight: usize,
) -> DecisionRecord {
    // `capability` keeps the caller's spelling; tier lookup uses ASCII-lowercased keys.
    let mut metadata = serde_json::json!({
        "capability": request.capability.as_str(),
        "capability_mapped": source == TierSource::CapabilityMap,
        "tier": tier_name(tier),
        "tier_source": source.as_str(),
        "provider_id": provider_id.as_str(),
        "requires_tools": request.requires_tools,
        "requires_structured_output": request.requires_structured_output,
        "in_flight_at_route": in_flight,
    });
    let object = metadata
        .as_object_mut()
        .expect("route metadata is constructed as an object");
    if let Some(endpoint) = endpoint {
        object.insert(
            "endpoint_id".into(),
            serde_json::json!(endpoint.id.as_str()),
        );
        object.insert("model".into(), serde_json::json!(endpoint.model));
    } else {
        object.insert("error".into(), serde_json::json!("no_endpoint"));
    }
    DecisionRecord {
        session: request.session,
        run: request.run,
        node: request.node,
        kind: DecisionKind::ModelRoute,
        metadata,
        content_hash: None,
        prompt_body: None,
    }
}

pub(crate) fn budget_decision_for_route(
    request: &RoutingRequest,
    tier: ModelTier,
    source: TierSource,
    check: BudgetCheck,
    counters: BudgetCounters,
    budget_source: &'static str,
    in_flight: usize,
) -> DecisionRecord {
    budget_decision(BudgetDecisionInput {
        session: request.session,
        run: request.run,
        node: request.node,
        capability: request.capability.as_str(),
        tier,
        source,
        check,
        counters,
        budget_source,
        in_flight,
    })
}

/// Budget denial recorded at complete-time recheck.
///
/// `in_flight` is the complete-time gauge; the metadata key remains
/// `in_flight_at_route` per §5.10 (stable wire name across route and complete).
pub(crate) fn budget_decision_for_complete(
    routed: &RoutedModel,
    check: BudgetCheck,
    counters: BudgetCounters,
    in_flight: usize,
) -> DecisionRecord {
    budget_decision(BudgetDecisionInput {
        session: routed.session(),
        run: routed.run(),
        node: routed.node(),
        capability: routed.capability().as_str(),
        tier: routed.tier(),
        source: if routed.capability_mapped() {
            TierSource::CapabilityMap
        } else {
            TierSource::Default
        },
        check,
        counters,
        budget_source: "meter",
        in_flight,
    })
}

fn budget_decision(input: BudgetDecisionInput<'_>) -> DecisionRecord {
    let metadata = serde_json::json!({
        "capability": input.capability,
        "capability_mapped": input.source == TierSource::CapabilityMap,
        "tier": tier_name(input.tier),
        "budget_check": budget_check_name(input.check),
        "tokens_in": input.counters.tokens_in,
        "tokens_out": input.counters.tokens_out,
        "usd_spent": input.counters.usd_spent,
        "budget_source": input.budget_source,
        "in_flight_at_route": input.in_flight,
    });
    DecisionRecord {
        session: input.session,
        run: input.run,
        node: input.node,
        kind: DecisionKind::Budget,
        metadata,
        content_hash: None,
        prompt_body: None,
    }
}

pub(crate) const fn budget_check_name(check: BudgetCheck) -> &'static str {
    match check {
        BudgetCheck::Ok => "ok",
        BudgetCheck::TokensExhausted => "tokens_exhausted",
        BudgetCheck::UsdExhausted => "usd_exhausted",
        BudgetCheck::TokensAndUsdExhausted => "tokens_and_usd_exhausted",
    }
}

pub(crate) const fn tier_name(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Premium => "premium",
        ModelTier::Standard => "standard",
        ModelTier::Economy => "economy",
        ModelTier::Local => "local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::BudgetSnapshot;
    use crate::types::ids::{CapabilityId, SessionId};

    #[test]
    fn budget_names_are_stable_and_metadata_is_explicit() {
        assert_eq!(
            budget_check_name(BudgetCheck::TokensAndUsdExhausted),
            "tokens_and_usd_exhausted"
        );
        let request = RoutingRequest {
            session: SessionId::new(),
            run: None,
            node: None,
            capability: CapabilityId::new("repair").unwrap(),
            complexity: None,
            budget_remaining: BudgetSnapshot {
                usd_spent: 1.0,
                tokens_in: 2,
                tokens_out: 3,
            },
            requires_tools: false,
            requires_structured_output: false,
        };
        let record = budget_decision_for_route(
            &request,
            ModelTier::Standard,
            TierSource::Default,
            BudgetCheck::UsdExhausted,
            BudgetCounters {
                tokens_in: 2,
                tokens_out: 3,
                usd_spent: Some(1.0),
            },
            "snapshot",
            1,
        );
        assert_eq!(record.kind, DecisionKind::Budget);
        assert_eq!(record.metadata["capability_mapped"], false);
        assert_eq!(record.metadata["budget_check"], "usd_exhausted");
    }
}
