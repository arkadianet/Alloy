//! Retry admission and tier escalation (RFC-0010 §5.11 A1-A6, ES1-ES6).
//!
//! Pure decision functions — no I/O, no store access — so they can be
//! table-tested directly (§11.1 style) without a `LinearScheduler`
//! instance. `loop_.rs`'s `admit_retry`/`dispatch_node` own the I/O: they
//! gather the inputs (cancellation state, remaining run budget, budget
//! exhaustion), call [`admit`]/[`escalation_for_attempt`] here, then call
//! [`super::checkpoint::Checkpoint::c8_retry`] (§5.8.3, built in P3) and
//! sleep the returned backoff interruptibly (B3) — this module never
//! touches the store or a cancellation token itself.

use std::time::Duration;

use crate::dag::{Backoff, RetryPolicy};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, RetryDisposition};

use super::ready::backoff_delay;

/// §5.11.1 A6: the minimum remaining run budget a retry needs beyond its
/// own backoff delay, so an admitted retry doesn't immediately time out.
pub(crate) const RETRY_BUDGET_SLICE: Duration = Duration::from_millis(250);

/// Why an admission check rejected a retry (Decision record `reason`,
/// §5.11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    /// A1: `failure.retry != Retryable`.
    NonRetryableFailure,
    /// A2: `failure.error_class` not in `node.retry.retry_on`.
    ErrorClassNotRetryable,
    /// A3: `attempts_started >= node.retry.max_attempts`.
    AttemptsExhausted,
    /// A4: `run_cancel` or `runtime_cancel` already fired.
    Cancelled,
    /// A5: the run budget is already exhausted (§5.16.3).
    BudgetExhausted,
    /// A6: remaining run budget does not exceed backoff + `RETRY_BUDGET_SLICE`.
    InsufficientRemainingBudget,
}

impl RejectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RejectReason::NonRetryableFailure => "non_retryable_failure",
            RejectReason::ErrorClassNotRetryable => "error_class_not_retryable",
            RejectReason::AttemptsExhausted => "attempts_exhausted",
            RejectReason::Cancelled => "cancelled",
            RejectReason::BudgetExhausted => "budget_exhausted",
            RejectReason::InsufficientRemainingBudget => "insufficient_remaining_budget",
        }
    }
}

/// A1-A6 admission inputs, gathered by the caller so this stays I/O-free.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdmissionInput<'a> {
    /// The failure's retry disposition (A1).
    pub retry_disposition: RetryDisposition,
    /// The failure's error class (A2).
    pub error_class: ErrorClass,
    /// `node.retry.retry_on` (A2).
    pub retry_on: &'a [ErrorClass],
    /// `node.retry.max_attempts` (A3).
    pub max_attempts: u32,
    /// Whether `run_cancel` or `runtime_cancel` has already fired (A4).
    pub cancelled: bool,
    /// Whether the run budget is already exhausted, via the same
    /// `check_budget` mechanism L6 uses (A5).
    pub budget_exhausted: bool,
    /// Remaining run budget per `RunCtx::remaining_run` (A6).
    pub remaining_run: Duration,
    /// `node.retry.backoff` (§5.11.3).
    pub backoff: &'a Backoff,
    /// `config.max_backoff` (§5.11.3).
    pub max_backoff: Duration,
}

/// Outcome of [`admit`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Admission {
    /// All of A1-A6 held; retry attempt `next_attempt` after sleeping `delay`.
    Admit {
        /// `attempts_started + 1`.
        next_attempt: u32,
        /// §5.11.3 backoff sleep before `next_attempt`'s C3.
        delay: Duration,
    },
    /// At least one admission condition failed; go durable `Failed` via C7.
    Reject(RejectReason),
}

/// §5.11.1: admit or reject a retry for the attempt that just failed.
/// `attempts_started` is that failed attempt's 1-based number (the same
/// value [`backoff_delay`]'s own `attempt` parameter expects — B1).
///
/// Order matches the RFC's A1-A6 table; the first failing condition wins
/// (mirrors `derive_dag_state`'s first-match-wins style from P2).
#[must_use]
pub(crate) fn admit(attempts_started: u32, input: AdmissionInput<'_>) -> Admission {
    if input.retry_disposition != RetryDisposition::Retryable {
        return Admission::Reject(RejectReason::NonRetryableFailure); // A1
    }
    if !input.retry_on.contains(&input.error_class) {
        return Admission::Reject(RejectReason::ErrorClassNotRetryable); // A2
    }
    if attempts_started >= input.max_attempts {
        return Admission::Reject(RejectReason::AttemptsExhausted); // A3
    }
    if input.cancelled {
        return Admission::Reject(RejectReason::Cancelled); // A4
    }
    if input.budget_exhausted {
        return Admission::Reject(RejectReason::BudgetExhausted); // A5
    }
    let delay = backoff_delay(input.backoff, attempts_started, input.max_backoff);
    if input.remaining_run <= delay + RETRY_BUDGET_SLICE {
        return Admission::Reject(RejectReason::InsufficientRemainingBudget); // A6
    }
    Admission::Admit {
        next_attempt: attempts_started + 1,
        delay,
    }
}

