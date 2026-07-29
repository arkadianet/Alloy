//! RFC-0015 §8 — approval UX: the terminal path from an `ApprovalRequested`
//! event to `RunController::approve`.
//!
//! The prompt goes to `/dev/tty` when available so `--json > out.json`
//! stays interactive (GA9); when it cannot be opened, GA5 applies.
//!
//! Author: arkadianet

use std::io::{BufRead, BufReader, Write};

use alloy_runtime::{Approval, SessionEvent};
use serde_json::Value;

/// How `run` / `resume` answer gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Prompt on `/dev/tty` (GA2/GA3).
    Interactive,
    /// `--yes`: answer `Allow`, still printing the block (GA4).
    AutoAllow,
    /// `--no-input`: never prompt; gate becomes `EX_GATE_REQUIRED` (GA5).
    NoInput,
}

/// GA2 — the prompt block, in order: run id, gate id, node id, reason, the
/// most recent patch reference from the event log, accepted answers.
#[must_use]
pub fn render_block(
    run: &str,
    payload: &Value,
    latest_edit: Option<&SessionEvent>,
    gate_deadline: Option<std::time::Duration>,
) -> String {
    let gate = payload
        .get("gate_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let node = payload
        .get("node_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("(none)");
    let mut block = String::new();
    block.push_str("── approval required ────────────────────────────\n");
    block.push_str(&format!("run   {run}   gate {gate}   node {node}\n"));
    block.push_str(&format!("reason: {reason}\n"));
    if let Some(edit) = latest_edit {
        let artifact = edit
            .payload
            .get("patch_artifact")
            .or_else(|| edit.payload.get("artifact_id"))
            .and_then(Value::as_str)
            .unwrap_or("(see events)");
        block.push_str(&format!("patch: {artifact}\n"));
    }
    // GA7 — report the deadline; never enforce it (expiry is RFC-0010's).
    let timeout_ms = payload.get("timeout_ms").and_then(Value::as_u64);
    if let Some(ms) = timeout_ms.filter(|ms| *ms > 0) {
        block.push_str(&format!("deadline: {}s from request\n", ms / 1000));
    } else if let Some(d) = gate_deadline {
        block.push_str(&format!("deadline: {}s from request\n", d.as_secs()));
    }
    block.push_str("[y] allow  [o] allow once  [n] deny  [?] details\n");
    block.push_str("─────────────────────────────────────────────────\n");
    block
}

/// GA3/GA9 — prompt on `/dev/tty`. Returns `None` when the tty cannot be
/// opened or the reader hits EOF (both are GA5: treat as `--no-input`).
/// Blocking; call from `spawn_blocking`.
#[must_use]
pub fn prompt_via_tty(block: &str) -> Option<Approval> {
    let mut tty_out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let tty_in = std::fs::File::open("/dev/tty").ok()?;
    let mut reader = BufReader::new(tty_in);
    loop {
        tty_out.write_all(block.as_bytes()).ok()?;
        tty_out.write_all(b"> ").ok()?;
        tty_out.flush().ok()?;
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF → GA5.
        }
        match parse_answer(&line) {
            ParsedAnswer::Decision(a) => return Some(a),
            ParsedAnswer::Reprint => continue,
            ParsedAnswer::Unrecognized => {
                let _ = tty_out.write_all(b"unrecognized answer\n");
            }
        }
    }
}

/// One parsed prompt answer (GA3). No default-on-Enter.
#[derive(Debug, PartialEq, Eq)]
pub enum ParsedAnswer {
    /// A decision.
    Decision(Approval),
    /// `?` — reprint the block.
    Reprint,
    /// Anything else — reprompt.
    Unrecognized,
}

/// GA3 answer table: `y`/`a` allow, `o` allow-once, `n`/`d` deny, `?`
/// reprints; anything else (including empty) reprompts.
#[must_use]
pub fn parse_answer(line: &str) -> ParsedAnswer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "a" => ParsedAnswer::Decision(Approval::Allow),
        "o" => ParsedAnswer::Decision(Approval::AllowOnce),
        "n" | "d" => ParsedAnswer::Decision(Approval::Deny),
        "?" => ParsedAnswer::Reprint,
        _ => ParsedAnswer::Unrecognized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ga3_answers() {
        assert_eq!(parse_answer("y\n"), ParsedAnswer::Decision(Approval::Allow));
        assert_eq!(parse_answer("a"), ParsedAnswer::Decision(Approval::Allow));
        assert_eq!(
            parse_answer("o"),
            ParsedAnswer::Decision(Approval::AllowOnce)
        );
        assert_eq!(parse_answer("N"), ParsedAnswer::Decision(Approval::Deny));
        assert_eq!(parse_answer("d"), ParsedAnswer::Decision(Approval::Deny));
        assert_eq!(parse_answer("?"), ParsedAnswer::Reprint);
        // No default-on-Enter (GA3).
        assert_eq!(parse_answer(""), ParsedAnswer::Unrecognized);
        assert_eq!(parse_answer("yes please"), ParsedAnswer::Unrecognized);
    }

    #[test]
    fn ga2_block_renders_ids_and_reason() {
        let block = render_block(
            "run-1",
            &serde_json::json!({"gate_id": "g-1", "node_id": "n-1", "reason": "template gate"}),
            None,
            None,
        );
        assert!(block.contains("run-1"));
        assert!(block.contains("g-1"));
        assert!(block.contains("n-1"));
        assert!(block.contains("template gate"));
        assert!(block.contains("[y] allow"));
    }
}
