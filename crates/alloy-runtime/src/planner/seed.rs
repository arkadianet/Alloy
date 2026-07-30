//! SD1–SD10 replan-seed projection (RFC-0017 §5.4).
//!
//! A [`FailureIr`] is never seeded verbatim into a new generation's root
//! envelope: `DiagnosticEvent.raw_json` is unbounded, unredacted tool JSON
//! (SD10/SEC7), so the seed carries [`SeedDiagnostic`] projections instead —
//! `raw_json` dropped structurally (the type has no such field), `children`
//! flattened to depth 1, counts capped, strings secret-redacted and truncated
//! on UTF-8 boundaries, and the whole payload bounded (SD9). The caps and the
//! redaction reuse the shipped seams (SD9a): [`redact_secrets`] and
//! [`truncate_utf8_bytes`] from `obs::redact`, and the RFC-0013 OC7 bounds
//! (4 KiB strings / 64 KiB total).

use serde::{Deserialize, Serialize};

use crate::obs::{redact_secrets, truncate_utf8_bytes};
use crate::types::diagnostic::{DiagnosticEvent, DiagnosticLevel, ErrorClass, FailureIr, SpanRef};
use crate::types::ids::{DiagnosticId, Digest};

/// SD9(c): flattened `children` entries per seed diagnostic.
pub(crate) const MAX_SEED_CHILDREN: usize = 8;
/// SD9(d): spans per seed diagnostic.
pub(crate) const MAX_SEED_SPANS: usize = 32;
/// SD9(d): diagnostics per seed payload.
pub(crate) const MAX_SEED_DIAGNOSTICS: usize = 64;
/// SD9(f): per-string byte bound (RFC-0013 OC7 string bound).
pub(crate) const MAX_SEED_STRING_BYTES: usize = 4 * 1024;
/// SD9(g): serialized seed payload bound (RFC-0013 OC7 total bound).
pub(crate) const MAX_SEED_PAYLOAD_BYTES: usize = 64 * 1024;

/// Uninhabited element type for [`SeedChild::children`]: the array can only
/// ever be empty, so SD9(c)'s depth-1 flatten stays structural while the
/// serialized child still carries the `children: []` key that
/// [`DiagnosticEvent`]'s strict decode requires (SD4 consumability).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum SeedNever {}

/// Depth-1 child of a [`SeedDiagnostic`] (SD9(c)): deeper nesting is not
/// representable ([`SeedNever`] is uninhabited) and no `raw_json` exists.
///
/// Serializes to a shape [`DiagnosticEvent`] decodes (SD4): the shipped
/// `diagnostics_from_payloads` consumer parses seed diagnostics strictly, so
/// `id`, `fingerprint`, and the (always empty) `children` key are retained
/// on the wire even though SD9(b)'s keep-list does not name them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SeedChild {
    /// Diagnostic id (opaque; retained for [`DiagnosticEvent`] decode).
    pub id: DiagnosticId,
    /// Optional error code.
    pub code: Option<String>,
    /// Severity.
    pub level: DiagnosticLevel,
    /// Sanitized message (SD9(e)/(f)).
    pub message: String,
    /// Related spans (capped, SD9(d)).
    pub spans: Vec<SpanRef>,
    /// Stable fingerprint for dedupe.
    pub fingerprint: Digest,
    /// Always empty (structural — the element type is uninhabited).
    pub children: Vec<SeedNever>,
}

/// Sanitized projection of a [`DiagnosticEvent`] (SD9). Deliberately a
/// *narrower* type: no `raw_json` field exists, so the compiler enforces
/// SEC7's "never seeded verbatim".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SeedDiagnostic {
    /// Diagnostic id (opaque; retained for [`DiagnosticEvent`] decode — SD4).
    pub id: DiagnosticId,
    /// Optional error code (`E0308`, …).
    pub code: Option<String>,
    /// Severity.
    pub level: DiagnosticLevel,
    /// Sanitized message.
    pub message: String,
    /// Related spans (≤ [`MAX_SEED_SPANS`]).
    pub spans: Vec<SpanRef>,
    /// Optional package name.
    pub package: Option<String>,
    /// Stable fingerprint for dedupe.
    pub fingerprint: Digest,
    /// Flattened depth-1 children (≤ [`MAX_SEED_CHILDREN`]).
    pub children: Vec<SeedChild>,
}

