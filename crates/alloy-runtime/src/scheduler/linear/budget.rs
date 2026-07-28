//! Effective budget ceilings (RFC-0010 §5.16.1, BG1-BG6).
//!
//! Meter rebuild (§5.16.2, B7-B10) and enforcement points (§5.16.3, BE1-BE4)
//! stay in `loop_.rs` — they're loop-shaped (R8, L6, A5, post-node), not pure
//! computation. This module owns only the fold from `deps.budget_policy` /
//! `session.budget` / the goal's `MaxUsd` constraints into one effective
//! `BudgetPolicy`.

use crate::types::budget::{BudgetPolicy, Constraint, Goal};

/// One session-row `max_parallel_*` field that wasn't `1` (BG5) — recorded,
/// never enforced (execution stays serial regardless).
pub(super) struct IgnoredParallelism {
    pub(super) field: &'static str,
    pub(super) value: u32,
}

/// Result of folding the three budget sources into one (BG4).
pub(super) struct EffectiveBudget {
    /// Carries the effective ceilings; `max_parallel_* == 1` always (BG4).
    pub(super) policy: BudgetPolicy,
    /// BG1: at least one goal `MaxUsd` constraint was non-finite and ignored.
    pub(super) ignored_max_usd_non_finite: bool,
    /// BG5: session-row parallelism fields that were ignored.
    pub(super) ignored_parallelism: Vec<IgnoredParallelism>,
}

/// §5.16.1: `effective_usd = min(policy, session, min(goal MaxUsd caps))`,
/// `effective_tokens = min(policy, session)`. Non-finite `MaxUsd` values are
/// dropped (BG1); negative finite values clamp to `0.0` before entering the
/// min (BG2). Always forces `max_parallel_* = 1` on the output (BG4),
/// independent of what `session_budget` says (BG5 handles that mismatch as
/// an ignored-and-recorded fact, not a size the caller should trust).
pub(super) fn effective_budget(
    deps_policy: &BudgetPolicy,
    session_budget: &BudgetPolicy,
    goal: &Goal,
) -> EffectiveBudget {
    let mut ignored_max_usd_non_finite = false;
    let mut goal_min_usd = f64::INFINITY;
    for c in &goal.constraints {
        if let Constraint::MaxUsd(v) = c {
            if !v.is_finite() {
                ignored_max_usd_non_finite = true; // BG1
                continue;
            }
            goal_min_usd = goal_min_usd.min(v.max(0.0)); // BG2
        }
    }

    let effective_usd = deps_policy
        .max_usd_per_run
        .min(session_budget.max_usd_per_run)
        .min(goal_min_usd);
    let effective_tokens = deps_policy
        .max_tokens_per_run
        .min(session_budget.max_tokens_per_run);

    let mut ignored_parallelism = Vec::new();
    if session_budget.max_parallel_nodes != 1 {
        ignored_parallelism.push(IgnoredParallelism {
            field: "max_parallel_nodes",
            value: session_budget.max_parallel_nodes,
        });
    }
    if session_budget.max_parallel_cargo != 1 {
        ignored_parallelism.push(IgnoredParallelism {
            field: "max_parallel_cargo",
            value: session_budget.max_parallel_cargo,
        });
    }
    if session_budget.max_parallel_edits != 1 {
        ignored_parallelism.push(IgnoredParallelism {
            field: "max_parallel_edits",
            value: session_budget.max_parallel_edits,
        });
    }

    EffectiveBudget {
        policy: BudgetPolicy {
            max_usd_per_run: effective_usd,
            max_tokens_per_run: effective_tokens,
            max_parallel_nodes: 1,
            max_parallel_cargo: 1,
            max_parallel_edits: 1,
        },
        ignored_max_usd_non_finite,
        ignored_parallelism,
    }
}

