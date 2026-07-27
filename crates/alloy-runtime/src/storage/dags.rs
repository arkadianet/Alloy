//! Durable DAG blob store over the reserved `dag_blobs` table (RFC-0009).

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::codec::parse_session_id;
use super::error::StoreError;
use super::gate::StorageGate;
use super::metrics::StorageMetrics;
use super::open::{spawn_db, DbHandle};
use crate::dag::TaskDag;
use crate::scheduler::DagState;
use crate::types::ids::{DagId, SessionId, Timestamp};

/// Errors from atomic [`DagStore::replace_for_replan`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReplanReplaceError {
    /// No row for the DAG id.
    #[error("dag not found")]
    NotFound,
    /// Stored generation differs from expected.
    #[error("generation mismatch: actual {actual}")]
    GenerationMismatch {
        /// Actual stored generation.
        actual: u64,
    },
    /// DAG is busy (e.g. [`DagState::Running`]).
    #[error("dag busy in state {state:?}")]
    DagBusy {
        /// Observed DAG state.
        state: DagState,
    },
    /// Underlying store failure.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Durable DAG blob API over `dag_blobs`.
#[async_trait]
pub trait DagStore: Send + Sync {
    /// Unconditional insert-or-overwrite by `dag.id`.
    ///
    /// MUST NOT run `DagValidator`. Documented for tests/admin only.
    /// Production plan uses [`Self::put_if_generation`]; production replan uses
    /// [`Self::replace_for_replan`]; scheduler checkpoints use
    /// [`Self::put_if_generation`].
    async fn put(&self, dag: &TaskDag) -> Result<(), StoreError>;

    /// Compare-and-set write inside a single `spawn_db` closure.
    ///
    /// - `expected = None` — insert only; existing row → [`StoreError::Conflict`].
    /// - `expected = Some(g)` — update only if stored generation equals `g`;
    ///   missing row → [`StoreError::Conflict`] (not NotFound).
    async fn put_if_generation(
        &self,
        dag: &TaskDag,
        expected: Option<u64>,
    ) -> Result<(), StoreError>;

    /// Atomic replan replace: SELECT → checks → UPDATE in one `spawn_db` closure.
    async fn replace_for_replan(
        &self,
        dag: &TaskDag,
        expected_generation: u64,
    ) -> Result<(), ReplanReplaceError>;

    /// Load by primary key. Does not run `DagValidator`.
    async fn get(&self, dag_id: DagId) -> Result<Option<TaskDag>, StoreError>;

    /// Delete by primary key. Missing row → `Ok(())` (idempotent).
    async fn delete(&self, dag_id: DagId) -> Result<(), StoreError>;

    /// List dag ids for a session (`updated_at ASC, dag_id ASC`).
    async fn list_by_session(&self, session_id: SessionId) -> Result<Vec<DagId>, StoreError>;
}

/// SQLite-backed [`DagStore`].
pub struct SqliteDagStore {
    db: Arc<DbHandle>,
    metrics: Arc<StorageMetrics>,
    gate: Arc<StorageGate>,
}

impl SqliteDagStore {
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

fn reject_generation_bound(generation: u64) -> Result<(), StoreError> {
    if generation > i64::MAX as u64 {
        return Err(StoreError::Internal(format!(
            "generation {generation} exceeds i64::MAX"
        )));
    }
    Ok(())
}

fn updated_at_rfc3339(ts: &Timestamp) -> Result<String, StoreError> {
    ts.0.format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| StoreError::Internal(format!("rfc3339: {e}")))
}

fn encode_blob(dag: &TaskDag) -> Result<String, StoreError> {
    serde_json::to_string(dag).map_err(|e| StoreError::Internal(e.to_string()))
}

fn decode_row(
    dag_id_col: &str,
    session_id_col: &str,
    generation_col: i64,
    blob_json: &str,
) -> Result<TaskDag, StoreError> {
    if generation_col < 0 {
        return Err(StoreError::Corrupt(format!(
            "negative generation column: {generation_col}"
        )));
    }
    let generation = generation_col as u64;
    let dag: TaskDag = serde_json::from_str(blob_json)
        .map_err(|e| StoreError::Corrupt(format!("dag blob json: {e}")))?;
    let id = DagId::parse(dag_id_col).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let session_id = parse_session_id(session_id_col)?;
    if dag.id != id {
        return Err(StoreError::Corrupt(format!(
            "dag_id column {} != blob {}",
            id, dag.id
        )));
    }
    if dag.session_id != session_id {
        return Err(StoreError::Corrupt(format!(
            "session_id column {} != blob {}",
            session_id, dag.session_id
        )));
    }
    if dag.generation != generation {
        return Err(StoreError::Corrupt(format!(
            "generation column {generation} != blob {}",
            dag.generation
        )));
    }
    Ok(dag)
}

