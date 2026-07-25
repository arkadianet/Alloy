//! Durable storage: SQLite session event log, artifact CAS, and thin session rows.
//!
//! Implements **RFC-0002**. Lives inside `alloy-runtime` (no sixth crate).
//!
//! # Lifecycle
//!
//! `open` → migrate → append/read → `checkpoint` → `close` → reopen/recover.
//!
//! # Handoff
//!
//! Default runtime sink remains [`crate::InMemoryEventSink`]. Call
//! [`install_sqlite_event_sink`] to open storage and atomically hand off via
//! [`crate::RuntimeHandle::handoff_event_sink`].

mod artifacts;
mod checkpoint;
mod error;
mod events;
mod install;
mod metrics;
mod migrate;
mod open;
mod paths;
mod sessions;

pub use artifacts::{
    ArtifactBlob, ArtifactKind, ArtifactMeta, ArtifactPut, ArtifactStore, FsArtifactStore,
};
pub use error::{store_to_session, StoreError};
pub use events::{EventStore, SqliteEventStore};
pub use install::{install_sqlite_event_sink, store_to_runtime};
pub use metrics::StorageMetricsSnapshot;
pub use paths::{SqliteSynchronous, StorageLayout, StorageOpenOptions};
pub use sessions::{RunRow, SessionRows, SqliteSessionRows};

// Re-export handoff snapshot from events for the RFC-0002 public surface.
pub use crate::events::HandoffSnapshot;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use artifacts::FsArtifactStore as FsArtifactStoreImpl;
use events::SqliteEventStore as SqliteEventStoreImpl;
use metrics::StorageMetrics;
use open::DbHandle;
use sessions::SqliteSessionRows as SqliteSessionRowsImpl;

/// Opened durable store: event log + artifacts + thin session/run rows.
pub struct AlloyStorage {
    layout: StorageLayout,
    schema_version: u32,
    db: Arc<DbHandle>,
    events: Arc<SqliteEventStoreImpl>,
    artifacts: Arc<FsArtifactStoreImpl>,
    sessions: Arc<SqliteSessionRowsImpl>,
    metrics: Arc<StorageMetrics>,
    closed: Arc<AtomicBool>,
}

impl AlloyStorage {
    /// Open → migrate → ready. Creates dirs + DB if missing.
    pub async fn open(opts: StorageOpenOptions) -> Result<Self, StoreError> {
        let opts_clone = opts.clone();
        let (db, schema_version) =
            tokio::task::spawn_blocking(move || open::open_db(&opts_clone)).await??;

        let metrics = Arc::new(StorageMetrics::new());
        let closed = Arc::new(AtomicBool::new(false));
        let events = Arc::new(SqliteEventStoreImpl::new(
            Arc::clone(&db),
            Arc::clone(&metrics),
            Arc::clone(&closed),
        ));
        let artifacts = Arc::new(FsArtifactStoreImpl::new(
            Arc::clone(&db),
            opts.layout.clone(),
            Arc::clone(&metrics),
            Arc::clone(&closed),
        ));
        let sessions = Arc::new(SqliteSessionRowsImpl::new(
            Arc::clone(&db),
            Arc::clone(&closed),
        ));

        Ok(Self {
            layout: opts.layout,
            schema_version,
            db,
            events,
            artifacts,
            sessions,
            metrics,
            closed,
        })
    }

    /// Current schema version after migrate.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// On-disk layout.
    #[must_use]
    pub fn layout(&self) -> &StorageLayout {
        &self.layout
    }

    /// Shared event store (also implements [`crate::EventSink`]).
    #[must_use]
    pub fn events(&self) -> Arc<SqliteEventStore> {
        Arc::clone(&self.events)
    }

    /// Artifact store handle.
    #[must_use]
    pub fn artifacts(&self) -> Arc<FsArtifactStore> {
        Arc::clone(&self.artifacts)
    }

    /// Thin session/run row API (for RFC-0003; no orchestration).
    #[must_use]
    pub fn sessions(&self) -> Arc<SqliteSessionRows> {
        Arc::clone(&self.sessions)
    }

    /// Force WAL checkpoint (uses connection `synchronous` from open).
    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        self.ensure_open()?;
        checkpoint::checkpoint_truncate_async(Arc::clone(&self.db)).await?;
        self.metrics.inc_checkpoints();
        Ok(())
    }

    /// In-process counter snapshot. Cheap atomics read; safe while store is open.
    #[must_use]
    pub fn metrics(&self) -> StorageMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub(crate) fn metrics_handle(&self) -> Arc<StorageMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Flush + close connections. Idempotent under shared ownership.
    ///
    /// After the first successful close, further ops return [`StoreError::Closed`];
    /// extra `close` calls are no-ops (`Ok(())`).
    pub async fn close(&self) -> Result<(), StoreError> {
        if self
            .closed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        // Best-effort checkpoint before close.
        let _ = checkpoint::checkpoint_truncate_async(Arc::clone(&self.db)).await;
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(StoreError::Closed)
        } else {
            Ok(())
        }
    }
}

impl Drop for AlloyStorage {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::SeqCst) {
            tracing::warn!("AlloyStorage dropped without close()");
        }
    }
}
