//! Process-local cost metering (RFC-0004 §3.12 / §6).

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::types::budget::{BudgetPolicy, BudgetSnapshot, ModelTier};
use crate::types::metrics::WorkerMetrics;

/// Point-in-time cost counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSnapshot {
    /// Accumulated known input tokens.
    pub tokens_in: u64,
    /// Accumulated known output tokens.
    pub tokens_out: u64,
    /// Sum of reported USD amounts. `None` until the first finite non-negative USD update.
    pub usd_spent: Option<f64>,
    /// Number of model-usage updates recorded.
    pub model_calls: u64,
    /// Updates where input or output tokens were unknown.
    pub unknown_token_events: u64,
    /// Per-tier breakdown.
    pub by_tier: CostByTier,
}

/// Per-tier cost buckets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostByTier {
    /// Premium tier.
    pub premium: TierCost,
    /// Standard tier.
    pub standard: TierCost,
    /// Economy tier.
    pub economy: TierCost,
    /// Local tier.
    pub local: TierCost,
}

/// Counters for a single [`ModelTier`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TierCost {
    /// Known input tokens.
    pub tokens_in: u64,
    /// Known output tokens.
    pub tokens_out: u64,
    /// Optional USD for this tier.
    pub usd: Option<f64>,
    /// Call count.
    pub calls: u64,
}

/// Result of comparing a meter against a [`BudgetPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    /// Under both ceilings.
    Ok,
    /// Token ceiling met or exceeded.
    TokensExhausted,
    /// USD ceiling met or exceeded (or degenerate policy).
    UsdExhausted,
    /// Both ceilings exhausted.
    TokensAndUsdExhausted,
}

impl BudgetCheck {
    /// True when any ceiling is exhausted.
    #[must_use]
    pub fn is_exhausted(self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// Process-local incremental meter. Not shared across tasks by itself.
#[derive(Debug, Default, Clone)]
pub struct CostMeter {
    tokens_in: u64,
    tokens_out: u64,
    usd_spent: Option<f64>,
    model_calls: u64,
    unknown_token_events: u64,
    by_tier: CostByTier,
}

impl CostMeter {
    /// Empty meter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record model usage. `None` means unknown — never fabricate.
    ///
    /// Non-finite or negative finite `usd` → skip USD update (`tracing::warn`); tokens still apply.
    /// Cumulative meter/tier USD uses finite saturating add.
    pub fn add_model_usage(
        &mut self,
        tier: ModelTier,
        input: Option<u64>,
        output: Option<u64>,
        usd: Option<f64>,
    ) {
        self.model_calls = self.model_calls.saturating_add(1);
        let tier_cost = self.tier_mut(tier);
        tier_cost.calls = tier_cost.calls.saturating_add(1);

        if let Some(n) = input {
            self.tokens_in = self.tokens_in.saturating_add(n);
            let tier_cost = self.tier_mut(tier);
            tier_cost.tokens_in = tier_cost.tokens_in.saturating_add(n);
        }
        if let Some(n) = output {
            self.tokens_out = self.tokens_out.saturating_add(n);
            let tier_cost = self.tier_mut(tier);
            tier_cost.tokens_out = tier_cost.tokens_out.saturating_add(n);
        }
        if input.is_none() || output.is_none() {
            self.unknown_token_events = self.unknown_token_events.saturating_add(1);
        }

        if let Some(x) = usd {
            if !x.is_finite() {
                tracing::warn!("non-finite usd ignored");
            } else if x < 0.0 {
                tracing::warn!("negative usd ignored");
            } else {
                self.usd_spent = Some(saturating_add_usd(self.usd_spent, x));
                let tier_cost = self.tier_mut(tier);
                tier_cost.usd = Some(saturating_add_usd(tier_cost.usd, x));
            }
        }
    }

    /// Feed a completed [`WorkerMetrics`].
    ///
    /// Treats token fields as known. Increments calls even when `error_class` is set.
    pub fn add_worker_metrics(&mut self, metrics: &WorkerMetrics, usd: Option<f64>) {
        self.add_model_usage(
            metrics.model_tier_used,
            Some(metrics.input_tokens),
            Some(metrics.output_tokens),
            usd,
        );
    }

    /// Point-in-time copy of counters.
    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot {
        CostSnapshot {
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            usd_spent: self.usd_spent,
            model_calls: self.model_calls,
            unknown_token_events: self.unknown_token_events,
            by_tier: self.by_tier.clone(),
        }
    }

    /// Map into [`BudgetSnapshot`]. Unknown USD becomes `0.0` for the snapshot field only.
    #[must_use]
    pub fn to_budget_snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            usd_spent: self.usd_spent.unwrap_or(0.0),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
        }
    }

