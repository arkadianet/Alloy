//! Shared session-plane state.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard};

use super::gates::GateWaiterRegistry;
use super::metrics::AtomicSessionMetrics;
use super::run_executor::{DirectRunExecutor, RunExecutor};
use super::run_state::RunControlState;
use crate::runtime::RuntimeHandle;
use crate::storage::AlloyStorage;
use crate::types::ids::{RunId, SessionId};

/// Shared inner for [`super::SessionPlane`].
pub(crate) struct SessionInner {
    pub handle: RuntimeHandle,
    pub storage: Arc<AlloyStorage>,
    session_locks: StdMutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    run_locks: StdMutex<HashMap<RunId, Arc<Mutex<()>>>>,
    /// Execution lease: run ids with outstanding `run_dag` awaits.
    live_execution: StdMutex<HashSet<RunId>>,
    /// Runs that emitted `RunAccepted` in this process (pruned on terminal).
    accepted_emitted: StdMutex<HashSet<RunId>>,
    pub gates: GateWaiterRegistry,
    pub metrics: AtomicSessionMetrics,
    /// RFC-0003 §6.3 step-8 execution seam (RFC-0017 AM-0003-2). Defaults to
    /// [`DirectRunExecutor`]; the assembly may swap in a generation driver
    /// via [`super::SessionPlane::set_executor`] before dispatching runs.
    executor: StdMutex<Arc<dyn RunExecutor>>,
    /// Test-only: next `upsert_run` via control plane fails once.
    #[cfg(test)]
    pub(crate) fail_next_run_upsert: AtomicBool,
    /// Test-only: next session-event append via control plane fails once.
    #[cfg(test)]
    pub(crate) fail_next_append: AtomicBool,
}

impl SessionInner {
    pub fn new(handle: RuntimeHandle, storage: Arc<AlloyStorage>) -> Self {
        let executor: Arc<dyn RunExecutor> = Arc::new(DirectRunExecutor::new(handle.clone()));
        Self {
            handle,
            storage,
            session_locks: StdMutex::new(HashMap::new()),
            run_locks: StdMutex::new(HashMap::new()),
            live_execution: StdMutex::new(HashSet::new()),
            accepted_emitted: StdMutex::new(HashSet::new()),
            gates: GateWaiterRegistry::new(),
            metrics: AtomicSessionMetrics::new(),
            executor: StdMutex::new(executor),
            #[cfg(test)]
            fail_next_run_upsert: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_append: AtomicBool::new(false),
        }
    }

