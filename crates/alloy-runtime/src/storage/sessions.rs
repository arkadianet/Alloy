//! Thin session/run row persistence (orchestration in RFC-0003).

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::codec::{parse_run_id, parse_session_id, path_to_utf8, ts_from_text, ts_to_text};
use super::error::StoreError;
use super::gate::StorageGate;
use super::metrics::StorageMetrics;
use super::open::{spawn_db, DbHandle};
use crate::session::Session;
use crate::types::ids::{GraphVersion, RunId, SessionId, Timestamp};

/// Opaque run row for RFC-0003 (state vocabulary owned there).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRow {
    /// Run id.
    pub id: RunId,
    /// Parent session.
    pub session_id: SessionId,
    /// Goal JSON (opaque to 0002).
    pub goal_json: serde_json::Value,
    /// Opaque state string until RFC-0003 pins an enum.
    pub state: String,
    /// Created at.
    pub created_at: Timestamp,
    /// Updated at.
    pub updated_at: Timestamp,
}

/// Thin session/run persistence helpers.
#[async_trait]
pub trait SessionRows: Send + Sync {
    /// Upsert a session row, persisting `provenance` atomically with it.
    ///
    /// Research §7.11 item 4: consent cannot be obtained retroactively, so
    /// provenance is **required creation input** and **write-once** — on
    /// conflict the mutable session fields update but `provenance_json` is
    /// never touched. There is deliberately no post-creation mutation
    /// surface: consent elevation means a new session.
    async fn upsert_session(
        &self,
        session: &Session,
        provenance: &crate::types::provenance::SessionProvenance,
    ) -> Result<(), StoreError>;
    /// Load a session row.
    async fn get_session(&self, id: SessionId) -> Result<Option<Session>, StoreError>;

    /// Load session provenance. `Ok(None)` = never recorded (pre-v4 legacy
    /// rows only) — read it as "no consent, provenance unknown" (fail
    /// closed).
    async fn get_provenance(
        &self,
        id: SessionId,
    ) -> Result<Option<crate::types::provenance::SessionProvenance>, StoreError>;

    /// Upsert a run row.
    async fn upsert_run(&self, row: &RunRow) -> Result<(), StoreError>;
    /// Load a run row.
    async fn get_run(&self, id: RunId) -> Result<Option<RunRow>, StoreError>;
    /// List runs for a session (created_at ascending).
    async fn list_runs(&self, session: SessionId) -> Result<Vec<RunRow>, StoreError>;

    /// Write `sessions.graph_version` (RFC-0015 §5.6 amendment A3; discharges
    /// RFC-0011 Appendix E.4 item 2). The column exists since RFC-0002 and
    /// `upsert_session` writes `NULL`; `alloy index` is the only caller.
    ///
    /// Returns [`StoreError::Corrupt`] when the session row is missing so a
    /// typo'd id is not silently a no-op.
    async fn set_graph_version(
        &self,
        id: SessionId,
        version: GraphVersion,
    ) -> Result<(), StoreError>;
}

/// SQLite implementation of [`SessionRows`].
pub struct SqliteSessionRows {
    db: Arc<DbHandle>,
    metrics: Arc<StorageMetrics>,
    gate: Arc<StorageGate>,
}

impl SqliteSessionRows {
    pub(crate) fn new(
        db: Arc<DbHandle>,
        metrics: Arc<StorageMetrics>,
        gate: Arc<StorageGate>,
    ) -> Self {
        Self { db, metrics, gate }
    }

    fn map_busy(&self, err: StoreError) -> StoreError {
        if matches!(err, StoreError::Busy) {
            self.metrics.inc_busy_errors();
        }
        err
    }
}

