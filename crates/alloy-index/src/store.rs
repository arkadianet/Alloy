//! `SqliteProjectGraph` — the single writer for its data directory
//! (RFC-0011 §3.10, §5, §6, §8).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alloy_runtime::graph::{
    FileChange, FixEvent, GraphError, GraphQuery, GraphView, IngestReport, ProjectGraph,
};
use alloy_runtime::types::diagnostic::DiagnosticEvent;
use alloy_runtime::types::ids::{GraphSnapshotId, GraphVersion, Timestamp};
use async_trait::async_trait;
use rusqlite::Connection;

use crate::db::{acquire_instance_lock, from_rusqlite, open_connection, spawn_graph_db, GraphDb};
use crate::ingest::{self, scan_workspace, ScanOutput};
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
        if opts.limits.max_items == 0 {
            // SY15: a zero item cap would let the store claim SynDeep while
            // emitting nothing — rejected before any file is touched.
            return Err(GraphError::LimitExceeded(
                "max_items = 0 is rejected at open (SY15)".into(),
            ));
        }
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
        items: scan.items,
        imports: scan.imports,
        files: scan.files.len() as u32,
        skipped: scan.skipped,
        warnings: scan.warnings.clone(),
        // RS4/A-0014-4: fidelity is decided by the one seam function over
        // the model version this pass ingested under, never by a literal.
        source: migrate::fidelity_for_model_version(GRAPH_MODEL_VERSION),
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

        // RFC-0014 OQ6 (Beta default): any manifest or `.rs` change —
        // including a plain modification — re-derives from the tree. Item
        // and import facts come from the parse, so a per-file digest patch
        // cannot keep IN10's incremental ≡ rebuild equivalence; per-file
        // item invalidation waits on T14's digest-equality proof. Non-`.rs`,
        // non-manifest paths are ignored and counted (§6.6).
        let relevant = changes.iter().any(|c| {
            c.path == "Cargo.toml" || c.path.ends_with("/Cargo.toml") || c.path.ends_with(".rs")
        });
        if !relevant {
            GraphMetrics::add(&self.metrics.files_skipped, changes.len() as u64);
            let db = Arc::clone(&self.db);
            return spawn_graph_db(db, |db| db.with(read_version)).await;
        }

        let limits = self.limits;
        let scan = tokio::task::spawn_blocking(move || scan_workspace(&root, &limits))
            .await
            .map_err(|e| GraphError::Internal(format!("spawn_blocking join: {e}")))??;
        let db = Arc::clone(&self.db);
        let report =
            spawn_graph_db(db, move |db| db.with_mut(|conn| commit_scan(conn, &scan))).await?;
        GraphMetrics::add(&self.metrics.files_skipped, u64::from(report.skipped));
        Ok(report.version)
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
