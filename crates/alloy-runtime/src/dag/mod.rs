//! Task DAG types, validation, templates, cache keys, and I/O envelopes (RFC-0009).
//!
//! Persistence lives in [`crate::storage::DagStore`]; planning in [`crate::planner`].

mod types;
mod validate;

pub use types::*;
pub use validate::{DagValidationError, DagValidator, RetryIncoherence, ValidateOpts};
