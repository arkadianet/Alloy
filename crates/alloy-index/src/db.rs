//! Connection handle, PRAGMAs, error mapping, and the X1 instance lock.
//!
//! Mirrors RFC-0002's `storage::open` conventions (§2.3, §5.5): the shapes
//! are re-implemented here because `DbHandle`/`spawn_db` are crate-private
//! in `alloy-runtime`.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};

use alloy_runtime::graph::GraphError;
use rusqlite::{Connection, OpenFlags};

use crate::layout::GraphOpenOptions;

/// Convert a rusqlite error using SQLite error codes (rule S9 — never by
/// message substring), matching `storage::error::from_rusqlite`'s taxonomy.
pub(crate) fn from_rusqlite(e: rusqlite::Error) -> GraphError {
    match e {
        rusqlite::Error::SqliteFailure(err, msg) => match err.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                GraphError::Busy
            }
            rusqlite::ErrorCode::ConstraintViolation => {
                // A constraint violation in a single-writer derived cache is
                // a bug, not contention (§9.2).
                GraphError::Internal(msg.unwrap_or_else(|| "constraint violation".into()))
            }
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                GraphError::Corrupt(msg.unwrap_or_else(|| err.to_string()))
            }
            rusqlite::ErrorCode::SystemIoFailure
            | rusqlite::ErrorCode::DiskFull
            | rusqlite::ErrorCode::CannotOpen
            | rusqlite::ErrorCode::ReadOnly
            | rusqlite::ErrorCode::NoLargeFileSupport
            | rusqlite::ErrorCode::PermissionDenied => {
                GraphError::Io(msg.unwrap_or_else(|| err.to_string()))
            }
            _ => GraphError::Internal(msg.unwrap_or_else(|| format!("{err:?}"))),
        },
        other => GraphError::Internal(other.to_string()),
    }
}

/// Shared SQLite connection guarded for single-process use (§5.5).
#[derive(Debug)]
pub(crate) struct GraphDb {
    conn: Mutex<Option<Connection>>,
    /// Held for the instance's lifetime; the advisory lock releases when the
    /// file handle drops (X1).
    _lock: std::fs::File,
}

impl GraphDb {
    pub(crate) fn new(conn: Connection, lock: std::fs::File) -> Arc<Self> {
        Arc::new(Self {
            conn: Mutex::new(Some(conn)),
            _lock: lock,
        })
    }

    /// Lock the connection for a synchronous operation.
    pub(crate) fn with<F, T>(&self, f: F) -> Result<T, GraphError>
    where
        F: FnOnce(&Connection) -> Result<T, GraphError>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|_| GraphError::Internal("db mutex poisoned".into()))?;
        let conn = guard.as_ref().ok_or(GraphError::Closed)?;
        f(conn)
    }

    /// Lock the connection mutably (transaction helpers).
    pub(crate) fn with_mut<F, T>(&self, f: F) -> Result<T, GraphError>
    where
        F: FnOnce(&mut Connection) -> Result<T, GraphError>,
    {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| GraphError::Internal("db mutex poisoned".into()))?;
        let conn = guard.as_mut().ok_or(GraphError::Closed)?;
        f(conn)
    }

    /// Take the connection out for final close (idempotent).
    pub(crate) fn take_connection(&self) -> Result<Option<Connection>, GraphError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| GraphError::Internal("db mutex poisoned".into()))?;
        Ok(guard.take())
    }

    /// Whether the SQLite connection is still held (not yet closed).
    pub(crate) fn connection_present(&self) -> bool {
        self.conn
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }
}

/// Run `f` on a blocking thread with the shared handle (rule X3).
pub(crate) async fn spawn_graph_db<F, T>(db: Arc<GraphDb>, f: F) -> Result<T, GraphError>
where
    F: FnOnce(&GraphDb) -> Result<T, GraphError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))?
}

/// X1: acquire the per-directory advisory instance lock, `graph.lock`.
pub(crate) fn acquire_instance_lock(graph_root: &Path) -> Result<std::fs::File, GraphError> {
    let path = graph_root.join("graph.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| GraphError::Io(format!("open {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(GraphError::Busy),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(GraphError::Io(format!("lock {}: {e}", path.display())))
        }
    }
}

/// Open the SQLite file with RFC-0002's flags and PRAGMA order (rule S7).
pub(crate) fn open_connection(opts: &GraphOpenOptions) -> Result<Connection, GraphError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&opts.layout.db_path, flags).map_err(from_rusqlite)?;
    apply_pragmas(&conn, opts)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection, opts: &GraphOpenOptions) -> Result<(), GraphError> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(from_rusqlite)?;
    // Busy timeout before WAL so journal_mode acquisition observes it.
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(
        opts.busy_timeout_ms,
    )))
    .map_err(from_rusqlite)?;
    if opts.wal {
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))
            .map_err(from_rusqlite)?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "requested WAL but journal_mode is different");
        }
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous = {};",
        opts.synchronous.as_pragma()
    ))
    .map_err(from_rusqlite)?;
    Ok(())
}
