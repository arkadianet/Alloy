//! Model-response extraction (RFC-0013 §7.4, rules PS1–PS10) and the local
//! unified-diff → `PatchSet` parser (rule EW4).
//!
//! Free text from a model is never used directly: every LLM worker extracts
//! one JSON object here, then validates it against its own
//! `deny_unknown_fields` schema. Local diff parsing means an unusable diff
//! never becomes a permission-denied tool error.

use std::collections::HashMap;

use serde::Deserialize;
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

/// AM-0013-1: max ops in one `ops` response (mirrors [`MAX_HUNKS_PER_FILE`] —
/// every op compiles to exactly one hunk).
pub(crate) const MAX_OPS_PER_RESPONSE: usize = 256;
/// AM-0013-1: max total lines carried across all `expect`/`new` arrays
/// (mirrors the tool backend's `MAX_LINES_PER_HUNK`).
pub(crate) const MAX_OPS_TOTAL_LINES: usize = 10_000;
/// AM-0013-1: max total bytes across all `expect`/`new` line content
/// (mirrors EW5's `MAX_ARGUMENT_BYTES`; the compiled `PatchSet` is still
/// re-checked against the real EW5 bound in the worker).
pub(crate) const MAX_OPS_TOTAL_BYTES: usize = 64 * 1024;

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

// --- line ops (AM-0013-1) -----------------------------------------------

/// `replace_lines`: replace the 1-based inclusive range `start..=end` with
/// `new`. `expect` must list the current content of every replaced line —
/// the honesty guard standing in for a diff's deleted/context lines.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceLinesOp {
    /// Jail-relative target path.
    pub path: String,
    /// First replaced line (1-based, as shown in the excerpt gutter).
    pub start: u32,
    /// Last replaced line (inclusive).
    pub end: u32,
    /// Current content of lines `start..=end`, verbatim, without newlines.
    pub expect: Vec<String>,
    /// Replacement lines (no trailing newlines).
    pub new: Vec<String>,
}

/// `insert_lines`: insert `new` after 1-based line `after_line`
/// (`0` inserts before the first line).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InsertLinesOp {
    /// Jail-relative target path.
    pub path: String,
    /// Line the insertion follows (0 = top of file).
    pub after_line: u32,
    /// Inserted lines (no trailing newlines).
    pub new: Vec<String>,
}

/// `delete_lines`: delete the 1-based inclusive range `start..=end`.
/// `expect` must list the current content of every deleted line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteLinesOp {
    /// Jail-relative target path.
    pub path: String,
    /// First deleted line (1-based).
    pub start: u32,
    /// Last deleted line (inclusive).
    pub end: u32,
    /// Current content of lines `start..=end`, verbatim.
    pub expect: Vec<String>,
}

/// One model-authored line operation (AM-0013-1). Line numbers are 1-based
/// and refer to the CURRENT file content — the same numbers the working-set
/// excerpt gutter shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineOp {
    /// Replace a line range.
    Replace(ReplaceLinesOp),
    /// Insert after a line.
    Insert(InsertLinesOp),
    /// Delete a line range.
    Delete(DeleteLinesOp),
}

impl LineOp {
    /// Jail-relative target path of this op.
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Replace(op) => &op.path,
            Self::Insert(op) => &op.path,
            Self::Delete(op) => &op.path,
        }
    }
}

/// Parse one op object (PS5-strict: the `op` tag selects a
/// `deny_unknown_fields` schema; serde's internally tagged enums cannot
/// enforce that, so the dispatch is manual).
pub(crate) fn parse_line_op(value: &Value) -> Result<LineOp, String> {
    let Value::Object(obj) = value else {
        return Err("op is not a JSON object".into());
    };
    let Some(tag) = obj.get("op").and_then(Value::as_str) else {
        return Err("op object without a string \"op\" tag".into());
    };
    let mut body = obj.clone();
    body.remove("op");
    let body = Value::Object(body);
    match tag {
        "replace_lines" => serde_json::from_value::<ReplaceLinesOp>(body)
            .map(LineOp::Replace)
            .map_err(|e| format!("replace_lines: {e}")),
        "insert_lines" => serde_json::from_value::<InsertLinesOp>(body)
            .map(LineOp::Insert)
            .map_err(|e| format!("insert_lines: {e}")),
        "delete_lines" => serde_json::from_value::<DeleteLinesOp>(body)
            .map(LineOp::Delete)
            .map_err(|e| format!("delete_lines: {e}")),
        other => Err(format!("unknown op: {other:.40}")),
    }
}

