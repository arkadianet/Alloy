//! Budget discipline: effective budget and weighted allowances
//! (RFC-0012 §6).

use super::profile::ContextProfile;
use super::types::{AssembleInputs, DomainId};

/// `effective = min(req.token_budget, profile.total_token_budget,
/// inputs.budget.max_input)`. Absent inputs are skipped, never defaulted
/// upward (rule B1).
#[must_use]
pub(super) fn effective_budget(
    request_budget: usize,
    profile: &ContextProfile,
    inputs: &AssembleInputs,
) -> usize {
    let mut effective = request_budget.min(profile.total_token_budget);
    if let Some(budget) = &inputs.budget {
        effective = effective.min(usize::try_from(budget.max_input).unwrap_or(usize::MAX));
    }
    effective
}

/// Weighted allowances over `remainder = effective - reserve -
/// must_include_est` (rule B4). Integer floor, computed in `DomainId::LIVE`
/// order; floor loss is never redistributed (§6.3).
#[must_use]
pub(super) fn allowances(profile: &ContextProfile, remainder: usize) -> [(DomainId, usize); 3] {
    let live_sum = f64::from(profile.weights.live_sum());
    let mut out = [(DomainId::Conversation, 0); 3];
    for (slot, domain) in out.iter_mut().zip(DomainId::LIVE) {
        let weight = f64::from(profile.weights.weight_of(domain));
        // Guarded by DomainWeights::validate (D2): live_sum > 0.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let allowance = ((remainder as f64) * weight / live_sum).floor() as usize;
        *slot = (domain, allowance);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::TokenBudget;

    // T2d — B1.
    #[test]
    fn effective_budget_is_min_of_three_sources() {
        let profile = ContextProfile::v2_defaults();
        let mut inputs = AssembleInputs::default();
        assert_eq!(effective_budget(40_000, &profile, &inputs), 32_000);
        assert_eq!(effective_budget(10_000, &profile, &inputs), 10_000);
        inputs.budget = Some(TokenBudget {
            max_input: 8_000,
            max_output: 1_000,
        });
        assert_eq!(effective_budget(10_000, &profile, &inputs), 8_000);
        inputs.budget = Some(TokenBudget {
            max_input: 0,
            max_output: 0,
        });
        assert_eq!(effective_budget(10_000, &profile, &inputs), 0);
    }

    // T2g — B4: the §6.3 worked table, exact integers.
    #[test]
    fn allowances_match_the_section_six_three_table() {
        let profile = ContextProfile::v2_defaults();
        // effective 32_000 − reserve 512 − must_include 1_200 = 30_288.
        let got = allowances(&profile, 30_288);
        assert_eq!(got[0], (DomainId::Conversation, 6_057));
        assert_eq!(got[1], (DomainId::WorkingSet, 16_658));
        assert_eq!(got[2], (DomainId::Artifacts, 7_572));
        let sum: usize = got.iter().map(|(_, a)| a).sum();
        assert_eq!(sum, 30_287, "floor loss <= 3 and never redistributed");
    }
}