/// Seed predecessor payload body (SD3/SD4): the verify success shape's
/// `{ ok, diagnostics }` mirror with `ok: false`, minus `raw_artifact`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SeedPayload {
    /// Always `false` for a seed (the predecessor failed).
    pub ok: bool,
    /// Failure class from the [`FailureIr`].
    pub error_class: ErrorClass,
    /// True when any diagnostic, child, span, or string was dropped or
    /// truncated by the SD9 projection.
    pub truncated: bool,
    /// Sanitized diagnostics (≤ [`MAX_SEED_DIAGNOSTICS`]).
    pub diagnostics: Vec<SeedDiagnostic>,
    /// Sanitized failure notes (RFC-0010 F4 constrains these; SD9 re-redacts
    /// and bounds them anyway).
    pub notes: String,
}

/// Sanitize one string per SD9(e)/(f): secrets redacted, then truncated at
/// [`MAX_SEED_STRING_BYTES`] on a UTF-8 boundary. Returns the sanitized
/// string and whether truncation occurred.
fn sanitize_string(value: &str) -> (String, bool) {
    let redacted = redact_secrets(value);
    let truncated = truncate_utf8_bytes(&redacted, MAX_SEED_STRING_BYTES);
    let was_truncated = truncated.len() < redacted.len();
    (truncated, was_truncated)
}

/// Bound-only truncation for non-prose strings (paths, codes): no redaction,
/// same byte cap.
fn bound_string(value: &str) -> (String, bool) {
    let truncated = truncate_utf8_bytes(value, MAX_SEED_STRING_BYTES);
    let was_truncated = truncated.len() < value.len();
    (truncated, was_truncated)
}

fn project_spans(spans: &[SpanRef], truncated: &mut bool) -> Vec<SpanRef> {
    if spans.len() > MAX_SEED_SPANS {
        *truncated = true;
    }
    spans
        .iter()
        .take(MAX_SEED_SPANS)
        .map(|s| {
            let (path, cut) = bound_string(&s.path);
            if cut {
                *truncated = true;
            }
            SpanRef {
                path,
                start_line: s.start_line,
                start_col: s.start_col,
                end_line: s.end_line,
                end_col: s.end_col,
            }
        })
        .collect()
}

/// Depth-first flatten of the nested `children` tree into depth-1
/// [`SeedChild`] entries, capped at [`MAX_SEED_CHILDREN`] (SD9(c)).
fn flatten_children(children: &[DiagnosticEvent], out: &mut Vec<SeedChild>, truncated: &mut bool) {
    for child in children {
        if out.len() >= MAX_SEED_CHILDREN {
            *truncated = true;
            return;
        }
        let (message, cut) = sanitize_string(&child.message);
        if cut {
            *truncated = true;
        }
        let code = child.code.as_deref().map(|c| {
            let (code, cut) = bound_string(c);
            if cut {
                *truncated = true;
            }
            code
        });
        out.push(SeedChild {
            id: child.id,
            code,
            level: child.level,
            message,
            spans: project_spans(&child.spans, truncated),
            fingerprint: child.fingerprint.clone(),
            children: Vec::new(),
        });
        flatten_children(&child.children, out, truncated);
    }
}

/// True when the diagnostic projects to nothing worth seeding: no code, an
/// empty message, no spans, and no children — the shape of a
/// [`DiagnosticEvent`] whose only content was `raw_json` (GN4's "checking
/// post-projection matters").
fn projected_is_empty(d: &SeedDiagnostic) -> bool {
    d.code.is_none() && d.message.trim().is_empty() && d.spans.is_empty() && d.children.is_empty()
}

fn project_diagnostic(d: &DiagnosticEvent, truncated: &mut bool) -> SeedDiagnostic {
    let (message, cut) = sanitize_string(&d.message);
    if cut {
        *truncated = true;
    }
    let code = d.code.as_deref().map(|c| {
        let (code, cut) = bound_string(c);
        if cut {
            *truncated = true;
        }
        code
    });
    let package = d.package.as_deref().map(|p| {
        let (package, cut) = bound_string(p);
        if cut {
            *truncated = true;
        }
        package
    });
    let mut children = Vec::new();
    flatten_children(&d.children, &mut children, truncated);
    SeedDiagnostic {
        id: d.id,
        code,
        level: d.level,
        message,
        spans: project_spans(&d.spans, truncated),
        package,
        fingerprint: d.fingerprint.clone(),
        children,
    }
}

