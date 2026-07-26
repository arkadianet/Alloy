//! Decision recording types and EventStore-backed log (RFC-0004 §3 / §5).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::RuntimeConfig;
use crate::error::SessionError;
use crate::events::{NewSessionEvent, SessionEventType};
use crate::obs::error::ObsError;
use crate::obs::redact::{
    apply_body_retention, redact_json_strings, redact_secrets, truncate_utf8_bytes,
    RetentionPolicy, BODY_MAX_BYTES, METADATA_MAX_BYTES,
};
use crate::runtime::RuntimeHandle;
use crate::storage::{AlloyStorage, SessionRows};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::ErrorClass;
use crate::types::ids::{Digest, EndpointId, EventSeq, NodeId, ProviderId, RunId, SessionId};

/// Kind of attributable decision (wire: snake_case / externally tagged Custom).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Model routing choice.
    ModelRoute,
    /// Context inclusion choice.
    ContextInclusion,
    /// Tool grant / deny.
    ToolGrant,
    /// Retry decision.
    Retry,
    /// Gate / approval decision.
    Gate,
    /// Budget-related decision.
    Budget,
    /// Extension point.
    Custom(String),
}

/// In-memory decision record (not the wire payload).
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    /// Session owning the decision.
    pub session: SessionId,
    /// Optional run.
    pub run: Option<RunId>,
    /// Optional DAG node.
    pub node: Option<NodeId>,
    /// Decision kind.
    pub kind: DecisionKind,
    /// JSON object metadata (`Null` normalized to `{}`).
    pub metadata: serde_json::Value,
    /// Optional content hash.
    pub content_hash: Option<Digest>,
    /// Optional prompt body (subject to retention).
    pub prompt_body: Option<String>,
}

/// Provenance of a recorded USD amount on a model call (RFC-0007 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUsdSource {
    /// Provider reported dollars (reserved; MVP openai-compatible does not use).
    ProviderReported,
    /// Operator price table × known tokens (RFC-0007 §5.8).
    OperatorPriceTable,
}

/// In-memory model-call record.
///
/// Additive attribution fields (`endpoint_id`, `model`, …) are RFC-0007.
/// Marked `#[non_exhaustive]` so later additive fields do not break consumers;
/// construct via [`ModelCallRecord::new`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ModelCallRecord {
    /// Session.
    pub session: SessionId,
    /// Optional run.
    pub run: Option<RunId>,
    /// Optional node.
    pub node: Option<NodeId>,
    /// Provider catalog id.
    pub provider_id: ProviderId,
    /// Model tier used.
    pub model_tier: ModelTier,
    /// Known input tokens, if any.
    pub input_tokens: Option<u64>,
    /// Known output tokens, if any.
    pub output_tokens: Option<u64>,
    /// Optional USD (must be finite for append).
    pub usd: Option<f64>,
    /// Optional duration.
    pub duration_ms: Option<u64>,
    /// Optional confidence.
    pub confidence: Option<f32>,
    /// Optional error class.
    pub error_class: Option<ErrorClass>,
    /// Optional content hash.
    pub content_hash: Option<Digest>,
    /// Optional prompt body.
    pub prompt_body: Option<String>,
    /// Endpoint catalog id that served the call (RFC-0007).
    pub endpoint_id: Option<EndpointId>,
    /// Operator-configured wire model id (BYOM; never a core constant).
    pub model: Option<String>,
    /// Event seq of the successful route decision, when recorded.
    pub route_event_seq: Option<EventSeq>,
    /// USD provenance when `usd` is `Some`.
    pub usd_source: Option<ModelUsdSource>,
    /// Provider finish reason (redacted/truncated at router boundary).
    pub finish_reason: Option<String>,
    /// Provider request id (redacted/truncated at router boundary).
    pub provider_request_id: Option<String>,
}

impl ModelCallRecord {
    /// Required identity fields; all other fields start as `None` / defaults.
    #[must_use]
    pub fn new(session: SessionId, provider_id: ProviderId, model_tier: ModelTier) -> Self {
        Self {
            session,
            run: None,
            node: None,
            provider_id,
            model_tier,
            input_tokens: None,
            output_tokens: None,
            usd: None,
            duration_ms: None,
            confidence: None,
            error_class: None,
            content_hash: None,
            prompt_body: None,
            endpoint_id: None,
            model: None,
            route_event_seq: None,
            usd_source: None,
            finish_reason: None,
            provider_request_id: None,
        }
    }

