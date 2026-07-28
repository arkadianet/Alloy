//! `SqliteProjectGraph` — the single writer for its data directory
//! (RFC-0011 §3.10, §5, §6, §8).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alloy_runtime::graph::{
    FileChange, FileChangeKind, FixEvent, GraphError, GraphFidelity, GraphQuery, GraphView,
    IngestReport, ProjectGraph,
};
use alloy_runtime::types::diagnostic::DiagnosticEvent;
use alloy_runtime::types::ids::{Digest, GraphSnapshotId, GraphVersion, Timestamp};
use async_trait::async_trait;
use rusqlite::Connection;

use crate::db::{acquire_instance_lock, from_rusqlite, open_connection, spawn_graph_db, GraphDb};
use crate::ingest::{self, hash_file, scan_workspace, ScanOutput};
use crate::layout::{GraphLayout, GraphOpenOptions, IngestLimits};
use crate::metrics::{GraphMetrics, GraphMetricsSnapshot};
use crate::migrate::{self, GRAPH_MODEL_VERSION};
use crate::query;

/// RFC-3339 wall-clock string matching RFC-0002's `Timestamp` serde.
pub(crate) fn now_rfc3339() -> Result<String, GraphError> {
    let json = serde_json::to_string(&Timestamp::now())
        .map_err(|e| GraphError::Internal(format!("encode timestamp: {e}")))?;
    Ok(json.trim_matches('"').to_string())
}

/// SQLite-backed [`ProjectGraph`]. The single writer for its data directory.
#[derive(Debug)]
pub struct SqliteProjectGraph {
    db: Arc<GraphDb>,
    layout: GraphLayout,
    limits: IngestLimits,
    schema_version: u32,
    metrics: Arc<GraphMetrics>,
    /// Root of the last `rebuild` in this process; `apply_incremental`
    /// resolves change paths against it.
    workspace_root: Mutex<Option<PathBuf>>,
}

impl SqliteProjectGraph {
    /// Open (creating and migrating as needed). Quarantines a corrupt file
    /// when `quarantine_on_corrupt` is set (rule S8).
    #[tracing::instrument(skip(opts), fields(db = %opts.layout.db_path.display()), name = "index.open")]
    pub async fn open(opts: GraphOpenOptions) -> Result<Self, GraphError> {
        let metrics = Arc::new(GraphMetrics::default());
        let metrics2 = Arc::clone(&metrics);
        tokio::task::spawn_blocking(move || Self::open_sync(&opts, &metrics2))
            .await
            .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))?
            .map(|mut s| {
                s.metrics = metrics;
                s
            })
    }

    fn open_sync(opts: &GraphOpenOptions, metrics: &Arc<GraphMetrics>) -> Result<Self, GraphError> {
        opts.layout.ensure_dirs()?;
        let lock = acquire_instance_lock(&opts.layout.root)?;

        let attempt = |opts: &GraphOpenOptions| -> Result<(Connection, u32), GraphError> {
            let conn = open_connection(opts)?;
            let version = migrate::migrate(&conn, opts.refuse_newer_schema)?;
            migrate::check_model_version(&conn)?;
            Ok((conn, version))
        };

        let (conn, schema_version) = match attempt(opts) {
            Ok(ok) => ok,
            Err(GraphError::Corrupt(reason)) if opts.quarantine_on_corrupt => {
                quarantine(&opts.layout, &reason)?;
                GraphMetrics::bump(&metrics.quarantines);
                attempt(opts)?
            }
            Err(e) => return Err(e),
        };

        Ok(Self {
            db: GraphDb::new(conn, lock),
            layout: opts.layout.clone(),
            limits: opts.limits,
            schema_version,
            metrics: Arc::clone(metrics),
            workspace_root: Mutex::new(None),
        })
    }

    /// Schema version of the open database.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Model version of the open database (§5.4).
    #[must_use]
    pub fn model_version(&self) -> u32 {
        GRAPH_MODEL_VERSION
    }

    /// Layout in use.
    #[must_use]
    pub fn layout(&self) -> &GraphLayout {
        &self.layout
    }

    /// Metrics snapshot (§10.2).
    #[must_use]
    pub fn metrics(&self) -> GraphMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Full ingest returning the detailed report (`rebuild` returns only
    /// the version). Memoizes `root` on success so `apply_incremental`
    /// works after either entry point.
    pub async fn rebuild_reported(&self, root: &Path) -> Result<IngestReport, GraphError> {
        let owned_root = root.to_path_buf();
        let limits = self.limits;
        let scan = {
            let scan_root = owned_root.clone();
            tokio::task::spawn_blocking(move || scan_workspace(&scan_root, &limits))
                .await
                .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))??
        };

        let db = Arc::clone(&self.db);
        let report =
            spawn_graph_db(db, move |db| db.with_mut(|conn| commit_scan(conn, &scan))).await?;

        // Memoize on success so `apply_incremental` works after either
        // `rebuild` or `rebuild_reported`.
        *self
            .workspace_root
            .lock()
            .map_err(|_| GraphError::Internal("root mutex poisoned".into()))? = Some(owned_root);

        GraphMetrics::bump(&self.metrics.rebuilds);
        if report.unchanged {
            GraphMetrics::bump(&self.metrics.rebuilds_unchanged);
        }
        GraphMetrics::add(&self.metrics.files_skipped, u64::from(report.skipped));
        Ok(report)
    }

    /// WAL truncate-checkpoint and close. Idempotent (X5).
    #[tracing::instrument(skip(self), name = "index.close")]
    pub async fn close(&self) -> Result<(), GraphError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let Some(conn) = db.take_connection()? else {
                return Ok(()); // already closed — idempotent.
            };
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(from_rusqlite)?;
            drop(conn);
            Ok(())
        })
        .await
        .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))?
    }

    fn remembered_root(&self) -> Result<PathBuf, GraphError> {
        self.workspace_root
            .lock()
            .map_err(|_| GraphError::Internal("root mutex poisoned".into()))?
            .clone()
            .ok_or_else(|| {
                GraphError::Workspace(
                    "apply_incremental requires a prior rebuild in this process".into(),
                )
            })
    }
}

