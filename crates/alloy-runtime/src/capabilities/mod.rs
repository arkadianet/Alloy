//! Capability registry and MVP workers (RFC-0013).
//!
//! A **capability is a contract, not a persona** (V2 §9.1): the scheduler
//! knows *when* to run a capability node and *how* to retry it (RFC-0010);
//! this module supplies *what happens inside one attempt*. A worker is a
//! pure function of `(node input envelope, assembled context, model
//! completion, tool results)` to a JSON payload or a `FailureIr`. It owns no
//! topology, no retry, no tier escalation, and no graph write.
//!
//! Seams bound together here: the model router (RFC-0007) through
//! [`RunRouterProvider`], the context engine (RFC-0012), the tool bus
//! (RFC-0006) through `ToolCaller`, and the patch IR (RFC-0008) through the
//! `apply_patch` builtin only.

mod deps;
mod executor;
mod parse;
mod payload;
mod perms;
mod prompt;
mod registry;
mod traits;
mod workers;

pub use deps::{
    CapabilityContext, ProcessRunRouterProvider, RunRouterProvider, WorkerConfig, WorkerDeps,
};
pub use executor::RegistryCapabilityExecutor;
pub use payload::{
    EditAppliedPayload, PlanningProposalPayload, RepairPlanPayload, RepairStep, ReviewFinding,
    ReviewPayload, ReviewSeverity, ReviewVerdict, PAYLOAD_SCHEMA_VERSION,
};
pub use perms::{SessionWorkerPermissions, WorkerPermissions, WorkerToolClass};
pub use prompt::{
    system_instruction_digest, truncation_marker, EDIT_SYSTEM, PLANNING_SYSTEM, REPAIR_SYSTEM,
    REVIEW_SYSTEM,
};
pub use registry::{CapabilityRegistry, RegError, ResolveHints};
pub use traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
pub use workers::{EditWorker, PlanningWorker, RepairWorker, ReviewWorker};

/// Closed MVP catalog (RG2). Order is the registration order used by
/// [`CapabilityRegistry::mvp`]. Adding an entry is an RFC amendment, not a
/// config change.
pub const CAPABILITY_CATALOG: [&str; 4] = ["planning", "repair", "edit", "review"];

/// Hard cap on registered capabilities (V2 §9.2, roadmap M7; RG1, SEC6).
pub const MAX_LLM_CAPABILITIES: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_closed_at_four() {
        // RG1/RG2/SEC6.
        assert_eq!(CAPABILITY_CATALOG.len(), MAX_LLM_CAPABILITIES);
        assert_eq!(CAPABILITY_CATALOG, ["planning", "repair", "edit", "review"]);
    }
}