    /// Set the optional run id.
    #[must_use]
    pub fn run(mut self, run: RunId) -> Self {
        self.run = Some(run);
        self
    }

    /// Set the optional node id.
    #[must_use]
    pub fn node(mut self, node: NodeId) -> Self {
        self.node = Some(node);
        self
    }

    /// Set token counts (`None` means unknown — never fabricate zeros).
    #[must_use]
    pub fn tokens(mut self, input: Option<u64>, output: Option<u64>) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    /// Set optional USD.
    #[must_use]
    pub fn usd(mut self, usd: Option<f64>) -> Self {
        self.usd = usd;
        self
    }

    /// Set optional duration in milliseconds.
    #[must_use]
    pub fn duration_ms(mut self, ms: Option<u64>) -> Self {
        self.duration_ms = ms;
        self
    }

    /// Set optional confidence.
    #[must_use]
    pub fn confidence(mut self, c: Option<f32>) -> Self {
        self.confidence = c;
        self
    }

    /// Set optional error class.
    #[must_use]
    pub fn error_class(mut self, c: Option<ErrorClass>) -> Self {
        self.error_class = c;
        self
    }

    /// Set optional content hash.
    #[must_use]
    pub fn content_hash(mut self, h: Option<Digest>) -> Self {
        self.content_hash = h;
        self
    }

    /// Set optional prompt body.
    #[must_use]
    pub fn prompt_body(mut self, body: Option<String>) -> Self {
        self.prompt_body = body;
        self
    }

    /// Set optional endpoint id.
    #[must_use]
    pub fn endpoint_id(mut self, id: Option<EndpointId>) -> Self {
        self.endpoint_id = id;
        self
    }

    /// Set optional wire model id.
    #[must_use]
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Set optional route decision event seq.
    #[must_use]
    pub fn route_event_seq(mut self, seq: Option<EventSeq>) -> Self {
        self.route_event_seq = seq;
        self
    }

    /// Set optional USD provenance.
    #[must_use]
    pub fn usd_source(mut self, source: Option<ModelUsdSource>) -> Self {
        self.usd_source = source;
        self
    }

    /// Set optional finish reason.
    #[must_use]
    pub fn finish_reason(mut self, reason: Option<String>) -> Self {
        self.finish_reason = reason;
        self
    }

    /// Set optional provider request id.
    #[must_use]
    pub fn provider_request_id(mut self, id: Option<String>) -> Self {
        self.provider_request_id = id;
        self
    }
}

/// In-memory tool-call record.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    /// Session.
    pub session: SessionId,
    /// Optional run.
    pub run: Option<RunId>,
    /// Optional node.
    pub node: Option<NodeId>,
    /// Tool name.
    pub tool_name: String,
    /// Optional tool server.
    pub tool_server: Option<String>,
    /// Optional latency.
    pub latency_ms: Option<u64>,
    /// Whether the call was denied.
    pub denied: bool,
    /// Optional content hash.
    pub content_hash: Option<Digest>,
    /// Optional body (subject to retention).
    pub body: Option<String>,
}

/// Append attributable decisions through the session event log.
#[async_trait]
pub trait DecisionLog: Send + Sync {
    /// Append a [`SessionEventType::Decision`] after redaction/retention.
    async fn record(&self, rec: DecisionRecord) -> Result<EventSeq, ObsError>;

    /// Append a [`SessionEventType::ModelCall`] after redaction/retention.
    async fn record_model_call(&self, rec: ModelCallRecord) -> Result<EventSeq, ObsError>;

    /// Append a [`SessionEventType::ToolCall`] after redaction/retention.
    async fn record_tool_call(&self, rec: ToolCallRecord) -> Result<EventSeq, ObsError>;
}

/// [`DecisionLog`] backed by [`RuntimeHandle::append_session`] + retention from config.
///
/// Normative: a session row MUST exist before append (§3.7); missing →
/// [`ObsError::Session`]`(`[`SessionError::NotFound`]`)`. Append failures are fail-closed
/// (§5.9) — no substitute success event is invented.
pub struct EventDecisionLog {
    handle: RuntimeHandle,
    storage: Arc<AlloyStorage>,
    retention: RetentionPolicy,
}