fn screen_op_lines(lines: &[String]) -> Result<(), String> {
    for line in lines {
        if line.contains('\n') || line.contains('\0') {
            return Err("op lines must not contain newlines or NUL".into());
        }
    }
    Ok(())
}

/// Static screen over a parsed `ops` array, before any file is read: paths
/// jail-relative, ranges well-formed, `expect` sized to its range, and the
/// EW4/EW5-mirroring caps ([`MAX_OPS_PER_RESPONSE`], [`MAX_PATCH_FILES`]
/// distinct paths, [`MAX_OPS_TOTAL_LINES`], [`MAX_OPS_TOTAL_BYTES`]).
pub(crate) fn screen_line_ops(ops: &[LineOp]) -> Result<(), String> {
    if ops.is_empty() {
        return Err("ops array is empty".into());
    }
    if ops.len() > MAX_OPS_PER_RESPONSE {
        return Err(format!("more than {MAX_OPS_PER_RESPONSE} ops"));
    }
    let mut paths: Vec<&str> = Vec::new();
    let mut total_lines = 0usize;
    let mut total_bytes = 0usize;
    for op in ops {
        require_jail_relative(op.path())?;
        if !paths.contains(&op.path()) {
            paths.push(op.path());
        }
        let range_check = |start: u32, end: u32, expect: &[String]| -> Result<(), String> {
            if start == 0 {
                return Err("line numbers are 1-based; start must be >= 1".into());
            }
            if end < start {
                return Err(format!("end {end} is before start {start}"));
            }
            let span = usize::try_from(end - start + 1).unwrap_or(usize::MAX);
            if expect.len() != span {
                return Err(format!(
                    "expect lists {} line(s) but start..=end covers {span}; \
                     expect must repeat every current line in the range",
                    expect.len()
                ));
            }
            Ok(())
        };
        let line_arrays: Vec<&[String]> = match op {
            LineOp::Replace(op) => {
                range_check(op.start, op.end, &op.expect)?;
                if op.new.is_empty() {
                    return Err("replace_lines with empty new — use delete_lines".into());
                }
                vec![&op.expect, &op.new]
            }
            LineOp::Insert(op) => {
                if op.new.is_empty() {
                    return Err("insert_lines with empty new".into());
                }
                vec![&op.new]
            }
            LineOp::Delete(op) => {
                range_check(op.start, op.end, &op.expect)?;
                vec![&op.expect]
            }
        };
        for lines in line_arrays {
            screen_op_lines(lines)?;
            total_lines = total_lines.saturating_add(lines.len());
            total_bytes = total_bytes.saturating_add(lines.iter().map(String::len).sum::<usize>());
        }
    }
    if paths.len() > MAX_PATCH_FILES {
        return Err(format!("ops touch more than {MAX_PATCH_FILES} files"));
    }
    if total_lines > MAX_OPS_TOTAL_LINES {
        return Err(format!("ops carry more than {MAX_OPS_TOTAL_LINES} lines"));
    }
    if total_bytes > MAX_OPS_TOTAL_BYTES {
        return Err(format!("ops carry more than {MAX_OPS_TOTAL_BYTES} bytes"));
    }
    Ok(())
}

/// Split `text` the way the apply backend does: an empty file has no lines,
/// and a trailing `\n` is a file property rather than an empty last line.
fn split_current_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), true);
    }
    let eof_newline = text.ends_with('\n');
    let body = if eof_newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    (body.split('\n').collect(), eof_newline)
}

