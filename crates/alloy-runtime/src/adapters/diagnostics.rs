//! rustc-JSON (NDJSON) diagnostics ingest and dedupe fingerprinting
//! (RFC-0010 §5.13.3-§5.13.4, DG1-DG8, FP1-FP5). Pure; no I/O.

use serde_json::Value;

use crate::types::diagnostic::{DiagnosticEvent, DiagnosticLevel, SpanRef};
use crate::types::ids::{DiagnosticId, Digest};

/// DG6: dedupe cap. A `Note`-level marker is appended when truncated.
const MAX_DIAGNOSTICS: usize = 200;

/// Fixed byte prefix versioning the fingerprint framing (FP5).
const FINGERPRINT_PREFIX: &[u8] = b"alloy.diag.v1";

/// Parse `cargo check --message-format=json` NDJSON output into
/// deduped, capped `DiagnosticEvent`s, preserving first-seen order.
///
/// Unparseable lines are skipped (not fatal) and reported once at `debug`
/// (DG2). Only `reason == "compiler-message"` objects contribute (DG1).
#[must_use]
pub fn parse_rustc_diagnostics(stdout_utf8: &str) -> Vec<DiagnosticEvent> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<DiagnosticEvent> = Vec::new();
    let mut unparseable: u32 = 0;
    let mut truncated = false;

    for raw_line in stdout_utf8.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                unparseable += 1;
                continue;
            }
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue; // DG1: not an unparseable line, just a different reason.
        }
        let Some(message) = value.get("message") else {
            unparseable += 1;
            continue;
        };
        let package = value
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string); // DG5

        let Some(event) = build_diagnostic_event(message, package.as_deref()) else {
            unparseable += 1;
            continue;
        };

        if !seen.insert(event.fingerprint.clone()) {
            continue; // DG6: dedupe by fingerprint, first-seen order kept.
        }
        if out.len() >= MAX_DIAGNOSTICS {
            truncated = true;
            continue;
        }
        out.push(event);
    }

    if unparseable > 0 {
        tracing::debug!(unparseable, "rustc diagnostics: skipped unparseable lines");
    }
    if truncated {
        out.push(truncation_marker());
    }
    out
}

fn truncation_marker() -> DiagnosticEvent {
    let message = format!("diagnostics truncated at {MAX_DIAGNOSTICS} (MAX_DIAGNOSTICS)");
    let fingerprint = diagnostic_fingerprint(None, DiagnosticLevel::Note, &message, None);
    DiagnosticEvent {
        id: DiagnosticId::new(),
        code: None,
        level: DiagnosticLevel::Note,
        message,
        spans: vec![],
        children: vec![],
        package: None,
        fingerprint,
        raw_json: None,
    }
}

/// Build one `DiagnosticEvent` from a rustc `message` object. Returns `None`
/// (DG2/DG3: skip, don't fail the node) when `level` doesn't map to one of
/// the four known levels or the `message` text field is missing.
fn build_diagnostic_event(message: &Value, package: Option<&str>) -> Option<DiagnosticEvent> {
    let level = parse_level(message.get("level").and_then(Value::as_str)?)?; // DG3
    let text = message.get("message").and_then(Value::as_str)?.to_string();
    let code = message
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let spans: Vec<SpanRef> = message
        .get("spans")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true)) // DG4
                .filter_map(parse_span)
                .collect()
        })
        .unwrap_or_default();
    let children = message
        .get("children")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| build_diagnostic_event(c, package))
                .collect()
        })
        .unwrap_or_default();

    let fingerprint = diagnostic_fingerprint(code.as_deref(), level, &text, spans.first()); // FP3

    Some(DiagnosticEvent {
        id: DiagnosticId::new(),
        code,
        level,
        message: text,
        spans,
        children,
        package: package.map(str::to_string),
        fingerprint,
        raw_json: Some(message.clone()), // DG8
    })
}

fn parse_level(s: &str) -> Option<DiagnosticLevel> {
    match s {
        "error" => Some(DiagnosticLevel::Error),
        "warning" => Some(DiagnosticLevel::Warning),
        "note" => Some(DiagnosticLevel::Note),
        "help" => Some(DiagnosticLevel::Help),
        _ => None, // DG3: anything else is skipped.
    }
}

fn parse_span(s: &Value) -> Option<SpanRef> {
    Some(SpanRef {
        path: s.get("file_name").and_then(Value::as_str)?.to_string(),
        start_line: u32::try_from(s.get("line_start").and_then(Value::as_u64)?).ok()?,
        start_col: u32::try_from(s.get("column_start").and_then(Value::as_u64)?).ok()?,
        end_line: u32::try_from(s.get("line_end").and_then(Value::as_u64)?).ok()?,
        end_col: u32::try_from(s.get("column_end").and_then(Value::as_u64)?).ok()?,
    })
}

fn level_str(level: DiagnosticLevel) -> &'static str {
    match level {
        DiagnosticLevel::Error => "error",
        DiagnosticLevel::Warning => "warning",
        DiagnosticLevel::Note => "note",
        DiagnosticLevel::Help => "help",
    }
}