impl std::fmt::Debug for EventDecisionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventDecisionLog")
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl EventDecisionLog {
    /// Construct with an explicit retention policy.
    #[must_use]
    pub fn new(
        handle: RuntimeHandle,
        storage: Arc<AlloyStorage>,
        retention: RetentionPolicy,
    ) -> Self {
        Self {
            handle,
            storage,
            retention,
        }
    }

    /// Load retention from `handle.config()` (requires configure).
    pub fn from_handle(
        handle: RuntimeHandle,
        storage: Arc<AlloyStorage>,
    ) -> Result<Self, ObsError> {
        let cfg: Arc<RuntimeConfig> = handle.config()?;
        Ok(Self::new(
            handle,
            storage,
            RetentionPolicy::from(cfg.as_ref()),
        ))
    }

    async fn require_session(&self, session: SessionId) -> Result<(), ObsError> {
        match self.storage.sessions().get_session(session).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(ObsError::Session(SessionError::NotFound(session))),
            Err(e) => Err(ObsError::Store(e)),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DecisionPayload {
    pub(crate) kind: DecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) node_id: Option<NodeId>,
    pub(crate) metadata: serde_json::Value,
    pub(crate) content_hash: Option<Digest>,
    pub(crate) prompt_body: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelCallPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) node_id: Option<NodeId>,
    pub(crate) provider_id: ProviderId,
    pub(crate) model_tier: ModelTier,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) usage_unknown: bool,
    pub(crate) usd: Option<f64>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) confidence: Option<f32>,
    pub(crate) error_class: Option<ErrorClass>,
    pub(crate) content_hash: Option<Digest>,
    pub(crate) prompt_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint_id: Option<EndpointId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) route_event_seq: Option<EventSeq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usd_source: Option<ModelUsdSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolCallPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) node_id: Option<NodeId>,
    pub(crate) tool_name: String,
    pub(crate) tool_server: Option<String>,
    pub(crate) latency_ms: Option<u64>,
    pub(crate) denied: bool,
    pub(crate) content_hash: Option<Digest>,
    pub(crate) body: Option<String>,
}

fn normalize_metadata(meta: serde_json::Value) -> Result<serde_json::Value, ObsError> {
    let meta = if meta.is_null() {
        serde_json::json!({})
    } else {
        meta
    };
    if !meta.is_object() {
        return Err(ObsError::Invalid("metadata must be object".into()));
    }
    let bytes = serde_json::to_vec(&meta).map_err(|e| ObsError::Invalid(e.to_string()))?;
    if bytes.len() > METADATA_MAX_BYTES {
        return Err(ObsError::Invalid(format!(
            "metadata exceeds {METADATA_MAX_BYTES} bytes"
        )));
    }
    Ok(meta)
}

