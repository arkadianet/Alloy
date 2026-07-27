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
///
/// Production callers MUST clear gate waiters via
/// [`crate::RunController::request_replan`] first, and the owning scheduler MUST
/// checkpoint a non-[`DagState::Running`] state (typically
/// [`DagState::ReplanRequired`]) at the same generation before replan can
/// succeed — otherwise [`Self::DagBusy`] is permanent (RFC-0009 Appendix C).
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
///
/// All production writes MUST use compare-and-set SQL inside a single
/// `BEGIN IMMEDIATE` transaction (RFC-0009 §6.6). Implementations MUST NOT run
/// [`crate::DagValidator`].
#[async_trait]
pub trait DagStore: Send + Sync {
    /// Unconditional insert-or-overwrite by `dag.id`.
    ///
    /// Tests/admin only. Production plan uses [`Self::put_if_generation`];
    /// production replan uses [`Self::replace_for_replan`]; scheduler
    /// checkpoints use [`Self::put_if_generation`].
    ///
    /// MUST reject `dag.generation > i64::MAX as u64` with [`StoreError::Internal`].
    /// MUST reject rewriting an existing row to a different `session_id` with
    /// [`StoreError::Internal`].
    #[doc(hidden)]
    async fn put(&self, dag: &TaskDag) -> Result<(), StoreError>;

    /// Compare-and-set write inside a single immediate SQLite transaction.
    ///
    /// - `expected = None` — insert only; existing row → [`StoreError::Conflict`].
    /// - `expected = Some(g)` — update only if the stored generation equals `g`;
    ///   missing row → [`StoreError::Conflict`] (not NotFound).
    ///
    /// **Monotonicity:** when `expected = Some(g)`, require
    /// `dag.generation >= g`; otherwise [`StoreError::Internal`]. Scheduler
    /// checkpoints use `dag.generation == g`; replan MUST use
    /// [`Self::replace_for_replan`].
    ///
    /// MUST reject `dag.generation > i64::MAX as u64` or
    /// `expected.is_some_and(|g| g > i64::MAX as u64)` with [`StoreError::Internal`].
    /// MUST reject rewriting an existing row to a different `session_id` with
    /// [`StoreError::Internal`].
    /// MUST NOT run `DagValidator`.
    async fn put_if_generation(
        &self,
        dag: &TaskDag,
        expected: Option<u64>,
    ) -> Result<(), StoreError>;

    /// Atomic replan replace inside a single immediate SQLite transaction:
    /// `SELECT` → checks → `UPDATE`.
    ///
    /// Check order (RFC-0009 §3.6):
    /// 1. Generation bound overflow → [`ReplanReplaceError::Store`] / [`StoreError::Internal`]
    /// 2. Missing row → [`ReplanReplaceError::NotFound`]
    /// 3. Column/blob integrity → [`ReplanReplaceError::Store`] / [`StoreError::Corrupt`]
    /// 4. Stored generation ≠ `expected_generation` → [`ReplanReplaceError::GenerationMismatch`]
    /// 5. Decoded `state == Running` → [`ReplanReplaceError::DagBusy`]
    /// 6. `dag.generation != expected_generation + 1` → [`StoreError::Internal`]
    /// 7. `dag.session_id` differs from stored → [`StoreError::Internal`]
    /// 8. Else write and `Ok(())`
    ///
    /// Callers: clear gate waiters via `RunController::request_replan` first;
    /// scheduler must leave a non-`Running` checkpoint or [`ReplanReplaceError::DagBusy`]
    /// persists.
    async fn replace_for_replan(
        &self,
        dag: &TaskDag,
        expected_generation: u64,
    ) -> Result<(), ReplanReplaceError>;

    /// Load by primary key.
    ///
    /// Decode/serde failure, negative generation, or mismatch between column
    /// `generation`/`dag_id`/`session_id` and blob fields →
    /// [`StoreError::Corrupt`]. Does **not** run `DagValidator`.
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

    fn map_replan_busy(&self, err: ReplanReplaceError) -> ReplanReplaceError {
        match err {
            ReplanReplaceError::Store(StoreError::Busy) => {
                self.metrics.inc_busy_errors();
                ReplanReplaceError::Store(StoreError::Busy)
            }
            other => other,
        }
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
    // Fixed-width fractional seconds so ORDER BY updated_at ASC is chronological.
    // Variable-precision RFC3339 (`…20.1Z` vs `…20.12Z`) is not lexicographic-safe.
    let utc = ts.0.to_offset(time::UtcOffset::UTC);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z",
        year = utc.year(),
        month = u8::from(utc.month()),
        day = utc.day(),
        hour = utc.hour(),
        minute = utc.minute(),
        second = utc.second(),
        nanos = utc.nanosecond(),
    ))
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

