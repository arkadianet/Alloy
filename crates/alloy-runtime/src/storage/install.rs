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

    handle
        .handoff_event_sink(events.clone(), {
            let events = Arc::clone(&events);
            move |snap| async move { events.import_handoff_snapshot(snap).await }
        })
        .await?;

    storage.metrics_handle().inc_handoffs();
    let snap = storage.metrics();
    tracing::info!(
        events = snap.events_appended,
        runtime = snap.runtime_events_appended,
        "sqlite event sink installed"
    );
    Ok(storage)
}

/// Map [`StoreError`] into [`RuntimeError`] for install / handoff boundaries.
pub fn store_to_runtime(e: StoreError) -> RuntimeError {
    match e {
        StoreError::Busy => RuntimeError::EventSinkBusy,
        StoreError::Io(s) => RuntimeError::Io(std::io::Error::other(s)),
        other => RuntimeError::Internal(other.to_string()),
    }
}