fn check_body_size(body: Option<&str>) -> Result<(), ObsError> {
    if let Some(b) = body {
        if b.len() > BODY_MAX_BYTES {
            return Err(ObsError::Invalid(format!(
                "body exceeds {BODY_MAX_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn resolve_hash(
    session: SessionId,
    caller_hash: Option<Digest>,
    raw: Option<&str>,
    helper_hash: Option<Digest>,
) -> Option<Digest> {
    match (caller_hash, raw, helper_hash) {
        (Some(h), Some(_), Some(hh)) if h != hh => {
            tracing::warn!(%session, "content_hash mismatch; using recomputed");
            Some(hh)
        }
        (_, Some(_), Some(hh)) => Some(hh),
        (Some(h), None, _) => Some(h),
        (_, _, hh) => hh,
    }
}

fn log_deny_strip(session: SessionId) {
    tracing::warn!(%session, reason = "path_deny_list", "retention deny-list stripped body");
}

/// Prepare decision fields for append or in-memory recording (shared with [`super::recording`]).
pub(crate) fn prepare_decision(
    mut rec: DecisionRecord,
    retention: RetentionPolicy,
) -> Result<DecisionRecord, ObsError> {
    rec.metadata = normalize_metadata(rec.metadata)?;
    check_body_size(rec.prompt_body.as_deref())?;
    let session = rec.session;
    let caller_hash = rec.content_hash.take();
    let raw_owned = rec.prompt_body.take();
    let outcome = apply_body_retention(raw_owned.as_deref(), retention.retain_full_prompts, true)?;
    if outcome.deny_list_stripped {
        log_deny_strip(session);
    }
    rec.content_hash = resolve_hash(session, caller_hash, raw_owned.as_deref(), outcome.hash);
    rec.metadata = redact_json_strings(&rec.metadata);
    rec.prompt_body = outcome.body;
    Ok(rec)
}

pub(crate) fn prepare_model_call(
    mut rec: ModelCallRecord,
    retention: RetentionPolicy,
) -> Result<ModelCallRecord, ObsError> {
    check_body_size(rec.prompt_body.as_deref())?;
    if let Some(u) = rec.usd {
        if !u.is_finite() {
            return Err(ObsError::Invalid("usd must be finite".into()));
        }
    }
    let session = rec.session;
    let caller_hash = rec.content_hash.take();
    let raw_owned = rec.prompt_body.take();
    let outcome = apply_body_retention(raw_owned.as_deref(), retention.retain_full_prompts, true)?;
    if outcome.deny_list_stripped {
        log_deny_strip(session);
    }
    rec.content_hash = resolve_hash(session, caller_hash, raw_owned.as_deref(), outcome.hash);
    rec.prompt_body = outcome.body;
    rec.finish_reason = rec
        .finish_reason
        .as_deref()
        .map(redact_secrets)
        .map(|value| truncate_utf8_bytes(&value, 128));
    rec.provider_request_id = rec
        .provider_request_id
        .as_deref()
        .map(redact_secrets)
        .map(|value| truncate_utf8_bytes(&value, 256));
    Ok(rec)
}

pub(crate) fn prepare_tool_call(
    mut rec: ToolCallRecord,
    retention: RetentionPolicy,
) -> Result<ToolCallRecord, ObsError> {
    check_body_size(rec.body.as_deref())?;
    let session = rec.session;
    let caller_hash = rec.content_hash.take();
    let raw_owned = rec.body.take();
    let outcome = apply_body_retention(raw_owned.as_deref(), retention.retain_tool_bodies, false)?;
    if outcome.deny_list_stripped {
        log_deny_strip(session);
    }
    rec.content_hash = resolve_hash(session, caller_hash, raw_owned.as_deref(), outcome.hash);
    rec.body = outcome.body;
    Ok(rec)
}

fn decision_to_payload(rec: DecisionRecord) -> Result<serde_json::Value, ObsError> {
    let payload = DecisionPayload {
        kind: rec.kind,
        node_id: rec.node,
        metadata: rec.metadata,
        content_hash: rec.content_hash,
        prompt_body: rec.prompt_body,
    };
    serde_json::to_value(payload).map_err(|e| ObsError::Internal(e.to_string()))
}

fn model_to_payload(rec: ModelCallRecord) -> Result<serde_json::Value, ObsError> {
    let usage_unknown = rec.input_tokens.is_none() || rec.output_tokens.is_none();
    let payload = ModelCallPayload {
        node_id: rec.node,
        provider_id: rec.provider_id,
        model_tier: rec.model_tier,
        input_tokens: rec.input_tokens,
        output_tokens: rec.output_tokens,
        usage_unknown,
        usd: rec.usd,
        duration_ms: rec.duration_ms,
        confidence: rec.confidence,
        error_class: rec.error_class,
        content_hash: rec.content_hash,
        prompt_body: rec.prompt_body,
        endpoint_id: rec.endpoint_id,
        model: rec.model,
        route_event_seq: rec.route_event_seq,
        usd_source: rec.usd_source,
        finish_reason: rec.finish_reason,
        provider_request_id: rec.provider_request_id,
    };
    serde_json::to_value(payload).map_err(|e| ObsError::Internal(e.to_string()))
}

fn tool_to_payload(rec: ToolCallRecord) -> Result<serde_json::Value, ObsError> {
    let payload = ToolCallPayload {
        node_id: rec.node,
        tool_name: rec.tool_name,
        tool_server: rec.tool_server,
        latency_ms: rec.latency_ms,
        denied: rec.denied,
        content_hash: rec.content_hash,
        body: rec.body,
    };
    serde_json::to_value(payload).map_err(|e| ObsError::Internal(e.to_string()))
}

#[async_trait]
impl DecisionLog for EventDecisionLog {
    async fn record(&self, rec: DecisionRecord) -> Result<EventSeq, ObsError> {
        self.require_session(rec.session).await?;
        let session = rec.session;
        let run = rec.run;
        let prepared = prepare_decision(rec, self.retention)?;
        let kind = prepared.kind.clone();
        let payload = decision_to_payload(prepared)?;
        match self
            .handle
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: run,
                type_: SessionEventType::Decision,
                payload,
            })
            .await
        {
            Ok(seq) => Ok(seq),
            Err(e) => {
                tracing::error!(%session, ?run, ?kind, error = %e, "decision append failed");
                Err(ObsError::Append(e))
            }
        }
    }

    async fn record_model_call(&self, rec: ModelCallRecord) -> Result<EventSeq, ObsError> {
        self.require_session(rec.session).await?;
        let session = rec.session;
        let run = rec.run;
        let prepared = prepare_model_call(rec, self.retention)?;
        let payload = model_to_payload(prepared)?;
        match self
            .handle
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: run,
                type_: SessionEventType::ModelCall,
                payload,
            })
            .await
        {
            Ok(seq) => Ok(seq),
            Err(e) => {
                tracing::error!(%session, ?run, error = %e, "model_call append failed");
                Err(ObsError::Append(e))
            }
        }
    }

    async fn record_tool_call(&self, rec: ToolCallRecord) -> Result<EventSeq, ObsError> {
        self.require_session(rec.session).await?;
        let session = rec.session;
        let run = rec.run;
        let prepared = prepare_tool_call(rec, self.retention)?;
        let payload = tool_to_payload(prepared)?;
        match self
            .handle
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: run,
                type_: SessionEventType::ToolCall,
                payload,
            })
            .await
        {
            Ok(seq) => Ok(seq),
            Err(e) => {
                tracing::error!(%session, ?run, error = %e, "tool_call append failed");
                Err(ObsError::Append(e))
            }
        }
    }
}

