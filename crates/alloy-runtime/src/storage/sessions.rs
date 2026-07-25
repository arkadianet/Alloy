//! Thin session/run row persistence (orchestration in RFC-0003).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::error::StoreError;
use super::open::{spawn_db, DbHandle};
use crate::session::Session;
use crate::types::ids::{RunId, SessionId, Timestamp};

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
    /// Upsert a session row.
    async fn upsert_session(&self, session: &Session) -> Result<(), StoreError>;
    /// Load a session row.
    async fn get_session(&self, id: SessionId) -> Result<Option<Session>, StoreError>;

    /// Upsert a run row.
    async fn upsert_run(&self, row: &RunRow) -> Result<(), StoreError>;
    /// Load a run row.
    async fn get_run(&self, id: RunId) -> Result<Option<RunRow>, StoreError>;
    /// List runs for a session (created_at ascending).
    async fn list_runs(&self, session: SessionId) -> Result<Vec<RunRow>, StoreError>;
}

/// SQLite implementation of [`SessionRows`].
pub struct SqliteSessionRows {
    db: Arc<DbHandle>,
    closed: Arc<AtomicBool>,
}

impl SqliteSessionRows {
    pub(crate) fn new(db: Arc<DbHandle>, closed: Arc<AtomicBool>) -> Self {
        Self { db, closed }
    }

    fn ensure_open(&self) -> Result<(), StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(StoreError::Closed)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl SessionRows for SqliteSessionRows {
    async fn upsert_session(&self, session: &Session) -> Result<(), StoreError> {
        self.ensure_open()?;
        let db = Arc::clone(&self.db);
        let session = session.clone();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let budget = serde_json::to_string(&session.budget)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let langs = serde_json::to_string(&session.language_backends)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_s = serde_json::to_string(&session.created_at)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_text = ts_s.trim_matches('"').to_owned();
                conn.execute(
                    "INSERT INTO sessions (
                        id, workspace_root, profile, budget_json,
                        language_backends_json, created_at, graph_version
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)
                     ON CONFLICT(id) DO UPDATE SET
                        workspace_root = excluded.workspace_root,
                        profile = excluded.profile,
                        budget_json = excluded.budget_json,
                        language_backends_json = excluded.language_backends_json,
                        created_at = excluded.created_at",
                    params![
                        session.id.to_string(),
                        session.workspace_root.to_string_lossy(),
                        session.profile.as_str(),
                        budget,
                        langs,
                        ts_text,
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    async fn get_session(&self, id: SessionId) -> Result<Option<Session>, StoreError> {
        self.ensure_open()?;
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
                    created_at: serde_json::from_str(&format!("\"{created_at}\""))
                        .map_err(|e| StoreError::Corrupt(format!("created_at: {e}")))?,
                }))
            })
        })
        .await
    }

    async fn upsert_run(&self, row: &RunRow) -> Result<(), StoreError> {
        self.ensure_open()?;
        let db = Arc::clone(&self.db);
        let row = row.clone();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let goal = serde_json::to_string(&row.goal_json)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created = serde_json::to_string(&row.created_at)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let updated = serde_json::to_string(&row.updated_at)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                conn.execute(
                    "INSERT INTO runs (
                        id, session_id, goal_json, state, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        session_id = excluded.session_id,
                        goal_json = excluded.goal_json,
                        state = excluded.state,
                        created_at = excluded.created_at,
                        updated_at = excluded.updated_at",
                    params![
                        row.id.to_string(),
                        row.session_id.to_string(),
                        goal,
                        row.state,
                        created.trim_matches('"'),
                        updated.trim_matches('"'),
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    async fn get_run(&self, id: RunId) -> Result<Option<RunRow>, StoreError> {
        self.ensure_open()?;
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
                    session_id: parse_session_id(&session_id).map_err(StoreError::Corrupt)?,
                    goal_json: serde_json::from_str(&goal)
                        .map_err(|e| StoreError::Corrupt(format!("goal: {e}")))?,
                    state,
                    created_at: serde_json::from_str(&format!("\"{created_at}\""))
                        .map_err(|e| StoreError::Corrupt(format!("created_at: {e}")))?,
                    updated_at: serde_json::from_str(&format!("\"{updated_at}\""))
                        .map_err(|e| StoreError::Corrupt(format!("updated_at: {e}")))?,
                }))
            })
        })
        .await
    }

    async fn list_runs(&self, session: SessionId) -> Result<Vec<RunRow>, StoreError> {
        self.ensure_open()?;
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
                        id: parse_run_id(&id).map_err(StoreError::Corrupt)?,
                        session_id: session,
                        goal_json: serde_json::from_str(&goal)
                            .map_err(|e| StoreError::Corrupt(format!("goal: {e}")))?,
                        state,
                        created_at: serde_json::from_str(&format!("\"{created_at}\""))
                            .map_err(|e| StoreError::Corrupt(format!("created_at: {e}")))?,
                        updated_at: serde_json::from_str(&format!("\"{updated_at}\""))
                            .map_err(|e| StoreError::Corrupt(format!("updated_at: {e}")))?,
                    });
                }
                Ok(out)
            })
        })
        .await
    }
}

fn parse_session_id(s: &str) -> Result<SessionId, String> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| e.to_string())?;
    serde_json::from_value(serde_json::json!(uuid)).map_err(|e| e.to_string())
}

fn parse_run_id(s: &str) -> Result<RunId, String> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| e.to_string())?;
    serde_json::from_value(serde_json::json!(uuid)).map_err(|e| e.to_string())
}
