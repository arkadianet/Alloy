//! In-process session/run counters.

use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of session-plane counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetrics {
    /// Sessions created.
    pub sessions_created: u64,
    /// Sessions resumed.
    pub sessions_resumed: u64,
    /// Goals submitted.
    pub goals_submitted: u64,
    /// Runs started (first or re-dispatch attempts).
    pub runs_started: u64,
    /// Starts that returned scheduler unavailable.
    pub runs_start_unavailable: u64,
    /// Runs cancelled.
    pub runs_cancelled: u64,
    /// Approvals resolved.
    pub approvals_resolved: u64,
    /// Replans requested.
    pub replans_requested: u64,
    /// Budget warnings signaled.
    pub budget_warnings: u64,
}

pub(crate) struct AtomicSessionMetrics {
    pub sessions_created: AtomicU64,
    pub sessions_resumed: AtomicU64,
    pub goals_submitted: AtomicU64,
    pub runs_started: AtomicU64,
    pub runs_start_unavailable: AtomicU64,
    pub runs_cancelled: AtomicU64,
    pub approvals_resolved: AtomicU64,
    pub replans_requested: AtomicU64,
    pub budget_warnings: AtomicU64,
}

impl AtomicSessionMetrics {
    pub fn new() -> Self {
        Self {
            sessions_created: AtomicU64::new(0),
            sessions_resumed: AtomicU64::new(0),
            goals_submitted: AtomicU64::new(0),
            runs_started: AtomicU64::new(0),
            runs_start_unavailable: AtomicU64::new(0),
            runs_cancelled: AtomicU64::new(0),
            approvals_resolved: AtomicU64::new(0),
            replans_requested: AtomicU64::new(0),
            budget_warnings: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> SessionMetrics {
        SessionMetrics {
            sessions_created: self.sessions_created.load(Ordering::Relaxed),
            sessions_resumed: self.sessions_resumed.load(Ordering::Relaxed),
            goals_submitted: self.goals_submitted.load(Ordering::Relaxed),
            runs_started: self.runs_started.load(Ordering::Relaxed),
            runs_start_unavailable: self.runs_start_unavailable.load(Ordering::Relaxed),
            runs_cancelled: self.runs_cancelled.load(Ordering::Relaxed),
            approvals_resolved: self.approvals_resolved.load(Ordering::Relaxed),
            replans_requested: self.replans_requested.load(Ordering::Relaxed),
            budget_warnings: self.budget_warnings.load(Ordering::Relaxed),
        }
    }

    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}
