//! Install SQLite event sink onto a [`RuntimeHandle`] via atomic handoff.

use std::sync::Arc;

use super::error::StoreError;
use super::events::EventStore;
use super::paths::StorageOpenOptions;
use super::AlloyStorage;
use crate::error::RuntimeError;
use crate::runtime::RuntimeHandle;

/// Open storage under `handle.config().data_dir`, migrate, atomic handoff, install SQLite sink.
///
/// Uses [`RuntimeHandle::handoff_event_sink`] (not bare `set_event_sink`) so a non-empty
/// in-memory buffer is drained losslessly. Dual-write is forbidden.
#[tracing::instrument(skip(handle, opts), name = "storage.handoff")]
pub async fn install_sqlite_event_sink(
    handle: &RuntimeHandle,
    opts: Option<StorageOpenOptions>,
) -> Result<Arc<AlloyStorage>, RuntimeError> {
    let opts = match opts {
        Some(o) => o,
        None => {
            let cfg = handle.config()?;
            StorageOpenOptions::from_env(cfg.data_dir.clone()).map_err(store_to_runtime)?
        }
    };

    let storage = AlloyStorage::open(opts).await.map_err(store_to_runtime)?;
    let storage = Arc::new(storage);
    let events = storage.events();
    let sink = Arc::clone(&events);

    let result = handle
        .handoff_event_sink(sink, move |snap| async move {
            let runtime_n = snap.runtime.len();
            let session_n: usize = snap.sessions.values().map(Vec::len).sum();
            tracing::info!(
                runtime_events = runtime_n,
                session_events = session_n,
                "draining in-memory sink into sqlite"
            );
            events.import_handoff_snapshot(snap).await
        })
        .await;

    match result {
        Ok(()) => {
            storage.metrics_handle().inc_handoffs();
            Ok(storage)
        }
        Err(e) => {
            if let Err(close_err) = storage.close().await {
                tracing::warn!(error = %close_err, "storage rollback close failed after handoff error");
            }
            Err(e)
        }
    }
}

/// Map [`StoreError`] into [`RuntimeError`] for install / handoff boundaries.
pub fn store_to_runtime(e: StoreError) -> RuntimeError {
    match e {
        StoreError::Busy => RuntimeError::EventSinkBusy,
        StoreError::Io(s) => RuntimeError::Io(std::io::Error::other(s)),
        other => RuntimeError::Internal(other.to_string()),
    }
}
