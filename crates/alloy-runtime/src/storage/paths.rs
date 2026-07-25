//! Canonical on-disk layout under `RuntimeConfig.data_dir`.

use std::path::{Path, PathBuf};

use super::error::StoreError;

/// Canonical layout under `RuntimeConfig.data_dir`.
#[derive(Debug, Clone)]
pub struct StorageLayout {
    /// `data_dir` root.
    pub root: PathBuf,
    /// `data_dir/alloy.sqlite`.
    pub db_path: PathBuf,
    /// `data_dir/artifacts`.
    pub artifacts_dir: PathBuf,
    /// `data_dir/graph` (reserved for RFC-0011).
    pub graph_dir: PathBuf,
    /// Informational WAL sidecar hint (`alloy.sqlite-wal`).
    pub wal_sidecar_hint: PathBuf,
}

impl StorageLayout {
    /// Derive layout paths from a data directory root.
    #[must_use]
    pub fn from_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        let root = data_dir.into();
        let db_path = root.join("alloy.sqlite");
        Self {
            wal_sidecar_hint: PathBuf::from(format!("{}-wal", db_path.display())),
            db_path,
            artifacts_dir: root.join("artifacts"),
            graph_dir: root.join("graph"),
            root,
        }
    }

    /// Create `root`, `artifacts/`, `artifacts/tmp/`, and `graph/` if missing.
    pub fn ensure_dirs(&self) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.artifacts_dir)?;
        std::fs::create_dir_all(self.artifacts_dir.join("tmp"))?;
        std::fs::create_dir_all(self.artifacts_dir.join("sha256"))?;
        std::fs::create_dir_all(&self.graph_dir)?;
        Ok(())
    }

    /// Content-addressed path for a digest: `artifacts/sha256/<ab>/<hex>`.
    pub fn cas_path(&self, digest_hex: &str) -> Result<PathBuf, StoreError> {
        let (prefix, digest_hex) = validate_digest_hex(digest_hex)?;
        Ok(self
            .artifacts_dir
            .join("sha256")
            .join(prefix)
            .join(digest_hex))
    }

    /// Relative path stored in the artifacts index.
    pub fn cas_rel_path(digest_hex: &str) -> Result<String, StoreError> {
        let (prefix, digest_hex) = validate_digest_hex(digest_hex)?;
        Ok(format!("sha256/{prefix}/{digest_hex}"))
    }

    /// Resolve a relative artifact path under `artifacts_dir` (rejects traversal).
    pub fn resolve_artifact_rel(&self, rel: &str) -> Result<PathBuf, StoreError> {
        if rel.is_empty() || rel.contains("..") || Path::new(rel).is_absolute() {
            return Err(StoreError::Corrupt(format!(
                "refusing artifact path: {rel}"
            )));
        }
        Ok(self.artifacts_dir.join(rel))
    }
}

/// Validate lowercase SHA-256 hex and return `(prefix2, full_hex)`.
fn validate_digest_hex(digest_hex: &str) -> Result<(&str, &str), StoreError> {
    if digest_hex.len() != 64
        || !digest_hex
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(StoreError::Corrupt(format!(
            "invalid digest for CAS path: {digest_hex}"
        )));
    }
    Ok((&digest_hex[..2], digest_hex))
}

/// Maps `ALLOY_SQLITE_SYNCHRONOUS` / `PRAGMA synchronous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqliteSynchronous {
    /// `PRAGMA synchronous = OFF` (voids durability AC; tests must not use).
    Off,
    /// `PRAGMA synchronous = NORMAL` (default).
    #[default]
    Normal,
    /// `PRAGMA synchronous = FULL`.
    Full,
    /// `PRAGMA synchronous = EXTRA`.
    Extra,
}

impl SqliteSynchronous {
    /// Parse `OFF|NORMAL|FULL|EXTRA` (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s.trim().to_ascii_uppercase().as_str() {
            "OFF" => Ok(Self::Off),
            "NORMAL" => Ok(Self::Normal),
            "FULL" => Ok(Self::Full),
            "EXTRA" => Ok(Self::Extra),
            other => Err(StoreError::Io(format!(
                "invalid ALLOY_SQLITE_SYNCHRONOUS={other:?}; expected OFF|NORMAL|FULL|EXTRA (see example.env)"
            ))),
        }
    }

    /// SQLite pragma value string.
    #[must_use]
    pub fn as_pragma(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }
}

