//! Conversation domain builder (RFC-0012 §4.2, Appendix F).
//!
//! Reads the tail window of the session event log and renders each admitted
//! event from its Appendix F payload pointers. Total (rule D4): failures
//! degrade, they never abort assembly.

use crate::storage::EventStore;
use crate::types::ids::{ArtifactId, Digest, EventSeq, SessionId};

use super::render::{bound_bytes, sanitize_line};
use super::types::{Degradation, DegradationReason, DomainId};

/// A reference from an admitted conversation event into the artifact store
/// (feeds the Artifacts candidate set, §4.4).
#[derive(Debug, Clone)]
pub(super) enum EventArtifactRef {
    /// `EditApplied` `/patch_artifact_id`.
    Id(ArtifactId),
    /// `Decision` `/content_hash`, resolved via `get_by_digest`.
    ContentDigest(Digest),
}

/// One rendered history line, keyed by its sequence number (D13).
#[derive(Debug, Clone)]
pub(super) struct EventLine {
    /// Event sequence.
    pub seq: EventSeq,
    /// Sanitised one-line rendering.
    pub line: String,
}

/// Raw Conversation inputs before clamping.
#[derive(Debug, Default)]
pub(super) struct ConversationRaw {
    /// Latest `GoalSubmitted` `/goal/text` in the window, sanitised (D3).
    pub goal_from_events: Option<String>,
    /// Admitted history lines, ascending `EventSeq` (D13).
    pub events: Vec<EventLine>,
    /// Artifact references discovered in admitted events (§4.4).
    pub artifact_refs: Vec<EventArtifactRef>,
    /// Admitted-type events skipped for a missing/mistyped pointer
    /// (Appendix F) — counted as omitted, never guessed.
    pub skipped_malformed: usize,
    /// Store failures, as degradations (E1).
    pub degradations: Vec<Degradation>,
}

/// Fetch the last `max_events` raw events and render the admitted ones
/// (§4.2). The window is computed from the tail via `last_seq`, never by
/// paging from zero.
pub(super) async fn fetch(
    store: &dyn EventStore,
    session: SessionId,
    max_events: usize,
) -> ConversationRaw {
    let mut raw = ConversationRaw::default();
    let last = match store.last_seq(session).await {
        Ok(Some(last)) => last,
        Ok(None) => return raw,
        Err(e) => {
            raw.degradations.push(store_degradation(&e.to_string()));
            return raw;
        }
    };
    // Exclusive cursor: `after = Some(s)` yields `seq > s`. When the session
    // is shorter than the window, `None` keeps `EventSeq(0)` inside it.
    let after = if last.0 >= max_events as u64 {
        Some(EventSeq(last.0 - max_events as u64))
    } else {
        None
    };
    let page = match store
        .list_session_events(session, after, max_events.max(1))
        .await
    {
        Ok(page) => page,
        Err(e) => {
            raw.degradations.push(store_degradation(&e.to_string()));
            return raw;
        }
    };

    for event in &page {
        use crate::events::SessionEventType as T;
        let payload = &event.payload;
        match event.type_ {
            T::GoalSubmitted => {
                // The goal frame, not a history line; latest wins (D3).
                match pointer_str(payload, "/goal/text") {
                    Some(text) => raw.goal_from_events = Some(sanitize_goal(text)),
                    None => raw.skipped_malformed += 1,
                }
            }
            T::Decision => {
                let Some(kind) = pointer_str(payload, "/kind") else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                let summary = payload
                    .pointer("/metadata")
                    .map(|m| bound_bytes(&sanitize_line(&compact_json(m)), 200))
                    .unwrap_or_default();
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: format!("decision {}: {summary}", sanitize_line(kind)),
                });
                if let Some(hash) = pointer_str(payload, "/content_hash") {
                    if let Ok(digest) = Digest::try_from_hex(hash) {
                        raw.artifact_refs
                            .push(EventArtifactRef::ContentDigest(digest));
                    }
                }
            }
            T::ApprovalRequested => {
                let Some(gate) = pointer_str(payload, "/gate_id") else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: format!("approval requested: gate {}", sanitize_line(gate)),
                });
            }
            T::ApprovalResolved => {
                let (Some(gate), Some(decision)) = (
                    pointer_str(payload, "/gate_id"),
                    pointer_str(payload, "/decision"),
                ) else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: format!(
                        "approval resolved: gate {} — {}",
                        sanitize_line(gate),
                        sanitize_line(decision)
                    ),
                });
            }
            T::EditApplied => {
                let (Some(txn), Some(files)) = (
                    pointer_str(payload, "/transaction_id"),
                    payload.pointer("/files_touched").and_then(|v| v.as_array()),
                ) else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: format!("edit {}: {} files", sanitize_line(txn), files.len()),
                });
                if let Some(id) = pointer_str(payload, "/patch_artifact_id") {
                    if let Ok(id) = ArtifactId::parse(id) {
                        raw.artifact_refs.push(EventArtifactRef::Id(id));
                    }
                }
            }
            T::BudgetWarning => {
                let Some(message) = pointer_str(payload, "/message") else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: format!("budget warning: {}", sanitize_line(message)),
                });
            }
            T::Error => {
                let Some(class) = pointer_str(payload, "/class") else {
                    raw.skipped_malformed += 1;
                    continue;
                };
                let message = pointer_str(payload, "/message").unwrap_or_default();
                let line = format!("error {}: {}", sanitize_line(class), sanitize_line(message));
                raw.events.push(EventLine {
                    seq: event.seq,
                    line: bound_bytes(&line, 400),
                });
            }
            // Excluded by rule D16: runtime telemetry and the largest,
            // least-safe payload bodies in the log.
            _ => {}
        }
    }
    raw.events.sort_by_key(|e| e.seq);
    raw
}

/// Sanitise the pinned goal text (SEC2; D3 caps enforcement is the
/// budgeter's, not the fetcher's).
pub(super) fn sanitize_goal(text: &str) -> String {
    super::render::sanitize_untrusted(text)
}

fn store_degradation(detail: &str) -> Degradation {
    Degradation {
        domain: DomainId::Conversation,
        reason: DegradationReason::StoreUnavailable,
        detail: bound_bytes(&sanitize_line(detail), 200),
    }
}

fn pointer_str<'a>(payload: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    payload.pointer(pointer).and_then(|v| v.as_str())
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
