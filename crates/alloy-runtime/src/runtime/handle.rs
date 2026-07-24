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
    pub fn config(&self) -> Result<Arc<RuntimeConfig>, RuntimeError> {
        self.inner
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| RuntimeError::Internal("runtime not configured".into()))
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

    /// Install or replace the scheduler.
    pub async fn set_scheduler(&self, sched: Arc<dyn Scheduler>) -> Result<(), RuntimeError> {
        match self.phase() {
            RuntimePhase::Configured | RuntimePhase::Running => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "set_scheduler",
                });
            }
        }
        if self.inner.run_in_flight.load(Ordering::SeqCst) {
            return Err(RuntimeError::SchedulerBusy);
        }
        *self.inner.scheduler.write().await = sched;
        let _ = self.emit(RuntimeEvent::SchedulerRegistered).await;
        Ok(())
    }

    /// Swap the event sink (RFC-0002 wires SQLite).
    ///
    /// Acquires the sink write lock (waits for in-flight `emit`/`append_session` readers).
    /// Day-1 refuses swap while the current sink still buffers events; RFC-0002 must perform
    /// an atomic lossless handoff before swapping a non-empty memory sink.
    pub async fn set_event_sink(&self, sink: Arc<dyn EventSink>) -> Result<(), RuntimeError> {
        match self.phase() {
            RuntimePhase::Configured | RuntimePhase::Running => {}
            other => {
                return Err(RuntimeError::InvalidPhase {
                    current: other,
                    op: "set_event_sink",
                });
            }
        }

        let mut guard = tokio::time::timeout(Duration::from_secs(5), self.inner.event_sink.write())
            .await
            .map_err(|_| RuntimeError::EventSinkBusy)?;

        if guard.buffered_len() > 0 {
            return Err(RuntimeError::Internal(
                "event sink handoff requires empty buffer until RFC-0002 atomic migrate".into(),
            ));
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
}