/// SD9: project a [`FailureIr`] into the sanitized [`SeedPayload`]. Pure and
/// sync; the persistence layer serializes the result into the seed
/// predecessor envelope (SD3), and GN4 admits a repair generation only when
/// `diagnostics` is non-empty here.
pub(crate) fn project_failure(f: &FailureIr) -> SeedPayload {
    let mut truncated = false;

    if f.diagnostics.len() > MAX_SEED_DIAGNOSTICS {
        truncated = true;
    }
    let mut diagnostics = Vec::new();
    for d in f.diagnostics.iter().take(MAX_SEED_DIAGNOSTICS) {
        let projected = project_diagnostic(d, &mut truncated);
        if projected_is_empty(&projected) {
            // A diagnostic whose only content was `raw_json` projects to
            // nothing; dropping it is a drop in SD9's sense.
            truncated = true;
            continue;
        }
        diagnostics.push(projected);
    }

    let (notes, cut) = sanitize_string(&f.notes);
    if cut {
        truncated = true;
    }

    let mut payload = SeedPayload {
        ok: false,
        error_class: f.error_class,
        truncated,
        diagnostics,
        notes,
    };

    // SD9(g): cap the whole serialized payload, dropping whole trailing
    // diagnostics (never truncating one mid-structure) until it fits.
    while !payload.diagnostics.is_empty() && serialized_len(&payload) > MAX_SEED_PAYLOAD_BYTES {
        payload.diagnostics.pop();
        payload.truncated = true;
    }
    payload
}

fn serialized_len(payload: &SeedPayload) -> usize {
    // An owned struct of bounded strings cannot fail to serialize; a defect
    // here must not panic the planner, so treat it as "over the cap".
    serde_json::to_vec(payload)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
}

