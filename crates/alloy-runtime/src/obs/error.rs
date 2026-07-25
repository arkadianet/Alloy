//! Observability error type (RFC-0004).

use crate::error::{RuntimeError, SessionError};
use crate::storage::StoreError;

/// Errors from decision recording, metering helpers, and event queries.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// Invalid record / retention / payload construction.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Append through [`crate::RuntimeHandle`] failed.
    #[error("append: {0}")]
    Append(#[from] RuntimeError),
    /// Budget warning hook / session lookup failed.
    #[error("session: {0}")]
    Session(#[from] SessionError),
    /// [`crate::EventStore`] query/read failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Redaction/retention helper misuse.
    #[error("redaction: {0}")]
    Redaction(String),
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}