    pub async fn lock_session(self: &Arc<Self>, id: SessionId) -> SessionLock {
        let arc = {
            let mut map = self
                .session_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.entry(id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let guard = arc.clone().lock_owned().await;
        SessionLock {
            inner: Arc::clone(self),
            id,
            arc,
            guard: Some(guard),
        }
    }

    pub async fn lock_run(self: &Arc<Self>, id: RunId) -> RunLock {
        let arc = {
            let mut map = self
                .run_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.entry(id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let guard = arc.clone().lock_owned().await;
        RunLock {
            inner: Arc::clone(self),
            id,
            arc,
            guard: Some(guard),
        }
    }

    /// Current step-8 executor (AM-0003-2).
    pub fn executor(&self) -> Arc<dyn RunExecutor> {
        self.executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the step-8 executor (assembly-time injection, rule RX4).
    pub fn set_executor(&self, executor: Arc<dyn RunExecutor>) {
        *self
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = executor;
    }

    pub fn has_live(&self, id: RunId) -> bool {
        self.live_execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
    }

    /// Acquire an RAII execution lease (cleared on [`Drop`] if not released).
    pub fn acquire_lease(self: &Arc<Self>, id: RunId) -> ExecutionLease {
        self.live_execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
        ExecutionLease {
            inner: Arc::clone(self),
            id,
            armed: true,
        }
    }

    /// Clear a lease by id (e.g. `cancel` while `start` may still hold a guard).
    pub fn clear_lease(&self, id: RunId) {
        self.live_execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    pub fn mark_accepted_emitted(&self, id: RunId) {
        self.accepted_emitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub fn clear_accepted_emitted(&self, id: RunId) {
        self.accepted_emitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// True if this process emitted `RunAccepted` **or** durable state has left `Created`.
    pub fn was_accepted(&self, run: RunId, durable: RunControlState) -> bool {
        durable != RunControlState::Created
            || self
                .accepted_emitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&run)
    }

    /// Mark terminal: prune process-local accepted tracking.
    pub fn on_terminal(&self, run: RunId) {
        self.clear_accepted_emitted(run);
        self.clear_lease(run);
    }

    #[cfg(test)]
    pub(crate) fn take_fail_run_upsert(&self) -> bool {
        self.fail_next_run_upsert.swap(false, Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn take_fail_append(&self) -> bool {
        self.fail_next_append.swap(false, Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn run_lock_map_len(&self) -> usize {
        self.run_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub(crate) fn session_lock_map_len(&self) -> usize {
        self.session_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Clears `live_execution` on drop unless [`Self::disarm`] was called after a durable transition.
pub(crate) struct ExecutionLease {
    inner: Arc<SessionInner>,
    id: RunId,
    armed: bool,
}

impl ExecutionLease {
    /// Drop without clearing (caller already cleared via [`SessionInner::on_terminal`] / `clear_lease`).
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        if self.armed {
            self.inner.clear_lease(self.id);
        }
    }
}

/// Per-session lock guard with map eviction on drop.
pub(crate) struct SessionLock {
    inner: Arc<SessionInner>,
    id: SessionId,
    arc: Arc<Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        self.guard.take();
        evict_lock_map_entry(&self.inner.session_locks, &self.id, &self.arc);
    }
}

/// Per-run lock guard with map eviction on drop.
pub(crate) struct RunLock {
    inner: Arc<SessionInner>,
    id: RunId,
    arc: Arc<Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl RunLock {
    /// Drop the mutex guard while retaining the Arc for later eviction.
    pub fn unlock(mut self) -> RunLockTicket {
        self.guard.take();
        RunLockTicket {
            inner: Arc::clone(&self.inner),
            id: self.id,
            arc: Arc::clone(&self.arc),
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        self.guard.take();
        evict_run(&self.inner, self.id, &self.arc);
    }
}

/// Holds the lock-map Arc after releasing the mutex (between critical sections).
pub(crate) struct RunLockTicket {
    inner: Arc<SessionInner>,
    id: RunId,
    arc: Arc<Mutex<()>>,
}

impl RunLockTicket {
    /// Re-acquire the same run mutex.
    pub async fn relock(self) -> RunLock {
        let guard = Arc::clone(&self.arc).lock_owned().await;
        RunLock {
            inner: Arc::clone(&self.inner),
            id: self.id,
            arc: Arc::clone(&self.arc),
            guard: Some(guard),
        }
    }
}

impl Drop for RunLockTicket {
    fn drop(&mut self) {
        evict_run(&self.inner, self.id, &self.arc);
    }
}

fn evict_run(inner: &SessionInner, id: RunId, arc: &Arc<Mutex<()>>) {
    evict_lock_map_entry(&inner.run_locks, &id, arc);
}

/// Drop-path eviction shared by session and run lock maps.
///
/// Removes the entry only when this Arc is the last map+guard pair (`strong_count == 2`),
/// re-validating under the map lock with [`Arc::ptr_eq`] so a concurrent re-insert is kept.
fn evict_lock_map_entry<K>(map: &StdMutex<HashMap<K, Arc<Mutex<()>>>>, id: &K, arc: &Arc<Mutex<()>>)
where
    K: Eq + Hash,
{
    if Arc::strong_count(arc) == 2 {
        let mut map = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map
            .get(id)
            .is_some_and(|e| Arc::ptr_eq(e, arc) && Arc::strong_count(e) == 2)
        {
            map.remove(id);
        }
    }
}