#[async_trait]
impl SessionRows for SqliteSessionRows {
    async fn upsert_session(
        &self,
        session: &Session,
        provenance: &crate::types::provenance::SessionProvenance,
    ) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let session = session.clone();
        let provenance_json =
            serde_json::to_string(provenance).map_err(|e| StoreError::Internal(e.to_string()))?;
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let budget = serde_json::to_string(&session.budget)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let langs = serde_json::to_string(&session.language_backends)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_text = ts_to_text(&session.created_at)?;
                let root = path_to_utf8(&session.workspace_root)?;
                // One statement = atomic with creation; the conflict arm
                // deliberately omits provenance_json (write-once).
                conn.execute(
                    "INSERT INTO sessions (
                        id, workspace_root, profile, budget_json,
                        language_backends_json, created_at, graph_version,
                        provenance_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)
                     ON CONFLICT(id) DO UPDATE SET
                        workspace_root = excluded.workspace_root,
                        profile = excluded.profile,
                        budget_json = excluded.budget_json,
                        language_backends_json = excluded.language_backends_json",
                    params![
                        session.id.to_string(),
                        root,
                        session.profile.as_str(),
                        budget,
                        langs,
                        ts_text,
                        provenance_json,
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn get_provenance(
        &self,
        id: SessionId,
    ) -> Result<Option<crate::types::provenance::SessionProvenance>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let row: Option<Option<String>> = conn
                    .query_row(
                        "SELECT provenance_json FROM sessions WHERE id = ?1",
                        [&id_str],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(Some(json)) = row else {
                    return Ok(None);
                };
                serde_json::from_str(&json)
                    .map(Some)
                    .map_err(|e| StoreError::Corrupt(format!("provenance_json: {e}")))
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn get_session(&self, id: SessionId) -> Result<Option<Session>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let row = conn
                    .query_row(
                        "SELECT workspace_root, profile, budget_json,
                                language_backends_json, created_at
                         FROM sessions WHERE id = ?1",
                        [&id_str],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((root, profile, budget, langs, created_at)) = row else {
                    return Ok(None);
                };
                Ok(Some(Session {
                    id,
                    workspace_root: std::path::PathBuf::from(root),
                    profile: crate::types::ids::ProfileId::new(profile)
                        .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                    budget: serde_json::from_str(&budget)
                        .map_err(|e| StoreError::Corrupt(format!("budget: {e}")))?,
                    language_backends: serde_json::from_str(&langs)
                        .map_err(|e| StoreError::Corrupt(format!("language_backends: {e}")))?,
                    created_at: ts_from_text(&created_at)?,
                }))
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn upsert_run(&self, row: &RunRow) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let row = row.clone();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let goal = serde_json::to_string(&row.goal_json)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created = ts_to_text(&row.created_at)?;
                let updated = ts_to_text(&row.updated_at)?;
                conn.execute(
                    "INSERT INTO runs (
                        id, session_id, goal_json, state, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        session_id = excluded.session_id,
                        goal_json = excluded.goal_json,
                        state = excluded.state,
                        updated_at = excluded.updated_at",
                    params![
                        row.id.to_string(),
                        row.session_id.to_string(),
                        goal,
                        row.state,
                        created,
                        updated,
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn get_run(&self, id: RunId) -> Result<Option<RunRow>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let row = conn
                    .query_row(
                        "SELECT session_id, goal_json, state, created_at, updated_at
                         FROM runs WHERE id = ?1",
                        [&id_str],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((session_id, goal, state, created_at, updated_at)) = row else {
                    return Ok(None);
                };
                Ok(Some(RunRow {
                    id,
                    session_id: parse_session_id(&session_id)?,
                    goal_json: serde_json::from_str(&goal)
                        .map_err(|e| StoreError::Corrupt(format!("goal: {e}")))?,
                    state,
                    created_at: ts_from_text(&created_at)?,
                    updated_at: ts_from_text(&updated_at)?,
                }))
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn list_runs(&self, session: SessionId) -> Result<Vec<RunRow>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let sid = session.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, goal_json, state, created_at, updated_at FROM runs
                     WHERE session_id = ?1 ORDER BY created_at ASC, id ASC",
                )?;
                let rows = stmt.query_map([&sid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (id, goal, state, created_at, updated_at) = row?;
                    out.push(RunRow {
                        id: parse_run_id(&id)?,
                        session_id: session,
                        goal_json: serde_json::from_str(&goal)
                            .map_err(|e| StoreError::Corrupt(format!("goal: {e}")))?,
                        state,
                        created_at: ts_from_text(&created_at)?,
                        updated_at: ts_from_text(&updated_at)?,
                    });
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn set_graph_version(
        &self,
        id: SessionId,
        version: GraphVersion,
    ) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let version = i64::try_from(version.0).map_err(|_| {
                    StoreError::Internal(format!("graph_version {} out of range", version.0))
                })?;
                let updated = conn.execute(
                    "UPDATE sessions SET graph_version = ?2 WHERE id = ?1",
                    params![id_str, version],
                )?;
                if updated == 0 {
                    return Err(StoreError::Corrupt(format!(
                        "set_graph_version: session {id_str} not found"
                    )));
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }
}
