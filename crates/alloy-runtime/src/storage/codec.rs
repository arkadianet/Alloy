//! Timestamp helpers for SQLite TEXT columns (lexicographic == chronological).

use super::error::StoreError;
use crate::types::ids::{ArtifactId, RunId, SessionId, Timestamp};

/// Persist timestamps as zero-padded unix nanos so `ORDER BY` is chronological.
pub fn ts_to_text(ts: &Timestamp) -> Result<String, StoreError> {
    Ok(format!("{:020}", ts.0.unix_timestamp_nanos()))
}

/// Parse a nanos TEXT column back into [`Timestamp`].
pub fn ts_from_text(s: &str) -> Result<Timestamp, StoreError> {
    let nanos: i128 = s
        .parse()
        .map_err(|e| StoreError::Corrupt(format!("timestamp nanos: {e}")))?;
    let odt = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|e| StoreError::Corrupt(format!("timestamp: {e}")))?;
    Ok(Timestamp(odt))
}

/// Parse a transparent UUID newtype via `Uuid::parse_str` + serde.
fn parse_uuid_newtype<T>(s: &str) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    serde_json::from_value(serde_json::json!(uuid)).map_err(|e| StoreError::Corrupt(e.to_string()))
}

/// Parse a session id from TEXT (uses [`SessionId::parse`]).
pub fn parse_session_id(s: &str) -> Result<SessionId, StoreError> {
    SessionId::parse(s).map_err(|e| StoreError::Corrupt(e.to_string()))
}

/// Parse a run id from TEXT.
pub fn parse_run_id(s: &str) -> Result<RunId, StoreError> {
    parse_uuid_newtype(s)
}

/// Parse an artifact id from TEXT.
pub fn parse_artifact_id(s: &str) -> Result<ArtifactId, StoreError> {
    parse_uuid_newtype(s)
}

/// Require a path to be valid Unicode for durable TEXT storage.
pub fn path_to_utf8(path: &std::path::Path) -> Result<String, StoreError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Internal(format!("path is not valid UTF-8: {}", path.display())))
}
