//! Shared runtime state.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::events::{EventSink, InMemoryEventSink};
use crate::runtime::RuntimePhase;
use crate::scheduler::{NullScheduler, Scheduler};
use crate::types::ids::DagId;
use crate::types::metrics::RuntimeMetrics;

pub(crate) struct AtomicMetrics {
    pub phase_transitions: AtomicU64,
    pub runs_started: AtomicU64,
    pub runs_completed: AtomicU64,
    pub runs_failed: AtomicU64,
    pub shutdowns: AtomicU64,
}

impl AtomicMetrics {
    pub fn new() -> Self {
        Self {
            phase_transitions: AtomicU64::new(0),
            runs_started: AtomicU64::new(0),
            runs_completed: AtomicU64::new(0),
            runs_failed: AtomicU64::new(0),
            shutdowns: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            phase_transitions: self.phase_transitions.load(Ordering::Relaxed),
            runs_started: self.runs_started.load(Ordering::Relaxed),
            runs_completed: self.runs_completed.load(Ordering::Relaxed),
            runs_failed: self.runs_failed.load(Ordering::Relaxed),
            shutdowns: self.shutdowns.load(Ordering::Relaxed),
        }
    }
}

impl RuntimePhase {
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Self::Created => 0,
            Self::Configured => 1,
            Self::Starting => 2,
            Self::Running => 3,
            Self::Draining => 4,
            Self::Stopped => 5,
            Self::Failed => 6,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Created,
            1 => Self::Configured,
            2 => Self::Starting,
            3 => Self::Running,
            4 => Self::Draining,
            5 => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

/// Single-flight run bookkeeping (guarded by [`RuntimeInner::run_gate`]).
#[derive(Default)]
pub(crate) struct RunSlot {
    pub in_flight: bool,
    pub active_dag: Option<DagId>,
}

pub(crate) struct RuntimeInner {
    pub phase: AtomicU8,
    /// Std mutex so sync `configure` works inside tokio tests.
    pub config: Mutex<Option<Arc<RuntimeConfig>>>,
    pub cancel: CancellationToken,
    /// Sync RwLock so [`crate::RuntimeHandle::set_scheduler`] stays sync per RFC.
    pub scheduler: RwLock<Arc<dyn Scheduler>>,
    pub event_sink: AsyncRwLock<Arc<dyn EventSink>>,
    pub memory_sink: Arc<InMemoryEventSink>,
    /// Serializes run admission with drain phase transitions.
    pub run_gate: Mutex<RunSlot>,
    /// Fast path for drain wait loops (mirrors `run_gate.in_flight`).
    pub run_in_flight: AtomicBool,
    pub metrics: AtomicMetrics,
    pub stopped: AtomicBool,
    pub pending_configured_dir: Mutex<Option<String>>,
    /// Runtime events queued by sync APIs (e.g. `set_scheduler`) until the next async flush.
    pub pending_runtime_events: Mutex<Vec<crate::events::RuntimeEvent>>,
}

impl RuntimeInner {
    pub fn new() -> Self {
        let memory = Arc::new(InMemoryEventSink::new());
        let sink: Arc<dyn EventSink> = memory.clone();
        Self {
            phase: AtomicU8::new(RuntimePhase::Created.as_u8()),
            config: Mutex::new(None),
            cancel: CancellationToken::new(),
            scheduler: RwLock::new(Arc::new(NullScheduler)),
            event_sink: AsyncRwLock::new(sink),
            memory_sink: memory,
            run_gate: Mutex::new(RunSlot::default()),
            run_in_flight: AtomicBool::new(false),
            metrics: AtomicMetrics::new(),
            stopped: AtomicBool::new(false),
            pending_configured_dir: Mutex::new(None),
            pending_runtime_events: Mutex::new(Vec::new()),
        }
    }

    pub fn phase(&self) -> RuntimePhase {
        RuntimePhase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    pub fn set_phase(&self, next: RuntimePhase) {
        self.phase.store(next.as_u8(), Ordering::SeqCst);
        self.metrics
            .phase_transitions
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Admit a single-flight run while holding [`Self::run_gate`] (checks phase atomically).
    pub fn try_admit_run(self: &Arc<Self>, dag_id: DagId) -> Result<RunPermit, RuntimeError> {
        let mut slot = self
            .run_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let phase = self.phase();
        if phase != RuntimePhase::Running {
            return Err(RuntimeError::InvalidPhase {
                current: phase,
                op: "run",
            });
        }
        if slot.in_flight {
            return Err(RuntimeError::SchedulerBusy);
        }
        slot.in_flight = true;
        slot.active_dag = Some(dag_id);
        self.run_in_flight.store(true, Ordering::SeqCst);
        self.metrics.runs_started.fetch_add(1, Ordering::Relaxed);
        Ok(RunPermit {
            inner: Arc::clone(self),
        })
    }

    /// Enter draining while holding the run gate so no new run can admit mid-transition.
    /// Returns `(active_dag, newly_entered_draining)`.
    pub fn begin_drain(&self) -> Result<(Option<DagId>, bool), RuntimeError> {
        let slot = self
            .run_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.phase() {
            RuntimePhase::Running => {
                self.set_phase(RuntimePhase::Draining);
                Ok((slot.active_dag, true))
            }
            RuntimePhase::Draining => Ok((slot.active_dag, false)),
            other => Err(RuntimeError::InvalidPhase {
                current: other,
                op: "drain",
            }),
        }
    }
}

/// Clears single-flight state on drop (success, error, cancel, or panic).
pub(crate) struct RunPermit {
    inner: Arc<RuntimeInner>,
}

impl Drop for RunPermit {
    fn drop(&mut self) {
        let mut slot = self
            .inner
            .run_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.in_flight = false;
        slot.active_dag = None;
        self.inner.run_in_flight.store(false, Ordering::SeqCst);
    }
}