fn upsert_row(
    conn: &rusqlite::Connection,
    dag: &TaskDag,
    overwrite: bool,
) -> Result<(), StoreError> {
    reject_generation_bound(dag.generation)?;
    let blob = encode_blob(dag)?;
    let updated = updated_at_rfc3339(&Timestamp::now())?;
    let dag_id = dag.id.to_string();
    let session_id = dag.session_id.to_string();
    let generation = dag.generation as i64;

    if overwrite {
        // Reject rewriting an existing row to a different session_id.
        let existing: Option<String> = conn
            .query_row(
                "SELECT session_id FROM dag_blobs WHERE dag_id = ?1",
                [&dag_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prev) = existing {
            if prev != session_id {
                return Err(StoreError::Internal(format!(
                    "refusing session_id rewrite: {prev} -> {session_id}"
                )));
            }
            conn.execute(
                "UPDATE dag_blobs SET session_id = ?2, generation = ?3, blob_json = ?4, updated_at = ?5
                 WHERE dag_id = ?1",
                params![dag_id, session_id, generation, blob, updated],
            )?;
        } else {
            conn.execute(
                "INSERT INTO dag_blobs (dag_id, session_id, generation, blob_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![dag_id, session_id, generation, blob, updated],
            )?;
        }
        return Ok(());
    }

    // Unreachable for put path — kept for clarity.
    Ok(())
}

#[async_trait]
impl DagStore for SqliteDagStore {
    #[tracing::instrument(skip(self, dag), fields(dag_id = %dag.id, generation = dag.generation), name = "dag.store_put", level = "debug")]
    async fn put(&self, dag: &TaskDag) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        reject_generation_bound(dag.generation)?;
        let db = Arc::clone(&self.db);
        let dag = dag.clone();
        spawn_db(db, move |handle| {
            handle.with(|conn| upsert_row(conn, &dag, true))
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    #[tracing::instrument(
        skip(self, dag),
        fields(dag_id = %dag.id, expected = ?expected, generation = dag.generation),
        name = "dag.store_put_cas",
        level = "debug"
    )]
    async fn put_if_generation(
        &self,
        dag: &TaskDag,
        expected: Option<u64>,
    ) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        reject_generation_bound(dag.generation)?;
        if let Some(g) = expected {
            reject_generation_bound(g)?;
            if dag.generation < g {
                return Err(StoreError::Internal(format!(
                    "non-monotonic put_if_generation: dag.generation {} < expected {g}",
                    dag.generation
                )));
            }
        }
        let db = Arc::clone(&self.db);
        let dag = dag.clone();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let dag_id = dag.id.to_string();
                let session_id = dag.session_id.to_string();
                let row: Option<(String, i64)> = conn
                    .query_row(
                        "SELECT session_id, generation FROM dag_blobs WHERE dag_id = ?1",
                        [&dag_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                match (expected, row) {
                    (None, Some(_)) => {
                        return Err(StoreError::Conflict(format!(
                            "dag {dag_id} already exists"
                        )));
                    }
                    (Some(_), None) => {
                        return Err(StoreError::Conflict(format!(
                            "dag {dag_id} missing for expected generation"
                        )));
                    }
                    (Some(g), Some((stored_session, stored_gen))) => {
                        if stored_gen < 0 {
                            return Err(StoreError::Corrupt(format!(
                                "negative generation: {stored_gen}"
                            )));
                        }
                        if stored_gen as u64 != g {
                            return Err(StoreError::Conflict(format!(
                                "generation mismatch: expected {g}, actual {stored_gen}"
                            )));
                        }
                        if stored_session != session_id {
                            return Err(StoreError::Internal(format!(
                                "refusing session_id rewrite: {stored_session} -> {session_id}"
                            )));
                        }
                        let blob = encode_blob(&dag)?;
                        let updated = updated_at_rfc3339(&Timestamp::now())?;
                        conn.execute(
                            "UPDATE dag_blobs SET generation = ?2, blob_json = ?3, updated_at = ?4
                             WHERE dag_id = ?1",
                            params![dag_id, dag.generation as i64, blob, updated],
                        )?;
                    }
                    (None, None) => {
                        let blob = encode_blob(&dag)?;
                        let updated = updated_at_rfc3339(&Timestamp::now())?;
                        conn.execute(
                            "INSERT INTO dag_blobs (dag_id, session_id, generation, blob_json, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                dag_id,
                                session_id,
                                dag.generation as i64,
                                blob,
                                updated
                            ],
                        )?;
                    }
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn replace_for_replan(
        &self,
        dag: &TaskDag,
        expected_generation: u64,
    ) -> Result<(), ReplanReplaceError> {
        let _permit = self.gate.enter().map_err(ReplanReplaceError::Store)?;
        if expected_generation > i64::MAX as u64 || dag.generation > i64::MAX as u64 {
            return Err(ReplanReplaceError::Store(StoreError::Internal(
                "generation exceeds i64::MAX".into(),
            )));
        }
        let db = Arc::clone(&self.db);
        let dag = dag.clone();
        let result = spawn_db(db, move |handle| {
            handle.with(|conn| {
                let dag_id = dag.id.to_string();
                let row: Option<(String, i64, String)> = conn
                    .query_row(
                        "SELECT session_id, generation, blob_json FROM dag_blobs WHERE dag_id = ?1",
                        [&dag_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(StoreError::from)?;

                let Some((session_col, gen_col, blob_json)) = row else {
                    return Ok(Err(ReplanReplaceError::NotFound));
                };

                let stored = match decode_row(&dag_id, &session_col, gen_col, &blob_json) {
                    Ok(d) => d,
                    Err(e) => return Ok(Err(ReplanReplaceError::Store(e))),
                };

                if stored.generation != expected_generation {
                    return Ok(Err(ReplanReplaceError::GenerationMismatch {
                        actual: stored.generation,
                    }));
                }
                if stored.state == DagState::Running {
                    return Ok(Err(ReplanReplaceError::DagBusy {
                        state: DagState::Running,
                    }));
                }
                if dag.generation != expected_generation + 1 {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::Internal(
                        format!(
                            "replan generation must be expected+1: got {}, expected {}",
                            dag.generation,
                            expected_generation + 1
                        ),
                    ))));
                }
                if dag.session_id != stored.session_id {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::Internal(
                        format!(
                            "session_id mismatch on replan: {} != {}",
                            dag.session_id, stored.session_id
                        ),
                    ))));
                }

                let blob = match encode_blob(&dag) {
                    Ok(b) => b,
                    Err(e) => return Ok(Err(ReplanReplaceError::Store(e))),
                };
                let updated = match updated_at_rfc3339(&Timestamp::now()) {
                    Ok(u) => u,
                    Err(e) => return Ok(Err(ReplanReplaceError::Store(e))),
                };
                if let Err(e) = conn.execute(
                    "UPDATE dag_blobs SET generation = ?2, blob_json = ?3, updated_at = ?4
                     WHERE dag_id = ?1",
                    params![dag_id, dag.generation as i64, blob, updated],
                ) {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::from(e))));
                }
                Ok(Ok(()))
            })
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(StoreError::Busy) => {
                self.metrics.inc_busy_errors();
                Err(ReplanReplaceError::Store(StoreError::Busy))
            }
            Err(e) => Err(ReplanReplaceError::Store(e)),
        }
    }

    async fn get(&self, dag_id: DagId) -> Result<Option<TaskDag>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = dag_id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let row: Option<(String, i64, String)> = conn
                    .query_row(
                        "SELECT session_id, generation, blob_json FROM dag_blobs WHERE dag_id = ?1",
                        [&id_str],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                match row {
                    None => Ok(None),
                    Some((session_id, generation, blob)) => {
                        Ok(Some(decode_row(&id_str, &session_id, generation, &blob)?))
                    }
                }
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn delete(&self, dag_id: DagId) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = dag_id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                conn.execute("DELETE FROM dag_blobs WHERE dag_id = ?1", [&id_str])?;
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn list_by_session(&self, session_id: SessionId) -> Result<Vec<DagId>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let sid = session_id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT dag_id FROM dag_blobs WHERE session_id = ?1
                     ORDER BY updated_at ASC, dag_id ASC",
                )?;
                let rows = stmt.query_map([&sid], |r| r.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    let s = row?;
                    out.push(DagId::parse(&s).map_err(|e| StoreError::Corrupt(e.to_string()))?);
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{
        ApprovalSpec, Backoff, EdgeKind, NodeKind, NodeState, RetryPolicy, TaskNode,
    };
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::diagnostic::ErrorClass;
    use crate::types::ids::{ArtifactId, CapabilityId, GateId, NodeId};
    use std::collections::BTreeMap;

    fn sample_dag(session: SessionId, generation: u64) -> TaskDag {
        let node = NodeId::new();
        let gate = GateId::new();
        let mut nodes = BTreeMap::new();
        nodes.insert(
            node,
            TaskNode {
                id: node,
                kind: NodeKind::GateHuman,
                capability: None,
                input_ref: ArtifactId::new(),
                output_ref: None,
                state: NodeState::Pending,
                retry: RetryPolicy {
                    max_attempts: 1,
                    backoff: Backoff::Fixed { delay_ms: 0 },
                    retry_on: vec![],
                    escalate_after: None,
                    escalate_to_tier: None,
                },
                cache_key: None,
                budget: TokenBudget {
                    max_input: 0,
                    max_output: 0,
                },
                model_tier: ModelTier::Economy,
                approval: Some(ApprovalSpec {
                    gate,
                    reason: "ok".into(),
                }),
                timeout_ms: 1000,
            },
        );
        // Minimal single-node dag for store tests (validator not required).
        let _ = (CapabilityId::new("repair"), EdgeKind::Data, ErrorClass::Model);
        TaskDag {
            id: DagId::new(),
            session_id: session,
            generation,
            nodes,
            edges: vec![],
            state: DagState::Pending,
        }
    }

    async fn open_store() -> (tempfile::TempDir, AlloyStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        (dir, storage)
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let dag = sample_dag(session, 1);
        dags.put(&dag).await.unwrap();
        let got = dags.get(dag.id).await.unwrap().unwrap();
        assert_eq!(got, dag);
    }

    #[tokio::test]
    async fn put_if_generation_insert_and_conflict() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let dag = sample_dag(session, 1);
        dags.put_if_generation(&dag, None).await.unwrap();
        let err = dags.put_if_generation(&dag, None).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
    }

    #[tokio::test]
    async fn put_if_generation_update_and_mismatch() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let mut dag = sample_dag(session, 1);
        dags.put_if_generation(&dag, None).await.unwrap();
        dag.state = DagState::Failed;
        dags.put_if_generation(&dag, Some(1)).await.unwrap();
        let got = dags.get(dag.id).await.unwrap().unwrap();
        assert_eq!(got.state, DagState::Failed);
        // Stale expected generation → Conflict
        let err = dags.put_if_generation(&dag, Some(0)).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
    }

    #[tokio::test]
    async fn replace_for_replan_bumps_and_rejects_running() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let mut dag = sample_dag(session, 1);
        dags.put_if_generation(&dag, None).await.unwrap();

        let mut next = dag.clone();
        next.generation = 2;
        next.state = DagState::Pending;
        dags.replace_for_replan(&next, 1).await.unwrap();
        assert_eq!(dags.get(dag.id).await.unwrap().unwrap().generation, 2);

        dag.generation = 2;
        dag.state = DagState::Running;
        dags.put(&dag).await.unwrap();
        next.generation = 3;
        let err = dags.replace_for_replan(&next, 2).await.unwrap_err();
        assert!(matches!(
            err,
            ReplanReplaceError::DagBusy {
                state: DagState::Running
            }
        ));
    }

    #[tokio::test]
    async fn corrupt_blob_and_generation_mismatch() {
        let (dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let dag = sample_dag(session, 1);
        dags.put(&dag).await.unwrap();
        storage.close().await.unwrap();

        // Corrupt via raw SQL on the closed file, then reopen.
        {
            let conn = rusqlite::Connection::open(dir.path().join("alloy.sqlite")).unwrap();
            conn.execute(
                "UPDATE dag_blobs SET blob_json = ?2 WHERE dag_id = ?1",
                params![dag.id.to_string(), "{not-json"],
            )
            .unwrap();
        }
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let err = storage.dags().get(dag.id).await.unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)));
    }

    #[tokio::test]
    async fn closed_after_close() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let dag = sample_dag(session, 1);
        storage.close().await.unwrap();
        let err = dags.put(&dag).await.unwrap_err();
        assert!(matches!(err, StoreError::Closed));
    }

    #[tokio::test]
    async fn reject_generation_above_i64_max() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dag.generation = (i64::MAX as u64) + 1;
        let err = dags.put(&dag).await.unwrap_err();
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[tokio::test]
    async fn reject_session_id_rewrite() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dags.put(&dag).await.unwrap();
        dag.session_id = SessionId::new();
        let err = dags.put(&dag).await.unwrap_err();
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[tokio::test]
    async fn delete_idempotent_and_list() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let dag = sample_dag(session, 1);
        dags.put(&dag).await.unwrap();
        assert_eq!(dags.list_by_session(session).await.unwrap(), vec![dag.id]);
        dags.delete(dag.id).await.unwrap();
        dags.delete(dag.id).await.unwrap();
        assert!(dags.get(dag.id).await.unwrap().is_none());
    }
}
