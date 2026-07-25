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
pub(crate) const MAX_SCAN_PAGES: usize = 16;

/// Page of decision-related session events.
#[derive(Debug, Clone)]
pub struct DecisionPage {
    /// Matching events in ascending seq order.
    pub events: Vec<SessionEvent>,
    /// Exclusive resume cursor: pass as `after` on the next call.
    ///
    /// - `None` — the scan reached the end of the session log (no more events to scan).
    /// - `Some(seq)` — more store events may exist after `seq` (including when the
    ///   `MAX_SCAN_PAGES` budget was exhausted **with zero matches**). Callers MUST
    ///   resume with `after = next_after` rather than stopping on empty `events`.
    pub next_after: Option<EventSeq>,
}

fn is_decision_related(t: SessionEventType) -> bool {
    matches!(
        t,
        SessionEventType::Decision | SessionEventType::ModelCall | SessionEventType::ToolCall
    )
}

/// Page matching `Decision` | `ModelCall` | `ToolCall` via dyn-safe `list_session_events`.
///
/// # Cursor contract (RFC-0004 §3.14)
///
/// - Ascending `seq` order.
/// - `limit == 0` is treated as `1` matching event max.
/// - `limit` is the max **matching** events returned (clamped to [`MAX_EVENTS_PAGE`]).
/// - Internally scans store pages of size `clamp_events_page_limit(MAX_EVENTS_PAGE)` until
///   `events.len() == limit`, a store page returns short/empty, **or** `MAX_SCAN_PAGES`
///   (16) store pages have been read — then returns with `next_after` set so the caller
///   can resume. Empty `events` with `Some(next_after)` means “no matches in this scan
///   window; keep paging.”
/// - `next_after` is the `seq` of the last **scanned** store event (matching or not)
///   when more store events may exist; `None` when the store page was short/empty at
///   end of scan.
pub async fn list_decision_events(
    store: &dyn EventStore,
    session: SessionId,
    after: Option<EventSeq>,
    limit: usize,
) -> Result<DecisionPage, ObsError> {
    list_decision_events_bounded(store, session, after, limit, MAX_SCAN_PAGES).await
}

