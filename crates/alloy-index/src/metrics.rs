//! Graph counters (RFC-0011 §10, OB2 — RFC-0004 snapshot conventions).

use std::sync::atomic::{AtomicU64, Ordering};

/// Private atomics; public plain-`u64` snapshot (OB2).
#[derive(Debug, Default)]
pub(crate) struct GraphMetrics {
    pub(crate) rebuilds: AtomicU64,
    pub(crate) rebuilds_unchanged: AtomicU64,
    pub(crate) incrementals: AtomicU64,
    pub(crate) queries: AtomicU64,
    pub(crate) queries_stub: AtomicU64,
    pub(crate) queries_truncated: AtomicU64,
    pub(crate) diagnostics_recorded: AtomicU64,
    pub(crate) fixes_recorded: AtomicU64,
    pub(crate) snapshots: AtomicU64,
    pub(crate) busy_errors: AtomicU64,
    pub(crate) quarantines: AtomicU64,
    pub(crate) files_skipped: AtomicU64,
}

impl GraphMetrics {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> GraphMetricsSnapshot {
        GraphMetricsSnapshot {
            rebuilds: self.rebuilds.load(Ordering::Relaxed),
            rebuilds_unchanged: self.rebuilds_unchanged.load(Ordering::Relaxed),
            incrementals: self.incrementals.load(Ordering::Relaxed),
            queries: self.queries.load(Ordering::Relaxed),
            queries_stub: self.queries_stub.load(Ordering::Relaxed),
            queries_truncated: self.queries_truncated.load(Ordering::Relaxed),
            diagnostics_recorded: self.diagnostics_recorded.load(Ordering::Relaxed),
            fixes_recorded: self.fixes_recorded.load(Ordering::Relaxed),
            snapshots: self.snapshots.load(Ordering::Relaxed),
            busy_errors: self.busy_errors.load(Ordering::Relaxed),
            quarantines: self.quarantines.load(Ordering::Relaxed),
            files_skipped: self.files_skipped.load(Ordering::Relaxed),
        }
    }
}

/// Counter snapshot (RFC-0004 conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct GraphMetricsSnapshot {
    /// Successful full rebuilds.
    pub rebuilds: u64,
    /// Rebuilds that produced no version bump (IN6).
    pub rebuilds_unchanged: u64,
    /// `apply_incremental` calls.
    pub incrementals: u64,
    /// Queries served, all kinds.
    pub queries: u64,
    /// Queries that returned an empty Stub view (Q4–Q6).
    pub queries_stub: u64,
    /// Views truncated by `max_query_nodes` (Q9).
    pub queries_truncated: u64,
    /// Diagnostics ingested.
    pub diagnostics_recorded: u64,
    /// Fixes ingested.
    pub fixes_recorded: u64,
    /// Snapshots taken.
    pub snapshots: u64,
    /// SQLite busy-timeout errors.
    pub busy_errors: u64,
    /// Corrupt databases quarantined (S8).
    pub quarantines: u64,
    /// Files skipped by a cap or skip rule (IN3, IN4).
    pub files_skipped: u64,
}
