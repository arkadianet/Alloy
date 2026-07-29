//! Context Engine (RFC-0012, Architecture V2 §8).
//!
//! A **deterministic budgeted renderer**, not a retriever: it has no index,
//! no ranking model and no similarity metric (rule SEC7). It takes facts
//! other subsystems already own — session events, the working set on disk,
//! the ProjectGraph projection, artifact metadata — clamps them to a token
//! budget by fixed profile weights (V2 Appendix B), and renders a
//! [`crate::router::PromptPack`] whose every byte is attributable to a
//! [`crate::router::Citation`] carrying a content digest (rule CIT1).
//!
//! Exactly three domains are live — Conversation, WorkingSet, Artifacts
//! (rule D1); the other five [`DomainId`] variants render nothing and cost
//! nothing. The graph is reached only through
//! [`crate::graph::GraphViewHandle`] (rule SEC1), and a graph or store
//! failure degrades the affected domain, it never fails assembly (rule E1).
//!
//! This module never writes to the workspace, the artifact store or the
//! event log (rule A14), and appends no session events (rule OB1).

mod artifacts;
mod budget;
mod conversation;
mod default_engine;
mod engine;
mod error;
mod estimator;
mod profile;
mod render;
mod types;
mod working_set;

pub use default_engine::{ContextMetricsSnapshot, DefaultContextEngine};
pub use engine::{ContextEngine, NullContextEngine};
pub use error::ContextError;
pub use estimator::{BytesPerTokenEstimator, TokenEstimator};
pub use profile::{ContextProfile, DomainWeights};
pub use types::{
    AssembleInputs, AssembleRequest, CompactStrategy, ContextHandle, Degradation,
    DegradationReason, DomainId, EvictPolicy, EvictReport, FileExcerpt, GraphProjection,
    StaleReason, WorkingSet,
};

/// Section-grammar and manifest schema version (rules A5, CIT9).
pub const CONTEXT_FORMAT_VERSION: u32 = 1;

/// Estimated tokens reserved for the system frame before any domain
/// allowance (rule B3).
pub const SYSTEM_FRAME_RESERVE_EST: usize = 512;
