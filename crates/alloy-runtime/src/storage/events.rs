//! SQLite-backed [`EventSink`] + [`EventStore`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::error::StoreError;
use super::metrics::StorageMetrics;
use super::open::{spawn_db, DbHandle};
use crate::events::{
    EventSink, EventSinkError, HandoffSnapshot, NewSessionEvent, RuntimeEvent, SessionEvent,
    SessionEventType,
};
use crate::session::clamp_events_page_limit;
use crate::types::ids::{EventSeq, RunId, SessionId, Timestamp};

/// Read/replay APIs on top of [`EventSink`].
#[async_trait]
pub trait EventStore: EventSink {
    /// Exclusive cursor page — same semantics as `SessionService::events`.
    ///
    /// `after: None` → from `EventSeq(0)`; `after: Some(s)` → `seq > s`.
    /// Impls MUST clamp via [`clamp_events_page_limit`].
    async fn list_session_events(
        &self,
        session: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Replay all events for a session in seq order (internal pages).
    ///
    /// Returns `None` if the session has no events; otherwise `Some(last_seq)`.
    /// Callback `Err` aborts replay and propagates. Empty session: zero callbacks.
    async fn replay_session<F>(
        &self,
        session: SessionId,
        on_event: F,
    ) -> Result<Option<EventSeq>, StoreError>
    where
        F: FnMut(&SessionEvent) -> Result<(), StoreError> + Send;

    /// Highest assigned seq for session, or `None` if no events.
    async fn last_seq(&self, session: SessionId) -> Result<Option<EventSeq>, StoreError>;

    /// List runtime (host) events in append order (for recovery/tests).
    async fn list_runtime_events(
        &self,
        after_rowid: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, RuntimeEvent)>, StoreError>;

    /// Import a handoff snapshot with **exact** `seq` / `ts` (no re-allocation).
    ///
    /// Single DB transaction including post-import seq verification.
    async fn import_handoff_snapshot(&self, snap: HandoffSnapshot) -> Result<(), StoreError>;
}

/// SQLite-backed sink + store.
pub struct SqliteEventStore {
    db: Arc<DbHandle>,
    metrics: Arc<StorageMetrics>,
    closed: Arc<AtomicBool>,
}

impl SqliteEventStore {
    pub(crate) fn new(
        db: Arc<DbHandle>,
        metrics: Arc<StorageMetrics>,
        closed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            db,
            metrics,
            closed,
        }
    }

    fn ensure_open(&self) -> Result<(), StoreError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(StoreError::Closed)
        } else {
            Ok(())
        }
    }

    fn map_busy(&self, err: StoreError) -> StoreError {
        if matches!(err, StoreError::Busy) {
            self.metrics.inc_busy_errors();
        }
        err
    }
}

#[async_trait]
impl EventSink for SqliteEventStore {
    async fn append_runtime(&self, ev: RuntimeEvent) -> Result<(), EventSinkError> {
        self.ensure_open().map_err(EventSinkError::from)?;
        let db = Arc::clone(&self.db);
        let metrics = Arc::clone(&self.metrics);
        let result = spawn_db(db, move |handle| {
            handle.with(|conn| {
                let ts = Timestamp::now();
                let json = serde_json::to_string(&ev)
                    .map_err(|e| StoreError::Internal(format!("serialize runtime event: {e}")))?;
                let ts_s =
                    serde_json::to_string(&ts).map_err(|e| StoreError::Internal(e.to_string()))?;
                // Timestamp serializes as a JSON string; strip quotes for TEXT column.
                let ts_text = ts_s.trim_matches('"').to_owned();
                conn.execute(
                    "INSERT INTO runtime_events (ts, event_json) VALUES (?1, ?2)",
                    params![ts_text, json],
                )?;
                Ok(())
            })
        })
        .await;
        match result {
            Ok(()) => {
                metrics.inc_runtime_events_appended();
                Ok(())
            }
            Err(e) => Err(EventSinkError::from(self.map_busy(e))),
        }
    }

