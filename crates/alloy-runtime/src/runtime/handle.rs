//! [`RuntimeHandle`] — cheap cloneable process handle.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::inner::RuntimeInner;
use super::RuntimePhase;
use crate::config::RuntimeConfig;
use crate::error::{RuntimeError, SchedError};
use crate::events::{EventSink, HandoffSnapshot, InMemoryEventSink, NewSessionEvent, RuntimeEvent};
use crate::scheduler::{DagOutcome, Scheduler};
use crate::storage::StoreError;
use crate::types::ids::{DagId, EventSeq};
use crate::types::metrics::RuntimeMetrics;

/// Process-wide handle injected into Session / Scheduler / workers.
#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) inner: Arc<RuntimeInner>,
}

impl RuntimeHandle {
    pub(crate) fn new(inner: Arc<RuntimeInner>) -> Self {
        Self { inner }
    }

    /// Current phase.
    #[must_use]
    pub fn phase(&self) -> RuntimePhase {
        self.inner.phase()
    }

    /// Process cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Clone of the configured runtime config.
    ///
    /// Returns [`RuntimeError::InvalidPhase`] if called before
    /// [`crate::AlloyRuntime::configure`].
    pub fn config(&self) -> Result<Arc<RuntimeConfig>, RuntimeError> {
        self.inner
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(RuntimeError::InvalidPhase {
                current: self.phase(),
                op: "config",
            })
    }

    /// Snapshot of runtime counters.
    #[must_use]
    pub fn metrics(&self) -> RuntimeMetrics {
        self.inner.metrics.snapshot()
    }

    /// Access the default in-memory sink (tests / RFC-0002 handoff prep).
    #[must_use]
    pub fn memory_sink(&self) -> Arc<InMemoryEventSink> {
        self.inner.memory_sink.clone()
    }

