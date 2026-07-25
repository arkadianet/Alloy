//! WAL checkpoint helpers.

use std::sync::Arc;

use super::error::StoreError;
use super::open::DbHandle;

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` on the shared connection.
///
/// Returns [`StoreError::Busy`] when SQLite reports `busy != 0` (checkpoint
/// incomplete / could not reset the WAL).
#[tracing::instrument(skip(db), name = "storage.checkpoint")]
pub fn checkpoint_truncate(db: &DbHandle) -> Result<(), StoreError> {
    db.with(|conn| {
        let (busy, log, checkpointed): (i64, i64, i64) =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy != 0 {
            tracing::warn!(
                busy,
                log,
                checkpointed,
                "wal checkpoint incomplete (busy readers or writers)"
            );
            return Err(StoreError::Busy);
        }
        Ok(())
    })
}

/// Async wrapper around [`checkpoint_truncate`].
pub async fn checkpoint_truncate_async(db: Arc<DbHandle>) -> Result<(), StoreError> {
    tokio::task::spawn_blocking(move || checkpoint_truncate(&db)).await?
}
