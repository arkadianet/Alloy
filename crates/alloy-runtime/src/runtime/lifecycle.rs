//! [`AlloyRuntime`] construct / configure / start / run / drain / shutdown.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::handle::RuntimeHandle;
use super::inner::RuntimeInner;
use super::RuntimePhase;
use crate::config::RuntimeConfig;
use crate::error::{RuntimeError, SchedError};
use crate::events::RuntimeEvent;
use crate::logging;
use crate::scheduler::DagOutcome;
use crate::types::ids::DagId;

/// In-process Alloy execution host (RFC-0001).
pub struct AlloyRuntime {
    handle: RuntimeHandle,
    /// Set once shutdown completes so Drop does not warn.
    shut_down: bool,
}

impl AlloyRuntime {
    /// Phase: `Created`. No I/O.
    #[must_use]
    pub fn new() -> Self {
        let inner = Arc::new(RuntimeInner::new());
        Self {
            handle: RuntimeHandle::new(inner),
            shut_down: false,
        }
    }

    /// Borrow the process handle (also returned from [`Self::start`]).
    #[must_use]
    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }

    /// Phase: `Created` → `Configured`. Sync; never writes `.env`.
    pub fn configure(&mut self, cfg: RuntimeConfig) -> Result<&mut Self, RuntimeError> {
        let phase = self.handle.phase();
        if phase != RuntimePhase::Created {
            return Err(RuntimeError::InvalidPhase {
                current: phase,
                op: "configure",
            });
        }
        let data_dir = cfg.data_dir.display().to_string();
        let inner = self.handle.inner.clone();
        *inner
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(cfg));
        *inner
            .pending_configured_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(data_dir);
        inner.set_phase(RuntimePhase::Configured);
        Ok(self)
    }

    /// Phase: `Configured` → `Starting` → `Running`.
    pub async fn start(&mut self) -> Result<RuntimeHandle, RuntimeError> {
        let phase = self.handle.phase();
        if phase != RuntimePhase::Configured {
            return Err(RuntimeError::InvalidPhase {
                current: phase,
                op: "start",
            });
        }
        self.handle.inner.set_phase(RuntimePhase::Starting);

        match self.start_inner().await {
            Ok(handle) => Ok(handle),
            Err(e) => {
                self.handle.inner.set_phase(RuntimePhase::Failed);
                let _ = self
                    .handle
                    .emit(RuntimeEvent::Failed {
                        error: e.to_string(),
                    })
                    .await;
                Err(e)
            }
        }
    }

    async fn start_inner(&mut self) -> Result<RuntimeHandle, RuntimeError> {
        logging::init_tracing();
        let cfg = self.handle.config();
        tokio::fs::create_dir_all(&cfg.data_dir)
            .await
            .map_err(|e| {
                RuntimeError::Config(format!(
                    "create data_dir {} (rule: {}); see {}: {e}",
                    cfg.data_dir.display(),
                    cfg.data_dir_rule,
                    cfg.env_file_hint.display()
                ))
            })?;

        let pending = self
            .handle
            .inner
            .pending_configured_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(dir) = pending {
            self.handle
                .emit(RuntimeEvent::Configured { data_dir: dir })
                .await?;
        }

        self.handle.emit(RuntimeEvent::Started).await?;
        self.handle.inner.set_phase(RuntimePhase::Running);
        tracing::info!(data_dir = %cfg.data_dir.display(), "alloy runtime started");
        Ok(self.handle.clone())
    }

    /// Thin forwarder to [`crate::Scheduler::run`].
    ///
    /// Maps [`SchedError::Unavailable`] → [`RuntimeError::SchedulerUnavailable`].
    /// Does **not** emit `RunAccepted` / `RunFinished`.
    pub async fn run(&self, dag_id: DagId) -> Result<DagOutcome, RuntimeError> {
        let permit = self.handle.inner.try_admit_run(dag_id)?;
        let sched = self.handle.scheduler();
        let result = sched.run(dag_id).await;
        // Drop permit before mapping so busy state clears even if we return early.
        drop(permit);

        match result {
            Ok(outcome) => {
                self.handle
                    .inner
                    .metrics
                    .runs_completed
                    .fetch_add(1, Ordering::Relaxed);
                Ok(outcome)
            }
            Err(SchedError::Unavailable) => {
                self.handle
                    .inner
                    .metrics
                    .runs_failed
                    .fetch_add(1, Ordering::Relaxed);
                Err(RuntimeError::SchedulerUnavailable)
            }
            Err(e) => {
                self.handle
                    .inner
                    .metrics
                    .runs_failed
                    .fetch_add(1, Ordering::Relaxed);
                Err(RuntimeError::Scheduler(e))
            }
        }
    }

    /// Phase: `Running` → `Draining`.
    pub async fn drain(&self, grace: Duration) -> Result<(), RuntimeError> {
        let (active_dag, newly) = self.handle.inner.begin_drain()?;
        if newly {
            let _ = self
                .handle
                .emit(RuntimeEvent::DrainStarted {
                    grace_ms: u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
                })
                .await;
        }

        let sched = self.handle.scheduler();
        if let Some(dag_id) = active_dag {
            let _ = sched.cancel(dag_id).await;
        }

        let start = tokio::time::Instant::now();
        while self.handle.run_in_flight() {
            if start.elapsed() >= grace {
                self.handle.cancellation().cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.handle.run_in_flight() {
            self.handle.cancellation().cancel();
        }
        Ok(())
    }

    /// Phase → `Stopped`. Consumes `self`.
    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        match self.handle.phase() {
            RuntimePhase::Created
            | RuntimePhase::Configured
            | RuntimePhase::Running
            | RuntimePhase::Draining
            | RuntimePhase::Failed => {}
            RuntimePhase::Starting => {
                return Err(RuntimeError::InvalidPhase {
                    current: RuntimePhase::Starting,
                    op: "shutdown",
                });
            }
            RuntimePhase::Stopped => return Err(RuntimeError::AlreadyStopped),
        }

        if self.handle.phase() == RuntimePhase::Running {
            let _ = self.drain(Duration::from_secs(1)).await;
        }

        self.handle.cancellation().cancel();
        if !matches!(self.handle.phase(), RuntimePhase::Created) {
            let _ = self.handle.emit(RuntimeEvent::Stopped).await;
        }
        self.handle.inner.set_phase(RuntimePhase::Stopped);
        self.handle
            .inner
            .metrics
            .shutdowns
            .fetch_add(1, Ordering::Relaxed);
        self.handle.inner.stopped.store(true, Ordering::SeqCst);
        self.shut_down = true;
        tracing::info!("alloy runtime stopped");
        Ok(())
    }
}

impl Default for AlloyRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AlloyRuntime {
    fn drop(&mut self) {
        if !self.shut_down && !self.handle.inner.stopped.load(Ordering::SeqCst) {
            tracing::warn!("AlloyRuntime dropped without shutdown");
            self.handle.cancellation().cancel();
        }
    }
}
