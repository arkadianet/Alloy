//! Model-response extraction (RFC-0013 §7.4, rules PS1–PS10) and the local
//! unified-diff → `PatchSet` parser (rule EW4).
//!
//! Free text from a model is never used directly: every LLM worker extracts
//! one JSON object here, then validates it against its own
//! `deny_unknown_fields` schema. Local diff parsing means an unusable diff
//! never becomes a permission-denied tool error.

use serde_json::Value;

use crate::edit::{FilePatch, Hunk, PatchSet};
use crate::obs::hash_content;
use crate::router::ModelResponse;
use crate::types::ids::Digest;

/// PS10: bodies larger than this are rejected without a full parse attempt.
pub(crate) const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// EW4: max files per model-authored patch.
pub(crate) const MAX_PATCH_FILES: usize = 64;
/// EW4: max hunks per file in a model-authored patch.
pub(crate) const MAX_HUNKS_PER_FILE: usize = 256;

/// How the JSON object was obtained (recorded in the decision log, OB3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonSource {
    /// `ModelResponse.structured` (PS1).
    Structured,
    /// First ```json fenced block in the text body (PS2).
    FencedBlock,
    /// The trimmed whole body (PS3).
    WholeBody,
}

impl JsonSource {
    /// Stable label for decision metadata.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::FencedBlock => "fenced_block",
            Self::WholeBody => "whole_body",
        }
    }
}

/// The single shape every LLM worker extracts before schema validation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtractedJson {
    /// Extracted JSON object.
    pub value: Value,
    /// How it was obtained.
    pub source: JsonSource,
    /// Digest of the raw response body (PS9/OB4) — never the body itself.
    pub raw_digest: Digest,
}

/// Terminal extraction outcomes that are not schema errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtractError {
    /// PS7: model refusal — `Model` / `NonRetryable`.
    Refusal,
    /// PS8: `finish_reason == "length"` — `Model` / `Retryable`.
    Truncated,
    /// PS4: nothing parseable — candidate for the PS6 repair turn.
    Unparseable(String),
}

fn raw_body_digest(resp: &ModelResponse) -> Digest {
    if let Some(text) = &resp.text {
        return hash_content(text.as_bytes());
    }
    if let Some(structured) = &resp.structured {
        let bytes = serde_json::to_vec(structured).unwrap_or_default();
        return hash_content(&bytes);
    }
    hash_content(&[])
}

/// Extract one JSON object per PS1 → PS2 → PS3 → PS4, screening refusals
/// (PS7) and truncation (PS8) first.
pub(crate) fn extract_json(resp: &ModelResponse) -> Result<ExtractedJson, ExtractError> {
    let raw_digest = raw_body_digest(resp);

    match resp.finish_reason.as_deref() {
        Some("length") => return Err(ExtractError::Truncated),
        Some("content_filter") | Some("refusal") => return Err(ExtractError::Refusal),
        _ => {}
    }

    // PS10: allocation-bounded extraction.
    if resp
        .text
        .as_ref()
        .is_some_and(|t| t.len() > MAX_RESPONSE_BYTES)
    {
        return Err(ExtractError::Unparseable(format!(
            "response body exceeds {MAX_RESPONSE_BYTES} bytes"
        )));
    }

    // PS1.
    if let Some(structured) = &resp.structured {
        if let Value::Object(obj) = structured {
            if obj.get("refusal").is_some_and(|v| !v.is_null()) {
                return Err(ExtractError::Refusal);
            }
            return Ok(ExtractedJson {
                value: structured.clone(),
                source: JsonSource::Structured,
                raw_digest,
            });
        }
    }

    let Some(text) = &resp.text else {
        return Err(ExtractError::Unparseable("empty response body".into()));
    };

    // PS2: first ```json fenced block.
    if let Some(block) = first_json_fence(text) {
        if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(block) {
            if value.get("refusal").is_some_and(|v| !v.is_null()) {
                return Err(ExtractError::Refusal);
            }
            return Ok(ExtractedJson {
                value,
                source: JsonSource::FencedBlock,
                raw_digest,
            });
        }
    }

    // PS3: the trimmed whole body.
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(text.trim()) {
        if value.get("refusal").is_some_and(|v| !v.is_null()) {
            return Err(ExtractError::Refusal);
        }
        return Ok(ExtractedJson {
            value,
            source: JsonSource::WholeBody,
            raw_digest,
        });
    }

    // PS4: prose, apologies, and empty bodies all land here.
    Err(ExtractError::Unparseable(
        "no JSON object in response".into(),
    ))
}

fn first_json_fence(text: &str) -> Option<&str> {
    let open = text.find("```json")?;
    let rest = &text[open + "```json".len()..];
    let close = rest.find("```")?;
    Some(rest[..close].trim())
}

