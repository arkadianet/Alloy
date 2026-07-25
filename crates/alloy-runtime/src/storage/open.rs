//! Connection open, PRAGMA setup, and shared DB handle.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use super::error::StoreError;
use super::migrate::{self, CODE_SCHEMA_VERSION};
use super::paths::StorageOpenOptions;

/// Shared SQLite connection guarded for single-process MVP use.
#[derive(Debug)]
pub struct DbHandle {
    conn: Mutex<Option<Connection>>,
}

impl DbHandle {
    /// Wrap an open connection.
    pub(crate) fn new(conn: Connection) -> Arc<Self> {
        Arc::new(Self {
            conn: Mutex::new(Some(conn)),
        })
    }

    /// Lock the connection for a synchronous operation.
    pub fn with<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Internal("db mutex poisoned".into()))?;
        let conn = guard.as_ref().ok_or(StoreError::Closed)?;
        f(conn)
    }

    /// Lock the connection mutably (needed for some rusqlite transaction helpers).
    pub fn with_mut<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError>,
    {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Internal("db mutex poisoned".into()))?;
        let conn = guard.as_mut().ok_or(StoreError::Closed)?;
        f(conn)
    }

    /// Take the connection out for final close (idempotent).
    pub(crate) fn take_connection(&self) -> Result<Option<Connection>, StoreError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Internal("db mutex poisoned".into()))?;
        Ok(guard.take())
    }

    /// Whether the SQLite connection is still held (not yet taken for close).
    pub(crate) fn connection_present(&self) -> Result<bool, StoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Internal("db mutex poisoned".into()))?;
        Ok(guard.is_some())
    }
}

/// Open SQLite, apply PRAGMAs, migrate, verify seq consistency.
///
/// Returns `(handle, schema_version)`.
#[tracing::instrument(skip(opts), fields(db = %opts.layout.db_path.display()), name = "storage.open")]
pub fn open_db(opts: &StorageOpenOptions) -> Result<(Arc<DbHandle>, u32), StoreError> {
    opts.layout.ensure_dirs()?;
    cleanup_orphan_tmp(&opts.layout.artifacts_dir);

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;

    let conn = Connection::open_with_flags(&opts.layout.db_path, flags).map_err(|e| {
        StoreError::Io(format!(
            "open {}: {e} (see example.env)",
            opts.layout.db_path.display()
        ))
    })?;

    apply_pragmas(&conn, opts)?;
    let version = migrate::migrate(&conn, opts.refuse_newer_schema)?;
    migrate::verify_seq_consistency(&conn)?;

    // Optional orphan CAS scan (file without index) — do not delete in MVP.
    warn_orphan_blobs(
        &conn,
        &opts.layout.artifacts_dir,
        opts.orphan_blob_scan_limit,
    );

    let handle = DbHandle::new(conn);
    debug_assert!(version >= CODE_SCHEMA_VERSION || !opts.refuse_newer_schema);
    Ok((handle, version))
}

fn apply_pragmas(conn: &Connection, opts: &StorageOpenOptions) -> Result<(), StoreError> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    // Busy timeout before WAL so journal_mode acquisition observes it.
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(
        opts.busy_timeout_ms,
    )))?;
    if opts.wal {
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "requested WAL but journal_mode is different");
        }
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous = {};",
        opts.synchronous.as_pragma()
    ))?;
    Ok(())
}

/// Remove leftover CAS temp files under `artifacts/tmp/`.
///
/// Assumes single-process access to the storage directory. Concurrent processes
/// writing the same data dir may lose in-flight CAS temporary files.
fn cleanup_orphan_tmp(artifacts_dir: &std::path::Path) {
    let tmp = artifacts_dir.join("tmp");
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %e, "failed to clean orphan tmp artifact");
            } else {
                tracing::warn!(path = %path.display(), "removed orphan tmp artifact");
            }
        }
    }
}

/// Optionally scan for CAS files without an index row (best-effort diagnostics).
///
/// Disabled unless `orphan_blob_scan_limit` is set. Caps inspected files and
/// emits a single aggregated warning.
fn warn_orphan_blobs(conn: &Connection, artifacts_dir: &std::path::Path, scan_limit: Option<u32>) {
    let Some(limit) = scan_limit.filter(|n| *n > 0) else {
        return;
    };
    let sha_root = artifacts_dir.join("sha256");
    let Ok(prefixes) = std::fs::read_dir(&sha_root) else {
        return;
    };
    let mut inspected = 0u32;
    let mut orphans = 0u64;
    let mut check_errors = 0u64;
    'scan: for prefix in prefixes.flatten() {
        if !prefix.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(prefix.path()) else {
            continue;
        };
        for file in files.flatten() {
            if inspected >= limit {
                break 'scan;
            }
            let path = file.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            inspected += 1;
            let exists: Result<i64, _> = conn.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE digest = ?1 AND deleted_at IS NULL",
                [name],
                |r| r.get(0),
            );
            match exists {
                Ok(0) => orphans += 1,
                Ok(_) => {}
                Err(_) => check_errors += 1,
            }
        }
    }
    if orphans > 0 || check_errors > 0 {
        tracing::warn!(
            orphans,
            check_errors,
            inspected,
            limit,
            "orphan artifact blob scan"
        );
    }
}

/// Run `f` on a blocking thread with the shared DB handle.
pub async fn spawn_db<F, T>(db: Arc<DbHandle>, f: F) -> Result<T, StoreError>
where
    F: FnOnce(&DbHandle) -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&db)).await?
}
