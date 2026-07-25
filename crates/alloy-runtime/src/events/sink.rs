//! [`EventSink`] trait and in-memory MVP implementation.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{NewSessionEvent, SessionEvent};
use crate::events::RuntimeEvent;
use crate::types::ids::{EventSeq, SessionId, Timestamp};

/// Buffered state drained from [`InMemoryEventSink`] for lossless SQLite handoff.
///
/// Exact `seq` / `ts` must be preserved by [`crate::storage::EventStore::import_handoff_snapshot`].
#[derive(Debug, Clone, Default)]
pub struct HandoffSnapshot {
    /// Buffered host runtime events (FIFO).
    pub runtime: Vec<RuntimeEvent>,
    /// Buffered session events keyed by session (seq order within each vec).
    pub sessions: HashMap<SessionId, Vec<SessionEvent>>,
    /// Per-session next seq after drain (same maps [`InMemoryEventSink`] used).
    pub next_seq: HashMap<SessionId, u64>,
}

/// Errors from an [`EventSink`].
#[derive(Debug, thiserror::Error)]
pub enum EventSinkError {
    /// I/O style failure.
    #[error("io: {0}")]
    Io(String),
    /// Sink is busy.
    #[error("busy")]
    Busy,
    /// Internal failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// Injectable event sink (SQLite arrives in RFC-0002).
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Append a host lifecycle event.
    async fn append_runtime(&self, ev: RuntimeEvent) -> Result<(), EventSinkError>;

    /// Append a session event; returns the assigned per-session sequence.
    async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, EventSinkError>;
}

#[derive(Default)]
struct MemoryState {
    runtime: Vec<RuntimeEvent>,
    sessions: HashMap<SessionId, Vec<SessionEvent>>,
    next_seq: HashMap<SessionId, u64>,
}

/// Process-local sink with **per-session** gapless [`EventSeq`] starting at 0.
#[derive(Default)]
pub struct InMemoryEventSink {
    inner: Mutex<MemoryState>,
}

impl InMemoryEventSink {
    /// Create an empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot runtime events (tests).
    pub fn runtime_events(&self) -> Vec<RuntimeEvent> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime
            .clone()
    }

    /// Snapshot session events for `session_id` (tests).
    pub fn session_events(&self, session_id: SessionId) -> Vec<SessionEvent> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Buffered runtime + session event count (handoff / tests).
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session_n: usize = g.sessions.values().map(Vec::len).sum();
        g.runtime.len() + session_n
    }

    /// Take buffered state for lossless SQLite handoff (leaves sink empty).
    pub fn drain_for_handoff(&self) -> HandoffSnapshot {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HandoffSnapshot {
            runtime: std::mem::take(&mut g.runtime),
            sessions: std::mem::take(&mut g.sessions),
            next_seq: std::mem::take(&mut g.next_seq),
        }
    }

    /// Restore a snapshot after failed import (handoff abort).
    ///
    /// Merges the snapshot ahead of any events appended concurrently through
    /// [`Self`] after drain; `next_seq` becomes the max of snapshot and current.
    pub fn restore_handoff_snapshot(&self, snap: HandoffSnapshot) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut runtime = snap.runtime;
        runtime.append(&mut g.runtime);
        g.runtime = runtime;

        let mut sessions = snap.sessions;
        for (sid, mut current) in std::mem::take(&mut g.sessions) {
            sessions.entry(sid).or_default().append(&mut current);
        }
        g.sessions = sessions;

        let mut next_seq = snap.next_seq;
        for (sid, cur) in std::mem::take(&mut g.next_seq) {
            let entry = next_seq.entry(sid).or_insert(0);
            *entry = (*entry).max(cur);
        }
        g.next_seq = next_seq;
    }
}

#[async_trait]
impl EventSink for InMemoryEventSink {
    async fn append_runtime(&self, ev: RuntimeEvent) -> Result<(), EventSinkError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| EventSinkError::Internal("poisoned".into()))?;
        g.runtime.push(ev);
        Ok(())
    }

    async fn append_session(&self, ev: NewSessionEvent) -> Result<EventSeq, EventSinkError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| EventSinkError::Internal("poisoned".into()))?;
        let seq_num = *g.next_seq.entry(ev.session_id).or_insert(0);
        let seq = EventSeq(seq_num);
        g.next_seq.insert(ev.session_id, seq_num + 1);
        let full = SessionEvent {
            seq,
            ts: Timestamp::now(),
            session_id: ev.session_id,
            run_id: ev.run_id,
            type_: ev.type_,
            payload: ev.payload,
        };
        g.sessions.entry(ev.session_id).or_default().push(full);
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::SessionEventType;
    use serde_json::json;

    #[tokio::test]
    async fn per_session_seq_interleaved() {
        let sink = InMemoryEventSink::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let mk = |session_id| NewSessionEvent {
            session_id,
            run_id: None,
            type_: SessionEventType::SessionCreated,
            payload: json!({}),
        };
        assert_eq!(sink.append_session(mk(a)).await.unwrap(), EventSeq(0));
        assert_eq!(sink.append_session(mk(b)).await.unwrap(), EventSeq(0));
        assert_eq!(sink.append_session(mk(a)).await.unwrap(), EventSeq(1));
        assert_eq!(sink.append_session(mk(b)).await.unwrap(), EventSeq(1));
        assert_eq!(sink.append_session(mk(a)).await.unwrap(), EventSeq(2));
        let ae = sink.session_events(a);
        assert!(ae.windows(2).all(|w| w[1].seq.0 == w[0].seq.0 + 1));
    }
}
