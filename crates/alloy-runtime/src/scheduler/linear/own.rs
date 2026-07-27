//! Scheduler ownership: process-level `<data_dir>/scheduler.lock` (§4.5) and
//! DAG-level `OwnedDag`/`OwnedGuard` (§4.3-4.4).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::LinearScheduler;
use crate::error::SchedError;
use crate::scheduler::DagState;
use crate::types::ids::{DagId, RunId, SessionId};

/// Kept alive for the scheduler's lifetime; the advisory lock is released
/// when the file handle drops (process exit or scheduler drop).
pub(super) struct OwnershipLock {
    _file: std::fs::File,
    #[allow(dead_code)]
    // retained for future diagnostics (§4.5 L4); correctness never depends on it
    path: PathBuf,
}

impl OwnershipLock {
    /// L1-L3: create the data dir, open (never truncate) `scheduler.lock`,
    /// and take an exclusive advisory lock.
    pub(super) fn acquire(data_dir: &Path) -> Result<Self, SchedError> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| SchedError::Ownership(format!("create_dir_all: {e}")))?;
        let path = data_dir.join("scheduler.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| SchedError::Ownership(format!("open scheduler.lock: {e}")))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(std::fs::TryLockError::WouldBlock) => Err(SchedError::Ownership(format!(
                "scheduler.lock held by another process: {}",
                path.display()
            ))),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(SchedError::Ownership(format!("scheduler.lock: {e}")))
            }
        }
    }
}

/// DAG-level ownership record (§4.3). One per in-process `run`, or a
/// transient entry `cancel` inserts for an unowned DAG (§5.12.4).
pub(super) struct OwnedDag {
    /// Diagnostics only; the map key is authoritative.
    pub(super) dag_id: DagId,
    /// `run()` always populates `Some`. A cancel-side transient entry uses
    /// `None` when the run binding could not be resolved (§5.12.4 step 1/4)
    /// — the RFC's struct sketch shows a bare `RunId`, but §5.12.4 step 1
    /// explicitly allows a missing binding for the cancel path, so this
    /// field is `Option` rather than inventing a sentinel `RunId`.
    pub(super) run_id: Option<RunId>,
    pub(super) session_id: SessionId,
    /// Child of `deps.runtime_cancel` (O1); also fired by `cancel(dag_id)`.
    pub(super) run_cancel: CancellationToken,
    /// Notified exactly once when ownership is released (`OwnedGuard::drop`).
    pub(super) completed: Arc<Notify>,
    /// Set before `completed.notify_waiters()` (O3). `None` only while the
    /// owner is still running.
    pub(super) cancel_result: Mutex<Option<Result<DagState, SchedError>>>,
}

impl OwnedDag {
    /// O3: record the terminal result. MUST be called before the owning
    /// `OwnedGuard` drops (guard `Drop` does not write this itself — G3
    /// forbids `Drop` from doing anything but map-removal + notify).
    pub(super) fn set_cancel_result(&self, result: Result<DagState, SchedError>) {
        let mut slot = self
            .cancel_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(result);
    }

