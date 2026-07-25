//! Storage error types and conversions.

use crate::error::SessionError;
use crate::events::EventSinkError;

/// Durable store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Missing row or blob.
    #[error("not found: {0}")]
    NotFound(String),
    /// Unique/constraint conflict.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Corrupt data or seq inconsistency.
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Schema migration failure or refuse-newer.
    #[error("migration: {0}")]
    Migration(String),
    /// SQLite busy after timeout.
    #[error("busy")]
    Busy,
    /// Filesystem / SQLite I/O.
    #[error("io: {0}")]
    Io(String),
    /// Artifact bytes do not match stored digest.
    #[error("integrity: digest mismatch")]
    DigestMismatch,
    /// Store has been closed.
    #[error("closed")]
    Closed,
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StoreError> for EventSinkError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Busy => EventSinkError::Busy,
            StoreError::Io(s) => EventSinkError::Io(s),
            // Conflict / Corrupt / Migration / NotFound / Closed are not expected on the
            // happy-path EventSink append surface; map to Internal (not Io) so callers
            // do not treat integrity/schema bugs as transient disk errors.
            StoreError::Conflict(s)
            | StoreError::Corrupt(s)
            | StoreError::Migration(s)
            | StoreError::NotFound(s)
            | StoreError::Internal(s) => EventSinkError::Internal(s),
            StoreError::DigestMismatch => EventSinkError::Internal("digest mismatch".into()),
            StoreError::Closed => EventSinkError::Internal("store closed".into()),
        }
    }
}

/// Convert a rusqlite error using SQLite error codes (not message substrings).
pub(crate) fn from_rusqlite(e: rusqlite::Error) -> StoreError {
    match e {
        rusqlite::Error::SqliteFailure(err, msg) => match err.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                StoreError::Busy
            }
            rusqlite::ErrorCode::ConstraintViolation => {
                StoreError::Conflict(msg.unwrap_or_else(|| "constraint violation".into()))
            }
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                StoreError::Corrupt(msg.unwrap_or_else(|| err.to_string()))
            }
            rusqlite::ErrorCode::SystemIoFailure
            | rusqlite::ErrorCode::DiskFull
            | rusqlite::ErrorCode::CannotOpen
            | rusqlite::ErrorCode::ReadOnly
            | rusqlite::ErrorCode::NoLargeFileSupport
            | rusqlite::ErrorCode::PermissionDenied => {
                StoreError::Io(msg.unwrap_or_else(|| err.to_string()))
            }
            _ => StoreError::Internal(msg.unwrap_or_else(|| format!("{err:?}"))),
        },
        other => StoreError::Internal(other.to_string()),
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        from_rusqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e.to_string())
    }
}

impl From<tokio::task::JoinError> for StoreError {
    fn from(e: tokio::task::JoinError) -> Self {
        StoreError::Internal(format!("spawn_blocking join: {e}"))
    }
}

/// Map [`StoreError`] into [`SessionError`] at SessionService boundaries (RFC-0003).
///
/// Does not change [`SessionError`] variants — uses `Internal` / `Invalid` only.
#[must_use]
pub fn store_to_session(e: StoreError) -> SessionError {
    match e {
        StoreError::NotFound(s) => SessionError::Invalid(format!("not found: {s}")),
        StoreError::Conflict(s) | StoreError::Corrupt(s) | StoreError::Migration(s) => {
            SessionError::Invalid(s)
        }
        StoreError::Busy => SessionError::Internal("store busy".into()),
        StoreError::Closed => SessionError::Internal("store closed".into()),
        StoreError::DigestMismatch => SessionError::Internal("digest mismatch".into()),
        StoreError::Io(s) | StoreError::Internal(s) => SessionError::Internal(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_maps_to_event_sink() {
        assert!(matches!(
            EventSinkError::from(StoreError::Busy),
            EventSinkError::Busy
        ));
        assert!(matches!(
            EventSinkError::from(StoreError::Io("disk".into())),
            EventSinkError::Io(_)
        ));
        assert!(matches!(
            EventSinkError::from(StoreError::Conflict("x".into())),
            EventSinkError::Internal(_)
        ));
        assert!(matches!(
            EventSinkError::from(StoreError::Corrupt("x".into())),
            EventSinkError::Internal(_)
        ));
        assert!(matches!(
            EventSinkError::from(StoreError::Closed),
            EventSinkError::Internal(_)
        ));
    }
}
