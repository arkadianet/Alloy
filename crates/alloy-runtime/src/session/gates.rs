//! In-process gate waiter registry (RFC-0010 resumes via approve).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::adapters::Approval;
use crate::types::ids::{GateId, RunId};

/// Oneshot waiters keyed by `(RunId, GateId)`.
#[derive(Default)]
pub(crate) struct GateWaiterRegistry {
    waiters: Mutex<HashMap<(RunId, GateId), oneshot::Sender<Approval>>>,
}

impl GateWaiterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace any prior waiter for `(run, gate)`; return the new receiver.
    pub fn register(&self, run: RunId, gate: GateId) -> oneshot::Receiver<Approval> {
        let (tx, rx) = oneshot::channel();
        let mut g = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert((run, gate), tx);
        rx
    }

    /// Take the sender for `(run, gate)` if present.
    pub fn take(&self, run: RunId, gate: GateId) -> Option<oneshot::Sender<Approval>> {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(run, gate))
    }

    /// Put a sender back after a failed persist (approve must not consume the gate).
    pub fn restore(&self, run: RunId, gate: GateId, tx: oneshot::Sender<Approval>) {
        let mut g = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert((run, gate), tx);
    }

    /// Drop all waiters for a run (cancel / replan / deny).
    pub fn clear_run(&self, run: RunId) {
        let mut g = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.retain(|(r, _), _| *r != run);
    }
}
