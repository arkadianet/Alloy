//! Query helpers over [`EventStore`] (RFC-0004 §3.14 / §7.5).

use crate::events::{SessionEvent, SessionEventType};
use crate::obs::cost::CostMeter;
use crate::obs::decision::{
    parse_decision_payload, parse_model_payload, parse_tool_payload, DecisionRecord,
    ModelCallRecord, ToolCallRecord,
};
use crate::obs::error::ObsError;
use crate::session::{clamp_events_page_limit, MAX_EVENTS_PAGE};
use crate::storage::EventStore;
use crate::types::ids::{EventSeq, RunId, SessionId};

/// Max store pages scanned per [`list_decision_events`] call before yielding a resume cursor.
const MAX_SCAN_PAGES: usize = 16;

/// Page of decision-related session events.
#[derive(Debug, Clone)]
pub struct DecisionPage {
    /// Matching events in ascending seq order.
    pub events: Vec<SessionEvent>,
    /// Exclusive resume cursor (`after` on the next call). `None` when the scan reached the end.
    pub next_after: Option<EventSeq>,
}

fn is_decision_related(t: SessionEventType) -> bool {
    matches!(
        t,
        SessionEventType::Decision | SessionEventType::ModelCall | SessionEventType::ToolCall
    )
}

/// Page matching `Decision` | `ModelCall` | `ToolCall` via dyn-safe `list_session_events`.
pub async fn list_decision_events(
    store: &dyn EventStore,
    session: SessionId,
    after: Option<EventSeq>,
    limit: usize,
) -> Result<DecisionPage, ObsError> {
    let limit = if limit == 0 {
        1
    } else {
        limit.min(MAX_EVENTS_PAGE)
    };
    let store_page = clamp_events_page_limit(MAX_EVENTS_PAGE);

    let mut events = Vec::new();
    let mut cursor = after;
    let mut last_scanned: Option<EventSeq> = None;
    let mut pages = 0usize;
    let mut more_after_scan = false;

    while events.len() < limit && pages < MAX_SCAN_PAGES {
        let page = store
            .list_session_events(session, cursor, store_page)
            .await?;
        pages += 1;
        if page.is_empty() {
            more_after_scan = false;
            break;
        }
        let short_page = page.len() < store_page;
        let page_len = page.len();
        let mut hit_limit = false;
        for (i, ev) in page.into_iter().enumerate() {
            last_scanned = Some(ev.seq);
            cursor = Some(ev.seq);
            if is_decision_related(ev.type_) {
                events.push(ev);
                if events.len() == limit {
                    hit_limit = true;
                    // Unscanned remainder of this page, or a full page implying later pages.
                    more_after_scan = (i + 1) < page_len || !short_page;
                    break;
                }
            }
        }
        if hit_limit {
            break;
        }
        if short_page {
            more_after_scan = false;
            break;
        }
        // Full page consumed without filling limit — continue; may hit MAX_SCAN_PAGES.
        more_after_scan = true;
    }

    if pages >= MAX_SCAN_PAGES && events.len() < limit {
        more_after_scan = true;
    }

    let next_after = if more_after_scan { last_scanned } else { None };

    Ok(DecisionPage { events, next_after })
}

/// Restore a [`DecisionRecord`] from a session event.
pub fn parse_decision_event(ev: &SessionEvent) -> Result<DecisionRecord, ObsError> {
    if ev.type_ != SessionEventType::Decision {
        return Err(ObsError::Invalid("expected Decision event".into()));
    }
    let p = parse_decision_payload(&ev.payload)?;
    Ok(DecisionRecord {
        session: ev.session_id,
        run: ev.run_id,
        node: p.node_id,
        kind: p.kind,
        metadata: p.metadata,
        content_hash: p.content_hash,
        prompt_body: p.prompt_body,
    })
}

/// Restore a [`ModelCallRecord`] from a session event.
pub fn parse_model_call_event(ev: &SessionEvent) -> Result<ModelCallRecord, ObsError> {
    if ev.type_ != SessionEventType::ModelCall {
        return Err(ObsError::Invalid("expected ModelCall event".into()));
    }
    let p = parse_model_payload(&ev.payload)?;
    let tokens_unknown = p.input_tokens.is_none() || p.output_tokens.is_none();
    if p.usage_unknown != tokens_unknown {
        return Err(ObsError::Invalid("usage_unknown inconsistent".into()));
    }
    Ok(ModelCallRecord {
        session: ev.session_id,
        run: ev.run_id,
        node: p.node_id,
        provider_id: p.provider_id,
        model_tier: p.model_tier,
        input_tokens: p.input_tokens,
        output_tokens: p.output_tokens,
        usd: p.usd,
        duration_ms: p.duration_ms,
        confidence: p.confidence,
        error_class: p.error_class,
        content_hash: p.content_hash,
        prompt_body: p.prompt_body,
    })
}