/// Options for [`super::AlloyStorage::open`].
#[derive(Debug, Clone)]
pub struct StorageOpenOptions {
    /// On-disk layout.
    pub layout: StorageLayout,
    /// Enable WAL journal mode (default true).
    pub wal: bool,
    /// Busy timeout milliseconds (default 5000).
    pub busy_timeout_ms: u32,
    /// `PRAGMA synchronous` (default [`SqliteSynchronous::Normal`]).
    pub synchronous: SqliteSynchronous,
    /// Refuse open when DB schema_version is newer than this code (default true).
    pub refuse_newer_schema: bool,
    /// Max CAS files to inspect for orphan warnings (`None` = skip scan).
    pub orphan_blob_scan_limit: Option<u32>,
}

impl StorageOpenOptions {
    /// Defaults for `data_dir` (WAL on, busy 5000ms, NORMAL sync, refuse newer).
    #[must_use]
    pub fn for_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            layout: StorageLayout::from_data_dir(data_dir),
            wal: true,
            busy_timeout_ms: 5000,
            synchronous: SqliteSynchronous::Normal,
            refuse_newer_schema: true,
            orphan_blob_scan_limit: None,
        }
    }

    /// Build options from process env + `data_dir`.
    ///
    /// Reads `ALLOY_SQLITE_BUSY_TIMEOUT_MS`, `ALLOY_SQLITE_SYNCHRONOUS`, `ALLOY_STORAGE_WAL`,
    /// and optional `ALLOY_STORAGE_ORPHAN_SCAN` / `ALLOY_STORAGE_ORPHAN_SCAN_LIMIT`.
    /// Never reads or writes `.env`.
    pub fn from_env(data_dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let mut opts = Self::for_data_dir(data_dir);
        if let Ok(v) = std::env::var("ALLOY_SQLITE_BUSY_TIMEOUT_MS") {
            if !v.is_empty() {
                opts.busy_timeout_ms = v.parse::<u32>().map_err(|_| {
                    StoreError::Io(format!(
                        "invalid ALLOY_SQLITE_BUSY_TIMEOUT_MS={v:?} (see example.env)"
                    ))
                })?;
            }
        }
        if let Ok(v) = std::env::var("ALLOY_SQLITE_SYNCHRONOUS") {
            if !v.is_empty() {
                opts.synchronous = SqliteSynchronous::parse(&v)?;
            }
        }
        if let Ok(v) = std::env::var("ALLOY_STORAGE_WAL") {
            if !v.is_empty() {
                opts.wal = parse_bool_env(&v).ok_or_else(|| {
                    StoreError::Io(format!(
                        "invalid ALLOY_STORAGE_WAL={v:?}; expected true/false/1/0 (see example.env)"
                    ))
                })?;
            }
        }
        if let Ok(v) = std::env::var("ALLOY_STORAGE_ORPHAN_SCAN") {
            if !v.is_empty() {
                let enabled = parse_bool_env(&v).ok_or_else(|| {
                    StoreError::Io(format!(
                        "invalid ALLOY_STORAGE_ORPHAN_SCAN={v:?}; expected true/false/1/0 (see example.env)"
                    ))
                })?;
                if enabled {
                    let limit = match std::env::var("ALLOY_STORAGE_ORPHAN_SCAN_LIMIT") {
                        Ok(raw) if !raw.is_empty() => raw.parse::<u32>().map_err(|_| {
                            StoreError::Io(format!(
                                "invalid ALLOY_STORAGE_ORPHAN_SCAN_LIMIT={raw:?} (see example.env)"
                            ))
                        })?,
                        _ => 1024,
                    };
                    opts.orphan_blob_scan_limit = Some(limit);
                }
            }
        }
        Ok(opts)
    }
}

fn parse_bool_env(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths() {
        let layout = StorageLayout::from_data_dir("/tmp/alloy-data");
        assert_eq!(
            layout.db_path,
            PathBuf::from("/tmp/alloy-data/alloy.sqlite")
        );
        assert_eq!(
            layout.artifacts_dir,
            PathBuf::from("/tmp/alloy-data/artifacts")
        );
        assert_eq!(layout.graph_dir, PathBuf::from("/tmp/alloy-data/graph"));
    }

    #[test]
    fn cas_path_rejects_bad_digest() {
        let layout = StorageLayout::from_data_dir("/tmp/x");
        assert!(layout.cas_path("abcd").is_err());
        assert!(layout.resolve_artifact_rel("../etc/passwd").is_err());
    }

    #[test]
    fn synchronous_parse() {
        assert_eq!(
            SqliteSynchronous::parse("normal").unwrap(),
            SqliteSynchronous::Normal
        );
        assert!(SqliteSynchronous::parse("bogus").is_err());
    }
}