/// Compile screened line ops against the CURRENT file contents into a
/// context-correct [`PatchSet`] (AM-0013-1). Pure: `files` maps each
/// jail-relative path named by `ops` to its current text.
///
/// Guarantees on success: hunks per file are in ascending, non-overlapping
/// `old_start` order with `new_start` satisfying the backend's delta rule;
/// every `-` line was verified against the file (`expect`), so a compiled
/// hunk can only fail downstream if the file changes afterwards. The
/// trailing-newline property of a touched file is preserved. Errors are
/// model-repairable feedback strings (stale `expect`, out-of-range lines,
/// overlapping ops).
pub(crate) fn ops_to_patchset(
    ops: &[LineOp],
    files: &HashMap<String, String>,
) -> Result<PatchSet, String> {
    let mut order: Vec<&str> = Vec::new();
    let mut by_path: HashMap<&str, Vec<&LineOp>> = HashMap::new();
    for op in ops {
        let path = op.path();
        if !by_path.contains_key(path) {
            order.push(path);
        }
        by_path.entry(path).or_default().push(op);
    }
    if order.is_empty() {
        return Err("ops array is empty".into());
    }
    let mut file_patches = Vec::with_capacity(order.len());
    for path in order {
        let content = files
            .get(path)
            .ok_or_else(|| format!("no current content for {path}"))?;
        let hunks = compile_file_ops(path, content, &by_path[path])?;
        file_patches.push(FilePatch::Modify {
            path: path.to_owned(),
            hunks,
        });
    }
    Ok(PatchSet {
        files: file_patches,
    })
}

/// 0-based boundary an op sits at, for ordering: a range op consumes from
/// `start - 1`; an insert sits at the `after_line` boundary. Inserts sort
/// before a range starting at the same boundary (the backend accepts that
/// order and rejects the reverse as an overlap).
fn op_sort_key(op: &LineOp) -> (u32, u8) {
    match op {
        LineOp::Insert(op) => (op.after_line, 0),
        LineOp::Replace(ReplaceLinesOp { start, .. })
        | LineOp::Delete(DeleteLinesOp { start, .. }) => (start.saturating_sub(1), 1),
    }
}

fn verify_expect(path: &str, start: u32, expect: &[String], lines: &[&str]) -> Result<(), String> {
    for (offset, want) in expect.iter().enumerate() {
        let number = start as usize + offset;
        let actual = lines[number - 1];
        if actual != want {
            return Err(format!(
                "stale op: {path}:{number} is {actual:?} but the op expected {want:?}; \
                 the file differs from what you saw — re-read the numbered excerpt \
                 and re-anchor the op"
            ));
        }
    }
    Ok(())
}