/// Parse helpers need access to private payload shapes — used by query module.
pub(crate) fn parse_decision_payload(
    payload: &serde_json::Value,
) -> Result<DecisionPayload, ObsError> {
    DecisionPayload::deserialize(payload)
        .map_err(|e| ObsError::Invalid(format!("decision payload: {e}")))
}

pub(crate) fn parse_model_payload(
    payload: &serde_json::Value,
) -> Result<ModelCallPayload, ObsError> {
    ModelCallPayload::deserialize(payload)
        .map_err(|e| ObsError::Invalid(format!("model_call payload: {e}")))
}

pub(crate) fn parse_tool_payload(payload: &serde_json::Value) -> Result<ToolCallPayload, ObsError> {
    ToolCallPayload::deserialize(payload)
        .map_err(|e| ObsError::Invalid(format!("tool_call payload: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::hash::hash_prompt;

    #[test]
    fn decision_kind_serde_golden() {
        assert_eq!(
            serde_json::to_value(DecisionKind::ModelRoute).unwrap(),
            serde_json::json!("model_route")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::ContextInclusion).unwrap(),
            serde_json::json!("context_inclusion")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::ToolGrant).unwrap(),
            serde_json::json!("tool_grant")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::Retry).unwrap(),
            serde_json::json!("retry")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::Gate).unwrap(),
            serde_json::json!("gate")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::Budget).unwrap(),
            serde_json::json!("budget")
        );
        assert_eq!(
            serde_json::to_value(DecisionKind::Custom("x".into())).unwrap(),
            serde_json::json!({"custom":"x"})
        );
        assert_eq!(
            serde_json::from_value::<DecisionKind>(serde_json::json!({"custom":"x"})).unwrap(),
            DecisionKind::Custom("x".into())
        );
        assert!(serde_json::from_value::<DecisionKind>(serde_json::json!("nope")).is_err());
    }

    #[test]
    fn decision_payload_no_session_fields() {
        let rec = DecisionRecord {
            session: SessionId::new(),
            run: Some(RunId::new()),
            node: None,
            kind: DecisionKind::Retry,
            metadata: serde_json::json!({}),
            content_hash: Some(hash_prompt("a")),
            prompt_body: None,
        };
        let v = decision_to_payload(rec).unwrap();
        assert!(v.get("session").is_none());
        assert!(v.get("run").is_none());
        assert!(v.get("session_id").is_none());
        assert!(v.get("run_id").is_none());
        assert_eq!(v["kind"], "retry");
    }

    #[test]
    fn metadata_rejects_non_object() {
        let err = normalize_metadata(serde_json::json!([])).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(_)));
        let err = normalize_metadata(serde_json::json!("x")).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(_)));
        assert!(normalize_metadata(serde_json::Value::Null).is_ok());
    }

    #[test]
    fn size_cap_rejects_huge_body() {
        let huge = "x".repeat(BODY_MAX_BYTES + 1);
        let err = check_body_size(Some(&huge)).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(_)));
    }

    #[test]
    fn size_cap_rejects_huge_metadata() {
        let big = "y".repeat(METADATA_MAX_BYTES);
        let meta = serde_json::json!({ "k": big });
        let err = normalize_metadata(meta).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(msg) if msg.contains("metadata")));
    }

    #[test]
    fn content_hash_mismatch_replaced() {
        let wrong = hash_prompt("other");
        let rec = DecisionRecord {
            session: SessionId::new(),
            run: None,
            node: None,
            kind: DecisionKind::Retry,
            metadata: serde_json::json!({}),
            content_hash: Some(wrong),
            prompt_body: Some("actual".into()),
        };
        let prepared = prepare_decision(rec, RetentionPolicy::defaults()).unwrap();
        assert_eq!(prepared.content_hash, Some(hash_prompt("actual")));
        assert!(prepared.prompt_body.is_none());
    }

    #[test]
    fn metadata_secrets_redacted_on_prepare() {
        let rec = DecisionRecord {
            session: SessionId::new(),
            run: None,
            node: None,
            kind: DecisionKind::ModelRoute,
            metadata: serde_json::json!({"api_key": "sk-abcdefghij", "route": "ok"}),
            content_hash: None,
            prompt_body: None,
        };
        let prepared = prepare_decision(rec, RetentionPolicy::defaults()).unwrap();
        assert_eq!(prepared.metadata["api_key"], "[REDACTED]");
        assert_eq!(prepared.metadata["route"], "ok");
    }

    #[test]
    fn record_model_call_rejects_non_finite_usd() {
        let rec = ModelCallRecord::new(
            SessionId::new(),
            ProviderId::new("p").unwrap(),
            ModelTier::Standard,
        )
        .tokens(Some(1), Some(1))
        .usd(Some(f64::NAN));
        let err = prepare_model_call(rec, RetentionPolicy::defaults()).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(msg) if msg.contains("usd")));
    }

    #[test]
    fn model_call_provider_metadata_is_redacted_and_bounded_on_prepare() {
        let rec = ModelCallRecord::new(
            SessionId::new(),
            ProviderId::new("p").unwrap(),
            ModelTier::Standard,
        )
        .finish_reason(Some(format!("api_key=sk-abcdefghij {}", "é".repeat(100))))
        .provider_request_id(Some(format!(
            "Authorization: Bearer secret-token {}",
            "é".repeat(200)
        )));
        let prepared = prepare_model_call(rec, RetentionPolicy::defaults()).unwrap();
        let finish = prepared.finish_reason.unwrap();
        let request_id = prepared.provider_request_id.unwrap();
        assert!(finish.len() <= 128);
        assert!(request_id.len() <= 256);
        assert!(!finish.contains("sk-"));
        assert!(!request_id.contains("secret-token"));
        assert!(finish.is_char_boundary(finish.len()));
        assert!(request_id.is_char_boundary(request_id.len()));
    }

    #[test]
    fn worker_metrics_confidence_option() {
        let rec = ModelCallRecord::new(
            SessionId::new(),
            ProviderId::new("p").unwrap(),
            ModelTier::Standard,
        )
        .tokens(Some(1), Some(1));
        let v = model_to_payload(rec).unwrap();
        assert!(v.get("confidence").is_none() || v["confidence"].is_null());
        let parsed: ModelCallPayload = serde_json::from_value(v).unwrap();
        assert!(parsed.confidence.is_none());
    }

    #[test]
    fn model_call_payload_omits_new_fields_when_none() {
        let rec = ModelCallRecord::new(
            SessionId::new(),
            ProviderId::new("p").unwrap(),
            ModelTier::Standard,
        );
        let v = model_to_payload(rec).unwrap();
        assert!(v.get("endpoint_id").is_none());
        assert!(v.get("model").is_none());
        assert!(v.get("usd_source").is_none());
    }
}
