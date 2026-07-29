//! [`RunExecutor`] — the RFC-0003 §6.3 step-8 execution seam (RFC-0017
//! AM-0003-2).
//!
//! `RunController::start` awaits `executor.execute(ctx)` in place of a
//! hard-coded `handle.run_dag(dag_id)`. Everything around the seam — the
//! state guards, `RunAccepted`, the execution lease, the step-9 race merge,
//! the step-10 outcome mapping — is unchanged and observes only the
//! executor's **final** [`DagOutcome`]. The default [`DirectRunExecutor`]
//! preserves today's single-generation behaviour (rule RX3); RFC-0017's
//! `GenerationDriver` plugs in here at assembly.
//!
//! Author: arkadianet

use std::time::Instant;

use async_trait::async_trait;

use crate::error::RuntimeError;
use crate::runtime::RuntimeHandle;
use crate::scheduler::DagOutcome;
use crate::types::ids::{DagId, RunId, SessionId};

/// What [`crate::RunController::start`] awaits at RFC-0003 §6.3 step 8.
///
/// Implementations MUST surface exactly one final [`DagOutcome`] per
/// `execute` call and MUST NOT emit run lifecycle events or write run rows
/// (RFC-0017 rules RX1/RX2) — §6.3 owns both.
#[async_trait]
pub trait RunExecutor: Send + Sync {
    /// Execute the run's DAG to a final outcome.
    ///
    /// `Err` is infrastructure only; §6.3 step 10 maps it exactly as a
    /// direct `run_dag` error.
    async fn execute(&self, ctx: RunExecCtx) -> Result<DagOutcome, RuntimeError>;
}

/// Per-dispatch context for [`RunExecutor::execute`].
#[derive(Debug, Clone)]
pub struct RunExecCtx {
    /// Run being executed.
    pub run_id: RunId,
    /// Owning session.
    pub session_id: SessionId,
    /// DAG to execute.
    pub dag_id: DagId,
    /// **Absolute** wall-clock deadline for the whole run, computed once by
    /// `start` as `Instant::now() + RuntimeConfig.run_timeout` (RFC-0017
    /// GN7 / AM-0010-2). Every generation is dispatched with the *remaining*
    /// share; generations do not each get a fresh `run_timeout`.
    pub deadline: Instant,
}

/// Today's behaviour, preserved: one `run_dag_within` call, no loop.
///
/// The default executor whenever no RFC-0017 wiring is present (RX3).
pub struct DirectRunExecutor {
    handle: RuntimeHandle,
}

impl DirectRunExecutor {
    /// Construct over the process runtime handle.
    #[must_use]
    pub fn new(handle: RuntimeHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl RunExecutor for DirectRunExecutor {
    async fn execute(&self, ctx: RunExecCtx) -> Result<DagOutcome, RuntimeError> {
        let remaining = ctx.deadline.saturating_duration_since(Instant::now());
        self.handle.run_dag_within(ctx.dag_id, remaining).await
    }
}