/// GN4 seam: true when the SD9 projection of `f` yields **no** diagnostics —
/// no diagnostics, no seed, no bump (read by `GenerationDriver` admission).
pub(crate) fn seed_projection_is_empty(f: &FailureIr) -> bool {
    project_failure(f).diagnostics.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::DiagnosticId;
    use crate::types::ids::NodeId;

    fn digest() -> Digest {
        crate::obs::hash_prompt("seed-test")
    }

    fn diag(message: &str) -> DiagnosticEvent {
        DiagnosticEvent {
            id: DiagnosticId::new(),
            code: Some("E0308".into()),
            level: DiagnosticLevel::Error,
            message: message.into(),
            spans: vec![SpanRef {
                path: "src/lib.rs".into(),
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 2,
            }],
            children: vec![],
            package: Some("alloy-runtime".into()),
            fingerprint: digest(),
            raw_json: None,
        }
    }

    fn failure(diags: Vec<DiagnosticEvent>) -> FailureIr {
        FailureIr {
            node: NodeId::new(),
            error_class: ErrorClass::Compile,
            retry: Default::default(),
            diagnostics: diags,
            notes: "cargo check failed".into(),
        }
    }

    /// AC 20b / AC 39: a `raw_json` sentinel never reaches the seed bytes,
    /// and no `raw_json` key exists in the serialized payload.
    #[test]
    fn ac20b_raw_json_sentinel_absent_from_seed_bytes() {
        let mut d = diag("mismatched types");
        d.raw_json = Some(serde_json::json!({ "secret_sentinel": "RAWJSON_SENTINEL_XYZ" }));
        let payload = project_failure(&failure(vec![d]));
        let bytes = serde_json::to_vec(&payload).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("RAWJSON_SENTINEL_XYZ"));
        assert!(!text.contains("raw_json"));
        assert!(
            !payload.truncated,
            "dropping raw_json is structural, not a truncation"
        );
    }

    /// AC 20b: a secret-bearing message is redacted.
    #[test]
    fn ac20b_secret_in_message_redacted() {
        let d = diag("leaked api_key=sk-abcdefghij in build");
        let payload = project_failure(&failure(vec![d]));
        assert!(payload.diagnostics[0].message.contains("[REDACTED]"));
        assert!(!payload.diagnostics[0].message.contains("sk-abcdefghij"));
    }

    /// AC 20b: 200 diagnostics cap to 64 with `truncated: true`.
    #[test]
    fn ac20b_diagnostic_count_caps_at_64() {
        let diags = (0..200).map(|i| diag(&format!("err {i}"))).collect();
        let payload = project_failure(&failure(diags));
        assert_eq!(payload.diagnostics.len(), MAX_SEED_DIAGNOSTICS);
        assert!(payload.truncated);
    }

    /// AC 20b: a 1 MiB multibyte message truncates to ≤ 4 KiB on a UTF-8
    /// boundary.
    #[test]
    fn ac20b_huge_message_truncates_on_utf8_boundary() {
        let big = "é".repeat(512 * 1024);
        let payload = project_failure(&failure(vec![diag(&big)]));
        let m = &payload.diagnostics[0].message;
        assert!(m.len() <= MAX_SEED_STRING_BYTES);
        assert!(m.is_char_boundary(m.len()));
        assert!(payload.truncated);
    }

    /// AC 20b: children nested 5 deep flatten to depth 1, at most 8 entries.
    #[test]
    fn ac20b_children_flatten_to_depth_one() {
        let mut leaf = diag("leaf");
        for i in 0..5 {
            let mut parent = diag(&format!("level {i}"));
            parent.children = vec![leaf];
            leaf = parent;
        }
        let mut root = diag("root");
        root.children = vec![leaf];
        let payload = project_failure(&failure(vec![root]));
        let children = &payload.diagnostics[0].children;
        assert!(!children.is_empty());
        assert!(children.len() <= MAX_SEED_CHILDREN);
        // Depth 1 is structural: SeedChild's children element type is
        // uninhabited, so the serialized array is always empty.
        let value = serde_json::to_value(children).unwrap();
        for child in value.as_array().unwrap() {
            assert_eq!(child["children"], serde_json::json!([]));
        }
    }

    /// SD4 consumability: every serialized seed diagnostic (children
    /// included) decodes as a strict [`DiagnosticEvent`], because the shipped
    /// `diagnostics_from_payloads` consumer parses exactly that type — and
    /// the decoded event carries no `raw_json`.
    #[test]
    fn seed_diagnostics_decode_as_diagnostic_events() {
        let mut root = diag("outer");
        root.children = vec![diag("inner")];
        let payload = project_failure(&failure(vec![root]));
        let value = serde_json::to_value(&payload).unwrap();
        for item in value["diagnostics"].as_array().unwrap() {
            let event: DiagnosticEvent = serde_json::from_value(item.clone())
                .expect("seed diagnostic must decode as DiagnosticEvent (SD4)");
            assert!(event.raw_json.is_none());
            assert!(!event.children.is_empty() || event.message == "inner");
        }
    }

    #[test]
    fn ac20b_child_overflow_marks_truncated() {
        let mut root = diag("root");
        root.children = (0..20).map(|i| diag(&format!("child {i}"))).collect();
        let payload = project_failure(&failure(vec![root]));
        assert_eq!(payload.diagnostics[0].children.len(), MAX_SEED_CHILDREN);
        assert!(payload.truncated);
    }

    /// AC 20c (projection half): a diagnostic whose only content was
    /// `raw_json` projects to empty, and the GN4 helper reports it.
    #[test]
    fn ac20c_raw_json_only_failure_projects_empty() {
        let d = DiagnosticEvent {
            id: DiagnosticId::new(),
            code: None,
            level: DiagnosticLevel::Error,
            message: String::new(),
            spans: vec![],
            children: vec![],
            package: None,
            fingerprint: digest(),
            raw_json: Some(serde_json::json!({ "everything": "here" })),
        };
        let f = failure(vec![d]);
        assert!(seed_projection_is_empty(&f));
        assert!(project_failure(&f).diagnostics.is_empty());
    }

    #[test]
    fn span_count_caps_at_32() {
        let mut d = diag("many spans");
        d.spans = (0..50)
            .map(|i| SpanRef {
                path: format!("src/f{i}.rs"),
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 2,
            })
            .collect();
        let payload = project_failure(&failure(vec![d]));
        assert_eq!(payload.diagnostics[0].spans.len(), MAX_SEED_SPANS);
        assert!(payload.truncated);
    }

    /// SD9(g): oversized total payload drops whole trailing diagnostics.
    #[test]
    fn total_payload_caps_at_64kib() {
        let big = "x".repeat(4000);
        let diags = (0..64).map(|_| diag(&big)).collect();
        let payload = project_failure(&failure(diags));
        assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_SEED_PAYLOAD_BYTES);
        assert!(payload.diagnostics.len() < 64);
        assert!(!payload.diagnostics.is_empty());
        assert!(payload.truncated);
    }

    #[test]
    fn clean_failure_is_not_truncated() {
        let payload = project_failure(&failure(vec![diag("plain error")]));
        assert!(!payload.truncated);
        assert_eq!(payload.diagnostics.len(), 1);
        assert!(!payload.ok);
        assert_eq!(payload.error_class, ErrorClass::Compile);
    }
}