/// Stable dedupe fingerprint (§5.13.4 framing, FP1-FP5).
///
/// Only the **first** primary span participates (FP3); nested `children`
/// never contribute. `0x00` separators are mandatory (FP2) so
/// `code="E05"` + `message="02x"` cannot collide with `code="E0502"` +
/// `message="x"`. Integers are little-endian `u32` (FP4); a missing span
/// contributes an empty path plus four zero integers.
#[must_use]
pub fn diagnostic_fingerprint(
    code: Option<&str>,
    level: DiagnosticLevel,
    message: &str,
    first_span: Option<&SpanRef>,
) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(FINGERPRINT_PREFIX);
    buf.push(0);
    buf.extend_from_slice(code.unwrap_or("").as_bytes());
    buf.push(0);
    buf.extend_from_slice(level_str(level).as_bytes());
    buf.push(0);
    buf.extend_from_slice(message.as_bytes());
    buf.push(0);
    let (path, sl, sc, el, ec) = match first_span {
        Some(s) => (
            s.path.as_str(),
            s.start_line,
            s.start_col,
            s.end_line,
            s.end_col,
        ),
        None => ("", 0, 0, 0, 0),
    };
    buf.extend_from_slice(path.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&sl.to_le_bytes());
    buf.extend_from_slice(&sc.to_le_bytes());
    buf.extend_from_slice(&el.to_le_bytes());
    buf.extend_from_slice(&ec.to_le_bytes());
    Digest::sha256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(path: &str, sl: u32, sc: u32, el: u32, ec: u32) -> SpanRef {
        SpanRef {
            path: path.into(),
            start_line: sl,
            start_col: sc,
            end_line: el,
            end_col: ec,
        }
    }

    fn compiler_message_line(
        level: &str,
        code: Option<&str>,
        message: &str,
        spans: Value,
        children: Value,
        target_name: &str,
    ) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "target": { "name": target_name, "kind": ["lib"] },
            "message": {
                "code": code.map(|c| serde_json::json!({"code": c})),
                "level": level,
                "message": message,
                "spans": spans,
                "children": children,
            }
        })
        .to_string()
    }

    // ---- parse_rustc_diagnostics ----

    #[test]
    fn parses_a_single_compiler_message_with_primary_span() {
        let line = compiler_message_line(
            "error",
            Some("E0308"),
            "mismatched types",
            serde_json::json!([
                {"file_name": "src/lib.rs", "line_start": 3, "column_start": 5,
                 "line_end": 3, "column_end": 10, "is_primary": true},
                {"file_name": "src/other.rs", "line_start": 1, "column_start": 1,
                 "line_end": 1, "column_end": 2, "is_primary": false},
            ]),
            serde_json::json!([]),
            "demo",
        );
        let out = parse_rustc_diagnostics(&line);
        assert_eq!(out.len(), 1);
        let d = &out[0];
        assert_eq!(d.code.as_deref(), Some("E0308"));
        assert_eq!(d.level, DiagnosticLevel::Error);
        assert_eq!(d.message, "mismatched types");
        assert_eq!(d.package.as_deref(), Some("demo"));
        assert_eq!(d.spans, vec![span("src/lib.rs", 3, 5, 3, 10)]); // DG4: only is_primary
    }

    #[test]
    fn dg1_ignores_non_compiler_message_reasons() {
        let line = serde_json::json!({"reason": "compiler-artifact"}).to_string();
        assert!(parse_rustc_diagnostics(&line).is_empty());
    }

    #[test]
    fn dg2_skips_unparseable_lines_without_failing() {
        let good = compiler_message_line(
            "warning",
            None,
            "unused",
            serde_json::json!([]),
            serde_json::json!([]),
            "demo",
        );
        let stream = format!("not json at all\n{good}\n{{\"broken\":\n{good}");
        let out = parse_rustc_diagnostics(&stream);
        assert_eq!(out.len(), 1, "both good lines dedupe to one fingerprint");
    }

    #[test]
    fn dg3_maps_all_four_levels_and_skips_unknown() {
        for (input, expected) in [
            ("error", Some(DiagnosticLevel::Error)),
            ("warning", Some(DiagnosticLevel::Warning)),
            ("note", Some(DiagnosticLevel::Note)),
            ("help", Some(DiagnosticLevel::Help)),
            ("error: internal compiler error", None),
        ] {
            let line = compiler_message_line(
                input,
                None,
                "msg",
                serde_json::json!([]),
                serde_json::json!([]),
                "demo",
            );
            let out = parse_rustc_diagnostics(&line);
            match expected {
                Some(level) => {
                    assert_eq!(out.first().map(|d| d.level), Some(level), "input={input}")
                }
                None => assert!(out.is_empty(), "input={input} must be skipped"),
            }
        }
    }

    #[test]
    fn dg5_package_from_enclosing_target_name() {
        let line = compiler_message_line(
            "note",
            None,
            "msg",
            serde_json::json!([]),
            serde_json::json!([]),
            "my-crate",
        );
        let out = parse_rustc_diagnostics(&line);
        assert_eq!(out[0].package.as_deref(), Some("my-crate"));
    }

    #[test]
    fn dg6_dedupes_by_fingerprint_preserving_first_seen_order() {
        let a = compiler_message_line(
            "error",
            Some("E0001"),
            "first",
            serde_json::json!([]),
            serde_json::json!([]),
            "demo",
        );
        let b = compiler_message_line(
            "error",
            Some("E0002"),
            "second",
            serde_json::json!([]),
            serde_json::json!([]),
            "demo",
        );
        let stream = format!("{a}\n{b}\n{a}\n"); // a repeated
        let out = parse_rustc_diagnostics(&stream);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].code.as_deref(), Some("E0001"));
        assert_eq!(out[1].code.as_deref(), Some("E0002"));
    }

    #[test]
    fn dg6_caps_at_max_diagnostics_with_note_marker() {
        let mut stream = String::new();
        for i in 0..(MAX_DIAGNOSTICS + 5) {
            stream.push_str(&compiler_message_line(
                "error",
                Some(&format!("E{i:04}")),
                &format!("distinct message {i}"),
                serde_json::json!([]),
                serde_json::json!([]),
                "demo",
            ));
            stream.push('\n');
        }
        let out = parse_rustc_diagnostics(&stream);
        assert_eq!(out.len(), MAX_DIAGNOSTICS + 1, "capped plus one marker");
        let marker = out.last().unwrap();
        assert_eq!(marker.level, DiagnosticLevel::Note);
        assert!(marker.message.contains("truncated"));
    }

    #[test]
    fn children_are_parsed_but_do_not_count_as_top_level_diagnostics() {
        let line = compiler_message_line(
            "error",
            Some("E0308"),
            "parent",
            serde_json::json!([]),
            serde_json::json!([{
                "level": "help",
                "message": "child hint",
                "spans": [],
                "children": [],
            }]),
            "demo",
        );
        let out = parse_rustc_diagnostics(&line);
        assert_eq!(
            out.len(),
            1,
            "child does not become a separate top-level diagnostic"
        );
        assert_eq!(out[0].children.len(), 1);
        assert_eq!(out[0].children[0].level, DiagnosticLevel::Help);
        assert_eq!(out[0].children[0].message, "child hint");
    }

    #[test]
    fn dg8_raw_json_carries_original_message() {
        let line = compiler_message_line(
            "error",
            Some("E0308"),
            "boom",
            serde_json::json!([]),
            serde_json::json!([]),
            "demo",
        );
        let out = parse_rustc_diagnostics(&line);
        let raw = out[0].raw_json.as_ref().unwrap();
        assert_eq!(raw["message"], "boom");
    }

    #[test]
    fn missing_message_field_is_skipped_not_fatal() {
        let line = serde_json::json!({"reason": "compiler-message"}).to_string();
        assert!(parse_rustc_diagnostics(&line).is_empty());
    }

    // ---- diagnostic_fingerprint (FP1-FP5) ----

    #[test]
    fn fp2_framing_prevents_code_message_boundary_collision() {
        let a = diagnostic_fingerprint(Some("E05"), DiagnosticLevel::Error, "02x", None);
        let b = diagnostic_fingerprint(Some("E0502"), DiagnosticLevel::Error, "x", None);
        assert_ne!(a, b, "0x00 framing must prevent this collision");
    }

    #[test]
    fn fp3_only_first_primary_span_participates() {
        let s1 = span("a.rs", 1, 1, 1, 2);
        let s2 = span("b.rs", 9, 9, 9, 9);
        let with_first = diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Error, "m", Some(&s1));
        let with_first_again =
            diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Error, "m", Some(&s1));
        let with_second =
            diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Error, "m", Some(&s2));
        assert_eq!(with_first, with_first_again);
        assert_ne!(with_first, with_second);
    }

    #[test]
    fn fp4_missing_span_is_empty_path_plus_zero_integers() {
        let none_span = diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Error, "m", None);
        let zero_span = diagnostic_fingerprint(
            Some("E1"),
            DiagnosticLevel::Error,
            "m",
            Some(&span("", 0, 0, 0, 0)),
        );
        assert_eq!(none_span, zero_span);
    }

    #[test]
    fn level_sensitivity_changes_fingerprint() {
        let e = diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Error, "m", None);
        let w = diagnostic_fingerprint(Some("E1"), DiagnosticLevel::Warning, "m", None);
        assert_ne!(e, w);
    }

    #[test]
    fn missing_code_and_empty_string_code_fingerprint_identically() {
        // The name has to match the assertion. `None` and `Some("")` frame
        // identically (each contributes an empty byte run before the
        // separator), so this pins an equality, not a difference — the old
        // name claimed the opposite of what the body checks.
        let none_code = diagnostic_fingerprint(None, DiagnosticLevel::Error, "m", None);
        let empty_code = diagnostic_fingerprint(Some(""), DiagnosticLevel::Error, "m", None);
        assert_eq!(none_code, empty_code);
    }
}
