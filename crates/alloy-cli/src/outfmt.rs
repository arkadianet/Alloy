//! RFC-0015 §7.7 — output rendering: JSON envelope (OUT2/OUT3) and one-line
//! human event rendering through the typed payload parsers (OUT5).
//!
//! Author: arkadianet

use alloy_runtime::{
    parse_decision_event, parse_model_call_event, parse_tool_call_event, RuntimeConfig,
    SessionEvent, SessionEventType,
};
use serde_json::{json, Value};

use crate::errx::Exit;

/// JSON schema tag; changes only on a breaking shape change (OUT3).
pub const SCHEMA: &str = "alloy.cli/v1";

/// PR4 — resolution is reported, not guessed.
#[must_use]
pub fn config_echo(cfg: &RuntimeConfig) -> Value {
    json!({
        "data_dir": cfg.data_dir.display().to_string(),
        "data_dir_rule": cfg.data_dir_rule,
        "profile_path": cfg.profile_path.display().to_string(),
        "router_path": cfg.router_path.display().to_string(),
    })
}

/// OUT3 — the one JSON document a non-streaming subcommand emits.
#[must_use]
pub fn envelope(command: &str, exit: Exit, cfg: Option<&RuntimeConfig>, extra: Value) -> Value {
    let mut doc = json!({
        "schema": SCHEMA,
        "command": command,
        "ok": exit == Exit::Ok,
        "exit_code": exit.code(),
        "exit_name": exit.name(),
    });
    if let Some(cfg) = cfg {
        doc["config"] = config_echo(cfg);
    }
    if let Value::Object(extra) = extra {
        let obj = doc.as_object_mut().expect("envelope is an object");
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    doc
}

/// OUT5 — `<seq>  <ts>  <type>  <summary>`; summaries come from typed
/// parsers and fall back to the type name, never a raw JSON dump.
#[must_use]
pub fn event_line(ev: &SessionEvent) -> String {
    // RFC3339 via the Timestamp serde impl — no direct `time` dependency (T9).
    let ts = serde_json::to_value(&ev.ts)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!(
        "{:>6}  {}  {:<18}  {}",
        ev.seq.0,
        ts,
        type_name(ev.type_),
        summary(ev)
    )
}

/// Wire-ish snake-case name for a [`SessionEventType`].
#[must_use]
pub fn type_name(t: SessionEventType) -> &'static str {
    match t {
        SessionEventType::SessionCreated => "session_created",
        SessionEventType::GoalSubmitted => "goal_submitted",
        SessionEventType::PlanProduced => "plan_produced",
        SessionEventType::NodeState => "node_state",
        SessionEventType::Decision => "decision",
        SessionEventType::ModelCall => "model_call",
        SessionEventType::ToolCall => "tool_call",
        SessionEventType::EditApplied => "edit_applied",
        SessionEventType::ApprovalRequested => "approval_requested",
        SessionEventType::ApprovalResolved => "approval_resolved",
        SessionEventType::BudgetWarning => "budget_warning",
        SessionEventType::ReplanRequested => "replan_requested",
        SessionEventType::ReplanResumed => "replan_resumed",
        SessionEventType::RunCompleted => "run_completed",
        SessionEventType::Error => "error",
    }
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// One-line summary. Typed parsers first (OUT5); known payload keys for the
/// scheduler/control-plane types; the bare type name otherwise.
#[must_use]
pub fn summary(ev: &SessionEvent) -> String {
    let p = &ev.payload;
    match ev.type_ {
        SessionEventType::Decision => parse_decision_event(ev)
            .map(|d| format!("{:?}", d.kind))
            .unwrap_or_else(|_| type_name(ev.type_).to_owned()),
        SessionEventType::ModelCall => parse_model_call_event(ev)
            .map(|m| {
                format!(
                    "tier {:?} in={} out={}",
                    m.model_tier,
                    m.input_tokens.unwrap_or(0),
                    m.output_tokens.unwrap_or(0)
                )
            })
            .unwrap_or_else(|_| type_name(ev.type_).to_owned()),
        SessionEventType::ToolCall => parse_tool_call_event(ev)
            .map(|t| format!("{} denied={}", t.tool_name, t.denied))
            .unwrap_or_else(|_| type_name(ev.type_).to_owned()),
        SessionEventType::NodeState => match (str_field(p, "node_id"), str_field(p, "to")) {
            (Some(node), Some(to)) => {
                let from = str_field(p, "from").unwrap_or("?");
                format!("node {node} {from} -> {to}")
            }
            _ => type_name(ev.type_).to_owned(),
        },
        SessionEventType::ApprovalRequested => match str_field(p, "gate_id") {
            Some(gate) => format!(
                "gate {gate} node {} reason: {}",
                str_field(p, "node_id").unwrap_or("?"),
                str_field(p, "reason").unwrap_or("(none)")
            ),
            None => type_name(ev.type_).to_owned(),
        },
        SessionEventType::ApprovalResolved => match str_field(p, "gate_id") {
            Some(gate) => format!(
                "gate {gate} decision: {}",
                str_field(p, "decision").unwrap_or("?")
            ),
            None => type_name(ev.type_).to_owned(),
        },
        SessionEventType::RunCompleted => match str_field(p, "dag_state") {
            Some(state) => match str_field(p, "reason") {
                Some(reason) => format!("dag_state {state} ({reason})"),
                None => format!("dag_state {state}"),
            },
            None => type_name(ev.type_).to_owned(),
        },
        SessionEventType::BudgetWarning => str_field(p, "message")
            .map(str::to_owned)
            .unwrap_or_else(|| type_name(ev.type_).to_owned()),
        SessionEventType::Error => match str_field(p, "class") {
            Some(class) => format!("class {class}"),
            None => type_name(ev.type_).to_owned(),
        },
        SessionEventType::PlanProduced => match str_field(p, "template_id") {
            Some(t) => format!(
                "template {t} nodes {}",
                p.get("node_ids")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            ),
            None => type_name(ev.type_).to_owned(),
        },
        SessionEventType::SessionCreated
        | SessionEventType::GoalSubmitted
        | SessionEventType::EditApplied
        | SessionEventType::ReplanRequested
        | SessionEventType::ReplanResumed => type_name(ev.type_).to_owned(),
    }
}

/// JSONL form of an event (OUT2). Serializes the stored envelope as-is; the
/// obs layer already applied retention/redaction (OUT4).
#[must_use]
pub fn event_json(ev: &SessionEvent) -> Value {
    serde_json::to_value(ev).unwrap_or_else(|_| json!({"seq": ev.seq.0}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{EventSeq, SessionId, Timestamp};

    fn ev(type_: SessionEventType, payload: Value) -> SessionEvent {
        SessionEvent {
            seq: EventSeq(7),
            ts: Timestamp::now(),
            session_id: SessionId::new(),
            run_id: None,
            type_,
            payload,
        }
    }

    /// OUT5 — a payload that does not parse falls back to the type name,
    /// never a raw JSON dump.
    #[test]
    fn unparseable_payload_falls_back_to_type_name() {
        let e = ev(SessionEventType::NodeState, json!({"garbage": {"a": 1}}));
        let line = event_line(&e);
        assert!(line.contains("node_state"));
        assert!(!line.contains("garbage"));
    }

    #[test]
    fn node_state_summary_is_typed() {
        let e = ev(
            SessionEventType::NodeState,
            json!({"node_id": "n1", "from": "ready", "to": "running"}),
        );
        assert!(summary(&e).contains("ready -> running"));
    }

    #[test]
    fn envelope_carries_schema_and_exit() {
        let doc = envelope("cancel", Exit::Ok, None, json!({"run": "x"}));
        assert_eq!(doc["schema"], SCHEMA);
        assert_eq!(doc["command"], "cancel");
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["exit_code"], 0);
        assert_eq!(doc["run"], "x");
    }
}
