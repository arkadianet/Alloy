//! Scheduler host surface (RFC-0010).

mod linear;
mod null;
mod traits;

pub use linear::{
    backoff_delay, derive_dag_state, promotable_nodes, ready_nodes, DeriveFlags, LinearScheduler,
    LinearSchedulerDeps, SchedConfig, SchedulerMetrics,
};
pub use null::NullScheduler;
pub use traits::{DagOutcome, DagState, Scheduler};