    /// O4: race-free wait for `completed`, bounded by `deadline`. The
    /// **normative** pattern (§4.3): subscribe to `notified()` *before*
    /// re-checking `cancel_result`, never after — a single
    /// `notified().await` issued after the writer already called
    /// `notify_waiters` would hang forever (tokio `Notify` has no
    /// "already fired" memory across a subscribe that starts later).
    pub(super) async fn wait_for_completion(
        &self,
        deadline: tokio::time::Instant,
    ) -> Option<Result<DagState, SchedError>> {
        loop {
            if let Some(r) = self.snapshot_cancel_result() {
                return Some(r);
            }
            let notified = self.completed.notified(); // subscribe BEFORE re-check
            if let Some(r) = self.snapshot_cancel_result() {
                return Some(r);
            }
            tokio::select! {
                () = notified => continue,
                () = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    fn snapshot_cancel_result(&self) -> Option<Result<DagState, SchedError>> {
        self.cancel_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// RAII: releases ownership and notifies waiters on every exit path,
/// including panic unwind (§4.4).
pub(super) struct OwnedGuard<'a> {
    sched: &'a LinearScheduler,
    dag_id: DagId,
    pub(super) owned: Arc<OwnedDag>,
}

impl Drop for OwnedGuard<'_> {
    fn drop(&mut self) {
        // G3: no `.await`, no store I/O — map removal + notify only.
        if let Ok(mut map) = self.sched.owned.lock() {
            map.remove(&self.dag_id);
        }
        self.owned.completed.notify_waiters(); // G2: even if `cancel_result` is still `None`
    }
}

impl LinearScheduler {
    /// R4 / §5.12.4 step 2: insert-if-absent DAG ownership.
    ///
    /// `run_id = None` only for the cancel-side transient entry (§5.12.4).
    pub(super) fn try_acquire_dag(
        &self,
        dag_id: DagId,
        run_id: Option<RunId>,
        session_id: SessionId,
    ) -> Result<OwnedGuard<'_>, SchedError> {
        let owned_dag = Arc::new(OwnedDag {
            dag_id,
            run_id,
            session_id,
            run_cancel: self.deps.runtime_cancel.child_token(), // O1
            completed: Arc::new(Notify::new()),
            cancel_result: Mutex::new(None),
        });
        {
            let mut map = self
                .owned
                .lock()
                .map_err(|_| SchedError::Ownership("ownership map poisoned".into()))?;
            if map.contains_key(&dag_id) {
                return Err(SchedError::AlreadyOwned(dag_id));
            }
            map.insert(dag_id, Arc::clone(&owned_dag));
        }
        tracing::debug!(
            dag_id = %owned_dag.dag_id,
            run_id = ?owned_dag.run_id,
            session_id = %owned_dag.session_id,
            "dag ownership acquired"
        );
        Ok(OwnedGuard {
            sched: self,
            dag_id,
            owned: owned_dag,
        })
    }

    /// Look up the live `OwnedDag` for `dag_id`, if this process owns it
    /// (a real `run()` or a transient cancel-side entry).
    pub(super) fn lookup_owned(&self, dag_id: DagId) -> Result<Option<Arc<OwnedDag>>, SchedError> {
        let map = self
            .owned
            .lock()
            .map_err(|_| SchedError::Ownership("ownership map poisoned".into()))?;
        Ok(map.get(&dag_id).cloned())
    }

    /// §5.12.3 grace budget: `cancel_drain_grace + cancel_write_grace`.
    pub(super) fn cancel_grace(&self) -> Duration {
        self.deps.config.cancel_drain_grace + self.deps.config.cancel_write_grace
    }

    /// O4 race-free wait, bumping `SchedulerMetrics` and mapping the result
    /// per the §5.12.3 return table. Shared by `cancel_impl`'s owned path
    /// and its "ownership contended" fallback (a `run()` won the race).
    pub(super) async fn wait_for_cancel_result(&self, owned: &OwnedDag) -> Result<(), SchedError> {
        let started = tokio::time::Instant::now();
        let deadline = started + self.cancel_grace();
        match owned.wait_for_completion(deadline).await {
            Some(Ok(_state)) => {
                if started.elapsed() > self.deps.config.cancel_drain_grace {
                    self.metrics.inc_forced_cancel_writes();
                }
                Ok(())
            }
            Some(Err(e)) => Err(e), // CN4: Conflict / Store
            None => Err(SchedError::Internal("cancel drain grace exceeded".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::types::ids::DagId;

    // ----- helpers -----

    fn fresh() -> OwnedDag {
        OwnedDag {
            dag_id: DagId::new(),
            run_id: None,
            session_id: SessionId::new(),
            run_cancel: CancellationToken::new(),
            completed: Arc::new(Notify::new()),
            cancel_result: Mutex::new(None),
        }
    }

    // ----- happy path -----

    #[tokio::test(start_paused = true)]
    async fn wait_for_completion_returns_immediately_when_already_set() {
        // O4's core hazard: a terminal result written (and `notify_waiters`
        // fired) *before* the waiter ever subscribes must still resolve —
        // not hang until the deadline. This is the scenario a single
        // `notified().await` (subscribe-after-check) gets wrong.
        let owned = fresh();
        owned.set_cancel_result(Ok(DagState::Cancelled));
        owned.completed.notify_waiters(); // as `OwnedGuard::drop` would.

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            owned.wait_for_completion(deadline),
        )
        .await
        .expect("must not hang past the deadline check — it should return immediately");
        assert!(matches!(result, Some(Ok(DagState::Cancelled))));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_completion_wakes_on_later_notify() {
        let owned = Arc::new(fresh());
        let writer = Arc::clone(&owned);
        let waiter = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            owned.wait_for_completion(deadline).await
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        writer.set_cancel_result(Ok(DagState::Failed));
        writer.completed.notify_waiters();

        let result = waiter.await.unwrap();
        assert!(matches!(result, Some(Ok(DagState::Failed))));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_completion_propagates_err_result() {
        let owned = fresh();
        owned.set_cancel_result(Err(SchedError::Conflict {
            dag_id: DagId::new(),
        }));
        owned.completed.notify_waiters();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let result = owned.wait_for_completion(deadline).await;
        assert!(matches!(result, Some(Err(SchedError::Conflict { .. }))));
    }

    // ----- error paths -----

    #[tokio::test(start_paused = true)]
    async fn wait_for_completion_times_out_with_none_when_never_notified() {
        let owned = fresh();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let result = owned.wait_for_completion(deadline).await;
        assert!(result.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_completion_ignores_a_notify_with_no_result_yet() {
        // A stray `notify_waiters()` with `cancel_result` still `None` (e.g.
        // a spurious wake) must not be mistaken for completion — the loop
        // re-checks and keeps waiting until the deadline.
        let owned = Arc::new(fresh());
        let writer = Arc::clone(&owned);
        let waiter = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            owned.wait_for_completion(deadline).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        writer.completed.notify_waiters(); // spurious: no result set

        let result = waiter.await.unwrap();
        assert!(
            result.is_none(),
            "spurious notify must not fabricate a result"
        );
    }
}