/// PS5 path screen: jail-relative, `/`-separated, no leading `/`, no `..`,
/// no drive prefix, no NUL, no backslash (mirrors the RFC-0006 host check).
#[must_use]
pub(crate) fn is_jail_relative(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    path.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Parse a model-authored unified diff into a validated [`PatchSet`] (EW4).
///
/// Enforced locally, before any tool call: jail-relative paths only, no
/// rename/copy/binary hunks, ≤ [`MAX_PATCH_FILES`] files and
/// ≤ [`MAX_HUNKS_PER_FILE`] hunks per file. `/dev/null` on the old side is a
/// `Create`, on the new side a `Delete`.
pub(crate) fn parse_model_diff(diff: &str) -> Result<PatchSet, String> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = diff.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.trim().is_empty()
        {
            continue;
        }
        if line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("copy from")
            || line.starts_with("copy to")
        {
            return Err("rename/copy hunks are not allowed".into());
        }
        if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
            return Err("binary hunks are not allowed".into());
        }
        let Some(old_header) = line.strip_prefix("--- ") else {
            return Err(format!("unexpected line outside a hunk: {line:.80}"));
        };
        let Some(new_line) = lines.next() else {
            return Err("diff ends after old-file header".into());
        };
        let Some(new_header) = new_line.strip_prefix("+++ ") else {
            return Err("old-file header not followed by new-file header".into());
        };
        let old_path = strip_diff_path(old_header);
        let new_path = strip_diff_path(new_header);

        let mut hunks: Vec<Hunk> = Vec::new();
        while lines.peek().is_some_and(|l| l.starts_with("@@")) {
            let header = lines.next().expect("peeked");
            let mut hunk = parse_hunk_header(header)?;
            // Parse the body by *structure*, not by the header's declared
            // counts: local models routinely mis-count, and trusting the
            // header desynced the file loop into "unexpected line outside a
            // hunk" (dogfood, 2026-07-29). A body line is any ` `/`-`/`+`
            // sigil line; a `--- ` line is a deletion in the body unless the
            // *next* line is a `+++ ` header (which is what the count rule
            // existed to disambiguate). Counts are recomputed from the body;
            // the header's numbers are treated as a hint only.
            let mut body: Vec<String> = Vec::new();
            let (mut seen_old, mut seen_new) = (0u32, 0u32);
            while let Some(&l) = lines.peek() {
                if l.starts_with("@@") {
                    break;
                }
                if l.starts_with("--- ") {
                    // A file entry is always `--- ` + `+++ ` + `@@ ` (the
                    // parser rejects entries without hunks), so anything
                    // less — e.g. a body pair deleting `-- old` and adding
                    // `++ new` — stays hunk content.
                    let mut ahead = lines.clone();
                    ahead.next();
                    if ahead.next().is_some_and(|n| n.starts_with("+++ "))
                        && ahead.peek().is_some_and(|n| n.starts_with("@@"))
                    {
                        break; // next file entry, not a deleted body line
                    }
                }
                match l.as_bytes().first() {
                    Some(b' ') => {
                        seen_old += 1;
                        seen_new += 1;
                    }
                    Some(b'-') => seen_old += 1,
                    Some(b'+') => seen_new += 1,
                    _ => break, // not a body line; outer structure resumes
                }
                body.push(l.to_owned());
                lines.next();
            }
            if seen_old == 0 && seen_new == 0 {
                return Err("empty hunk body".into());
            }
            hunk.old_lines = seen_old;
            hunk.new_lines = seen_new;
            // Optional trailing no-newline marker(s).
            while lines
                .peek()
                .is_some_and(|l| l.starts_with("\\ No newline at end of file"))
            {
                lines.next();
                match body.last().map(|s: &String| s.as_bytes().first().copied()) {
                    Some(Some(b'+')) => hunk.eof_newline = false,
                    Some(Some(b'-')) => hunk.old_eof_no_newline = true,
                    Some(Some(b' ')) => {
                        hunk.eof_newline = false;
                        hunk.old_eof_no_newline = true;
                    }
                    _ => return Err("misplaced no-newline marker".into()),
                }
            }
            if body.is_empty() {
                return Err("empty hunk body".into());
            }
            hunk.lines = body;
            hunks.push(hunk);
            if hunks.len() > MAX_HUNKS_PER_FILE {
                return Err(format!("more than {MAX_HUNKS_PER_FILE} hunks in one file"));
            }
        }
        if hunks.is_empty() {
            return Err("file entry without hunks".into());
        }

        let patch = match (old_path.as_deref(), new_path.as_deref()) {
            (None, Some(path)) => {
                require_jail_relative(path)?;
                FilePatch::Create {
                    path: path.to_owned(),
                    hunks,
                }
            }
            (Some(path), None) => {
                require_jail_relative(path)?;
                FilePatch::Delete {
                    path: path.to_owned(),
                    validation_hunks: hunks,
                }
            }
            (Some(old), Some(new)) => {
                if old != new {
                    return Err("old and new paths differ (rename is not allowed)".into());
                }
                require_jail_relative(new)?;
                FilePatch::Modify {
                    path: new.to_owned(),
                    hunks,
                }
            }
            (None, None) => return Err("both sides are /dev/null".into()),
        };
        files.push(patch);
        if files.len() > MAX_PATCH_FILES {
            return Err(format!("more than {MAX_PATCH_FILES} files in one patch"));
        }
    }

    if files.is_empty() {
        return Err("empty diff".into());
    }
    Ok(PatchSet { files })
}

