//! [`Scheduler`] trait and DAG outcome types.

use std::time::Duration;

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

    /// Execute (or resume) a DAG with `remaining` as this invocation's
    /// wall-clock budget (RFC-0017 AM-0010-2).
    ///
    /// Lets a caller that spans several invocations over one run — the
    /// repair-generation driver — share a single **absolute** deadline
    /// instead of granting each invocation a fresh `run_timeout`.
    ///
    /// Default: delegates to [`Self::run`] (the implementor's own timeout
    /// policy), so existing implementors are unaffected. `LinearScheduler`
    /// overrides it by seeding its per-run clock from `remaining`.
    async fn run_within(
        &self,
        dag_id: DagId,
        remaining: Duration,
    ) -> Result<DagOutcome, SchedError> {
        let _ = remaining;
        self.run(dag_id).await
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Counts `run` calls; `run_within` is deliberately not overridden.
    struct CountingScheduler {
        runs: AtomicU32,
    }

    #[async_trait]
    impl Scheduler for CountingScheduler {
        async fn run(&self, dag_id: DagId) -> Result<DagOutcome, SchedError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(DagOutcome {
                dag_id,
                generation: 7,
                state: DagState::Succeeded,
                failed_node: None,
                failure: None,
            })
        }

        async fn cancel(&self, _dag_id: DagId) -> Result<(), SchedError> {
            Ok(())
        }
    }

    /// RFC-0017 AC 25c: the defaulted `run_within` body equals `run(dag_id)`
    /// — no implementor breaks, and the duration is ignored by default.
    #[tokio::test]
    async fn ac25c_run_within_default_delegates_to_run() {
        let sched = CountingScheduler {
            runs: AtomicU32::new(0),
        };
        let dag_id = DagId::new();
        let outcome = sched
            .run_within(dag_id, Duration::from_millis(5))
            .await
            .unwrap();
        assert_eq!(outcome.dag_id, dag_id);
        assert_eq!(outcome.generation, 7);
        assert_eq!(sched.runs.load(Ordering::SeqCst), 1);
    }
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