    #[tracing::instrument(skip(self, ev), fields(session_id = %ev.session_id, type = ?ev.type_), name = "storage.append_session", level = "debug")]
    async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, EventSinkError> {
        self.ensure_open().map_err(EventSinkError::from)?;
        let db = Arc::clone(&self.db);
        let metrics = Arc::clone(&self.metrics);
        let result = spawn_db(db, move |handle| {
            handle.with_mut(|conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;

                let session_id = ev.session_id.to_string();
                let next: i64 = tx
                    .query_row(
                        "SELECT next_seq FROM session_seq WHERE session_id = ?1",
                        [&session_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);

                let seq = EventSeq(next as u64);
                let ts = Timestamp::now();
                let type_json = serde_json::to_string(&ev.type_)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                // SessionEventType serializes as a JSON string.
                let type_text = type_json.trim_matches('"').to_owned();
                let payload = serde_json::to_string(&ev.payload)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_s =
                    serde_json::to_string(&ts).map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_text = ts_s.trim_matches('"').to_owned();
                let run_id = ev.run_id.map(|r| r.to_string());

                tx.execute(
                    "INSERT INTO session_events (session_id, seq, ts, run_id, type, payload_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session_id, next, ts_text, run_id, type_text, payload],
                )?;

                tx.execute(
                    "INSERT INTO session_seq (session_id, next_seq) VALUES (?1, ?2)
                     ON CONFLICT(session_id) DO UPDATE SET next_seq = excluded.next_seq",
                    params![ev.session_id.to_string(), next + 1],
                )?;

                tx.commit()?;
                tracing::Span::current().record("seq", seq.0);
                Ok(seq)
            })
        })
        .await;

        match result {
            Ok(seq) => {
                metrics.inc_events_appended();
                Ok(seq)
            }
            Err(e) => Err(EventSinkError::from(self.map_busy(e))),
        }
    }
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn list_session_events(
        &self,
        session: SessionId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.ensure_open()?;
        let limit = clamp_events_page_limit(limit);
        let db = Arc::clone(&self.db);
        let metrics = Arc::clone(&self.metrics);
        let after_seq = after.map(|s| s.0 as i64);
        let events = spawn_db(db, move |handle| {
            handle.with(|conn| list_session_events_sync(conn, session, after_seq, limit))
        })
        .await
        .map_err(|e| self.map_busy(e))?;
        metrics.add_events_read(events.len() as u64);
        Ok(events)
    }

    async fn replay_session<F>(
        &self,
        session: SessionId,
        mut on_event: F,
    ) -> Result<Option<EventSeq>, StoreError>
    where
        F: FnMut(&SessionEvent) -> Result<(), StoreError> + Send,
    {
        self.ensure_open()?;
        let mut cursor: Option<EventSeq> = None;
        let mut last: Option<EventSeq> = None;
        loop {
            let page = self
                .list_session_events(session, cursor, crate::session::MAX_EVENTS_PAGE)
                .await?;
            if page.is_empty() {
                break;
            }
            for ev in &page {
                on_event(ev)?;
                last = Some(ev.seq);
                cursor = Some(ev.seq);
            }
            if page.len() < crate::session::MAX_EVENTS_PAGE {
                break;
            }
        }
        Ok(last)
    }

    async fn last_seq(&self, session: SessionId) -> Result<Option<EventSeq>, StoreError> {
        self.ensure_open()?;
        let db = Arc::clone(&self.db);
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let max: Option<i64> = conn.query_row(
                    "SELECT MAX(seq) FROM session_events WHERE session_id = ?1",
                    [session.to_string()],
                    |r| r.get(0),
                )?;
                Ok(max.map(|s| EventSeq(s as u64)))
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn list_runtime_events(
        &self,
        after_rowid: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, RuntimeEvent)>, StoreError> {
        self.ensure_open()?;
        let limit = clamp_events_page_limit(limit);
        let db = Arc::clone(&self.db);
        let after = after_rowid.unwrap_or(-1);
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, event_json FROM runtime_events
                     WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![after, limit as i64], |row| {
                    let id: i64 = row.get(0)?;
                    let json: String = row.get(1)?;
                    Ok((id, json))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (id, json) = row?;
                    let ev: RuntimeEvent = serde_json::from_str(&json)
                        .map_err(|e| StoreError::Corrupt(format!("runtime_events id={id}: {e}")))?;
                    out.push((id, ev));
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn import_handoff_snapshot(&self, snap: HandoffSnapshot) -> Result<(), StoreError> {
        self.ensure_open()?;
        let db = Arc::clone(&self.db);
        spawn_db(db, move |handle| {
            handle.with_mut(|conn| import_handoff_snapshot_sync(conn, snap))
        })
        .await
        .map_err(|e| self.map_busy(e))
    }
}

fn list_session_events_sync(
    conn: &rusqlite::Connection,
    session: SessionId,
    after_seq: Option<i64>,
    limit: usize,
) -> Result<Vec<SessionEvent>, StoreError> {
    let sid = session.to_string();
    let (sql, after_val): (&str, i64) = match after_seq {
        None => (
            "SELECT seq, ts, run_id, type, payload_json FROM session_events
             WHERE session_id = ?1 AND seq >= 0 ORDER BY seq ASC LIMIT ?2",
            -1,
        ),
        Some(s) => (
            "SELECT seq, ts, run_id, type, payload_json FROM session_events
             WHERE session_id = ?1 AND seq > ?3 ORDER BY seq ASC LIMIT ?2",
            s,
        ),
    };

    let mut stmt = conn.prepare(sql)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, String, Option<String>, String, String)> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    };

    let rows = if after_seq.is_none() {
        stmt.query_map(params![sid, limit as i64], map_row)?
    } else {
        stmt.query_map(params![sid, limit as i64, after_val], map_row)?
    };

    let mut out = Vec::new();
    for row in rows {
        let (seq, ts_text, run_id, type_text, payload) = row?;
        let type_: SessionEventType = serde_json::from_str(&format!("\"{type_text}\""))
            .map_err(|e| StoreError::Corrupt(format!("event type: {e}")))?;
        let ts: Timestamp = serde_json::from_str(&format!("\"{ts_text}\""))
            .map_err(|e| StoreError::Corrupt(format!("event ts: {e}")))?;
        let payload: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| StoreError::Corrupt(format!("event payload: {e}")))?;
        let run_id = match run_id {
            None => None,
            Some(s) => {
                Some(parse_run_id(&s).map_err(|e| StoreError::Corrupt(format!("run_id: {e}")))?)
            }
        };
        out.push(SessionEvent {
            seq: EventSeq(seq as u64),
            ts,
            session_id: session,
            run_id,
            type_,
            payload,
        });
    }
    Ok(out)
}