fn require_jail_relative(path: &str) -> Result<(), String> {
    if is_jail_relative(path) {
        Ok(())
    } else {
        Err(format!("path is not jail-relative: {path:.120}"))
    }
}

/// `/dev/null` → `None`; strips the conventional `a/` / `b/` prefix and any
/// trailing tab-metadata.
fn strip_diff_path(header: &str) -> Option<String> {
    let raw = header.split('\t').next().unwrap_or(header).trim();
    if raw == "/dev/null" {
        return None;
    }
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    Some(stripped.to_owned())
}

fn parse_hunk_header(header: &str) -> Result<Hunk, String> {
    // `@@ -old_start[,old_lines] +new_start[,new_lines] @@[ context]`
    let inner = header
        .strip_prefix("@@ -")
        .ok_or_else(|| format!("bad hunk header: {header:.80}"))?;
    let (old_part, rest) = inner
        .split_once(" +")
        .ok_or_else(|| format!("bad hunk header: {header:.80}"))?;
    let (new_part, _) = rest
        .split_once(" @@")
        .ok_or_else(|| format!("bad hunk header: {header:.80}"))?;
    let (old_start, old_lines) = parse_range(old_part)?;
    let (new_start, new_lines) = parse_range(new_part)?;
    Ok(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
        eof_newline: true,
        old_eof_no_newline: false,
    })
}

