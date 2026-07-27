//! Process-level scheduler ownership: `<data_dir>/scheduler.lock` (RFC-0010 §4.5).
//!
//! DAG-level ownership ([`crate::dag::TaskDag`] per-run leasing) lands
//! alongside the serial loop in a later phase; this module covers only the
//! "one `LinearScheduler` per `data_dir` per host" layer.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::error::SchedError;

/// Kept alive for the scheduler's lifetime; the advisory lock is released
/// when the file handle drops (process exit or scheduler drop).
pub(super) struct OwnershipLock {
    _file: std::fs::File,
    #[allow(dead_code)]
    // retained for future diagnostics (§4.5 L4); correctness never depends on it
    path: PathBuf,
}

impl OwnershipLock {
    /// L1-L3: create the data dir, open (never truncate) `scheduler.lock`,
    /// and take an exclusive advisory lock.
    pub(super) fn acquire(data_dir: &Path) -> Result<Self, SchedError> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| SchedError::Ownership(format!("create_dir_all: {e}")))?;
        let path = data_dir.join("scheduler.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| SchedError::Ownership(format!("open scheduler.lock: {e}")))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(std::fs::TryLockError::WouldBlock) => Err(SchedError::Ownership(format!(
                "scheduler.lock held by another process: {}",
                path.display()
            ))),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(SchedError::Ownership(format!("scheduler.lock: {e}")))
            }
        }
    }
}