/// BG3: `effective_usd <= 0.0` MUST be treated as exhausted **before**
/// calling `check_budget` — `CostMeter::check_budget` only reports
/// `usd_exhausted` once `spent >= max`, and `spent` is `None` before the
/// first model call, so a `0.0` ceiling alone would let a whole run through.
pub(super) fn is_pre_dispatch_exhausted(policy: &BudgetPolicy) -> bool {
    policy.max_usd_per_run <= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- helpers -----

    fn policy(usd: f64, tokens: u64) -> BudgetPolicy {
        BudgetPolicy {
            max_usd_per_run: usd,
            max_tokens_per_run: tokens,
            max_parallel_nodes: 1,
            max_parallel_cargo: 1,
            max_parallel_edits: 1,
        }
    }

    fn goal(constraints: Vec<Constraint>) -> Goal {
        Goal {
            text: "x".into(),
            constraints,
            attachments: vec![],
        }
    }

    // ----- happy path -----

    #[test]
    fn takes_the_minimum_across_all_three_sources() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(5.0, 2000),
            &goal(vec![Constraint::MaxUsd(2.0)]),
        );
        assert_eq!(eff.policy.max_usd_per_run, 2.0);
        assert_eq!(eff.policy.max_tokens_per_run, 1000);
        assert!(!eff.ignored_max_usd_non_finite);
        assert!(eff.ignored_parallelism.is_empty());
    }

    #[test]
    fn no_goal_caps_falls_back_to_policy_and_session_minimum() {
        let eff = effective_budget(&policy(10.0, 1000), &policy(5.0, 2000), &goal(vec![]));
        assert_eq!(eff.policy.max_usd_per_run, 5.0);
    }

    #[test]
    fn multiple_goal_caps_take_the_lowest() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(10.0, 1000),
            &goal(vec![Constraint::MaxUsd(3.0), Constraint::MaxUsd(1.5)]),
        );
        assert_eq!(eff.policy.max_usd_per_run, 1.5);
    }

    #[test]
    fn output_always_forces_serial_parallelism() {
        let mut session = policy(5.0, 1000);
        session.max_parallel_nodes = 4;
        let eff = effective_budget(&policy(10.0, 1000), &session, &goal(vec![]));
        assert_eq!(eff.policy.max_parallel_nodes, 1);
        assert_eq!(eff.policy.max_parallel_cargo, 1);
        assert_eq!(eff.policy.max_parallel_edits, 1);
    }

    // ----- BG1/BG2/BG5 edge cases -----

    #[test]
    fn bg1_non_finite_max_usd_is_ignored_and_flagged() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(10.0, 1000),
            &goal(vec![
                Constraint::MaxUsd(f64::NAN),
                Constraint::MaxUsd(f64::INFINITY),
            ]),
        );
        assert!(eff.ignored_max_usd_non_finite);
        assert_eq!(eff.policy.max_usd_per_run, 10.0); // no finite cap survived
    }

    #[test]
    fn bg1_mixes_finite_and_non_finite_caps_correctly() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(10.0, 1000),
            &goal(vec![Constraint::MaxUsd(f64::NAN), Constraint::MaxUsd(4.0)]),
        );
        assert!(eff.ignored_max_usd_non_finite);
        assert_eq!(eff.policy.max_usd_per_run, 4.0);
    }

    #[test]
    fn bg2_negative_finite_max_usd_clamps_to_zero() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(10.0, 1000),
            &goal(vec![Constraint::MaxUsd(-5.0)]),
        );
        assert_eq!(eff.policy.max_usd_per_run, 0.0);
        assert!(!eff.ignored_max_usd_non_finite); // clamped, not ignored
    }

    #[test]
    fn bg5_flags_every_non_unit_session_parallelism_field() {
        let mut session = policy(5.0, 1000);
        session.max_parallel_nodes = 2;
        session.max_parallel_cargo = 3;
        session.max_parallel_edits = 4;
        let eff = effective_budget(&policy(10.0, 1000), &session, &goal(vec![]));
        let fields: Vec<&str> = eff.ignored_parallelism.iter().map(|p| p.field).collect();
        assert_eq!(
            fields,
            vec![
                "max_parallel_nodes",
                "max_parallel_cargo",
                "max_parallel_edits"
            ]
        );
        assert_eq!(eff.ignored_parallelism[0].value, 2);
        assert_eq!(eff.ignored_parallelism[1].value, 3);
        assert_eq!(eff.ignored_parallelism[2].value, 4);
    }

    #[test]
    fn ignores_non_max_usd_constraints() {
        let eff = effective_budget(
            &policy(10.0, 1000),
            &policy(10.0, 1000),
            &goal(vec![Constraint::RequireCargoCheck, Constraint::DenyRawBash]),
        );
        assert_eq!(eff.policy.max_usd_per_run, 10.0);
        assert!(!eff.ignored_max_usd_non_finite);
    }

    // ----- BG3 -----

    #[test]
    fn bg3_zero_ceiling_is_pre_dispatch_exhausted() {
        assert!(is_pre_dispatch_exhausted(&policy(0.0, 1000)));
    }

    #[test]
    fn bg3_negative_ceiling_is_pre_dispatch_exhausted() {
        assert!(is_pre_dispatch_exhausted(&policy(-1.0, 1000)));
    }

    #[test]
    fn bg3_positive_ceiling_is_not_pre_dispatch_exhausted() {
        assert!(!is_pre_dispatch_exhausted(&policy(0.01, 1000)));
    }
}
