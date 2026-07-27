//! Run-scoped cost meter provider (RFC-0010 §3.9).
//!
//! Injected so the host can share one [`SharedCostMeter`] between the
//! scheduler and the RFC-0007 router bridge for the same run.

use std::collections::HashMap;
use std::sync::Mutex;

use super::cost::SharedCostMeter;
use crate::types::ids::RunId;

/// Run-scoped meter provider.
pub trait CostMeterFactory: Send + Sync {
    /// Return the shared meter for `run`, creating one on first use.
    ///
    /// MUST return the **same** [`SharedCostMeter`] for repeated calls with
    /// the same [`RunId`] within a process, so a resumed run and the
    /// RFC-0007 router bridge accumulate into one meter.
    fn meter_for(&self, run: RunId) -> SharedCostMeter;
}

/// Process-local factory: one meter per [`RunId`], memoized, cleared on
/// [`ProcessCostMeterFactory::release`].
#[derive(Debug, Default)]
pub struct ProcessCostMeterFactory {
    meters: Mutex<HashMap<RunId, SharedCostMeter>>,
}

impl ProcessCostMeterFactory {
    /// Empty factory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, SharedCostMeter>> {
        self.meters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drop the memoized meter for a finished run.
    ///
    /// The scheduler MUST NOT call this from its ownership-release path
    /// (RFC-0010 B9): the host releases the meter after the outcome is
    /// surfaced, otherwise a re-dispatch inside the same process would lose
    /// accumulated spend.
    pub fn release(&self, run: RunId) {
        self.lock().remove(&run);
    }
}

impl CostMeterFactory for ProcessCostMeterFactory {
    fn meter_for(&self, run: RunId) -> SharedCostMeter {
        self.lock().entry(run).or_default().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::super::CostMeterFactory;
    use crate::obs::ProcessCostMeterFactory;
    use crate::types::budget::ModelTier;
    use crate::types::ids::RunId;

    #[test]
    fn meter_for_memoizes_per_run() {
        let factory = ProcessCostMeterFactory::new();
        let run = RunId::new();

        let a = factory.meter_for(run);
        a.add_model_usage(ModelTier::Standard, Some(10), Some(20), Some(0.5));

        let b = factory.meter_for(run);
        assert_eq!(b.snapshot().tokens_in, 10, "same run must share one meter");
    }

    #[test]
    fn meter_for_is_distinct_across_runs() {
        let factory = ProcessCostMeterFactory::new();
        let a = factory.meter_for(RunId::new());
        let b = factory.meter_for(RunId::new());
        a.add_model_usage(ModelTier::Standard, Some(10), Some(20), Some(0.5));
        assert_eq!(
            b.snapshot().tokens_in,
            0,
            "distinct runs must not share a meter"
        );
    }

    #[test]
    fn release_drops_the_memoized_meter() {
        let factory = ProcessCostMeterFactory::new();
        let run = RunId::new();
        let a = factory.meter_for(run);
        a.add_model_usage(ModelTier::Standard, Some(10), Some(20), Some(0.5));

        factory.release(run);

        let b = factory.meter_for(run);
        assert_eq!(
            b.snapshot().tokens_in,
            0,
            "release must drop accumulated spend"
        );
    }
}
