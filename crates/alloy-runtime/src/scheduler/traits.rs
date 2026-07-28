//! [`Scheduler`] trait and DAG outcome types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::SchedError;
use crate::types::diagnostic::FailureIr;
use crate::types::ids::{DagId, NodeId};

/// Ready-queue executor over a TaskDag (impl in RFC-0010).
#[async_trait]
pub trait Scheduler: Send + Sync {
    /// Execute (or resume) a DAG.
    async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError>;

    /// Cancel an active DAG.
    async fn cancel(&self, dag_id: DagId) -> Result<(), SchedError>;

    /// Reconcile a DAG blob toward a durable terminal control-plane state it
    /// never observed on its own (RFC-0010 §5.20, amendment A2/A6) — e.g. a
    /// gate deny/expiry that terminalized the *run* row while the *DAG* blob
    /// is still `waiting_approval` because the scheduler never got to see
    /// it. `terminal` MUST be `Succeeded` | `Failed` | `Cancelled`.
    ///
    /// Default: [`SchedError::Unavailable`] (matches every other
    /// unimplemented `Scheduler` method's placeholder behavior — only
    /// `LinearScheduler` overrides this).
    async fn reconcile_terminal_run(
        &self,
        _dag_id: DagId,
        _terminal: DagState,
    ) -> Result<(), SchedError> {
        Err(SchedError::Unavailable)
    }
}

/// Terminal/observable DAG outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagOutcome {
    /// DAG id.
    pub dag_id: DagId,
    /// Generation counter.
    pub generation: u64,
    /// Final/observable state.
    pub state: DagState,
    /// Failed node if any.
    pub failed_node: Option<NodeId>,
    /// Structured failure if any.
    pub failure: Option<FailureIr>,
}

/// DAG-level state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DagState {
    /// Not started.
    Pending,
    /// Executing.
    Running,
    /// Blocked on human gate.
    WaitingApproval,
    /// Succeeded.
    Succeeded,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Replan required.
    ReplanRequired,
}
