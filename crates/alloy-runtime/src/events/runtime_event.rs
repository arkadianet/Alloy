//! Host-only runtime events (not Appendix A).

use serde::{Deserialize, Serialize};

use crate::scheduler::DagOutcome;
use crate::types::ids::{DagId, RunId};

/// Host lifecycle events emitted via [`super::EventSink`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// Runtime configured.
    Configured {
        /// Data directory path.
        data_dir: String,
    },
    /// Runtime started.
    Started,
    /// Scheduler installed/replaced.
    SchedulerRegistered,
    /// Emitted by Session/RunController (RFC-0003), not by `AlloyRuntime::run`.
    RunAccepted {
        /// Run id.
        run_id: RunId,
        /// DAG id.
        dag_id: DagId,
    },
    /// Emitted by Session/RunController when a run completes.
    RunFinished {
        /// Run id.
        run_id: RunId,
        /// Outcome.
        outcome: DagOutcome,
    },
    /// Drain started.
    DrainStarted {
        /// Grace period milliseconds.
        grace_ms: u64,
    },
    /// Runtime stopped.
    Stopped,
    /// Fatal failure.
    Failed {
        /// Error message.
        error: String,
    },
}