/// Tier escalation outcome for one dispatch attempt (§5.11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Escalation {
    /// Not configured, or `attempt <= escalate_after`.
    None,
    /// Escalated; `CapabilityExecContext.effective_tier` MUST use this
    /// (ES3: `TaskNode.model_tier` is never written).
    To(ModelTier),
    /// `escalate_after` is due but no `escalate_to_tier` is configured (ES2).
    SkippedNoTarget,
}

/// ES1/ES2: decide escalation for `attempt` from `node.retry`. Stateless —
/// `attempt <= escalate_after` is false exactly once and stays false for
/// every larger `attempt`, so re-evaluating this fresh at every dispatch
/// already satisfies ES6's monotonicity without any stored "escalated"
/// flag (`TaskNode` carries none, and none is needed).
///
/// Callers MUST NOT invoke this for adapter node kinds (`VerifyCompile`,
/// `VerifyTest`, `GateHuman`, `Aggregate`) — ES5 ignores escalation for
/// them entirely; the kind gate lives in `loop_.rs`, not here, since this
/// function has no `NodeKind` to check.
#[must_use]
pub(crate) fn escalation_for_attempt(retry: &RetryPolicy, attempt: u32) -> Escalation {
    let Some(n) = retry.escalate_after else {
        return Escalation::None;
    };
    if attempt <= n {
        return Escalation::None;
    }
    match retry.escalate_to_tier {
        Some(tier) => Escalation::To(tier),
        None => Escalation::SkippedNoTarget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(overrides: impl FnOnce(&mut AdmissionInput<'static>)) -> AdmissionInput<'static> {
        static BACKOFF: Backoff = Backoff::Fixed { delay_ms: 0 };
        static RETRY_ON: [ErrorClass; 1] = [ErrorClass::Model];
        let mut i = AdmissionInput {
            retry_disposition: RetryDisposition::Retryable,
            error_class: ErrorClass::Model,
            retry_on: &RETRY_ON,
            max_attempts: 3,
            cancelled: false,
            budget_exhausted: false,
            remaining_run: Duration::from_secs(60),
            backoff: &BACKOFF,
            max_backoff: Duration::from_secs(60),
        };
        overrides(&mut i);
        i
    }

    // ---- A1-A6 admission ----

    #[test]
    fn admits_when_every_condition_holds() {
        let decision = admit(1, input(|_| {}));
        assert!(matches!(
            decision,
            Admission::Admit { next_attempt: 2, delay } if delay == Duration::ZERO
        ));
    }

    #[test]
    fn a1_rejects_non_retryable_failure() {
        let decision = admit(
            1,
            input(|i| i.retry_disposition = RetryDisposition::NonRetryable),
        );
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::NonRetryableFailure)
        ));
    }

    #[test]
    fn a2_rejects_error_class_outside_retry_on() {
        let decision = admit(1, input(|i| i.error_class = ErrorClass::Timeout));
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::ErrorClassNotRetryable)
        ));
    }

    #[test]
    fn a3_rejects_when_attempts_started_meets_max() {
        let decision = admit(3, input(|i| i.max_attempts = 3));
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::AttemptsExhausted)
        ));
    }

    #[test]
    fn a3_admits_at_the_last_eligible_attempt() {
        let decision = admit(2, input(|i| i.max_attempts = 3));
        assert!(matches!(
            decision,
            Admission::Admit {
                next_attempt: 3,
                ..
            }
        ));
    }

    #[test]
    fn a4_rejects_when_cancelled() {
        let decision = admit(1, input(|i| i.cancelled = true));
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::Cancelled)
        ));
    }

    #[test]
    fn a5_rejects_when_budget_already_exhausted() {
        let decision = admit(1, input(|i| i.budget_exhausted = true));
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::BudgetExhausted)
        ));
    }

    #[test]
    fn a6_rejects_when_remaining_budget_does_not_exceed_backoff_plus_slice() {
        static BACKOFF: Backoff = Backoff::Fixed { delay_ms: 1000 };
        let decision = admit(
            1,
            input(|i| {
                i.backoff = &BACKOFF;
                // delay (1000ms) + slice (250ms) = 1250ms; remaining exactly
                // at the boundary MUST reject (RFC uses "exceeds", not >=).
                i.remaining_run = Duration::from_millis(1250);
            }),
        );
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::InsufficientRemainingBudget)
        ));
    }

    #[test]
    fn a6_admits_when_remaining_budget_exceeds_backoff_plus_slice() {
        static BACKOFF: Backoff = Backoff::Fixed { delay_ms: 1000 };
        let decision = admit(
            1,
            input(|i| {
                i.backoff = &BACKOFF;
                i.remaining_run = Duration::from_millis(1251);
            }),
        );
        assert!(matches!(
            decision,
            Admission::Admit { delay, .. } if delay == Duration::from_millis(1000)
        ));
    }

    #[test]
    fn first_failing_condition_wins_a1_before_a2() {
        // Both A1 and A2 would fail; A1 (table order) must be reported.
        let decision = admit(
            1,
            input(|i| {
                i.retry_disposition = RetryDisposition::NonRetryable;
                i.error_class = ErrorClass::Timeout;
            }),
        );
        assert!(matches!(
            decision,
            Admission::Reject(RejectReason::NonRetryableFailure)
        ));
    }

    // ---- ES1-ES6 escalation ----

    fn retry_policy(
        escalate_after: Option<u32>,
        escalate_to_tier: Option<ModelTier>,
    ) -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            backoff: Backoff::Fixed { delay_ms: 0 },
            retry_on: vec![],
            escalate_after,
            escalate_to_tier,
        }
    }

    #[test]
    fn es1_no_escalation_configured() {
        let policy = retry_policy(None, None);
        assert_eq!(escalation_for_attempt(&policy, 5), Escalation::None);
    }

    #[test]
    fn es1_not_yet_due() {
        let policy = retry_policy(Some(2), Some(ModelTier::Premium));
        assert_eq!(escalation_for_attempt(&policy, 1), Escalation::None);
        assert_eq!(escalation_for_attempt(&policy, 2), Escalation::None); // k > n, not k >= n
    }

    #[test]
    fn es1_escalates_once_due() {
        let policy = retry_policy(Some(2), Some(ModelTier::Premium));
        assert_eq!(
            escalation_for_attempt(&policy, 3),
            Escalation::To(ModelTier::Premium)
        );
    }

    #[test]
    fn es2_skipped_when_no_target_tier() {
        let policy = retry_policy(Some(1), None);
        assert_eq!(
            escalation_for_attempt(&policy, 2),
            Escalation::SkippedNoTarget
        );
    }

    #[test]
    fn es6_monotone_across_later_attempts() {
        // Once due, every larger attempt escalates identically — no
        // stored "already escalated" flag needed (see doc comment).
        let policy = retry_policy(Some(1), Some(ModelTier::Premium));
        assert_eq!(
            escalation_for_attempt(&policy, 2),
            Escalation::To(ModelTier::Premium)
        );
        assert_eq!(
            escalation_for_attempt(&policy, 3),
            Escalation::To(ModelTier::Premium)
        );
        assert_eq!(
            escalation_for_attempt(&policy, 100),
            Escalation::To(ModelTier::Premium)
        );
    }

    /// The shipped `repair_local_diagnostic` manifest is the only day-1
    /// producer of escalation policy, so pin what the scheduler will actually
    /// decide for its model-backed nodes: base tier on attempt 1, Premium on
    /// the single retry. `es1_escalation_applies_to_capability_context_after_
    /// threshold` in `loop_.rs` proves the same decision reaches
    /// `CapabilityExecContext.effective_tier` (ES3), and
    /// `escalated_effective_tier_routes_to_the_premium_endpoint` in
    /// `tests/capabilities_rfc0013.rs` proves that tier reaches the routed
    /// endpoint.
    #[test]
    fn repair_template_llm_nodes_escalate_on_their_retry() {
        use crate::dag::{NodeKind, TemplateCatalog, TemplateId};

        let manifest = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
        let llm: Vec<_> = manifest
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Analyze | NodeKind::Edit))
            .collect();
        assert_eq!(llm.len(), 2, "analyze + edit are the model-backed nodes");
        for node in llm {
            assert_eq!(escalation_for_attempt(&node.retry, 1), Escalation::None);
            assert_eq!(
                escalation_for_attempt(&node.retry, 2),
                Escalation::To(ModelTier::Premium),
                "{} must escalate on its retry",
                node.name
            );
            // The escalated attempt must be reachable: A3 admits `attempt <
            // max_attempts`, so escalate_after + 1 <= max_attempts.
            assert!(node.retry.escalate_after.unwrap() < node.retry.max_attempts);
        }
        // ES5: adapter nodes carry no escalation at all (V9 forbids it).
        for node in manifest
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::VerifyCompile | NodeKind::GateHuman))
        {
            assert_eq!(escalation_for_attempt(&node.retry, 99), Escalation::None);
        }
    }

    #[test]
    fn reject_reason_as_str_is_stable_snake_case() {
        assert_eq!(
            RejectReason::NonRetryableFailure.as_str(),
            "non_retryable_failure"
        );
        assert_eq!(
            RejectReason::ErrorClassNotRetryable.as_str(),
            "error_class_not_retryable"
        );
        assert_eq!(
            RejectReason::AttemptsExhausted.as_str(),
            "attempts_exhausted"
        );
        assert_eq!(RejectReason::Cancelled.as_str(), "cancelled");
        assert_eq!(RejectReason::BudgetExhausted.as_str(), "budget_exhausted");
        assert_eq!(
            RejectReason::InsufficientRemainingBudget.as_str(),
            "insufficient_remaining_budget"
        );
    }
}