    /// Install or replace the scheduler (sync per RFC-0001).
    ///
    /// Queues [`RuntimeEvent::SchedulerRegistered`] for the next async flush so the
    /// sync API never spawns unsupervised tasks or reorders the event log.
    pub fn set_scheduler(&self, sched: Arc<dyn Scheduler>) -> Result<(), RuntimeError> {
        let slot = self
            .inner
            .run_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.phase() {
            RuntimePhase::Configured | RuntimePhase::Running => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "set_scheduler",
                });
            }
        }
        if slot.in_flight {
            return Err(RuntimeError::SchedulerBusy);
        }
        *self
            .inner
            .scheduler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = sched;
        drop(slot);
        self.inner
            .pending_runtime_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RuntimeEvent::SchedulerRegistered);
        Ok(())
    }

    /// Swap the event sink (RFC-0002 wires SQLite).
    ///
    /// Acquires the sink write lock (waits for in-flight `emit`/`append_session` readers).
    /// Day-1 refuses swap while the default in-memory buffer is non-empty.
    /// Non-empty lossless path is **only** [`Self::handoff_event_sink`].
    pub async fn set_event_sink(&self, sink: Arc<dyn EventSink>) -> Result<(), RuntimeError> {
        self.flush_pending_runtime_events().await?;

        let mut guard = tokio::time::timeout(Duration::from_secs(5), self.inner.event_sink.write())
            .await
            .map_err(|_| RuntimeError::EventSinkBusy)?;

        // Re-check phase under the write lock so drain/shutdown cannot race past admission.
        match self.phase() {
            RuntimePhase::Configured | RuntimePhase::Running => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "set_event_sink",
                });
            }
        }

        let mem: Arc<dyn EventSink> = self.inner.memory_sink.clone();
        if Arc::ptr_eq(&*guard, &mem) && self.inner.memory_sink.buffered_len() > 0 {
            return Err(RuntimeError::EventSinkBusy);
        }
        *guard = sink;
        Ok(())
    }

    /// Atomic lossless handoff from the default [`InMemoryEventSink`] to `sink`.
    ///
    /// Phase: `Configured` | `Running` only. Does not change the [`EventSink`] trait.
    ///
    /// Under the sink write lock (no concurrent `emit` / `append_session`):
    /// 1. Flush pending runtime events into the current sink.
    /// 2. If current sink is not the process `memory_sink`, behave like [`Self::set_event_sink`]
    ///    (swap only; no drain).
    /// 3. If memory buffer empty: swap and return.
    /// 4. Else: drain → import+verify (caller) → swap only on success.
    ///
    /// On import failure: restore memory snapshot; keep memory as active sink.
    pub async fn handoff_event_sink<F, Fut>(
        &self,
        sink: Arc<dyn EventSink>,
        import: F,
    ) -> Result<(), RuntimeError>
    where
        F: FnOnce(HandoffSnapshot) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), StoreError>> + Send,
    {
        self.flush_pending_runtime_events().await?;

        let mut guard = tokio::time::timeout(Duration::from_secs(5), self.inner.event_sink.write())
            .await
            .map_err(|_| RuntimeError::EventSinkBusy)?;

        match self.phase() {
            RuntimePhase::Configured | RuntimePhase::Running => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "handoff_event_sink",
                });
            }
        }

        // Re-flush under write lock if sync APIs queued more events.
        {
            let mut pending = {
                let mut q = self
                    .inner
                    .pending_runtime_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::take(&mut *q)
            };
            Self::drain_pending_into_sink(
                (*guard).as_ref(),
                &mut pending,
                &self.inner.pending_runtime_events,
            )
            .await?;
        }

        let mem: Arc<dyn EventSink> = self.inner.memory_sink.clone();
        if !Arc::ptr_eq(&*guard, &mem) {
            // Already durable / non-memory: swap only (same as set_event_sink).
            *guard = sink;
            return Ok(());
        }

        if self.inner.memory_sink.buffered_len() == 0 {
            *guard = sink;
            return Ok(());
        }

        let snap = self.inner.memory_sink.drain_for_handoff();
        match import(snap.clone()).await {
            Ok(()) => {
                *guard = sink;
                Ok(())
            }
            Err(e) => {
                self.inner.memory_sink.restore_handoff_snapshot(snap);
                // Keep memory as active sink — do not swap.
                Err(crate::storage::store_to_runtime(e))
            }
        }
    }

    /// Emit a host-level [`RuntimeEvent`] (holds sink read lock across append).
    pub async fn emit(&self, ev: RuntimeEvent) -> Result<(), RuntimeError> {
        self.flush_pending_runtime_events().await?;
        let sink = self.inner.event_sink.read().await;
        sink.append_runtime(ev).await?;
        Ok(())
    }

    /// Append a session event through the active sink (per-session seq).
    pub async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, RuntimeError> {
        self.flush_pending_runtime_events().await?;
        let sink = self.inner.event_sink.read().await;
        Ok(sink.append_session(ev).await?)
    }

    /// Thin DAG forwarder shared with [`crate::AlloyRuntime::run`].
    ///
    /// Single-flight admit; maps [`SchedError::Unavailable`] →
    /// [`RuntimeError::SchedulerUnavailable`]. Does **not** emit
    /// `RunAccepted` / `RunFinished`.
    pub async fn run_dag(&self, dag_id: DagId) -> Result<DagOutcome, RuntimeError> {
        self.flush_pending_runtime_events().await?;
        let permit = self.inner.try_admit_run(dag_id)?;
        let sched = self.scheduler();
        let result = sched.run(dag_id).await;
        drop(permit);

        match result {
            Ok(outcome) => {
                self.inner
                    .metrics
                    .runs_completed
                    .fetch_add(1, Ordering::Relaxed);
                Ok(outcome)
            }
            Err(SchedError::Unavailable) => {
                self.inner
                    .metrics
                    .runs_failed
                    .fetch_add(1, Ordering::Relaxed);
                Err(RuntimeError::SchedulerUnavailable)
            }
            Err(e) => {
                self.inner
                    .metrics
                    .runs_failed
                    .fetch_add(1, Ordering::Relaxed);
                Err(RuntimeError::Scheduler(e))
            }
        }
    }

    /// Cancel via the current [`Scheduler`] (NullScheduler: `Ok(())`).
    ///
    /// Phase: `Running` | `Draining`. Does not emit session events.
    pub async fn cancel_dag(&self, dag_id: DagId) -> Result<(), RuntimeError> {
        match self.phase() {
            RuntimePhase::Running | RuntimePhase::Draining => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "cancel_dag",
                });
            }
        }
        self.scheduler()
            .cancel(dag_id)
            .await
            .map_err(RuntimeError::Scheduler)
    }

    /// Drain sync-queued host events into the sink in FIFO order.
    pub(crate) async fn flush_pending_runtime_events(&self) -> Result<(), RuntimeError> {
        let mut pending = {
            let mut q = self
                .inner
                .pending_runtime_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *q)
        };
        if pending.is_empty() {
            return Ok(());
        }
        let sink = self.inner.event_sink.read().await;
        Self::drain_pending_into_sink(
            (*sink).as_ref(),
            &mut pending,
            &self.inner.pending_runtime_events,
        )
        .await
    }

    /// Append pending events from the front; on failure restore failed+remaining into `queue`.
    async fn drain_pending_into_sink(
        sink: &dyn EventSink,
        pending: &mut Vec<RuntimeEvent>,
        queue: &Mutex<Vec<RuntimeEvent>>,
    ) -> Result<(), RuntimeError> {
        while !pending.is_empty() {
            let ev = pending.remove(0);
            if let Err(e) = sink.append_runtime(ev.clone()).await {
                pending.insert(0, ev);
                let mut q = queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut restored = std::mem::take(pending);
                restored.append(&mut *q);
                *q = restored;
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub(crate) fn scheduler(&self) -> Arc<dyn Scheduler> {
        self.inner
            .scheduler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn run_in_flight(&self) -> bool {
        self.inner.run_in_flight.load(Ordering::SeqCst)
    }
}