fn import_handoff_snapshot_sync(
    conn: &mut rusqlite::Connection,
    snap: HandoffSnapshot,
) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(StoreError::from)?;

    for ev in &snap.runtime {
        let json = serde_json::to_string(ev)
            .map_err(|e| StoreError::Internal(format!("serialize runtime event: {e}")))?;
        let ts = Timestamp::now();
        let ts_s = serde_json::to_string(&ts).map_err(|e| StoreError::Internal(e.to_string()))?;
        let ts_text = ts_s.trim_matches('"').to_owned();
        tx.execute(
            "INSERT INTO runtime_events (ts, event_json) VALUES (?1, ?2)",
            params![ts_text, json],
        )?;
    }

    for (session_id, events) in &snap.sessions {
        let sid = session_id.to_string();
        for ev in events {
            if ev.session_id != *session_id {
                return Err(StoreError::Corrupt(
                    "handoff snapshot session_id mismatch".into(),
                ));
            }
            let type_json = serde_json::to_string(&ev.type_)
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let type_text = type_json.trim_matches('"').to_owned();
            let payload = serde_json::to_string(&ev.payload)
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let ts_s =
                serde_json::to_string(&ev.ts).map_err(|e| StoreError::Internal(e.to_string()))?;
            let ts_text = ts_s.trim_matches('"').to_owned();
            let run_id = ev.run_id.map(|r| r.to_string());
            tx.execute(
                "INSERT INTO session_events (session_id, seq, ts, run_id, type, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![sid, ev.seq.0 as i64, ts_text, run_id, type_text, payload],
            )?;
        }
    }

    for (session_id, next) in &snap.next_seq {
        tx.execute(
            "INSERT INTO session_seq (session_id, next_seq) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET next_seq = excluded.next_seq",
            params![session_id.to_string(), *next as i64],
        )?;
    }

    // Verify each session last_seq matches next_seq-1 when next_seq > 0.
    for (session_id, next) in &snap.next_seq {
        if *next == 0 {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM session_events WHERE session_id = ?1",
                [session_id.to_string()],
                |r| r.get(0),
            )?;
            if count != 0 {
                return Err(StoreError::Corrupt(format!(
                    "handoff verify: session {session_id} next_seq=0 but has events"
                )));
            }
            continue;
        }
        let max: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM session_events WHERE session_id = ?1",
            [session_id.to_string()],
            |r| r.get(0),
        )?;
        let expected = (*next as i64) - 1;
        match max {
            Some(m) if m == expected => {}
            other => {
                return Err(StoreError::Corrupt(format!(
                    "handoff verify: session {session_id} expected last_seq={expected}, got {other:?}"
                )));
            }
        }
    }

    // Also verify sessions that have events but were only in sessions map.
    for session_id in snap.sessions.keys() {
        if !snap.next_seq.contains_key(session_id) {
            return Err(StoreError::Corrupt(format!(
                "handoff snapshot missing next_seq for {session_id}"
            )));
        }
    }

    tx.commit()?;
    Ok(())
}

fn parse_run_id(s: &str) -> Result<RunId, String> {
    // RunId is a transparent UUID newtype; reconstruct via serde without exposing constructors.
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| e.to_string())?;
    serde_json::from_value(serde_json::json!(uuid)).map_err(|e| e.to_string())
}
