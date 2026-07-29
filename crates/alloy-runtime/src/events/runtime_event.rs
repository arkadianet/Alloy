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
    /// An audit record could not be persisted (issue #22). The failing
    /// `DecisionLog` call still returns its error; this event exists so a
    /// caller that swallows that error (per RFC-0006 §5.9, obs never
    /// changes a tool call's return value) leaves a durable trace instead
    /// of only a warn line.
    AuditRecordDropped {
        /// Session the record was for (may not exist — that can be the failure).
        session: String,
        /// Record type: `decision`, `model_call`, or `tool_call`.
        record_type: String,
        /// Failure detail.
        error: String,
    },
    /// Runtime stopped.
    Stopped,
    /// Fatal failure.
    Failed {
        /// Error message.
        error: String,
    },
}