impl Drop for SqliteProjectGraph {
    fn drop(&mut self) {
        if self.db.connection_present() {
            tracing::warn!(
                db = %self.layout.db_path.display(),
                "SqliteProjectGraph dropped without close(); WAL not checkpointed"
            );
        }
    }
}

/// S8: move the corrupt DB (with sidecars) into `quarantine/`.
#[tracing::instrument(skip(layout), name = "index.quarantine")]
fn quarantine(layout: &GraphLayout, reason: &str) -> Result<(), GraphError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| GraphError::Internal(format!("clock: {e}")))?
        .as_nanos();
    tracing::warn!(%reason, "quarantining corrupt graph database (S8)");
    let base = layout
        .db_path
        .file_name()
        .ok_or_else(|| GraphError::Internal("graph db path has no file name".into()))?;
    for suffix in ["", "-wal", "-shm"] {
        // OsStr-based so non-UTF-8 path components survive untouched.
        let mut name = base.to_os_string();
        name.push(suffix);
        let src = layout.db_path.with_file_name(&name);
        if src.exists() {
            name.push(format!(".{nanos}"));
            let dest = layout.quarantine_dir.join(&name);
            std::fs::rename(&src, &dest)
                .map_err(|e| GraphError::Io(format!("quarantine {}: {e}", src.display())))?;
        }
    }
    Ok(())
}

