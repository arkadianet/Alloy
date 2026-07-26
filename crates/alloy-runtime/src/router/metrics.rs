//! Process-local router metrics.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Point-in-time router counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterMetricsSnapshot {
    /// Successful routes.
    pub routes_ok: u64,
    /// Routes denied by budget.
    pub routes_budget_denied: u64,
    /// Routes for which no endpoint matched.
    pub routes_no_endpoint: u64,
    /// Routes that used the default tier.
    pub routes_default_tier: u64,
    /// Successful completions.
    pub completes_ok: u64,
    /// Completions returning an error.
    pub completes_err: u64,
    /// Calls currently admitted by the router.
    pub in_flight: usize,
    /// Failed observability appends.
    pub obs_record_errors: u64,
    /// Prompt bodies omitted because they exceeded the observability cap.
    pub model_call_prompt_body_oversize: u64,
}

#[derive(Default)]
pub(crate) struct RouterMetrics {
    pub(crate) routes_ok: AtomicU64,
    pub(crate) routes_budget_denied: AtomicU64,
    pub(crate) routes_no_endpoint: AtomicU64,
    pub(crate) routes_default_tier: AtomicU64,
    pub(crate) completes_ok: AtomicU64,
    pub(crate) completes_err: AtomicU64,
    pub(crate) in_flight: AtomicUsize,
    pub(crate) obs_record_errors: Arc<AtomicU64>,
    pub(crate) model_call_prompt_body_oversize: AtomicU64,
}

impl RouterMetrics {
    pub(crate) fn snapshot(&self) -> RouterMetricsSnapshot {
        RouterMetricsSnapshot {
            routes_ok: self.routes_ok.load(Ordering::Relaxed),
            routes_budget_denied: self.routes_budget_denied.load(Ordering::Relaxed),
            routes_no_endpoint: self.routes_no_endpoint.load(Ordering::Relaxed),
            routes_default_tier: self.routes_default_tier.load(Ordering::Relaxed),
            completes_ok: self.completes_ok.load(Ordering::Relaxed),
            completes_err: self.completes_err.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            obs_record_errors: self.obs_record_errors.load(Ordering::Relaxed),
            model_call_prompt_body_oversize: self
                .model_call_prompt_body_oversize
                .load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reads_all_counters() {
        let metrics = RouterMetrics::default();
        metrics.routes_ok.store(2, Ordering::Relaxed);
        metrics.in_flight.store(1, Ordering::SeqCst);
        metrics.obs_record_errors.store(3, Ordering::Relaxed);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.routes_ok, 2);
        assert_eq!(snapshot.in_flight, 1);
        assert_eq!(snapshot.obs_record_errors, 3);
    }
}
