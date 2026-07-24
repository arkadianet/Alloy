//! [`RuntimeHandle`] — cheap cloneable process handle.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::inner::RuntimeInner;
use super::RuntimePhase;
use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::events::{EventSink, InMemoryEventSink, NewSessionEvent, RuntimeEvent};
use crate::scheduler::Scheduler;
use crate::types::ids::EventSeq;
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
    /// # Panics
    /// Panics if called before [`crate::AlloyRuntime::configure`].
    #[must_use]
    pub fn config(&self) -> Arc<RuntimeConfig> {
        self.inner
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("RuntimeHandle::config requires configure()")
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
        // Best-effort registration event; ignore if no runtime / sink busy.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let this = self.clone();
            handle.spawn(async move {
                let _ = this.emit(RuntimeEvent::SchedulerRegistered).await;
            });
        }
        Ok(())
    }

    /// Swap the event sink (RFC-0002 wires SQLite).
    ///
    /// Acquires the sink write lock (waits for in-flight `emit`/`append_session` readers).
    /// Day-1 refuses swap while the default in-memory buffer is non-empty.
    pub async fn set_event_sink(&self, sink: Arc<dyn EventSink>) -> Result<(), RuntimeError> {
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

    /// Emit a host-level [`RuntimeEvent`] (holds sink read lock across append).
    pub async fn emit(&self, ev: RuntimeEvent) -> Result<(), RuntimeError> {
        let sink = self.inner.event_sink.read().await;
        sink.append_runtime(ev).await?;
        Ok(())
    }

    /// Append a session event through the active sink (per-session seq).
    pub async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, RuntimeError> {
        let sink = self.inner.event_sink.read().await;
        Ok(sink.append_session(ev).await?)
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
