//! Durable storage: SQLite session event log, artifact CAS, DAG blobs, and thin session rows.
//!
//! Implements **RFC-0002** (events/artifacts/sessions) and **RFC-0009** (`dag_blobs` /
//! [`DagStore`]). Lives inside `alloy-runtime` (no sixth crate).
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
mod codec;
mod dags;
mod error;
mod events;
mod gate;
mod install;
mod metrics;
mod migrate;
mod open;
mod paths;
mod sessions;

pub use artifacts::{
    ArtifactBlob, ArtifactKind, ArtifactMeta, ArtifactPut, ArtifactStore, FsArtifactStore,
};
pub use dags::{DagStore, ReplanReplaceError, SqliteDagStore};
pub use error::{store_to_session, StoreError};
pub use events::{EventStore, SqliteEventStore};
pub use install::{install_sqlite_event_sink, store_to_runtime};
pub use metrics::StorageMetricsSnapshot;
pub use paths::{SqliteSynchronous, StorageLayout, StorageOpenOptions};
pub use sessions::{RunRow, SessionRows, SqliteSessionRows};

// Re-export handoff snapshot from events for the RFC-0002 public surface.
pub use crate::events::HandoffSnapshot;

use std::sync::Arc;

use artifacts::FsArtifactStore as FsArtifactStoreImpl;
use events::SqliteEventStore as SqliteEventStoreImpl;
use gate::{CloseClaim, StorageGate};
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
    dags: Arc<SqliteDagStore>,
    metrics: Arc<StorageMetrics>,
    gate: Arc<StorageGate>,
}

impl AlloyStorage {
    /// Open → migrate → ready. Creates dirs + DB if missing.
    pub async fn open(opts: StorageOpenOptions) -> Result<Self, StoreError> {
        let opts_clone = opts.clone();
        let (db, schema_version) =
            tokio::task::spawn_blocking(move || open::open_db(&opts_clone)).await??;

        let metrics = Arc::new(StorageMetrics::new());
        let gate = StorageGate::new();
        let events = Arc::new(SqliteEventStoreImpl::new(
            Arc::clone(&db),
            Arc::clone(&metrics),
            Arc::clone(&gate),
        ));
        let artifacts = Arc::new(FsArtifactStoreImpl::new(
            Arc::clone(&db),
            opts.layout.clone(),
            Arc::clone(&metrics),
            Arc::clone(&gate),
        ));
        let sessions = Arc::new(SqliteSessionRowsImpl::new(
            Arc::clone(&db),
            Arc::clone(&metrics),
            Arc::clone(&gate),
        ));
        let dags = Arc::new(SqliteDagStore::new(
            Arc::clone(&db),
            Arc::clone(&metrics),
            Arc::clone(&gate),
        ));

        Ok(Self {
            layout: opts.layout,
            schema_version,
            db,
            events,
            artifacts,
            sessions,
            dags,
            metrics,
            gate,
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

    /// Shared DAG store handle (Arc clone of the open-time instance).
    #[must_use]
    pub fn dags(&self) -> Arc<SqliteDagStore> {
        Arc::clone(&self.dags)
    }

    /// Force WAL checkpoint (uses connection `synchronous` from open).
    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        // Admit briefly, then release before `spawn_blocking` so `close`'s
        // drain (which may itself run on the blocking pool) is not stuck
        // waiting on `in_flight` while this task waits for a pool thread.
        // `DbHandle`'s mutex still serializes connection use vs `take_connection`.
        {
            let _permit = self.gate.enter()?;
        }
        match checkpoint::checkpoint_truncate_async(Arc::clone(&self.db)).await {
            Ok(()) => {
                self.metrics.inc_checkpoints();
                Ok(())
            }
            Err(StoreError::Busy) => {
                self.metrics.inc_busy_errors();
                Err(StoreError::Busy)
            }
            Err(e) => Err(e),
        }
    }

    /// In-process counter snapshot. Cheap atomics read; safe while store is open.
    #[must_use]
    pub fn metrics(&self) -> StorageMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub(crate) fn metrics_handle(&self) -> Arc<StorageMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Flush + close connections. Idempotent barrier under shared ownership.
    ///
    /// 1. Atomically claim close ownership (refuse new ops).
    /// 2. Wait for in-flight ops to finish.
    /// 3. Winner: checkpoint then close the SQLite connection.
    /// 4. Followers: drain and wait until the winner finishes teardown.
    ///
    /// Extra `close` calls return `Ok(())` after observing a finished close.
    pub async fn close(&self) -> Result<(), StoreError> {
        let claim = self.gate.claim_close();
        let gate = Arc::clone(&self.gate);
        let db = Arc::clone(&self.db);
        let metrics = Arc::clone(&self.metrics);

        match claim {
            CloseClaim::Follower => {
                tokio::task::spawn_blocking(move || {
                    gate.begin_close_and_drain();
                    gate.wait_close_finished();
                    debug_assert!(!db.connection_present().unwrap_or(true));
                    Ok(())
                })
                .await?
            }
            CloseClaim::Winner => {
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        gate.begin_close_and_drain();
                        // Checkpoint while we still hold the connection, after in-flight finished.
                        match checkpoint::checkpoint_truncate(&db) {
                            Ok(()) => metrics.inc_checkpoints(),
                            Err(StoreError::Busy) => {
                                metrics.inc_busy_errors();
                                // Still close — but surface busy so callers know flush was incomplete.
                                if let Ok(Some(conn)) = db.take_connection() {
                                    let _ = conn.close();
                                }
                                return Err(StoreError::Busy);
                            }
                            Err(e) => {
                                if let Ok(Some(conn)) = db.take_connection() {
                                    let _ = conn.close();
                                }
                                return Err(e);
                            }
                        }
                        if let Some(conn) = db.take_connection()? {
                            conn.close().map_err(|(_c, e)| StoreError::from(e))?;
                        }
                        Ok(())
                    })();
                    gate.mark_close_finished();
                    result
                })
                .await?
            }
        }
    }
}

impl Drop for AlloyStorage {
    fn drop(&mut self) {
        if !self.gate.is_closed() {
            tracing::warn!("AlloyStorage dropped without close()");
        }
    }
}