fn compile_file_ops(path: &str, content: &str, ops: &[&LineOp]) -> Result<Vec<Hunk>, String> {
    let (lines, eof_newline) = split_current_lines(content);
    let total = u32::try_from(lines.len()).map_err(|_| format!("{path} is too large"))?;
    let mut sorted: Vec<&LineOp> = ops.to_vec();
    sorted.sort_by_key(|op| op_sort_key(op));

    let mut hunks: Vec<Hunk> = Vec::with_capacity(sorted.len());
    let mut cursor: u32 = 0; // old lines consumed (0-based boundary).
    let mut delta: i64 = 0; // new-side minus old-side lines so far.
    let mut last_insert_boundary: Option<u32> = None;
    for op in sorted {
        let hunk = match op {
            LineOp::Insert(op) => {
                if op.after_line > total {
                    return Err(format!(
                        "insert_lines after_line {} is beyond {path}, which has {total} \
                         line(s); the file may differ from what you saw",
                        op.after_line
                    ));
                }
                if op.after_line < cursor || last_insert_boundary == Some(op.after_line) {
                    return Err(format!(
                        "ops overlap at {path} line {}",
                        op.after_line.saturating_add(1)
                    ));
                }
                if op.after_line == 0 {
                    // The backend reserves `old_start == 0` for Create (its
                    // V8b rule), so a pure zero-length-range prepend hunk is
                    // unrepresentable. A top-of-file insert instead takes
                    // git's default prepend shape and anchors on line 1 as
                    // trailing context: `@@ -1,1 +1,N+1 @@`, `+new` lines
                    // first, then ` line1`. An empty file has no line to
                    // anchor on (and the backend rejects every Modify shape
                    // for it), so that case is repairable feedback.
                    let Some(first) = lines.first() else {
                        return Err(format!(
                            "insert_lines after_line 0: {path} is empty and the edit \
                             backend cannot line-edit an empty file; delete it and \
                             recreate it with the full content via a unified diff patch"
                        ));
                    };
                    cursor = 1;
                    last_insert_boundary = Some(0);
                    let new_start = 1 + delta;
                    let new_count = op.new.len().saturating_add(1);
                    delta += i64::try_from(op.new.len()).unwrap_or(i64::MAX);
                    let mut body: Vec<String> = op.new.iter().map(|l| format!("+{l}")).collect();
                    body.push(format!(" {first}"));
                    Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: u32::try_from(new_start)
                            .map_err(|_| format!("op positions overflow in {path}"))?,
                        new_lines: u32::try_from(new_count)
                            .map_err(|_| format!("too many lines in one op on {path}"))?,
                        lines: body,
                        // The context anchor is the last old line iff the
                        // file has exactly one line, and it stays last on
                        // the new side, carrying the eof property with it.
                        eof_newline: if total == 1 { eof_newline } else { true },
                        old_eof_no_newline: total == 1 && !eof_newline,
                    }
                } else {
                    cursor = op.after_line;
                    // `insert_lines` is the only op with no `expect`, so
                    // nothing ties it to the file it was authored against.
                    // Re-applying one across repair generations silently
                    // duplicated whole items — the measured cause of most
                    // E0428 "defined multiple times" failures. If the text is
                    // already there, the edit is a duplicate, not progress.
                    if !op.new.is_empty() {
                        let at = op.after_line as usize;
                        let already_present =
                            lines.get(at..at + op.new.len()).is_some_and(|follows| {
                                follows.iter().zip(&op.new).all(|(a, b)| *a == b.as_str())
                            });
                        if already_present {
                            return Err(format!(
                                "insert at {path} line {} would duplicate text that is \
                                 already there — re-read the file; if it already has \
                                 your change, do not re-send it",
                                op.after_line
                            ));
                        }
                    }
                    last_insert_boundary = Some(op.after_line);
                    let new_start = i64::from(op.after_line) + delta + 1;
                    delta += i64::try_from(op.new.len()).unwrap_or(i64::MAX);
                    Hunk {
                        old_start: op.after_line,
                        old_lines: 0,
                        new_start: u32::try_from(new_start)
                            .map_err(|_| format!("op positions overflow in {path}"))?,
                        new_lines: u32::try_from(op.new.len())
                            .map_err(|_| format!("too many lines in one op on {path}"))?,
                        lines: op.new.iter().map(|l| format!("+{l}")).collect(),
                        eof_newline: if op.after_line == total {
                            eof_newline
                        } else {
                            true
                        },
                        old_eof_no_newline: op.after_line == total && !eof_newline,
                    }
                }
            }
            LineOp::Replace(_) | LineOp::Delete(_) => {
                let (start, end, expect, new): (u32, u32, &[String], &[String]) = match op {
                    LineOp::Replace(op) => (op.start, op.end, &op.expect, &op.new),
                    LineOp::Delete(op) => (op.start, op.end, &op.expect, &[]),
                    LineOp::Insert(_) => unreachable!("matched above"),
                };
                if end > total {
                    return Err(format!(
                        "op targets lines {start}..={end} but {path} has {total} line(s); \
                         the file may differ from what you saw — re-read it"
                    ));
                }
                if start.saturating_sub(1) < cursor {
                    return Err(format!("ops overlap at {path} line {start}"));
                }
                verify_expect(path, start, expect, &lines)?;
                cursor = end;
                last_insert_boundary = None;
                let new_start = i64::from(start) + delta;
                let old_count = end - start + 1;
                delta += i64::try_from(new.len()).unwrap_or(i64::MAX) - i64::from(old_count);
                let mut body: Vec<String> = expect.iter().map(|l| format!("-{l}")).collect();
                body.extend(new.iter().map(|l| format!("+{l}")));
                Hunk {
                    old_start: start,
                    old_lines: old_count,
                    new_start: u32::try_from(new_start)
                        .map_err(|_| format!("op positions overflow in {path}"))?,
                    new_lines: u32::try_from(new.len())
                        .map_err(|_| format!("too many lines in one op on {path}"))?,
                    lines: body,
                    eof_newline: if end == total { eof_newline } else { true },
                    old_eof_no_newline: end == total && !eof_newline,
                }
            }
        };
        hunks.push(hunk);
        if hunks.len() > MAX_HUNKS_PER_FILE {
            return Err(format!("more than {MAX_HUNKS_PER_FILE} ops on {path}"));
        }
    }
    Ok(hunks)
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

    // --- line ops (AM-0013-1) --------------------------------------------

    fn op(value: Value) -> LineOp {
        parse_line_op(&value).unwrap()
    }

    fn one_file(path: &str, content: &str) -> HashMap<String, String> {
        HashMap::from([(path.to_owned(), content.to_owned())])
    }

    #[test]
    fn line_op_parse_is_strict_per_variant() {
        // Tagged dispatch to deny_unknown_fields schemas.
        assert!(matches!(
            op(serde_json::json!({
                "op": "replace_lines", "path": "a.rs", "start": 2, "end": 2,
                "expect": ["old"], "new": ["new"],
            })),
            LineOp::Replace(_)
        ));
        assert!(matches!(
            op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["x"],
            })),
            LineOp::Insert(_)
        ));
        assert!(matches!(
            op(serde_json::json!({
                "op": "delete_lines", "path": "a.rs", "start": 1, "end": 1, "expect": ["x"],
            })),
            LineOp::Delete(_)
        ));
        // Unknown tag, unknown field, missing field, non-object.
        assert!(parse_line_op(&serde_json::json!({ "op": "swap_lines" })).is_err());
        assert!(parse_line_op(&serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["x"], "extra": 1,
        }))
        .is_err());
        assert!(parse_line_op(&serde_json::json!({
            "op": "delete_lines", "path": "a.rs", "start": 1, "end": 1,
        }))
        .is_err());
        assert!(parse_line_op(&serde_json::json!("replace_lines")).is_err());
        assert!(parse_line_op(&serde_json::json!({ "path": "a.rs" })).is_err());
    }

    #[test]
    fn line_op_screen_rejects_bad_paths_ranges_and_caps() {
        let good = op(serde_json::json!({
            "op": "replace_lines", "path": "src/a.rs", "start": 2, "end": 2,
            "expect": ["old"], "new": ["new"],
        }));
        assert!(screen_line_ops(std::slice::from_ref(&good)).is_ok());
        // Empty ops.
        assert!(screen_line_ops(&[]).is_err());
        // PS5 path screen.
        for path in ["/etc/passwd", "../x.rs", "a\\b", ""] {
            let bad = op(serde_json::json!({
                "op": "delete_lines", "path": path, "start": 1, "end": 1, "expect": ["x"],
            }));
            assert!(screen_line_ops(&[bad]).is_err(), "{path}");
        }
        // 1-based start; inverted range; expect sized to the range.
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "delete_lines", "path": "a.rs", "start": 0, "end": 1, "expect": ["x", "y"],
        }))])
        .is_err());
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "delete_lines", "path": "a.rs", "start": 3, "end": 2, "expect": [],
        }))])
        .is_err());
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "replace_lines", "path": "a.rs", "start": 1, "end": 2,
            "expect": ["only one"], "new": ["n"],
        }))])
        .is_err());
        // Empty new on replace/insert.
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "replace_lines", "path": "a.rs", "start": 1, "end": 1,
            "expect": ["x"], "new": [],
        }))])
        .is_err());
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": [],
        }))])
        .is_err());
        // Embedded newline / NUL.
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["a\nb"],
        }))])
        .is_err());
        assert!(screen_line_ops(&[op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["a\0b"],
        }))])
        .is_err());
        // Op-count cap.
        let many: Vec<LineOp> = (0..=MAX_OPS_PER_RESPONSE).map(|_| good.clone()).collect();
        assert!(screen_line_ops(&many).is_err());
        // Distinct-path cap.
        let files: Vec<LineOp> = (0..=MAX_PATCH_FILES)
            .map(|i| {
                op(serde_json::json!({
                    "op": "insert_lines", "path": format!("f{i}.rs"),
                    "after_line": 0, "new": ["x"],
                }))
            })
            .collect();
        assert!(screen_line_ops(&files).is_err());
        // Byte cap.
        let fat = op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0,
            "new": ["x".repeat(MAX_OPS_TOTAL_BYTES + 1)],
        }));
        assert!(screen_line_ops(&[fat]).is_err());
        // Line-count cap (empty lines carry no bytes).
        let airy = op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 0,
            "new": vec![String::new(); MAX_OPS_TOTAL_LINES + 1],
        }));
        assert!(screen_line_ops(&[airy]).is_err());
    }

    #[test]
    fn ops_compile_replace_to_a_verified_hunk() {
        let ops = vec![op(serde_json::json!({
            "op": "replace_lines", "path": "src/main.rs", "start": 2, "end": 2,
            "expect": ["    let x: i32 = \"no\";"], "new": ["    let x: i32 = 42;"],
        }))];
        let files = one_file("src/main.rs", "fn main() {\n    let x: i32 = \"no\";\n}\n");
        let set = ops_to_patchset(&ops, &files).unwrap();
        assert_eq!(set.files.len(), 1);
        let FilePatch::Modify { path, hunks } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(
            hunks[0],
            Hunk {
                old_start: 2,
                old_lines: 1,
                new_start: 2,
                new_lines: 1,
                lines: vec![
                    "-    let x: i32 = \"no\";".into(),
                    "+    let x: i32 = 42;".into(),
                ],
                eof_newline: true,
                old_eof_no_newline: false,
            }
        );
    }

    #[test]
    fn ops_compile_insert_and_delete_with_correct_boundaries() {
        let files = one_file("a.rs", "one\ntwo\nthree\n");
        // Insert at top (after_line 0) and append at eof (after_line 3).
        let set = ops_to_patchset(
            &[
                op(serde_json::json!({
                    "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["zero"],
                })),
                op(serde_json::json!({
                    "op": "insert_lines", "path": "a.rs", "after_line": 3, "new": ["four"],
                })),
            ],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        // V8b reserves `old_start == 0` for Create, so a top-of-file insert
        // must be the context-anchored git prepend shape: consume line 1 as
        // trailing context (`@@ -1,1 +1,2 @@` / `+zero` / ` one`).
        assert_eq!(
            hunks[0],
            Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                lines: vec!["+zero".into(), " one".into()],
                eof_newline: true,
                old_eof_no_newline: false,
            }
        );
        // Backend delta rule: new_start = after_line + delta + 1, where the
        // top-insert contributed delta = new_lines - old_lines = 1.
        assert_eq!((hunks[1].old_start, hunks[1].old_lines), (3, 0));
        assert_eq!((hunks[1].new_start, hunks[1].new_lines), (5, 1));

        let set = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "delete_lines", "path": "a.rs", "start": 2, "end": 3,
                "expect": ["two", "three"],
            }))],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(
            hunks[0],
            Hunk {
                old_start: 2,
                old_lines: 2,
                new_start: 2,
                new_lines: 0,
                lines: vec!["-two".into(), "-three".into()],
                eof_newline: true,
                old_eof_no_newline: false,
            }
        );
    }

    /// A top-of-file insert into a one-line file consumes that line as its
    /// context anchor, so it inherits the file's eof-newline properties.
    #[test]
    fn ops_compile_top_insert_anchors_on_line_one_and_keeps_eof_flags() {
        let files = one_file("a.txt", "only");
        let set = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "insert_lines", "path": "a.txt", "after_line": 0, "new": ["first"],
            }))],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(
            hunks[0],
            Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                lines: vec!["+first".into(), " only".into()],
                eof_newline: false,
                old_eof_no_newline: true,
            }
        );
    }

    /// The backend's Modify grammar cannot express an insertion into an
    /// empty file (V8b bans `old_start == 0` outside Create, and there is no
    /// line to anchor context on), so the compile must fail with
    /// model-repairable feedback rather than emit a hunk the dry run rejects.
    /// `insert_lines` carries no `expect`, so nothing ties it to the file it
    /// was authored against. Re-sending one across repair generations
    /// duplicated whole items — measured as the largest single cause of
    /// failed Alloy attempts, surfacing as E0428 "defined multiple times".
    #[test]
    fn ops_compile_rejects_an_insert_whose_text_is_already_present() {
        let files = one_file("a.rs", "pub fn keep() {}\npub struct Reader;\n");
        let err = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 1,
                "new": ["pub struct Reader;"],
            }))],
            &files,
        )
        .unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
        assert!(err.contains("a.rs"), "{err}");
    }

    /// The guard must compare the whole block, not just its first line, or a
    /// multi-line item that merely starts alike would be refused.
    #[test]
    fn ops_compile_allows_an_insert_that_only_partly_matches() {
        let files = one_file("a.rs", "pub fn one() {}\npub fn two() {}\n");
        ops_to_patchset(
            &[op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 1,
                "new": ["pub fn two() {}", "pub fn three() {}"],
            }))],
            &files,
        )
        .expect("a genuinely new block must still apply");
    }

    #[test]
    fn ops_compile_rejects_insert_into_empty_file() {
        let files = one_file("a.txt", "");
        let err = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "insert_lines", "path": "a.txt", "after_line": 0, "new": ["first"],
            }))],
            &files,
        )
        .unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(err.contains("a.txt"), "{err}");
    }

    /// The top-insert consumes line 1 as its anchor, so a second op that
    /// also touches line 1 is an overlap (the backend would reject the pair
    /// as overlapping hunks anyway).
    #[test]
    fn ops_compile_top_insert_conflicts_with_an_op_on_line_one() {
        let files = one_file("a.rs", "one\ntwo\n");
        let err = ops_to_patchset(
            &[
                op(serde_json::json!({
                    "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["zero"],
                })),
                op(serde_json::json!({
                    "op": "replace_lines", "path": "a.rs", "start": 1, "end": 1,
                    "expect": ["one"], "new": ["ONE"],
                })),
            ],
            &files,
        )
        .unwrap_err();
        assert!(err.contains("overlap"), "{err}");
        // Line 2 onward stays available after a top insert.
        let set = ops_to_patchset(
            &[
                op(serde_json::json!({
                    "op": "insert_lines", "path": "a.rs", "after_line": 0, "new": ["zero"],
                })),
                op(serde_json::json!({
                    "op": "replace_lines", "path": "a.rs", "start": 2, "end": 2,
                    "expect": ["two"], "new": ["TWO"],
                })),
            ],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks.len(), 2);
        // delta after the top insert is +1: replace of old line 2 lands at
        // new line 3.
        assert_eq!((hunks[1].old_start, hunks[1].new_start), (2, 3));
    }

    #[test]
    fn ops_compile_orders_hunks_and_tracks_the_delta() {
        // Emitted out of order; hunks must come out ascending with
        // new_start satisfying the backend rule (insert offset +1).
        let files = one_file("a.rs", "l1\nl2\nl3\nl4\nl5\n");
        let set = ops_to_patchset(
            &[
                op(serde_json::json!({
                    "op": "replace_lines", "path": "a.rs", "start": 4, "end": 4,
                    "expect": ["l4"], "new": ["L4a", "L4b"],
                })),
                op(serde_json::json!({
                    "op": "delete_lines", "path": "a.rs", "start": 1, "end": 2,
                    "expect": ["l1", "l2"],
                })),
                op(serde_json::json!({
                    "op": "insert_lines", "path": "a.rs", "after_line": 2, "new": ["mid"],
                })),
            ],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert_eq!(hunks.len(), 3);
        // delete l1-2: new_start = 1 + 0.
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));
        // insert after 2: new_start = 2 + (-2) + 1 = 1.
        assert_eq!((hunks[1].old_start, hunks[1].old_lines), (2, 0));
        assert_eq!(hunks[1].new_start, 1);
        // replace l4: delta is -2 + 1 = -1 → new_start = 3.
        assert_eq!((hunks[2].old_start, hunks[2].new_start), (4, 3));
        assert_eq!(hunks[2].new_lines, 2);
    }

    #[test]
    fn ops_compile_rejects_overlaps() {
        let files = one_file("a.rs", "l1\nl2\nl3\nl4\n");
        // Overlapping ranges.
        let overlapping = [
            op(serde_json::json!({
                "op": "replace_lines", "path": "a.rs", "start": 1, "end": 2,
                "expect": ["l1", "l2"], "new": ["x"],
            })),
            op(serde_json::json!({
                "op": "delete_lines", "path": "a.rs", "start": 2, "end": 3,
                "expect": ["l2", "l3"],
            })),
        ];
        assert!(ops_to_patchset(&overlapping, &files)
            .unwrap_err()
            .contains("overlap"));
        // Two inserts at the same boundary (backend OverlappingHunks rule).
        let double_insert = [
            op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 1, "new": ["a"],
            })),
            op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 1, "new": ["b"],
            })),
        ];
        assert!(ops_to_patchset(&double_insert, &files)
            .unwrap_err()
            .contains("overlap"));
        // Insert inside a replaced range.
        let insert_inside = [
            op(serde_json::json!({
                "op": "replace_lines", "path": "a.rs", "start": 2, "end": 3,
                "expect": ["l2", "l3"], "new": ["x"],
            })),
            op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 2, "new": ["y"],
            })),
        ];
        assert!(ops_to_patchset(&insert_inside, &files)
            .unwrap_err()
            .contains("overlap"));
        // Adjacent, non-overlapping ops are fine: insert at a boundary
        // between two touched ranges.
        let adjacent = [
            op(serde_json::json!({
                "op": "replace_lines", "path": "a.rs", "start": 1, "end": 1,
                "expect": ["l1"], "new": ["L1"],
            })),
            op(serde_json::json!({
                "op": "insert_lines", "path": "a.rs", "after_line": 1, "new": ["mid"],
            })),
            op(serde_json::json!({
                "op": "replace_lines", "path": "a.rs", "start": 2, "end": 2,
                "expect": ["l2"], "new": ["L2"],
            })),
        ];
        assert!(ops_to_patchset(&adjacent, &files).is_ok());
    }

    #[test]
    fn ops_compile_rejects_stale_expect_and_out_of_range_lines() {
        let files = one_file("a.rs", "l1\nl2\n");
        // The honesty guard: expect must match the CURRENT content.
        let stale = [op(serde_json::json!({
            "op": "replace_lines", "path": "a.rs", "start": 2, "end": 2,
            "expect": ["something remembered"], "new": ["x"],
        }))];
        let err = ops_to_patchset(&stale, &files).unwrap_err();
        assert!(err.contains("stale"), "{err}");
        assert!(err.contains("a.rs:2"), "{err}");
        // Line numbers past eof.
        let past = [op(serde_json::json!({
            "op": "delete_lines", "path": "a.rs", "start": 2, "end": 5,
            "expect": ["l2", "l3", "l4", "l5"],
        }))];
        assert!(ops_to_patchset(&past, &files)
            .unwrap_err()
            .contains("2 line(s)"));
        let insert_past = [op(serde_json::json!({
            "op": "insert_lines", "path": "a.rs", "after_line": 3, "new": ["x"],
        }))];
        assert!(ops_to_patchset(&insert_past, &files).is_err());
        // A path nobody read.
        let unknown = [op(serde_json::json!({
            "op": "insert_lines", "path": "b.rs", "after_line": 0, "new": ["x"],
        }))];
        assert!(ops_to_patchset(&unknown, &files)
            .unwrap_err()
            .contains("no current content"));
        // Empty ops.
        assert!(ops_to_patchset(&[], &files).is_err());
    }

    #[test]
    fn ops_compile_preserves_the_missing_trailing_newline() {
        // File without a trailing newline: an op touching eof must carry
        // the old no-newline proof and keep the property on the new side.
        let files = one_file("a.txt", "one\ntwo");
        let set = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "replace_lines", "path": "a.txt", "start": 2, "end": 2,
                "expect": ["two"], "new": ["TWO"],
            }))],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert!(!hunks[0].eof_newline);
        assert!(hunks[0].old_eof_no_newline);
        // An op that stays clear of eof leaves both flags at their defaults.
        let set = ops_to_patchset(
            &[op(serde_json::json!({
                "op": "replace_lines", "path": "a.txt", "start": 1, "end": 1,
                "expect": ["one"], "new": ["ONE"],
            }))],
            &files,
        )
        .unwrap();
        let FilePatch::Modify { hunks, .. } = &set.files[0] else {
            panic!("expected Modify");
        };
        assert!(hunks[0].eof_newline);
        assert!(!hunks[0].old_eof_no_newline);
    }

    #[test]
    fn ops_compile_groups_files_in_first_seen_order() {
        let mut files = one_file("b.rs", "b1\n");
        files.insert("a.rs".into(), "a1\n".into());
        let set = ops_to_patchset(
            &[
                op(serde_json::json!({
                    "op": "replace_lines", "path": "b.rs", "start": 1, "end": 1,
                    "expect": ["b1"], "new": ["B1"],
                })),
                op(serde_json::json!({
                    "op": "replace_lines", "path": "a.rs", "start": 1, "end": 1,
                    "expect": ["a1"], "new": ["A1"],
                })),
            ],
            &files,
        )
        .unwrap();
        assert_eq!(set.files.len(), 2);
        assert_eq!(set.files[0].path(), "b.rs");
        assert_eq!(set.files[1].path(), "a.rs");
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
