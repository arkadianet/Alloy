//! Versioned SQL migrations for the Alloy SQLite store.

use rusqlite::Connection;

use super::error::StoreError;

/// Schema version shipped by this crate (RFC-0002).
pub const CODE_SCHEMA_VERSION: u32 = 2;

// `schema_migrations` is bootstrapped in `ensure_migrations_table` before v1 runs.
// PK (session_id, seq) already covers session_events lookups; no redundant index.
const V1_SQL: &str = r#"
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  workspace_root TEXT NOT NULL,
  profile TEXT NOT NULL,
  budget_json TEXT NOT NULL,
  language_backends_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  graph_version INTEGER NULL
);

CREATE TABLE runs (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id),
  goal_json TEXT NOT NULL,
  state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE session_seq (
  session_id TEXT PRIMARY KEY,
  next_seq INTEGER NOT NULL
);

CREATE TABLE session_events (
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  ts TEXT NOT NULL,
  run_id TEXT NULL,
  type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
);

CREATE TABLE runtime_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  event_json TEXT NOT NULL
);

CREATE TABLE artifacts (
  id TEXT PRIMARY KEY,
  digest TEXT NOT NULL,
  kind TEXT NOT NULL,
  content_type TEXT NULL,
  byte_len INTEGER NOT NULL,
  rel_path TEXT NOT NULL,
  session_id TEXT NULL,
  run_id TEXT NULL,
  labels_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  deleted_at TEXT NULL
);

CREATE INDEX idx_artifacts_digest ON artifacts(digest);

-- Reserved for RFC-0009 (unused by 0002 logic beyond create)
CREATE TABLE dag_blobs (
  dag_id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  generation INTEGER NOT NULL,
  blob_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
"#;

const V2_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_runs_session_created_id
  ON runs(session_id, created_at, id);
"#;

/// Apply pending migrations. Returns the schema version after migrate.
///
/// When `refuse_newer` is true and the DB reports a version above
/// [`CODE_SCHEMA_VERSION`], returns [`StoreError::Migration`].
#[tracing::instrument(skip(conn), level = "info", name = "storage.migrate")]
pub fn migrate(conn: &Connection, refuse_newer: bool) -> Result<u32, StoreError> {
    ensure_migrations_table(conn)?;
    let current = current_version(conn)?;

    if current > CODE_SCHEMA_VERSION {
        if refuse_newer {
            return Err(StoreError::Migration(format!(
                "database schema_version {current} is newer than code version {CODE_SCHEMA_VERSION}"
            )));
        }
        tracing::warn!(
            current,
            code = CODE_SCHEMA_VERSION,
            "opening database with newer schema than this binary"
        );
        return Ok(current);
    }

    if current < 1 {
        tracing::info!(version = 1, "applying migration");
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        tx.execute_batch(V1_SQL)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        // `schema_migrations` was bootstrapped empty by `ensure_migrations_table`.
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![1i64, now_rfc3339()],
        )
        .map_err(|e| StoreError::Migration(e.to_string()))?;
        tx.commit()
            .map_err(|e| StoreError::Migration(e.to_string()))?;
    }

    let current = current_version(conn)?;
    if current < 2 {
        tracing::info!(version = 2, "applying migration");
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        tx.execute_batch(V2_SQL)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![2i64, now_rfc3339()],
        )
        .map_err(|e| StoreError::Migration(e.to_string()))?;
        tx.commit()
            .map_err(|e| StoreError::Migration(e.to_string()))?;
    }

    let after = current_version(conn)?;
    if after < CODE_SCHEMA_VERSION {
        return Err(StoreError::Migration(format!(
            "migration incomplete: at {after}, expected {CODE_SCHEMA_VERSION}"
        )));
    }
    Ok(after)
}

fn ensure_migrations_table(conn: &Connection) -> Result<(), StoreError> {
    // Before v1, the table may not exist. Create a bootstrap table so we can
    // read the version; v1 SQL also creates it (IF NOT EXISTS via our check).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
    )
    .map_err(|e| StoreError::Migration(e.to_string()))?;
    Ok(())
}

fn current_version(conn: &Connection) -> Result<u32, StoreError> {
    let max: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .map_err(|e| StoreError::Migration(e.to_string()))?;
    Ok(max.unwrap_or(0) as u32)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Cross-check `session_seq` against `MAX(session_events.seq)` (recovery).
pub fn verify_seq_consistency(conn: &Connection) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.next_seq, MAX(e.seq) AS max_seq
         FROM session_seq s
         LEFT JOIN session_events e ON e.session_id = s.session_id
         GROUP BY s.session_id, s.next_seq
         HAVING s.next_seq < 0
            OR (MAX(e.seq) IS NULL AND s.next_seq != 0)
            OR (MAX(e.seq) IS NOT NULL AND s.next_seq != MAX(e.seq) + 1)",
    )?;
    let mut rows = stmt.query_map([], |row| {
        let sid: String = row.get(0)?;
        let next: i64 = row.get(1)?;
        let max_seq: Option<i64> = row.get(2)?;
        Ok((sid, next, max_seq))
    })?;

    // Any HAVING row is an inconsistency; report the first with session detail.
    if let Some(row) = rows.next() {
        let (sid, next_seq, max_seq) = row?;
        if next_seq < 0 {
            return Err(StoreError::Corrupt(format!(
                "session_seq.next_seq < 0 for {sid}"
            )));
        }
        match max_seq {
            None => {
                return Err(StoreError::Corrupt(format!(
                    "session {sid}: no events but next_seq={next_seq}"
                )));
            }
            Some(max) => {
                return Err(StoreError::Corrupt(format!(
                    "session {sid}: next_seq={next_seq} but MAX(seq)={max}"
                )));
            }
        }
    }

    // Events without a session_seq row are corrupt.
    let orphan: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_events e
         WHERE NOT EXISTS (
           SELECT 1 FROM session_seq s WHERE s.session_id = e.session_id
         )",
        [],
        |r| r.get(0),
    )?;
    if orphan > 0 {
        return Err(StoreError::Corrupt(format!(
            "{orphan} session_events rows without session_seq"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrate_fresh_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        let v = migrate(&conn, true).unwrap();
        assert_eq!(v, 2);
        let v2 = migrate(&conn, true).unwrap();
        assert_eq!(v2, 2);
    }

    #[test]
    fn refuse_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn, true).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (99, 't')",
            [],
        )
        .unwrap();
        let err = migrate(&conn, true).unwrap_err();
        assert!(matches!(err, StoreError::Migration(_)));
    }
}