/// Same as [`list_decision_events`] with an explicit scan-page budget (tests / internals).
pub(crate) async fn list_decision_events_bounded(
    store: &dyn EventStore,
    session: SessionId,
    after: Option<EventSeq>,
    limit: usize,
    max_scan_pages: usize,
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

    while events.len() < limit && pages < max_scan_pages {
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
        // Full page consumed without filling limit — more pages may exist.
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
///
/// Parse order follows RFC-0004 §7.5: every `ModelCall` is parsed first (corrupt
/// payloads fail the rebuild), then the run filter is applied.
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
            let rec = parse_model_call_event(ev)?;
            if let Some(want) = run {
                if rec.run != Some(want) {
                    continue;
                }
            }
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
    use crate::events::{EventSink, EventSinkError, NewSessionEvent, RuntimeEvent};
    use crate::storage::StoreError;
    use crate::types::budget::ModelTier;
    use crate::types::ids::{EventSeq, ProviderId, SessionId, Timestamp};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FakeStore {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl FakeStore {
        fn with(events: Vec<SessionEvent>) -> Self {
            Self {
                events: Mutex::new(events),
            }
        }
    }

    #[async_trait]
    impl EventSink for FakeStore {
        async fn append_runtime(&self, _ev: RuntimeEvent) -> Result<(), EventSinkError> {
            Ok(())
        }
        async fn append_session(&self, _ev: NewSessionEvent) -> Result<EventSeq, EventSinkError> {
            Err(EventSinkError::Internal("unused".into()))
        }
    }

    #[async_trait]
    impl EventStore for FakeStore {
        async fn list_session_events(
            &self,
            _session: SessionId,
            after: Option<EventSeq>,
            limit: usize,
        ) -> Result<Vec<SessionEvent>, StoreError> {
            let limit = clamp_events_page_limit(limit);
            let all = self.events.lock().unwrap();
            let start = after.map(|s| s.0 + 1).unwrap_or(0);
            Ok(all
                .iter()
                .filter(|e| e.seq.0 >= start)
                .take(limit)
                .cloned()
                .collect())
        }

        async fn replay_session<F>(
            &self,
            _session: SessionId,
            _on_event: F,
        ) -> Result<Option<EventSeq>, StoreError>
        where
            Self: Sized,
            F: FnMut(&SessionEvent) -> Result<(), StoreError> + Send,
        {
            Ok(None)
        }

        async fn last_seq(&self, _session: SessionId) -> Result<Option<EventSeq>, StoreError> {
            Ok(None)
        }

        async fn list_runtime_events(
            &self,
            _after_rowid: Option<i64>,
            _limit: usize,
        ) -> Result<Vec<(i64, RuntimeEvent)>, StoreError> {
            Ok(vec![])
        }

        async fn has_session_event_for_run(
            &self,
            _session: SessionId,
            _run: crate::types::ids::RunId,
            _type_: SessionEventType,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn has_run_accepted_event(
            &self,
            _run: crate::types::ids::RunId,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn has_run_finished_event(
            &self,
            _run: crate::types::ids::RunId,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn import_handoff_snapshot(
            &self,
            _snap: crate::events::HandoffSnapshot,
        ) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn pad(seq: u64, session: SessionId) -> SessionEvent {
        SessionEvent {
            seq: EventSeq(seq),
            ts: Timestamp::now(),
            session_id: session,
            run_id: None,
            type_: SessionEventType::Error,
            payload: serde_json::json!({}),
        }
    }

    fn decision(seq: u64, session: SessionId) -> SessionEvent {
        SessionEvent {
            seq: EventSeq(seq),
            ts: Timestamp::now(),
            session_id: session,
            run_id: None,
            type_: SessionEventType::Decision,
            payload: serde_json::json!({
                "kind": "retry",
                "metadata": {},
                "content_hash": null,
                "prompt_body": null
            }),
        }
    }

    #[tokio::test]
    async fn scan_budget_exhausted_empty_events_with_resume() {
        let session = SessionId::new();
        // Two full store pages of non-matching events (page size clamped from MAX_EVENTS_PAGE),
        // then a decision — with max_scan_pages=1 the first call returns empty + next_after.
        let mut events = Vec::new();
        for i in 0..MAX_EVENTS_PAGE {
            events.push(pad(i as u64, session));
        }
        events.push(decision(MAX_EVENTS_PAGE as u64, session));
        let store = FakeStore::with(events);

        let page = list_decision_events_bounded(&store, session, None, 10, 1)
            .await
            .unwrap();
        assert!(page.events.is_empty());
        assert_eq!(
            page.next_after,
            Some(EventSeq((MAX_EVENTS_PAGE as u64) - 1))
        );

        let page2 = list_decision_events_bounded(&store, session, page.next_after, 10, 1)
            .await
            .unwrap();
        assert_eq!(page2.events.len(), 1);
        assert_eq!(page2.events[0].type_, SessionEventType::Decision);
    }

    #[test]
    fn usage_unknown_consistency_parse() {
        let session = SessionId::new();
        let ev = SessionEvent {
            seq: EventSeq(0),
            ts: Timestamp::now(),
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
        let ev = SessionEvent {
            seq: EventSeq(1),
            ts: Timestamp::now(),
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
        assert_eq!(parsed.input_tokens, None);
        assert_eq!(parsed.output_tokens, Some(1));
        assert_eq!(parsed.model_tier, ModelTier::Standard);
        assert_eq!(parsed.provider_id, ProviderId::new("p").unwrap());
    }

    #[test]
    fn wrong_type_rejected() {
        let ev = SessionEvent {
            seq: EventSeq(0),
            ts: Timestamp::now(),
            session_id: SessionId::new(),
            run_id: None,
            type_: SessionEventType::ToolCall,
            payload: serde_json::json!({}),
        };
        assert!(parse_decision_event(&ev).is_err());
    }
}