fn parse_range(range: &str) -> Result<(u32, u32), String> {
    let (start, count) = match range.split_once(',') {
        Some((s, c)) => (s, c),
        None => (range, "1"),
    };
    let start: u32 = start
        .parse()
        .map_err(|_| format!("bad hunk range: {range:.40}"))?;
    let count: u32 = count
        .parse()
        .map_err(|_| format!("bad hunk range: {range:.40}"))?;
    Ok((start, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Usage;

    fn resp(text: Option<&str>, structured: Option<Value>, finish: Option<&str>) -> ModelResponse {
        ModelResponse {
            text: text.map(String::from),
            structured,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
            },
            provider_request_id: None,
            finish_reason: finish.map(String::from),
        }
    }

    #[test]
    fn parse_extracts_structured_then_fenced_then_whole_body() {
        // PS1.
        let e = extract_json(&resp(
            Some("ignored"),
            Some(serde_json::json!({"a": 1})),
            Some("stop"),
        ))
        .unwrap();
        assert_eq!(e.source, JsonSource::Structured);

        // PS2.
        let e = extract_json(&resp(
            Some("prose\n```json\n{\"a\": 2}\n```\nmore"),
            None,
            Some("stop"),
        ))
        .unwrap();
        assert_eq!(e.source, JsonSource::FencedBlock);
        assert_eq!(e.value["a"], 2);

        // PS3.
        let e = extract_json(&resp(Some("  {\"a\": 3} "), None, None)).unwrap();
        assert_eq!(e.source, JsonSource::WholeBody);

        // PS4.
        assert!(matches!(
            extract_json(&resp(Some("I am sorry, I cannot."), None, None)),
            Err(ExtractError::Unparseable(_))
        ));
        assert!(matches!(
            extract_json(&resp(None, None, None)),
            Err(ExtractError::Unparseable(_))
        ));
    }

    #[test]
    fn parse_refusal_is_detected_from_field_and_finish_reason() {
        // PS7.
        assert_eq!(
            extract_json(&resp(
                None,
                Some(serde_json::json!({"refusal": "no"})),
                Some("stop"),
            )),
            Err(ExtractError::Refusal)
        );
        assert_eq!(
            extract_json(&resp(Some("{}"), None, Some("content_filter"))),
            Err(ExtractError::Refusal)
        );
    }

    #[test]
    fn parse_truncated_finish_reason_is_reported() {
        // PS8.
        assert_eq!(
            extract_json(&resp(Some("{\"a\":"), None, Some("length"))),
            Err(ExtractError::Truncated)
        );
    }

    #[test]
    fn parse_rejects_body_over_256_kib_without_full_parse() {
        // PS10.
        let big = "x".repeat(MAX_RESPONSE_BYTES + 1);
        assert!(matches!(
            extract_json(&resp(Some(&big), None, None)),
            Err(ExtractError::Unparseable(_))
        ));
    }

    #[test]
    fn jail_relative_screen_rejects_escapes() {
        // PS5.
        assert!(is_jail_relative("src/lib.rs"));
        assert!(!is_jail_relative("/etc/passwd"));
        assert!(!is_jail_relative("../x.rs"));
        assert!(!is_jail_relative("a/../../x.rs"));
        assert!(!is_jail_relative("C:/win"));
        assert!(!is_jail_relative("a\\b"));
        assert!(!is_jail_relative(""));
        assert!(!is_jail_relative("./a.rs"));
    }

    const GOOD_DIFF: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-let x: &str = 1;\n+let x: i32 = 1;\n context\n";

    #[test]
    fn unified_diff_parses_a_simple_modify() {
        let set = parse_model_diff(GOOD_DIFF).unwrap();
        assert_eq!(set.files.len(), 1);
        let FilePatch::Modify { path, hunks } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].lines.len(), 3);
        assert_eq!(hunks[0].lines[0], "-let x: &str = 1;");
    }

    #[test]
    fn unified_diff_parses_create_and_delete() {
        let create = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+fn f() {}\n";
        let set = parse_model_diff(create).unwrap();
        assert!(matches!(&set.files[0], FilePatch::Create { path, .. } if path == "src/new.rs"));

        let delete = "--- a/src/old.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-fn f() {}\n";
        let set = parse_model_diff(delete).unwrap();
        assert!(matches!(&set.files[0], FilePatch::Delete { path, .. } if path == "src/old.rs"));
    }

    #[test]
    fn unified_diff_parse_rejects_rename_binary_and_dotdot_paths() {
        // EW4.
        assert!(parse_model_diff("rename from a\nrename to b\n").is_err());
        assert!(parse_model_diff("Binary files a and b differ\n").is_err());
        assert!(parse_model_diff(
            "--- a/../escape.rs\n+++ b/../escape.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n"
        )
        .is_err());
        assert!(parse_model_diff("--- a/x.rs\n+++ b/y.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n").is_err());
        assert!(parse_model_diff("").is_err());
    }

    #[test]
    fn unified_diff_no_newline_markers_set_hunk_flags() {
        let diff =
            "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let set = parse_model_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert!(!hunks[0].eof_newline);
        assert!(!hunks[0].old_eof_no_newline);
    }

    /// Dogfood finding (2026-07-29, qwen2.5-coder:14b): local models
    /// routinely mis-count hunk headers. The body is parsed by structure
    /// (sigil lines) with counts recomputed, so an off-by-one header no
    /// longer desyncs the file loop into "unexpected line outside a hunk".
    #[test]
    fn tolerates_wrong_hunk_counts_by_recomputing_from_body() {
        // Header claims 3/3; body actually has 4 old / 4 new lines.
        let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -2,3 +2,3 @@ fn main() {\n fn main() {\n-    let x: i32 = \"no\";\n+    let x: i32 = 0;\n     println!();\n }\n";
        let set = parse_model_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 4);
        assert_eq!(hunks[0].new_lines, 4);
        assert_eq!(hunks[0].lines.len(), 5);
    }

    /// A second `@@` hunk is recognized even when the first header's counts
    /// were wrong (previously the leftover lines fell to the outer loop).
    #[test]
    fn tolerates_wrong_counts_before_second_hunk() {
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n z\n@@ -9,1 +9,1 @@\n-p\n+q\n";
        let set = parse_model_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_lines, 2);
        assert_eq!(hunks[0].new_lines, 2);
    }

    /// The count-exactness rule existed to disambiguate deleted lines that
    /// themselves begin with `---`. Structure parsing keeps that safe: a
    /// `--- ` line only ends the hunk when the *next* line is a `+++ `
    /// header.
    #[test]
    fn deletion_line_starting_with_dashes_stays_in_body() {
        let diff = "--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,1 @@\n---- separator\n context\n+ context\n";
        // (body: deletion of "--- separator", context stays.)
        let set = parse_model_diff(diff).unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks[0].lines.len(), 3);
    }

    /// A body pair `--- old` / `+++ new` (deleting `-- old`, adding
    /// `++ new`) is hunk content, not a file boundary: a real file entry is
    /// always followed by an `@@` header (the parser rejects entries
    /// without hunks), so only a pair followed by `@@` ends the hunk.
    #[test]
    fn body_dash_plus_pair_without_hunk_header_stays_in_body() {
        let diff = "--- a/f.md\n+++ b/f.md\n@@ -1,2 +1,2 @@\n--- old\n+++ new\n context\n";
        let set = parse_model_diff(diff).unwrap();
        assert_eq!(set.files.len(), 1);
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines, vec!["--- old", "+++ new", " context"]);
    }
}
