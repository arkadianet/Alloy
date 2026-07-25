//! In-memory [`DecisionLog`] test double (RFC-0004 §3.7a).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::obs::decision::{
    prepare_decision, prepare_model_call, prepare_tool_call, DecisionLog, DecisionRecord,
    ModelCallRecord, ToolCallRecord,
};
use crate::obs::error::ObsError;
use crate::obs::redact::RetentionPolicy;
use crate::types::ids::EventSeq;

/// Records decisions in memory after the same retention/redaction as [`super::EventDecisionLog`].
pub struct RecordingDecisionLog {
    retention: RetentionPolicy,
    records: Mutex<Vec<DecisionRecord>>,
    model_calls: Mutex<Vec<ModelCallRecord>>,
    tool_calls: Mutex<Vec<ToolCallRecord>>,
    next_seq: AtomicU64,
}

impl std::fmt::Debug for RecordingDecisionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingDecisionLog")
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl RecordingDecisionLog {
    /// Create with the given retention policy.
    #[must_use]
    pub fn new(retention: RetentionPolicy) -> Self {
        Self {
            retention,
            records: Mutex::new(Vec::new()),
            model_calls: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            next_seq: AtomicU64::new(0),
        }
    }

    fn lock<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
        match m.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::error!("recording decision log mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    fn next_seq(&self) -> EventSeq {
        EventSeq(self.next_seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Post-retention decision records.
    #[must_use]
    pub fn recorded_decisions(&self) -> Vec<DecisionRecord> {
        Self::lock(&self.records).clone()
    }

    /// Post-retention model-call records.
    #[must_use]
    pub fn recorded_model_calls(&self) -> Vec<ModelCallRecord> {
        Self::lock(&self.model_calls).clone()
    }

    /// Post-retention tool-call records.
    #[must_use]
    pub fn recorded_tool_calls(&self) -> Vec<ToolCallRecord> {
        Self::lock(&self.tool_calls).clone()
    }
}

#[async_trait]
impl DecisionLog for RecordingDecisionLog {
    async fn record(&self, rec: DecisionRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_decision(rec, self.retention)?;
        let mut guard = Self::lock(&self.records);
        let seq = self.next_seq();
        guard.push(prepared);
        Ok(seq)
    }

    async fn record_model_call(&self, rec: ModelCallRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_model_call(rec, self.retention)?;
        let mut guard = Self::lock(&self.model_calls);
        let seq = self.next_seq();
        guard.push(prepared);
        Ok(seq)
    }

    async fn record_tool_call(&self, rec: ToolCallRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_tool_call(rec, self.retention)?;
        let mut guard = Self::lock(&self.tool_calls);
        let seq = self.next_seq();
        guard.push(prepared);
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::decision::{DecisionKind, DecisionRecord};
    use crate::types::ids::SessionId;

    #[tokio::test]
    async fn recording_applies_retention_and_monotonic_seq() {
        let log = RecordingDecisionLog::new(RetentionPolicy::defaults());
        let s1 = log
            .record(DecisionRecord {
                session: SessionId::new(),
                run: None,
                node: None,
                kind: DecisionKind::Retry,
                metadata: serde_json::json!({}),
                content_hash: None,
                prompt_body: Some("api_key=sk-12345678".into()),
            })
            .await
            .unwrap();
        let s2 = log
            .record(DecisionRecord {
                session: SessionId::new(),
                run: None,
                node: None,
                kind: DecisionKind::Gate,
                metadata: serde_json::json!({}),
                content_hash: None,
                prompt_body: None,
            })
            .await
            .unwrap();
        assert_eq!(s1.0, 0);
        assert_eq!(s2.0, 1);
        let recs = log.recorded_decisions();
        assert_eq!(recs.len(), 2);
        assert!(recs[0].prompt_body.is_none());
        assert!(recs[0].content_hash.is_some());
    }

    #[tokio::test]
    async fn recording_as_dyn_decision_log() {
        let log: std::sync::Arc<dyn DecisionLog> =
            std::sync::Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let seq = log
            .record(DecisionRecord {
                session: SessionId::new(),
                run: None,
                node: None,
                kind: DecisionKind::Budget,
                metadata: serde_json::Value::Null,
                content_hash: None,
                prompt_body: None,
            })
            .await
            .unwrap();
        assert_eq!(seq.0, 0);
    }
}