/// Prepared write payload moved into `spawn_blocking` (avoids cloning `TaskDag`).
struct PreparedDagWrite {
    dag_id: String,
    session_id: String,
    generation: u64,
    blob: String,
    updated_at: String,
    session_id_typed: SessionId,
}

fn prepare_write(dag: &TaskDag) -> Result<PreparedDagWrite, StoreError> {
    reject_generation_bound(dag.generation)?;
    Ok(PreparedDagWrite {
        dag_id: dag.id.to_string(),
        session_id: dag.session_id.to_string(),
        generation: dag.generation,
        blob: encode_blob(dag)?,
        updated_at: updated_at_rfc3339(&Timestamp::now())?,
        session_id_typed: dag.session_id,
    })
}

fn put_overwrite_tx(
    conn: &mut rusqlite::Connection,
    w: &PreparedDagWrite,
) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(StoreError::from)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT session_id FROM dag_blobs WHERE dag_id = ?1",
            [&w.dag_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(prev) = existing {
        if prev != w.session_id {
            return Err(StoreError::Internal(format!(
                "refusing session_id rewrite: {prev} -> {}",
                w.session_id
            )));
        }
        tx.execute(
            "UPDATE dag_blobs SET session_id = ?2, generation = ?3, blob_json = ?4, updated_at = ?5
             WHERE dag_id = ?1",
            params![
                w.dag_id,
                w.session_id,
                w.generation as i64,
                w.blob,
                w.updated_at
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO dag_blobs (dag_id, session_id, generation, blob_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                w.dag_id,
                w.session_id,
                w.generation as i64,
                w.blob,
                w.updated_at
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[async_trait]
impl DagStore for SqliteDagStore {
    #[tracing::instrument(
        skip(self, dag),
        fields(dag_id = %dag.id, generation = dag.generation),
        name = "dag.store_put",
        level = "debug"
    )]
    async fn put(&self, dag: &TaskDag) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let prepared = prepare_write(dag)?;
        let db = Arc::clone(&self.db);
        spawn_db(db, move |handle| {
            handle.with_mut(|conn| put_overwrite_tx(conn, &prepared))
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
        if let Some(g) = expected {
            reject_generation_bound(g)?;
            if dag.generation < g {
                return Err(StoreError::Internal(format!(
                    "non-monotonic put_if_generation: dag.generation {} < expected {g}",
                    dag.generation
                )));
            }
        }
        let prepared = prepare_write(dag)?;
        let db = Arc::clone(&self.db);
        spawn_db(db, move |handle| {
            handle.with_mut(|conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;
                let row: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT session_id, generation FROM dag_blobs WHERE dag_id = ?1",
                        [&prepared.dag_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                match (expected, row) {
                    (None, Some(_)) => {
                        return Err(StoreError::Conflict(format!(
                            "dag {} already exists",
                            prepared.dag_id
                        )));
                    }
                    (Some(_), None) => {
                        return Err(StoreError::Conflict(format!(
                            "dag {} missing for expected generation",
                            prepared.dag_id
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
                        if stored_session != prepared.session_id {
                            return Err(StoreError::Internal(format!(
                                "refusing session_id rewrite: {stored_session} -> {}",
                                prepared.session_id
                            )));
                        }
                        let n = tx.execute(
                            "UPDATE dag_blobs SET generation = ?2, blob_json = ?3, updated_at = ?4
                             WHERE dag_id = ?1 AND generation = ?5",
                            params![
                                prepared.dag_id,
                                prepared.generation as i64,
                                prepared.blob,
                                prepared.updated_at,
                                g as i64
                            ],
                        )?;
                        if n != 1 {
                            return Err(StoreError::Conflict(format!(
                                "cas lost race for dag {}",
                                prepared.dag_id
                            )));
                        }
                    }
                    (None, None) => {
                        tx.execute(
                            "INSERT INTO dag_blobs (dag_id, session_id, generation, blob_json, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                prepared.dag_id,
                                prepared.session_id,
                                prepared.generation as i64,
                                prepared.blob,
                                prepared.updated_at
                            ],
                        )?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    #[tracing::instrument(
        skip(self, dag),
        fields(
            dag_id = %dag.id,
            expected_generation,
            generation = dag.generation
        ),
        name = "dag.store_replace_replan",
        level = "debug"
    )]
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
        let prepared = prepare_write(dag).map_err(ReplanReplaceError::Store)?;
        let incoming_session = prepared.session_id_typed;
        let incoming_generation = prepared.generation;
        let db = Arc::clone(&self.db);
        let result = spawn_db(db, move |handle| {
            handle.with_mut(|conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;

                let row: Option<(String, i64, String)> = tx
                    .query_row(
                        "SELECT session_id, generation, blob_json FROM dag_blobs WHERE dag_id = ?1",
                        [&prepared.dag_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(StoreError::from)?;

                let Some((session_col, gen_col, blob_json)) = row else {
                    return Ok(Err(ReplanReplaceError::NotFound));
                };

                let stored = match decode_row(&prepared.dag_id, &session_col, gen_col, &blob_json) {
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
                if incoming_generation != expected_generation + 1 {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::Internal(
                        format!(
                            "replan generation must be expected+1: got {incoming_generation}, expected {}",
                            expected_generation + 1
                        ),
                    ))));
                }
                if incoming_session != stored.session_id {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::Internal(
                        format!(
                            "session_id mismatch on replan: {incoming_session} != {}",
                            stored.session_id
                        ),
                    ))));
                }

                let n = match tx.execute(
                    "UPDATE dag_blobs SET generation = ?2, blob_json = ?3, updated_at = ?4
                     WHERE dag_id = ?1 AND generation = ?5",
                    params![
                        prepared.dag_id,
                        prepared.generation as i64,
                        prepared.blob,
                        prepared.updated_at,
                        expected_generation as i64
                    ],
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        return Ok(Err(ReplanReplaceError::Store(StoreError::from(e))));
                    }
                };
                if n != 1 {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::Internal(
                        "replan cas rowcount invariant violated".into(),
                    ))));
                }
                if let Err(e) = tx.commit() {
                    return Ok(Err(ReplanReplaceError::Store(StoreError::from(e))));
                }
                Ok(Ok(()))
            })
        })
        .await;

        match result {
            Ok(inner) => inner.map_err(|e| self.map_replan_busy(e)),
            Err(e) => Err(self.map_replan_busy(ReplanReplaceError::Store(e))),
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
    use crate::dag::{ApprovalSpec, Backoff, NodeKind, NodeState, RetryPolicy, TaskNode};
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::budget::{ModelTier, TokenBudget};
    use crate::types::ids::{ArtifactId, GateId, NodeId};
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
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn put_if_generation_insert_and_conflict() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        dags.put_if_generation(&dag, None).await.unwrap();
        let err = dags.put_if_generation(&dag, None).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn put_if_generation_update_and_mismatch() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dags.put_if_generation(&dag, None).await.unwrap();
        dag.state = DagState::Failed;
        dags.put_if_generation(&dag, Some(1)).await.unwrap();
        let got = dags.get(dag.id).await.unwrap().unwrap();
        assert_eq!(got.state, DagState::Failed);
        let err = dags.put_if_generation(&dag, Some(0)).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn put_if_generation_missing_row_is_conflict() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        let err = dags.put_if_generation(&dag, Some(1)).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn put_if_generation_non_monotonic_is_internal() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dags.put_if_generation(&dag, None).await.unwrap();
        dag.generation = 0;
        let err = dags.put_if_generation(&dag, Some(1)).await.unwrap_err();
        assert!(matches!(err, StoreError::Internal(_)));
        storage.close().await.unwrap();
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
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn replace_for_replan_not_found_and_mismatch() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 2);
        let err = dags.replace_for_replan(&dag, 1).await.unwrap_err();
        assert!(matches!(err, ReplanReplaceError::NotFound));

        let mut base = sample_dag(SessionId::new(), 1);
        dags.put_if_generation(&base, None).await.unwrap();
        base.generation = 2;
        let err = dags.replace_for_replan(&base, 99).await.unwrap_err();
        assert!(matches!(
            err,
            ReplanReplaceError::GenerationMismatch { actual: 1 }
        ));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_blob_json() {
        let (dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        dags.put(&dag).await.unwrap();
        storage.close().await.unwrap();

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
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_generation_column_mismatch() {
        let (dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        dags.put(&dag).await.unwrap();
        storage.close().await.unwrap();

        {
            let conn = rusqlite::Connection::open(dir.path().join("alloy.sqlite")).unwrap();
            conn.execute(
                "UPDATE dag_blobs SET generation = generation + 1 WHERE dag_id = ?1",
                params![dag.id.to_string()],
            )
            .unwrap();
        }
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let err = storage.dags().get(dag.id).await.unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_negative_generation_column() {
        let (dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        dags.put(&dag).await.unwrap();
        storage.close().await.unwrap();

        {
            let conn = rusqlite::Connection::open(dir.path().join("alloy.sqlite")).unwrap();
            conn.execute(
                "UPDATE dag_blobs SET generation = -1 WHERE dag_id = ?1",
                params![dag.id.to_string()],
            )
            .unwrap();
        }
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        let err = storage.dags().get(dag.id).await.unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn closed_after_close() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let dag = sample_dag(SessionId::new(), 1);
        storage.close().await.unwrap();
        assert!(matches!(
            dags.put(&dag).await.unwrap_err(),
            StoreError::Closed
        ));
        assert!(matches!(
            dags.get(dag.id).await.unwrap_err(),
            StoreError::Closed
        ));
        assert!(matches!(
            dags.put_if_generation(&dag, None).await.unwrap_err(),
            StoreError::Closed
        ));
        assert!(matches!(
            dags.replace_for_replan(&dag, 0).await.unwrap_err(),
            ReplanReplaceError::Store(StoreError::Closed)
        ));
        assert!(matches!(
            dags.delete(dag.id).await.unwrap_err(),
            StoreError::Closed
        ));
        assert!(matches!(
            dags.list_by_session(SessionId::new()).await.unwrap_err(),
            StoreError::Closed
        ));
    }

    #[tokio::test]
    async fn reject_generation_above_i64_max_all_writes() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dag.generation = (i64::MAX as u64) + 1;
        assert!(matches!(
            dags.put(&dag).await.unwrap_err(),
            StoreError::Internal(_)
        ));
        assert!(matches!(
            dags.put_if_generation(&dag, None).await.unwrap_err(),
            StoreError::Internal(_)
        ));
        assert!(matches!(
            dags.replace_for_replan(&dag, 1).await.unwrap_err(),
            ReplanReplaceError::Store(StoreError::Internal(_))
        ));

        let mut ok = sample_dag(SessionId::new(), 1);
        dags.put_if_generation(&ok, None).await.unwrap();
        ok.generation = 1;
        let err = dags
            .put_if_generation(&ok, Some((i64::MAX as u64) + 1))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Internal(_)));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn reject_session_id_rewrite_all_writes() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let mut dag = sample_dag(SessionId::new(), 1);
        dags.put(&dag).await.unwrap();
        dag.session_id = SessionId::new();
        assert!(matches!(
            dags.put(&dag).await.unwrap_err(),
            StoreError::Internal(_)
        ));
        assert!(matches!(
            dags.put_if_generation(&dag, Some(1)).await.unwrap_err(),
            StoreError::Internal(_)
        ));
        dag.generation = 2;
        assert!(matches!(
            dags.replace_for_replan(&dag, 1).await.unwrap_err(),
            ReplanReplaceError::Store(StoreError::Internal(_))
        ));
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn delete_idempotent_and_list_order() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let session = SessionId::new();
        let a = sample_dag(session, 1);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let b = sample_dag(session, 1);
        dags.put(&a).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        dags.put(&b).await.unwrap();
        let listed = dags.list_by_session(session).await.unwrap();
        assert_eq!(listed, vec![a.id, b.id]);
        dags.delete(a.id).await.unwrap();
        dags.delete(a.id).await.unwrap();
        assert!(dags.get(a.id).await.unwrap().is_none());
        storage.close().await.unwrap();
    }

    #[tokio::test]
    async fn busy_mapping_increments_busy_errors() {
        let (_dir, storage) = open_store().await;
        let dags = storage.dags();
        let before = storage.metrics().busy_errors;

        let err = dags.map_busy(StoreError::Busy);
        assert!(matches!(err, StoreError::Busy));
        assert_eq!(storage.metrics().busy_errors, before + 1);

        let err = dags.map_busy(StoreError::Closed);
        assert!(matches!(err, StoreError::Closed));
        assert_eq!(storage.metrics().busy_errors, before + 1);

        let err = dags.map_replan_busy(ReplanReplaceError::Store(StoreError::Busy));
        assert!(matches!(err, ReplanReplaceError::Store(StoreError::Busy)));
        assert_eq!(storage.metrics().busy_errors, before + 2);

        let err = dags.map_replan_busy(ReplanReplaceError::NotFound);
        assert!(matches!(err, ReplanReplaceError::NotFound));
        assert_eq!(storage.metrics().busy_errors, before + 2);

        storage.close().await.unwrap();
    }

    #[test]
    fn updated_at_fixed_width_sorts_chronologically() {
        // Variable-precision RFC3339 would put …20.12Z before …20.1Z lexicographically.
        let a = Timestamp(
            time::OffsetDateTime::from_unix_timestamp_nanos(1_720_000_000_100_000_000).unwrap(),
        );
        let b = Timestamp(
            time::OffsetDateTime::from_unix_timestamp_nanos(1_720_000_000_120_000_000).unwrap(),
        );
        let sa = updated_at_rfc3339(&a).unwrap();
        let sb = updated_at_rfc3339(&b).unwrap();
        assert!(sa.ends_with(".100000000Z"), "{sa}");
        assert!(sb.ends_with(".120000000Z"), "{sb}");
        assert!(sa < sb, "{sa} should sort before {sb}");
    }
}