    /// Compare against policy ceilings (`>=`, saturating token sum).
    #[must_use]
    pub fn check_budget(&self, policy: &BudgetPolicy) -> BudgetCheck {
        let max_tok = policy.max_tokens_per_run;
        let max_usd = policy.max_usd_per_run;

        let tokens_exhausted = self.tokens_in.saturating_add(self.tokens_out) >= max_tok;

        let usd_exhausted = if !max_usd.is_finite() || max_usd < 0.0 {
            true
        } else if let Some(spent) = self.usd_spent {
            spent >= max_usd
        } else {
            false
        };

        match (tokens_exhausted, usd_exhausted) {
            (false, false) => BudgetCheck::Ok,
            (true, false) => BudgetCheck::TokensExhausted,
            (false, true) => BudgetCheck::UsdExhausted,
            (true, true) => BudgetCheck::TokensAndUsdExhausted,
        }
    }

    fn tier_mut(&mut self, tier: ModelTier) -> &mut TierCost {
        match tier {
            ModelTier::Premium => &mut self.by_tier.premium,
            ModelTier::Standard => &mut self.by_tier.standard,
            ModelTier::Economy => &mut self.by_tier.economy,
            ModelTier::Local => &mut self.by_tier.local,
        }
    }
}

fn saturating_add_usd(current: Option<f64>, x: f64) -> f64 {
    match current {
        None => x,
        Some(cur) => {
            let sum = cur + x;
            if sum.is_finite() {
                sum
            } else {
                f64::MAX
            }
        }
    }
}

/// [`Arc`]`<`[`Mutex`]`<`[`CostMeter`]`>>` for concurrent producers.
#[derive(Clone, Default)]
pub struct SharedCostMeter {
    inner: Arc<Mutex<CostMeter>>,
}

impl std::fmt::Debug for SharedCostMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedCostMeter").finish_non_exhaustive()
    }
}

impl SharedCostMeter {
    /// Empty shared meter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, CostMeter> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::error!("cost meter mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Record model usage under the lock.
    pub fn add_model_usage(
        &self,
        tier: ModelTier,
        input: Option<u64>,
        output: Option<u64>,
        usd: Option<f64>,
    ) {
        self.lock().add_model_usage(tier, input, output, usd);
    }

    /// Feed [`WorkerMetrics`] under the lock.
    pub fn add_worker_metrics(&self, metrics: &WorkerMetrics, usd: Option<f64>) {
        self.lock().add_worker_metrics(metrics, usd);
    }

    /// Snapshot under the lock.
    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot {
        self.lock().snapshot()
    }

    /// Budget snapshot under the lock.
    #[must_use]
    pub fn to_budget_snapshot(&self) -> BudgetSnapshot {
        self.lock().to_budget_snapshot()
    }

    /// Budget check under the lock.
    #[must_use]
    pub fn check_budget(&self, policy: &BudgetPolicy) -> BudgetCheck {
        self.lock().check_budget(policy)
    }

