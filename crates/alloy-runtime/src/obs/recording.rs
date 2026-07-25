//! In-memory [`DecisionLog`] test double (RFC-0004 §3.7a).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

    fn next(&self) -> EventSeq {
        EventSeq(self.next_seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Post-retention decision records.
    #[must_use]
    pub fn recorded_decisions(&self) -> Vec<DecisionRecord> {
        self.records.lock().expect("recording lock").clone()
    }

    /// Post-retention model-call records.
    #[must_use]
    pub fn recorded_model_calls(&self) -> Vec<ModelCallRecord> {
        self.model_calls.lock().expect("recording lock").clone()
    }

    /// Post-retention tool-call records.
    #[must_use]
    pub fn recorded_tool_calls(&self) -> Vec<ToolCallRecord> {
        self.tool_calls.lock().expect("recording lock").clone()
    }
}

#[async_trait]
impl DecisionLog for RecordingDecisionLog {
    async fn record(&self, rec: DecisionRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_decision(rec, self.retention)?;
        self.records.lock().expect("recording lock").push(prepared);
        Ok(self.next())
    }

    async fn record_model_call(&self, rec: ModelCallRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_model_call(rec, self.retention)?;
        self.model_calls
            .lock()
            .expect("recording lock")
            .push(prepared);
        Ok(self.next())
    }

    async fn record_tool_call(&self, rec: ToolCallRecord) -> Result<EventSeq, ObsError> {
        let prepared = prepare_tool_call(rec, self.retention)?;
        self.tool_calls
            .lock()
            .expect("recording lock")
            .push(prepared);
        Ok(self.next())
    }
}
