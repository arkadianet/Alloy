//! In-process storage counters.

use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of storage counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageMetricsSnapshot {
    /// Successful session event appends.
    pub events_appended: u64,
    /// Successful runtime event appends.
    pub runtime_events_appended: u64,
    /// Events returned by list/replay pages.
    pub events_read: u64,
    /// Successful artifact puts.
    pub artifacts_put: u64,
    /// Successful artifact gets.
    pub artifacts_get: u64,
    /// Successful checkpoints.
    pub checkpoints: u64,
    /// Successful install handoffs.
    pub handoffs: u64,
    /// Busy errors returned to callers.
    pub busy_errors: u64,
}

/// Atomic counters backing [`StorageMetricsSnapshot`].
#[derive(Debug, Default)]
pub struct StorageMetrics {
    pub(crate) events_appended: AtomicU64,
    pub(crate) runtime_events_appended: AtomicU64,
    pub(crate) events_read: AtomicU64,
    pub(crate) artifacts_put: AtomicU64,
    pub(crate) artifacts_get: AtomicU64,
    pub(crate) checkpoints: AtomicU64,
    pub(crate) handoffs: AtomicU64,
    pub(crate) busy_errors: AtomicU64,
}

impl StorageMetrics {
    /// Create zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy current counter values.
    #[must_use]
    pub fn snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot {
            events_appended: self.events_appended.load(Ordering::Relaxed),
            runtime_events_appended: self.runtime_events_appended.load(Ordering::Relaxed),
            events_read: self.events_read.load(Ordering::Relaxed),
            artifacts_put: self.artifacts_put.load(Ordering::Relaxed),
            artifacts_get: self.artifacts_get.load(Ordering::Relaxed),
            checkpoints: self.checkpoints.load(Ordering::Relaxed),
            handoffs: self.handoffs.load(Ordering::Relaxed),
            busy_errors: self.busy_errors.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn inc_events_appended(&self) {
        self.events_appended.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_runtime_events_appended(&self) {
        self.runtime_events_appended.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_events_read(&self, n: u64) {
        self.events_read.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn inc_artifacts_put(&self) {
        self.artifacts_put.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_artifacts_get(&self) {
        self.artifacts_get.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_checkpoints(&self) {
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_handoffs(&self) {
        self.handoffs.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_busy_errors(&self) {
        self.busy_errors.fetch_add(1, Ordering::Relaxed);
    }
}
