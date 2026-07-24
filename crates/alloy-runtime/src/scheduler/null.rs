//! MVP null scheduler stub.

use async_trait::async_trait;

use super::traits::{DagOutcome, Scheduler};
use crate::error::SchedError;
use crate::types::ids::DagId;

/// Placeholder scheduler registered until RFC-0010.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullScheduler;

#[async_trait]
impl Scheduler for NullScheduler {
    async fn run(&self, _dag_id: DagId) -> Result<DagOutcome, SchedError> {
        Err(SchedError::Unavailable)
    }

    async fn cancel(&self, _dag_id: DagId) -> Result<(), SchedError> {
        Ok(())
    }
}