/// §6.5 step 7: one transaction — replace rows, conditionally bump.
fn commit_scan(conn: &mut Connection, scan: &ScanOutput) -> Result<IngestReport, GraphError> {
    let digest = scan.content_digest();
    let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;

    let (stored_version, stored_digest): (u64, String) = tx
        .query_row(
            "SELECT graph_version, content_digest FROM graph_meta WHERE id = 1",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
        )
        .map_err(from_rusqlite)?;

    tx.execute_batch(
        "DELETE FROM graph_edges;
         DELETE FROM graph_nodes;
         DELETE FROM graph_files;",
    )
    .map_err(from_rusqlite)?;

    {
        let mut node_stmt = tx
            .prepare(
                "INSERT INTO graph_nodes (id, kind, path, crate_id, file, digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(from_rusqlite)?;
        for n in &scan.nodes {
            node_stmt
                .execute(rusqlite::params![
                    n.id.to_string(),
                    n.kind.as_str(),
                    n.path,
                    n.crate_id,
                    n.file,
                    n.digest.as_ref().map(|d| d.as_hex().to_string()),
                ])
                .map_err(from_rusqlite)?;
        }
        let mut edge_stmt = tx
            .prepare(
                "INSERT INTO graph_edges (from_id, to_id, kind, confidence)
                 VALUES (?1, ?2, ?3, 1.0)",
            )
            .map_err(from_rusqlite)?;
        for e in &scan.edges {
            edge_stmt
                .execute(rusqlite::params![
                    e.from.to_string(),
                    e.to.to_string(),
                    e.kind.as_str(),
                ])
                .map_err(from_rusqlite)?;
        }
        let mut file_stmt = tx
            .prepare(
                "INSERT INTO graph_files (path, crate_id, module_id, digest, byte_len)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(from_rusqlite)?;
        for f in &scan.files {
            file_stmt
                .execute(rusqlite::params![
                    f.path,
                    f.crate_id,
                    f.module_id.to_string(),
                    f.digest.as_hex(),
                    f.byte_len as i64,
                ])
                .map_err(from_rusqlite)?;
        }
    }

    let unchanged = stored_digest == digest.as_hex();
    let version = if unchanged {
        stored_version
    } else {
        stored_version + 1
    };
    tx.execute(
        "UPDATE graph_meta
           SET graph_version = ?1, content_digest = ?2, workspace_root_rule = 'caller',
               updated_at = ?3
         WHERE id = 1",
        rusqlite::params![version as i64, digest.as_hex(), now_rfc3339()?],
    )
    .map_err(from_rusqlite)?;
    tx.commit().map_err(from_rusqlite)?;

    Ok(IngestReport {
        version: GraphVersion(version),
        unchanged,
        crates: scan.crates,
        modules: scan.modules,
        files: scan.files.len() as u32,
        skipped: scan.skipped,
        warnings: scan.warnings.clone(),
        source: GraphFidelity::Manifest,
    })
}

/// Read the current version inside an open connection.
pub(crate) fn read_version(conn: &Connection) -> Result<GraphVersion, GraphError> {
    let v: i64 = conn
        .query_row(
            "SELECT graph_version FROM graph_meta WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .map_err(from_rusqlite)?;
    Ok(GraphVersion(v as u64))
}

/// Recompute the §4.6 content digest from the stored rows and bump the
/// version iff it changed (shared by the incremental path).
fn recompute_and_bump(conn: &Connection) -> Result<(GraphVersion, bool), GraphError> {
    let digest = digest_from_rows(conn)?;
    let (stored_version, stored_digest): (i64, String) = conn
        .query_row(
            "SELECT graph_version, content_digest FROM graph_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(from_rusqlite)?;
    if stored_digest == digest.as_hex() {
        return Ok((GraphVersion(stored_version as u64), false));
    }
    let next = stored_version + 1;
    conn.execute(
        "UPDATE graph_meta SET graph_version = ?1, content_digest = ?2, updated_at = ?3
         WHERE id = 1",
        rusqlite::params![next, digest.as_hex(), now_rfc3339()?],
    )
    .map_err(from_rusqlite)?;
    Ok((GraphVersion(next as u64), true))
}

/// §4.6 canonical rendering over the stored rows.
fn digest_from_rows(conn: &Connection) -> Result<Digest, GraphError> {
    use alloy_runtime::types::ids::DigestHasher;
    let mut hasher = DigestHasher::new();
    let mut stmt = conn
        .prepare(
            "SELECT kind, path, crate_id, file, digest FROM graph_nodes
             ORDER BY CASE kind
                        WHEN 'workspace' THEN 0 WHEN 'crate' THEN 1
                        WHEN 'module' THEN 2 ELSE 3 END, path",
        )
        .map_err(from_rusqlite)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(from_rusqlite)?;
    for row in rows {
        let (kind, path, crate_id, file, digest) = row.map_err(from_rusqlite)?;
        hasher.update(kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(crate_id.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
        hasher.update(file.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\n");
    }
    let mut stmt = conn
        .prepare(
            "SELECT nf.path, nt.path, e.kind
               FROM graph_edges e
               JOIN graph_nodes nf ON nf.id = e.from_id
               JOIN graph_nodes nt ON nt.id = e.to_id
              ORDER BY nf.path, nt.path, e.kind",
        )
        .map_err(from_rusqlite)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(from_rusqlite)?;
    for row in rows {
        let (from_path, to_path, kind) = row.map_err(from_rusqlite)?;
        hasher.update(from_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(to_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(kind.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hasher.finish())
}

#[async_trait]
impl ProjectGraph for SqliteProjectGraph {
    async fn rebuild(&self, root: &Path) -> Result<GraphVersion, GraphError> {
        Ok(self.rebuild_reported(root).await?.version)
    }

    #[tracing::instrument(skip_all, name = "index.incremental")]
    async fn apply_incremental(&self, changes: &[FileChange]) -> Result<GraphVersion, GraphError> {
        GraphMetrics::bump(&self.metrics.incrementals);
        if changes.is_empty() {
            // §6.6: no-op returning the current version without a write.
            let db = Arc::clone(&self.db);
            return spawn_graph_db(db, |db| db.with(read_version)).await;
        }
        for change in changes {
            ingest::validate_change_path(&change.path)?; // IN11
        }
        let root = self.remembered_root()?;

        // Classify: any structural change (manifest, created/deleted .rs)
        // re-derives from the tree — a conservative superset of §6.6's
        // per-subtree table that IN10's rebuild-equivalence sanctions.
        let structural = changes.iter().any(|c| {
            c.path == "Cargo.toml"
                || c.path.ends_with("/Cargo.toml")
                || (c.path.ends_with(".rs")
                    && matches!(c.kind, FileChangeKind::Created | FileChangeKind::Deleted))
        });

        if structural {
            let limits = self.limits;
            let root2 = root.clone();
            let scan = tokio::task::spawn_blocking(move || scan_workspace(&root2, &limits))
                .await
                .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))??;
            let db = Arc::clone(&self.db);
            let report =
                spawn_graph_db(db, move |db| db.with_mut(|conn| commit_scan(conn, &scan))).await?;
            GraphMetrics::add(&self.metrics.files_skipped, u64::from(report.skipped));
            return Ok(report.version);
        }

        // Modified-only path: re-hash tracked module files; untracked or
        // non-.rs paths are ignored and counted (§6.6). Filesystem work runs
        // on the blocking pool (X3's posture), and the same symlink/escape
        // rules as the scan walk apply (IN4/SEC7): a symlinked file, or a
        // path whose resolution leaves the workspace root, is skipped.
        let limits = self.limits;
        let change_paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let hash_root = root.clone();
        let (updates, skipped) = tokio::task::spawn_blocking(
            move || -> Result<(Vec<(String, Digest, u64)>, u64), GraphError> {
                let canonical_root = hash_root
                    .canonicalize()
                    .map_err(|e| GraphError::Workspace(format!("canonicalize root: {e}")))?;
                let mut updates: Vec<(String, Digest, u64)> = Vec::new();
                let mut skipped = 0u64;
                for path in change_paths {
                    if !path.ends_with(".rs") {
                        skipped += 1;
                        continue;
                    }
                    let abs = hash_root.join(&path);
                    // IN4/SEC7: never hash through a symlink...
                    let is_symlink = std::fs::symlink_metadata(&abs)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false);
                    if is_symlink || !abs.is_file() {
                        skipped += 1;
                        continue;
                    }
                    // ...and never a path that resolves outside the root
                    // (a symlinked intermediate directory).
                    match abs.canonicalize() {
                        Ok(resolved) if resolved.starts_with(&canonical_root) => {}
                        _ => {
                            skipped += 1;
                            continue;
                        }
                    }
                    let (digest, byte_len, oversized) = hash_file(&abs, &limits)?;
                    if oversized {
                        skipped += 1;
                    }
                    updates.push((path, digest, byte_len));
                }
                Ok((updates, skipped))
            },
        )
        .await
        .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))??;
        GraphMetrics::add(&self.metrics.files_skipped, skipped);

        let db = Arc::clone(&self.db);
        spawn_graph_db(db, move |db| {
            db.with_mut(|conn| {
                let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
                {
                    let mut files_stmt = tx
                        .prepare_cached(
                            "UPDATE graph_files SET digest = ?1, byte_len = ?2 WHERE path = ?3",
                        )
                        .map_err(from_rusqlite)?;
                    let mut nodes_stmt = tx
                        .prepare_cached("UPDATE graph_nodes SET digest = ?1 WHERE file = ?2")
                        .map_err(from_rusqlite)?;
                    for (path, digest, byte_len) in &updates {
                        let changed = files_stmt
                            .execute(rusqlite::params![digest.as_hex(), *byte_len as i64, path])
                            .map_err(from_rusqlite)?;
                        if changed == 0 {
                            continue; // not a tracked module file — ignored (§6.6).
                        }
                        nodes_stmt
                            .execute(rusqlite::params![digest.as_hex(), path])
                            .map_err(from_rusqlite)?;
                    }
                }
                let (version, _bumped) = recompute_and_bump(&tx)?;
                tx.commit().map_err(from_rusqlite)?;
                Ok(version)
            })
        })
        .await
    }

    #[tracing::instrument(skip_all, name = "index.query")]
    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError> {
        GraphMetrics::bump(&self.metrics.queries);
        let limits = self.limits;
        let db = Arc::clone(&self.db);
        let view = spawn_graph_db(db, move |db| db.with(|conn| query::run(conn, &q, &limits)))
            .await
            .inspect_err(|e| {
                if matches!(e, GraphError::Busy) {
                    GraphMetrics::bump(&self.metrics.busy_errors);
                }
            })?;
        if view.truncated {
            if view.is_empty() {
                GraphMetrics::bump(&self.metrics.queries_stub);
            } else {
                GraphMetrics::bump(&self.metrics.queries_truncated);
            }
        }
        Ok(view)
    }

    #[tracing::instrument(skip_all, name = "index.record_diagnostic")]
    async fn record_diagnostic(&self, d: DiagnosticEvent) -> Result<(), GraphError> {
        let root = self
            .workspace_root
            .lock()
            .map_err(|_| GraphError::Internal("root mutex poisoned".into()))?
            .clone();
        let db = Arc::clone(&self.db);
        spawn_graph_db(db, move |db| {
            db.with_mut(|conn| query::record_diagnostic(conn, &d, root.as_deref()))
        })
        .await?;
        GraphMetrics::bump(&self.metrics.diagnostics_recorded);
        Ok(())
    }

    #[tracing::instrument(skip_all, name = "index.record_fix")]
    async fn record_fix(&self, f: FixEvent) -> Result<(), GraphError> {
        let db = Arc::clone(&self.db);
        spawn_graph_db(db, move |db| {
            db.with_mut(|conn| query::record_fix(conn, &f))
        })
        .await?;
        GraphMetrics::bump(&self.metrics.fixes_recorded);
        Ok(())
    }

    #[tracing::instrument(skip_all, name = "index.snapshot")]
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
        let db = Arc::clone(&self.db);
        let id = spawn_graph_db(db, move |db| {
            db.with_mut(|conn| {
                let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
                let (version, digest): (i64, String) = tx
                    .query_row(
                        "SELECT graph_version, content_digest FROM graph_meta WHERE id = 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .map_err(from_rusqlite)?;
                let nodes: i64 = tx
                    .query_row("SELECT COUNT(*) FROM graph_nodes", [], |r| r.get(0))
                    .map_err(from_rusqlite)?;
                let edges: i64 = tx
                    .query_row("SELECT COUNT(*) FROM graph_edges", [], |r| r.get(0))
                    .map_err(from_rusqlite)?;
                let id = GraphSnapshotId::new();
                tx.execute(
                    "INSERT INTO graph_snapshots
                       (snapshot_id, graph_version, content_digest, node_count, edge_count,
                        created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id.to_string(),
                        version,
                        digest,
                        nodes,
                        edges,
                        now_rfc3339()?
                    ],
                )
                .map_err(from_rusqlite)?;
                tx.commit().map_err(from_rusqlite)?;
                Ok(id)
            })
        })
        .await?;
        GraphMetrics::bump(&self.metrics.snapshots);
        Ok(id)
    }

    async fn version(&self) -> Result<GraphVersion, GraphError> {
        let db = Arc::clone(&self.db);
        spawn_graph_db(db, |db| db.with(read_version)).await
    }
}
