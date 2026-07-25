//! Shared session-plane state.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard};

use super::gates::GateWaiterRegistry;
use super::metrics::AtomicSessionMetrics;
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
    pub live_execution: StdMutex<HashSet<RunId>>,
    /// Runs that emitted `RunAccepted` in this process.
    pub accepted_emitted: StdMutex<HashSet<RunId>>,
    pub gates: GateWaiterRegistry,
    pub metrics: AtomicSessionMetrics,
}

impl SessionInner {
    pub fn new(handle: RuntimeHandle, storage: Arc<AlloyStorage>) -> Self {
        Self {
            handle,
            storage,
            session_locks: StdMutex::new(HashMap::new()),
            run_locks: StdMutex::new(HashMap::new()),
            live_execution: StdMutex::new(HashSet::new()),
            accepted_emitted: StdMutex::new(HashSet::new()),
            gates: GateWaiterRegistry::new(),
            metrics: AtomicSessionMetrics::new(),
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

    pub fn has_live(&self, id: RunId) -> bool {
        self.live_execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
    }

    pub fn set_live(&self, id: RunId, live: bool) {
        let mut set = self
            .live_execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if live {
            set.insert(id);
        } else {
            set.remove(&id);
        }
    }

    pub fn mark_accepted_emitted(&self, id: RunId) {
        self.accepted_emitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub fn was_accepted_emitted(&self, id: RunId) -> bool {
        self.accepted_emitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
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
        // map + self.arc => strong_count == 2 means no other holders
        if Arc::strong_count(&self.arc) == 2 {
            let mut map = self
                .inner
                .session_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if map
                .get(&self.id)
                .is_some_and(|e| Arc::ptr_eq(e, &self.arc) && Arc::strong_count(e) == 2)
            {
                map.remove(&self.id);
            }
        }
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
    ///
    /// The ticket's own `Drop` cannot evict the map entry here: the returned
    /// [`RunLock`] holds another clone of the same `Arc`.
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
    if Arc::strong_count(arc) == 2 {
        let mut map = inner
            .run_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map
            .get(&id)
            .is_some_and(|e| Arc::ptr_eq(e, arc) && Arc::strong_count(e) == 2)
        {
            map.remove(&id);
        }
    }
}
