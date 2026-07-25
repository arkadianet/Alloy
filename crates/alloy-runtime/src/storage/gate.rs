//! Operation gate: refuse new work after close begins, wait for in-flight to finish.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::error::StoreError;

/// Result of atomically claiming exclusive close ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseClaim {
    /// This caller owns checkpoint + connection close.
    Winner,
    /// Another caller already claimed close; wait for drain + finish only.
    Follower,
}

/// Shared open/close admission control for storage backends.
#[derive(Debug)]
pub struct StorageGate {
    closed: AtomicBool,
    /// Set when the close winner has finished checkpoint/connection teardown.
    close_finished: AtomicBool,
    in_flight: AtomicUsize,
    /// Pair used to wait for `in_flight == 0` during close.
    wait: Mutex<()>,
    cv: Condvar,
    /// Pair used to wait for close teardown to finish.
    finish_wait: Mutex<()>,
    finish_cv: Condvar,
}

impl StorageGate {
    /// Create an open gate.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            close_finished: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            wait: Mutex::new(()),
            cv: Condvar::new(),
            finish_wait: Mutex::new(()),
            finish_cv: Condvar::new(),
        })
    }

    /// Whether close has begun (or completed).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Admit one operation. Hold the returned guard for the full op lifetime.
    pub fn enter(self: &Arc<Self>) -> Result<OpPermit, StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StoreError::Closed);
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        // Re-check after increment so close cannot miss us.
        if self.closed.load(Ordering::SeqCst) {
            self.leave();
            return Err(StoreError::Closed);
        }
        Ok(OpPermit {
            gate: Arc::clone(self),
        })
    }

    fn leave(&self) {
        let prev = self.in_flight.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0);
        if prev == 1 {
            let _g = self
                .wait
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.cv.notify_all();
        }
    }

    /// Atomically claim exclusive close ownership and mark the gate closed.
    ///
    /// Exactly one caller receives [`CloseClaim::Winner`]; concurrent callers
    /// become followers and must not run checkpoint / connection close.
    pub(crate) fn claim_close(&self) -> CloseClaim {
        if self.closed.swap(true, Ordering::SeqCst) {
            CloseClaim::Follower
        } else {
            CloseClaim::Winner
        }
    }

    /// Mark closed so new [`Self::enter`] fails, then block until in-flight ops finish.
    ///
    /// Idempotent: if already closed, returns immediately once in-flight is zero.
    pub fn begin_close_and_drain(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let mut g = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            g = self
                .cv
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Signal that the close winner finished connection teardown.
    pub(crate) fn mark_close_finished(&self) {
        self.close_finished.store(true, Ordering::SeqCst);
        let _g = self
            .finish_wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.finish_cv.notify_all();
    }

    /// Wait until [`Self::mark_close_finished`] (idempotent if already finished).
    pub(crate) fn wait_close_finished(&self) {
        let mut g = self
            .finish_wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.close_finished.load(Ordering::SeqCst) {
            g = self
                .finish_cv
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// RAII permit that keeps the gate's in-flight count elevated.
#[derive(Debug)]
pub struct OpPermit {
    gate: Arc<StorageGate>,
}

impl Drop for OpPermit {
    fn drop(&mut self) {
        self.gate.leave();
    }
}