    /// Run a closure under the meter lock (keep sections short).
    ///
    /// Non-reentrant: calling other [`SharedCostMeter`] methods inside `f` deadlocks.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut CostMeter) -> R) -> R {
        f(&mut self.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::diagnostic::ErrorClass;
    use crate::types::ids::ProviderId;
    use std::sync::Arc;
    use std::thread;

    fn policy(tokens: u64, usd: f64) -> BudgetPolicy {
        BudgetPolicy {
            max_usd_per_run: usd,
            max_tokens_per_run: tokens,
            ..BudgetPolicy::default()
        }
    }

    #[test]
    fn cost_snapshot_arithmetic() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Premium, Some(10), Some(5), Some(0.1));
        m.add_model_usage(ModelTier::Standard, Some(20), Some(7), Some(0.2));
        m.add_model_usage(ModelTier::Economy, Some(1), Some(1), Some(0.01));
        m.add_model_usage(ModelTier::Local, Some(3), Some(2), None);
        let s = m.snapshot();
        assert_eq!(s.tokens_in, 34);
        assert_eq!(s.tokens_out, 15);
        assert!((s.usd_spent.unwrap() - 0.31).abs() < 1e-9);
        assert_eq!(s.model_calls, 4);
        assert_eq!(s.by_tier.premium.calls, 1);
        assert_eq!(s.by_tier.standard.calls, 1);
        assert_eq!(s.by_tier.economy.calls, 1);
        assert_eq!(s.by_tier.local.calls, 1);
        assert!(s.by_tier.local.usd.is_none());
    }

    #[test]
    fn cost_unknown_usage_no_fabricated_tokens() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, None, None, None);
        let s = m.snapshot();
        assert_eq!(s.tokens_in, 0);
        assert_eq!(s.tokens_out, 0);
        assert_eq!(s.unknown_token_events, 1);
        assert_eq!(s.model_calls, 1);
    }

    #[test]
    fn cost_unknown_usd_none() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), None);
        assert!(m.snapshot().usd_spent.is_none());
    }

    #[test]
    fn cost_non_finite_usd_ignored() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(f64::NAN));
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(f64::INFINITY));
        assert!(m.snapshot().usd_spent.is_none());
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(1.5));
        assert!((m.snapshot().usd_spent.unwrap() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn cost_negative_usd_ignored() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(2.0));
        m.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(-1.0));
        assert!((m.snapshot().usd_spent.unwrap() - 2.0).abs() < 1e-9);
        assert!((m.snapshot().by_tier.standard.usd.unwrap() - 2.0).abs() < 1e-9);
        m.add_model_usage(ModelTier::Standard, Some(0), Some(0), Some(0.5));
        assert!((m.snapshot().usd_spent.unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_overflow_saturates() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, Some(0), Some(0), Some(f64::MAX));
        m.add_model_usage(ModelTier::Standard, Some(0), Some(0), Some(f64::MAX));
        assert_eq!(m.snapshot().usd_spent, Some(f64::MAX));
        assert_eq!(m.snapshot().by_tier.standard.usd, Some(f64::MAX));
    }

    #[test]
    fn add_worker_metrics_counts_failed_calls() {
        let mut m = CostMeter::new();
        let metrics = WorkerMetrics {
            model_tier_used: ModelTier::Economy,
            provider_id: ProviderId::new("p").unwrap(),
            input_tokens: 4,
            output_tokens: 2,
            tool_calls: 0,
            cache_hits: 0,
            duration_ms: 10,
            confidence: None,
            error_class: Some(ErrorClass::Model),
        };
        m.add_worker_metrics(&metrics, Some(0.05));
        let s = m.snapshot();
        assert_eq!(s.model_calls, 1);
        assert_eq!(s.tokens_in, 4);
        assert_eq!(s.tokens_out, 2);
        assert_eq!(s.unknown_token_events, 0);
        assert_eq!(s.by_tier.economy.calls, 1);
    }

    #[test]
    fn budget_check_tokens_and_usd() {
        let mut m = CostMeter::new();
        m.add_model_usage(ModelTier::Standard, Some(5), Some(5), Some(1.0));
        assert_eq!(m.check_budget(&policy(11, 2.0)), BudgetCheck::Ok);
        assert_eq!(
            m.check_budget(&policy(10, 2.0)),
            BudgetCheck::TokensExhausted
        );
        assert_eq!(m.check_budget(&policy(11, 1.0)), BudgetCheck::UsdExhausted);
        assert_eq!(
            m.check_budget(&policy(10, 1.0)),
            BudgetCheck::TokensAndUsdExhausted
        );
        // saturating_add: near u64::MAX
        let mut big = CostMeter::new();
        big.add_model_usage(ModelTier::Standard, Some(u64::MAX), Some(1), None);
        assert_eq!(
            big.check_budget(&policy(u64::MAX, 100.0)),
            BudgetCheck::TokensExhausted
        );
    }

    #[test]
    fn budget_zero_tokens_immediately_exhausted() {
        let m = CostMeter::new();
        assert_eq!(
            m.check_budget(&policy(0, 100.0)),
            BudgetCheck::TokensExhausted
        );
    }

    #[test]
    fn budget_non_finite_usd_ceiling_exhausted() {
        let m = CostMeter::new();
        assert_eq!(
            m.check_budget(&policy(100, f64::NAN)),
            BudgetCheck::UsdExhausted
        );
        assert_eq!(
            m.check_budget(&policy(100, -1.0)),
            BudgetCheck::UsdExhausted
        );
    }

    #[test]
    fn shared_cost_meter_no_lost_updates() {
        let shared = SharedCostMeter::new();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = shared.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    s.add_model_usage(ModelTier::Standard, Some(1), Some(1), Some(0.01));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let snap = shared.snapshot();
        assert_eq!(snap.model_calls, 800);
        assert_eq!(snap.tokens_in, 800);
        assert_eq!(snap.tokens_out, 800);
        assert!((snap.usd_spent.unwrap() - 8.0).abs() < 1e-6);
    }

    #[test]
    fn shared_cost_meter_poison_recovers() {
        let shared = SharedCostMeter::new();
        let s2 = shared.clone();
        let _ = thread::spawn(move || {
            let _guard = s2.lock();
            panic!("poison");
        })
        .join();
        shared.add_model_usage(ModelTier::Local, Some(1), Some(0), None);
        assert_eq!(shared.snapshot().tokens_in, 1);
    }

    #[test]
    fn to_budget_snapshot_unknown_usd_is_zero_field() {
        let m = CostMeter::new();
        let b = m.to_budget_snapshot();
        assert_eq!(b.usd_spent, 0.0);
        assert!(m.snapshot().usd_spent.is_none());
    }

    #[test]
    fn arc_shared_compiles() {
        let _: Arc<SharedCostMeter> = Arc::new(SharedCostMeter::new());
    }
}
