//! Shared runtime state.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::RuntimeConfig;
use crate::events::{EventSink, InMemoryEventSink};
use crate::runtime::RuntimePhase;
use crate::scheduler::{NullScheduler, Scheduler};
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

pub(crate) struct RuntimeInner {
    pub phase: AtomicU8,
    /// Std mutex so sync `configure` works inside tokio tests without `blocking_write`.
    pub config: Mutex<Option<Arc<RuntimeConfig>>>,
    pub cancel: CancellationToken,
    pub scheduler: RwLock<Arc<dyn Scheduler>>,
    pub event_sink: RwLock<Arc<dyn EventSink>>,
    pub memory_sink: Arc<InMemoryEventSink>,
    pub run_in_flight: AtomicBool,
    pub metrics: AtomicMetrics,
    pub stopped: AtomicBool,
    pub pending_configured_dir: Mutex<Option<String>>,
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
            event_sink: RwLock::new(sink),
            memory_sink: memory,
            run_in_flight: AtomicBool::new(false),
            metrics: AtomicMetrics::new(),
            stopped: AtomicBool::new(false),
            pending_configured_dir: Mutex::new(None),
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
}