/// Restore a [`ToolCallRecord`] from a session event.
pub fn parse_tool_call_event(ev: &SessionEvent) -> Result<ToolCallRecord, ObsError> {
    if ev.type_ != SessionEventType::ToolCall {
        return Err(ObsError::Invalid("expected ToolCall event".into()));
    }
    let p = parse_tool_payload(&ev.payload)?;
    Ok(ToolCallRecord {
        session: ev.session_id,
        run: ev.run_id,
        node: p.node_id,
        tool_name: p.tool_name,
        tool_server: p.tool_server,
        latency_ms: p.latency_ms,
        denied: p.denied,
        content_hash: p.content_hash,
        body: p.body,
    })
}

/// Rebuild a [`CostMeter`] from durable `ModelCall` events.
///
/// `run: None` aggregates all runs; `Some(id)` filters by envelope run id.
/// Meter-only updates without a durable model call are not recovered.
pub async fn reaccumulate_cost_from_events(
    store: &dyn EventStore,
    session: SessionId,
    run: Option<RunId>,
) -> Result<CostMeter, ObsError> {
    let mut meter = CostMeter::new();
    let mut after: Option<EventSeq> = None;
    loop {
        let page = store
            .list_session_events(session, after, MAX_EVENTS_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        for ev in &page {
            after = Some(ev.seq);
            if ev.type_ != SessionEventType::ModelCall {
                continue;
            }
            if let Some(want) = run {
                if ev.run_id != Some(want) {
                    continue;
                }
            }
            let rec = parse_model_call_event(ev)?;
            meter.add_model_usage(rec.model_tier, rec.input_tokens, rec.output_tokens, rec.usd);
        }
        if page_len < MAX_EVENTS_PAGE {
            break;
        }
    }
    Ok(meter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::decision::{DecisionKind, ModelCallRecord};
    use crate::types::budget::ModelTier;
    use crate::types::ids::{EventSeq, ProviderId, SessionId};

    #[test]
    fn usage_unknown_consistency_parse() {
        let session = SessionId::new();
        let ev = SessionEvent {
            seq: EventSeq(0),
            ts: crate::types::ids::Timestamp::now(),
            session_id: session,
            run_id: None,
            type_: SessionEventType::ModelCall,
            payload: serde_json::json!({
                "provider_id": "p",
                "model_tier": "standard",
                "input_tokens": 1,
                "output_tokens": 1,
                "usage_unknown": true,
                "usd": null,
                "duration_ms": null,
                "confidence": null,
                "error_class": null,
                "content_hash": null,
                "prompt_body": null
            }),
        };
        let err = parse_model_call_event(&ev).unwrap_err();
        assert!(matches!(err, ObsError::Invalid(msg) if msg.contains("usage_unknown")));
    }

    #[test]
    fn parse_model_ok() {
        let session = SessionId::new();
        let rec = ModelCallRecord {
            session,
            run: None,
            node: None,
            provider_id: ProviderId::new("p").unwrap(),
            model_tier: ModelTier::Standard,
            input_tokens: None,
            output_tokens: Some(1),
            usd: None,
            duration_ms: None,
            confidence: None,
            error_class: None,
            content_hash: None,
            prompt_body: None,
        };
        // Build via serde of private shape through list path — synthesize consistent wire.
        let ev = SessionEvent {
            seq: EventSeq(1),
            ts: crate::types::ids::Timestamp::now(),
            session_id: session,
            run_id: None,
            type_: SessionEventType::ModelCall,
            payload: serde_json::json!({
                "provider_id": "p",
                "model_tier": "standard",
                "input_tokens": null,
                "output_tokens": 1,
                "usage_unknown": true,
                "usd": null,
                "duration_ms": null,
                "confidence": null,
                "error_class": null,
                "content_hash": null,
                "prompt_body": null
            }),
        };
        let parsed = parse_model_call_event(&ev).unwrap();
        assert_eq!(parsed.input_tokens, rec.input_tokens);
        assert_eq!(parsed.output_tokens, rec.output_tokens);
    }

    #[test]
    fn wrong_type_rejected() {
        let ev = SessionEvent {
            seq: EventSeq(0),
            ts: crate::types::ids::Timestamp::now(),
            session_id: SessionId::new(),
            run_id: None,
            type_: SessionEventType::ToolCall,
            payload: serde_json::json!({}),
        };
        assert!(parse_decision_event(&ev).is_err());
        let _ = DecisionKind::Retry; // keep import used if optimized
    }
}
