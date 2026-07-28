//! On-disk layout and open options (RFC-0011 §3.10, §5).

use std::path::PathBuf;

use alloy_runtime::graph::GraphError;
use alloy_runtime::storage::StorageLayout;
use alloy_runtime::SqliteSynchronous;

/// On-disk layout under `StorageLayout::graph_dir` (rule S1).
#[derive(Debug, Clone)]
pub struct GraphLayout {
    /// `<data_dir>/graph`.
    pub root: PathBuf,
    /// `<data_dir>/graph/graph.sqlite`.
    pub db_path: PathBuf,
    /// `<data_dir>/graph/quarantine`.
    pub quarantine_dir: PathBuf,
}

impl GraphLayout {
    /// Derive from the RFC-0002 storage layout.
    #[must_use]
    pub fn from_storage_layout(layout: &StorageLayout) -> Self {
        Self::from_graph_root(layout.graph_dir.clone())
    }

    /// Derive from a data directory root.
    #[must_use]
    pub fn from_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self::from_graph_root(data_dir.into().join("graph"))
    }

    fn from_graph_root(root: PathBuf) -> Self {
        Self {
            db_path: root.join("graph.sqlite"),
            quarantine_dir: root.join("quarantine"),
            root,
        }
    }

    /// Create `root` and `quarantine/` if missing.
    pub fn ensure_dirs(&self) -> Result<(), GraphError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| GraphError::Io(format!("create {}: {e}", self.root.display())))?;
        std::fs::create_dir_all(&self.quarantine_dir).map_err(|e| {
            GraphError::Io(format!("create {}: {e}", self.quarantine_dir.display()))
        })?;
        Ok(())
    }
}

/// Open options mirroring `StorageOpenOptions` (RFC-0002).
#[derive(Debug, Clone)]
pub struct GraphOpenOptions {
    /// Paths.
    pub layout: GraphLayout,
    /// `PRAGMA journal_mode = WAL` (default `true`).
    pub wal: bool,
    /// SQLite busy timeout (default `5000`).
    pub busy_timeout_ms: u32,
    /// `PRAGMA synchronous` (default `Normal`).
    pub synchronous: SqliteSynchronous,
    /// Refuse a DB whose schema version is newer than this build (default
    /// `true`).
    pub refuse_newer_schema: bool,
    /// Quarantine + recreate on `Corrupt` at open instead of failing
    /// (default `true`).
    pub quarantine_on_corrupt: bool,
    /// Ingest caps.
    pub limits: IngestLimits,
}

impl GraphOpenOptions {
    /// Defaults for a data directory root (§3.10 defaults).
    #[must_use]
    pub fn for_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            layout: GraphLayout::from_data_dir(data_dir),
            wal: true,
            busy_timeout_ms: 5_000,
            synchronous: SqliteSynchronous::Normal,
            refuse_newer_schema: true,
            quarantine_on_corrupt: true,
            limits: IngestLimits::default(),
        }
    }
}

/// Deterministic ingest caps (IN3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestLimits {
    /// Max directory depth below the workspace root (default `32`).
    pub max_depth: u32,
    /// Max source files visited per pass (default `50_000`).
    pub max_files: u32,
    /// Max packages (default `1_000`).
    pub max_crates: u32,
    /// Max bytes hashed per file (default `4 MiB`); larger files are tracked
    /// by a marker digest over their length and counted as skipped.
    pub max_file_bytes: u64,
    /// Max nodes returned by one query (default `2_000`) — Q9.
    pub max_query_nodes: u32,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_files: 50_000,
            max_crates: 1_000,
            max_file_bytes: 4 * 1024 * 1024,
            max_query_nodes: 2_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_derives_the_reserved_graph_dir_shape() {
        let layout = GraphLayout::from_data_dir("/tmp/alloy-data");
        assert_eq!(layout.root, PathBuf::from("/tmp/alloy-data/graph"));
        assert_eq!(
            layout.db_path,
            PathBuf::from("/tmp/alloy-data/graph/graph.sqlite")
        );
        assert_eq!(
            layout.quarantine_dir,
            PathBuf::from("/tmp/alloy-data/graph/quarantine")
        );
    }
}
