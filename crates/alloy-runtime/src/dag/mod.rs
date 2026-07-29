//! Task DAG types, validation, templates, cache keys, and I/O envelopes (RFC-0009).
//!
//! Persistence lives in [`crate::storage::DagStore`]; planning in [`crate::planner`].

mod cache;
mod io;
mod templates;
mod types;
mod validate;

pub use cache::{
    compiler_fingerprint_digest, compute_cache_key, goal_content_digest, policy_hash_digest,
    tool_versions_digest, CacheKeyMaterials,
};
pub(crate) use io::{encode_json, PendingPredPlaceholder};
pub use io::{
    NodeInputEnvelope, NodeInputPayload, NodeOutputEnvelope, PredecessorOutput,
    ENVELOPE_SCHEMA_VERSION,
};
pub use templates::{
    allocate_ids, build_topology, BuildTopology, TemplateApprovalSpec, TemplateCatalog,
    TemplateEdgeSpec, TemplateId, TemplateIdMap, TemplateManifest, TemplateNodeSpec,
};
pub use types::*;
/// Kind ↔ capability-id map shared with the RFC-0013 registry (rule RG3).
pub(crate) use validate::expected_capability;
/// Shared predecessor-satisfaction rule (RFC-0009 §5.3.1), reused by
/// `scheduler::linear::ready::promotable_nodes` (RFC-0010 §3.13) so the
/// scheduler does not reimplement the readiness predicate.
pub(crate) use validate::preds_satisfied;
pub use validate::{DagValidationError, DagValidator, RetryIncoherence, ValidateOpts};

/// Optional re-export of the storage trait for convenience (concrete type stays in storage).
pub mod store {
    pub use crate::storage::{DagStore, ReplanReplaceError, SqliteDagStore};
}
