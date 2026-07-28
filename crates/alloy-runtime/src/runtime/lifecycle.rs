//! [`AlloyRuntime`] construct / configure / start / run / drain / shutdown.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::handle::RuntimeHandle;
use super::inner::RuntimeInner;
use super::RuntimePhase;
use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
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
        let cfg = self.handle.config()?;
        let cancel = self.handle.cancellation();
        if cancel.is_cancelled() {
            return Err(RuntimeError::Internal("cancelled during start".into()));
        }

        // RFC failure table: data_dir create → RuntimeError::Io; start → Failed.
        // Observe process cancellation during async I/O so early SIGINT/SIGTERM aborts startup.
        let create = tokio::fs::create_dir_all(&cfg.data_dir);
        tokio::pin!(create);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(RuntimeError::Internal("cancelled during start".into()));
            }
            result = &mut create => {
                result.map_err(|e| {
                    std::io::Error::new(
                        e.kind(),
                        format!(
                            "create data_dir {} (rule: {}); see {}: {e}",
                            cfg.data_dir.display(),
                            cfg.data_dir_rule,
                            cfg.env_file_hint.display()
                        ),
                    )
                })?;
            }
        }

        if cancel.is_cancelled() {
            return Err(RuntimeError::Internal("cancelled during start".into()));
        }

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

        if cancel.is_cancelled() {
            return Err(RuntimeError::Internal("cancelled during start".into()));
        }

        self.handle.emit(RuntimeEvent::Started).await?;
        self.handle.inner.set_phase(RuntimePhase::Running);
        tracing::info!(data_dir = %cfg.data_dir.display(), "alloy runtime started");
        Ok(self.handle.clone())
    }

    /// Thin forwarder to [`crate::Scheduler::run`] via [`RuntimeHandle::run_dag`].
    ///
    /// Maps [`crate::SchedError::Unavailable`] → [`RuntimeError::SchedulerUnavailable`].
    /// Does **not** emit `RunAccepted` / `RunFinished`.
    pub async fn run(&self, dag_id: DagId) -> Result<DagOutcome, RuntimeError> {
        self.handle.run_dag(dag_id).await
    }

    /// Phase: `Running` → `Draining`.
    ///
    /// Amendment A1 (RFC-0010 §5.12.5, DR1): `deadline` is computed **before**
    /// awaiting `Scheduler::cancel`, and that await is itself bounded by the
    /// remaining budget — RFC-0010 §5.12 makes `cancel` genuinely block until
    /// the run's own terminal checkpoint lands, so a deadline taken only
    /// after that await returns would let a slow (or grace-exceeding) cancel
    /// consume the entire `grace` budget before the in-flight wait even
    /// starts, silently doubling the effective drain window.
    pub async fn drain(&self, grace: Duration) -> Result<(), RuntimeError> {
        let deadline = tokio::time::Instant::now() + grace; // A1: FIRST.
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
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, sched.cancel(dag_id)).await {
                Ok(Ok(())) => {}
                // DR4: a cancel error during drain is logged, never fatal —
                // the in-flight wait below still runs and can still force
                // progress via `runtime_cancel` once the grace elapses.
                Ok(Err(e)) => {
                    tracing::warn!(dag_id = %dag_id, error = %e, "drain: scheduler.cancel failed");
                }
                Err(_elapsed) => {
                    tracing::warn!(dag_id = %dag_id, "drain: scheduler.cancel timed out");
                }
            }
        }

        while self.handle.run_in_flight() {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    grace_ms = grace.as_millis(),
                    "drain grace elapsed; cancelling in-flight work"
                );
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
