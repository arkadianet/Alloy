//! WAL checkpoint helpers.

use std::sync::Arc;

use super::error::StoreError;
use super::open::DbHandle;

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` on the shared connection.
#[tracing::instrument(skip(db), name = "storage.checkpoint")]
pub fn checkpoint_truncate(db: &DbHandle) -> Result<(), StoreError> {
    db.with(|conn| {
        // Returns (busy, log, checkpointed); we only care about error.
        let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(())
    })
}

/// Async wrapper around [`checkpoint_truncate`].
pub async fn checkpoint_truncate_async(db: Arc<DbHandle>) -> Result<(), StoreError> {
    tokio::task::spawn_blocking(move || checkpoint_truncate(&db)).await?
}
