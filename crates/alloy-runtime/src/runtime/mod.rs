//! Alloy runtime host lifecycle.

mod handle;
mod inner;
mod lifecycle;

pub use crate::events::RuntimeEvent;
pub use handle::RuntimeHandle;
pub use lifecycle::AlloyRuntime;

/// Runtime phase state machine (RFC-0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePhase {
    /// Fresh `AlloyRuntime::new`.
    Created,
    /// Configuration applied.
    Configured,
    /// `start` in progress.
    Starting,
    /// Accepting work.
    Running,
    /// Draining in-flight work.
    Draining,
    /// Fully stopped.
    Stopped,
    /// Fatal failure; requires shutdown.
    Failed,
}
