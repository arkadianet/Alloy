//! Versioned SQL migrations for the graph store (RFC-0011 §5, Appendix A).
//!
//! RFC-0002's ledger shape (rule S5): `graph_schema_migrations` is
//! bootstrapped before v1, sequential `if current < N` blocks run inside one
//! transaction, and `current_version = SELECT MAX(version)`.

use alloy_runtime::graph::GraphError;
use rusqlite::Connection;

use crate::db::from_rusqlite;

/// Schema version shipped by this crate (rule S3).
pub(crate) const GRAPH_SCHEMA_VERSION: u32 = 1;

/// Ingest-semantics version (rule S4). A mismatch truncates and re-ingests;
/// it never migrates — the graph is a derived cache (G1).
pub(crate) const GRAPH_MODEL_VERSION: u32 = 1;

// Appendix A, verbatim.
const V1_SQL: &str = r#"
CREATE TABLE graph_meta (
  id                  INTEGER PRIMARY KEY CHECK (id = 1),
  model_version       INTEGER NOT NULL,
  graph_version       INTEGER NOT NULL,
  content_digest      TEXT NOT NULL,
  workspace_root_rule TEXT NOT NULL,
  updated_at          TEXT NOT NULL
);

CREATE TABLE graph_nodes (
  id       TEXT PRIMARY KEY,
  kind     TEXT NOT NULL CHECK (kind IN ('workspace','crate','module','item')),
  path     TEXT NOT NULL,
  crate_id TEXT NULL,
  file     TEXT NULL,
  digest   TEXT NULL,
  UNIQUE (kind, path)
);

CREATE INDEX idx_graph_nodes_crate ON graph_nodes(crate_id);
CREATE INDEX idx_graph_nodes_file  ON graph_nodes(file);

CREATE TABLE graph_edges (
  from_id    TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  to_id      TEXT NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('defines','imports')),
  confidence REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (from_id, to_id, kind)
);

CREATE INDEX idx_graph_edges_to ON graph_edges(to_id, kind);

CREATE TABLE graph_files (
  path      TEXT PRIMARY KEY,
  crate_id  TEXT NULL,
  module_id TEXT NULL REFERENCES graph_nodes(id) ON DELETE SET NULL,
  digest    TEXT NOT NULL,
  byte_len  INTEGER NOT NULL
);

CREATE INDEX idx_graph_files_crate ON graph_files(crate_id);

CREATE TABLE graph_diagnostics (
  diagnostic_id TEXT PRIMARY KEY,
  code          TEXT NULL,
  level         TEXT NOT NULL,
  package       TEXT NULL,
  fingerprint   TEXT NOT NULL,
  primary_path  TEXT NULL,
  message       TEXT NOT NULL,
  event_json    TEXT NOT NULL,
  recorded_at   TEXT NOT NULL
);

CREATE INDEX idx_graph_diagnostics_pkg_time ON graph_diagnostics(package, recorded_at);
CREATE INDEX idx_graph_diagnostics_code     ON graph_diagnostics(code);
CREATE INDEX idx_graph_diagnostics_fp       ON graph_diagnostics(fingerprint);

CREATE TABLE graph_fixes (
  fix_id          TEXT PRIMARY KEY,
  diagnostic_id   TEXT NULL,
  diagnostic_code TEXT NULL,
  crate_id        TEXT NULL,
  transaction_id  TEXT NULL,
  patch_artifact  TEXT NULL,
  verified        INTEGER NOT NULL CHECK (verified IN (0,1)),
  recorded_at     TEXT NOT NULL
);

CREATE INDEX idx_graph_fixes_code ON graph_fixes(diagnostic_code, recorded_at);

CREATE TABLE graph_snapshots (
  snapshot_id    TEXT PRIMARY KEY,
  graph_version  INTEGER NOT NULL,
  content_digest TEXT NOT NULL,
  node_count     INTEGER NOT NULL,
  edge_count     INTEGER NOT NULL,
  created_at     TEXT NOT NULL
);

CREATE INDEX idx_graph_snapshots_version ON graph_snapshots(graph_version);
"#;

/// Run pending migrations; return the schema version in effect.
#[tracing::instrument(skip(conn), name = "index.migrate")]
pub(crate) fn migrate(conn: &Connection, refuse_newer: bool) -> Result<u32, GraphError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS graph_schema_migrations (
           version    INTEGER PRIMARY KEY,
           applied_at TEXT NOT NULL
         );",
    )
    .map_err(from_rusqlite)?;

    let current: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM graph_schema_migrations",
            [],
            |r| r.get(0),
        )
        .map_err(from_rusqlite)?;

    if current > GRAPH_SCHEMA_VERSION {
        if refuse_newer {
            return Err(GraphError::Migration(format!(
                "graph schema version {current} is newer than this build's {GRAPH_SCHEMA_VERSION}"
            )));
        }
        return Ok(current);
    }

    if current < 1 {
        let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
        tx.execute_batch(V1_SQL).map_err(from_rusqlite)?;
        tx.execute(
            "INSERT INTO graph_schema_migrations (version, applied_at) VALUES (1, ?1)",
            [crate::store::now_rfc3339()?],
        )
        .map_err(from_rusqlite)?;
        tx.execute(
            "INSERT INTO graph_meta
               (id, model_version, graph_version, content_digest, workspace_root_rule, updated_at)
             VALUES (1, ?1, 0, '', 'unset', ?2)",
            rusqlite::params![GRAPH_MODEL_VERSION, crate::store::now_rfc3339()?],
        )
        .map_err(from_rusqlite)?;
        tx.commit().map_err(from_rusqlite)?;
        tracing::info!(version = 1, "applied graph migration");
    }

    let version: u32 = conn
        .query_row(
            "SELECT MAX(version) FROM graph_schema_migrations",
            [],
            |r| r.get(0),
        )
        .map_err(from_rusqlite)?;
    Ok(version)
}

/// Rule S4: model-version discipline. On mismatch, truncate the derived
/// tables and reset the meta row; never merge or migrate.
pub(crate) fn check_model_version(conn: &Connection) -> Result<(), GraphError> {
    let stored: u32 = conn
        .query_row(
            "SELECT model_version FROM graph_meta WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .map_err(from_rusqlite)?;
    if stored == GRAPH_MODEL_VERSION {
        return Ok(());
    }
    tracing::warn!(
        stored,
        code = GRAPH_MODEL_VERSION,
        "graph model version mismatch; truncating derived cache for re-ingest (S4)"
    );
    let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
    tx.execute_batch(
        "DELETE FROM graph_edges;
         DELETE FROM graph_nodes;
         DELETE FROM graph_files;",
    )
    .map_err(from_rusqlite)?;
    tx.execute(
        "UPDATE graph_meta
           SET model_version = ?1, graph_version = 0, content_digest = '', updated_at = ?2
         WHERE id = 1",
        rusqlite::params![GRAPH_MODEL_VERSION, crate::store::now_rfc3339()?],
    )
    .map_err(from_rusqlite)?;
    tx.commit().map_err(from_rusqlite)?;
    Ok(())
}
