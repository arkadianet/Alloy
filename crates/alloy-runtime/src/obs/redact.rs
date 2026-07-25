//! Secret redaction and retention helpers (RFC-0004 §5.6–5.7).
//!
//! `ObsError::Redaction` is reserved for helper misuse; current helpers are
//! infallible for well-formed `&str` / JSON inputs and return `Ok`.

use crate::obs::error::ObsError;
use crate::obs::hash::{hash_prompt, hash_tool_body};
use crate::types::ids::Digest;

/// Replacement for every matched secret span.
const REDACTED: &str = "[REDACTED]";

/// Max metadata JSON bytes (pre-redaction).
pub(crate) const METADATA_MAX_BYTES: usize = 64 * 1024;
/// Max prompt/tool body UTF-8 bytes (pre-redaction).
pub(crate) const BODY_MAX_BYTES: usize = 256 * 1024;

/// Retention flags drawn from [`crate::RuntimeConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// When true, retain redacted prompt bodies.
    pub retain_full_prompts: bool,
    /// When true, retain redacted tool bodies.
    pub retain_tool_bodies: bool,
}

impl RetentionPolicy {
    /// ADR F-17 defaults: metadata + hashes only.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            retain_full_prompts: false,
            retain_tool_bodies: false,
        }
    }
}

impl From<&crate::config::RuntimeConfig> for RetentionPolicy {
    fn from(c: &crate::config::RuntimeConfig) -> Self {
        Self {
            retain_full_prompts: c.retain_full_prompts,
            retain_tool_bodies: c.retain_tool_bodies,
        }
    }
}

/// Outcome of applying retention to a prompt or tool body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetentionOutcome {
    /// Pre-redaction content hash when input was present.
    pub hash: Option<Digest>,
    /// Retained body (redacted), or `None` when stripped / not retained.
    pub body: Option<String>,
    /// True when a path deny-list hit forced stripping under opt-in retention.
    pub deny_list_stripped: bool,
}

/// Redact secret-like substrings in `text` (API keys, Bearer tokens, env assignments, PEM).
///
/// Each match span is replaced with `[REDACTED]`. Leftmost-longest; non-overlapping.
/// Returns `text` unchanged (no allocation) when nothing matches.
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    let mut i = 0;
    let mut first_match: Option<(usize, usize)> = None;
    while i < text.len() {
        if let Some(len) = match_at(text, i) {
            first_match = Some((i, len));
            break;
        }
        let ch = text[i..].chars().next().expect("valid utf-8 index");
        i += ch.len_utf8();
    }
    let Some((start, len)) = first_match else {
        return text.to_owned();
    };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(REDACTED);
    i = start + len;
    while i < text.len() {
        if let Some(mlen) = match_at(text, i) {
            out.push_str(REDACTED);
            i += mlen;
        } else {
            // Copy an unmatched span until the next match (or end).
            let span_start = i;
            while i < text.len() {
                if match_at(text, i).is_some() {
                    break;
                }
                let ch = text[i..].chars().next().expect("valid utf-8 index");
                i += ch.len_utf8();
            }
            out.push_str(&text[span_start..i]);
        }
    }
    out
}

/// Recursively redact JSON string leaves and secret-named object values.
#[must_use]
pub fn redact_json_strings(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if json_key_is_sensitive(k) {
                    out.insert(k.clone(), serde_json::Value::String(REDACTED.to_owned()));
                } else {
                    out.insert(k.clone(), redact_json_strings(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(redact_json_strings).collect())
        }
        serde_json::Value::String(s) => serde_json::Value::String(redact_secrets(s)),
        other => other.clone(),
    }
}

/// Apply prompt retention: hash pre-redaction; body only when opt-in and not deny-listed.
pub fn apply_prompt_retention(
    prompt: Option<&str>,
    policy: RetentionPolicy,
) -> Result<(Option<Digest>, Option<String>), ObsError> {
    let o = apply_body_retention(prompt, policy.retain_full_prompts, true)?;
    Ok((o.hash, o.body))
}

/// Apply tool-body retention analogously using `retain_tool_bodies`.
pub fn apply_tool_retention(
    body: Option<&str>,
    policy: RetentionPolicy,
) -> Result<(Option<Digest>, Option<String>), ObsError> {
    let o = apply_body_retention(body, policy.retain_tool_bodies, false)?;
    Ok((o.hash, o.body))
}

pub(crate) fn apply_body_retention(
    raw: Option<&str>,
    retain: bool,
    is_prompt: bool,
) -> Result<RetentionOutcome, ObsError> {
    let Some(text) = raw else {
        return Ok(RetentionOutcome {
            hash: None,
            body: None,
            deny_list_stripped: false,
        });
    };
    let hash = if is_prompt {
        hash_prompt(text)
    } else {
        hash_tool_body(text)
    };
    if !retain {
        return Ok(RetentionOutcome {
            hash: Some(hash),
            body: None,
            deny_list_stripped: false,
        });
    }
    let redacted = redact_secrets(text);
    if path_deny_list_hit(&redacted) || path_deny_list_hit(text) {
        return Ok(RetentionOutcome {
            hash: Some(hash),
            body: None,
            deny_list_stripped: true,
        });
    }
    Ok(RetentionOutcome {
        hash: Some(hash),
        body: Some(redacted),
        deny_list_stripped: false,
    })
}

/// True if text contains a `.env` path segment or ends with `/.env`.
pub(crate) fn path_deny_list_hit(text: &str) -> bool {
    if text == ".env" || text.ends_with("/.env") || text.ends_with("\\.env") {
        return true;
    }
    for needle in ["/.env", "\\.env"] {
        let mut start = 0;
        while let Some(rel) = text[start..].find(needle) {
            let abs = start + rel;
            let after = abs + needle.len();
            if env_path_boundary_after(text, after) {
                return true;
            }
            start = abs + 1;
        }
    }
    // Split on path and JSON/structural delimiters so `{"path":".env"}` hits.
    for part in text.split(|c: char| {
        c == '/'
            || c == '\\'
            || c.is_whitespace()
            || matches!(
                c,
                '{' | '}' | '[' | ']' | ':' | ',' | '"' | '\'' | '(' | ')' | ';' | '='
            )
    }) {
        if part == ".env" {
            return true;
        }
    }
    false
}

fn env_path_boundary_after(text: &str, after: usize) -> bool {
    after >= text.len()
        || text.as_bytes()[after].is_ascii_whitespace()
        || matches!(
            text.as_bytes()[after],
            b'/' | b'\\' | b'"' | b'\'' | b')' | b']' | b'}' | b',' | b';' | b':' | b'{'
        )
}

fn json_key_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "api_key",
        "api-key",
        "secret",
        "password",
        "token",
        "authorization",
        "credential",
    ];
    NEEDLES.iter().any(|n| lower == *n || lower.contains(n))
}

fn match_at(text: &str, i: usize) -> Option<usize> {
    let mut best = 0usize;
    if let Some(n) = match_pem(text, i) {
        best = best.max(n);
    }
    if let Some(n) = match_bearer(text, i) {
        best = best.max(n);
    }
    if let Some(n) = match_env_assignment(text, i) {
        best = best.max(n);
    }
    if let Some(n) = match_sk_token(text, i) {
        best = best.max(n);
    }
    (best > 0).then_some(best)
}

/// Match a PEM private-key block.
///
/// Searches for a matching `-----END … PRIVATE KEY-----` over the full remainder
/// (including single-line PEMs). Unterminated private-key blocks fail closed:
/// the span extends to the end of the input.
fn match_pem(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest_ci_starts_with(rest, "-----BEGIN") {
        return None;
    }
    let after_begin = "-----BEGIN".len();
    let rel_close = find_ci(&rest[after_begin..], "-----")?;
    let header_end = after_begin + rel_close + "-----".len();
    let header = &rest[..header_end];
    if !contains_ci(header, "PRIVATE KEY") {
        return None;
    }

    let mut search = header_end;
    while let Some(rel) = find_ci(&rest[search..], "-----END") {
        let abs = search + rel;
        let after_end_kw = abs + "-----END".len();
        let Some(rel_end_close) = find_ci(&rest[after_end_kw..], "-----") else {
            search = abs + "-----END".len();
            continue;
        };
        let end_marker_end = after_end_kw + rel_end_close + "-----".len();
        let end_line = &rest[abs..end_marker_end];
        if contains_ci(end_line, "PRIVATE KEY") {
            let mut len = end_marker_end;
            if len < rest.len() && rest.as_bytes()[len] == b'\n' {
                len += 1;
            }
            return Some(len);
        }
        search = abs + "-----END".len();
    }
    // Unterminated PRIVATE KEY block — fail closed.
    Some(rest.len())
}

fn match_bearer(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest_ci_starts_with(rest, "authorization") {
        return None;
    }
    let mut pos = "authorization".len();
    pos = skip_spaces(rest, pos);
    if pos >= rest.len() || rest.as_bytes()[pos] != b':' {
        return None;
    }
    pos += 1;
    pos = skip_spaces(rest, pos);
    if !rest_ci_starts_with(&rest[pos..], "bearer") {
        return None;
    }
    pos += "bearer".len();
    pos = skip_spaces(rest, pos);
    let token_start = pos;
    while pos < rest.len() && !rest.as_bytes()[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos == token_start {
        return None;
    }
    Some(pos)
}

fn match_env_assignment(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if i > 0 {
        let prev = text[..i].chars().next_back().unwrap();
        if is_ident_char(prev) {
            return None;
        }
    }
    let (name, name_len) = parse_secret_name(rest)?;
    if !is_secret_assignment_name(name) {
        return None;
    }
    let mut pos = name_len;
    pos = skip_spaces(rest, pos);
    if pos >= rest.len() {
        return None;
    }
    // `=` is normative (§5.6); `:` covers common log/YAML forms.
    let sep = rest.as_bytes()[pos];
    if sep != b'=' && sep != b':' {
        return None;
    }
    pos += 1;
    pos = skip_spaces(rest, pos);
    let value_start = pos;
    while pos < rest.len() && !rest.as_bytes()[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos == value_start {
        return None;
    }
    Some(pos)
}

/// `sk-` + 8+ of `[A-Za-z0-9_-]` (covers `sk-proj-…` / `sk-svcacct-…` forms).
fn match_sk_token(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest.starts_with("sk-") {
        return None;
    }
    if i > 0 {
        let prev = text[..i].chars().next_back().unwrap();
        if prev.is_ascii_alphanumeric() || prev == '_' || prev == '-' {
            return None;
        }
    }
    let mut pos = 3;
    while pos < rest.len() {
        let b = rest.as_bytes()[pos];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            pos += 1;
        } else {
            break;
        }
    }
    let body_len = pos - 3;
    if body_len < 8 {
        return None;
    }
    Some(pos)
}

fn parse_secret_name(rest: &str) -> Option<(&str, usize)> {
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let c0 = bytes[0];
    if !(c0.is_ascii_alphabetic() || c0 == b'_') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            i += 1;
        } else {
            break;
        }
    }
    Some((&rest[..i], i))
}

fn is_secret_assignment_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "api_key" | "api-key" | "secret" | "token" | "password" | "authorization"
    ) || lower.ends_with("_api_key")
        || lower.ends_with("_secret")
        || lower.ends_with("_token")
        || lower.ends_with("_password")
        || lower.ends_with("-api-key")
        || lower.ends_with("-secret")
        || lower.ends_with("-token")
        || lower.ends_with("-password")
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn skip_spaces(s: &str, mut pos: usize) -> usize {
    let bytes = s.as_bytes();
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    pos
}

fn rest_ci_starts_with(s: &str, prefix: &str) -> bool {
    let sb = s.as_bytes();
    let pb = prefix.as_bytes();
    if sb.len() < pb.len() {
        return false;
    }
    sb[..pb.len()]
        .iter()
        .zip(pb.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    find_ci(hay, needle).is_some()
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| {
        w.iter()
            .zip(n.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_redaction_strips_api_key() {
        let out = redact_secrets("before api_key=sk-12345678 after");
        assert_eq!(out, "before [REDACTED] after");
        assert!(!out.contains("sk-"));
        assert!(!out.contains("api_key="));
    }

    #[test]
    fn my_api_key_assignment_redacted() {
        assert_eq!(redact_secrets("MY_API_KEY=abc"), "[REDACTED]");
    }

    #[test]
    fn password_colon_assignment_redacted() {
        assert_eq!(redact_secrets("password: hunter2xyz"), "[REDACTED]");
    }

    #[test]
    fn bearer_redacted() {
        let out = redact_secrets("Authorization: Bearer tokensecret99");
        assert_eq!(out, "[REDACTED]");
    }

    #[test]
    fn bare_sk_token_redacted() {
        assert_eq!(redact_secrets("key sk-abcdefgh ij"), "key [REDACTED] ij");
    }

    #[test]
    fn sk_proj_token_redacted() {
        let out = redact_secrets("k=sk-proj-AbCdEfGhIjKlMnOpQrSt");
        assert!(out.contains("[REDACTED]"), "{out}");
        assert!(!out.contains("sk-proj-"));
    }

    #[test]
    fn json_key_redaction_masks_secret_values() {
        let v = json!({"api_key": "sk-abcdefghij", "ok": "fine"});
        let out = redact_json_strings(&v);
        assert_eq!(out["api_key"], json!(REDACTED));
        assert_eq!(out["ok"], json!("fine"));
    }

    #[test]
    fn retention_default_strips_prompt_body() {
        let (h, b) =
            apply_prompt_retention(Some("secret api_key=x"), RetentionPolicy::defaults()).unwrap();
        assert!(h.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn retention_opt_in_keeps_redacted_body() {
        let policy = RetentionPolicy {
            retain_full_prompts: true,
            retain_tool_bodies: false,
        };
        let (h, b) = apply_prompt_retention(Some("hello api_key=sk-12345678"), policy).unwrap();
        assert!(h.is_some());
        let body = b.unwrap();
        assert!(body.contains("[REDACTED]"));
        assert!(!body.contains("sk-12345678"));
        assert_eq!(h.unwrap(), hash_prompt("hello api_key=sk-12345678"));
    }

    #[test]
    fn retention_tool_bodies_default_off() {
        let (h, b) = apply_tool_retention(Some("body"), RetentionPolicy::defaults()).unwrap();
        assert!(h.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn tool_redaction_strips_env_path() {
        let policy = RetentionPolicy {
            retain_full_prompts: false,
            retain_tool_bodies: true,
        };
        let (h, b) = apply_tool_retention(Some("read /.env for config"), policy).unwrap();
        assert!(h.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn tool_redaction_strips_env_path_in_json() {
        let policy = RetentionPolicy {
            retain_full_prompts: false,
            retain_tool_bodies: true,
        };
        let (h, b) = apply_tool_retention(Some(r#"{"path":".env"}"#), policy).unwrap();
        assert!(h.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn prompt_deny_list_strips_env_path() {
        let policy = RetentionPolicy {
            retain_full_prompts: true,
            retain_tool_bodies: false,
        };
        let (h, b) = apply_prompt_retention(Some("cat /home/u/.env"), policy).unwrap();
        assert!(h.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn pem_private_key_redacted() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nABC\n-----END RSA PRIVATE KEY-----\n";
        let out = redact_secrets(pem);
        assert_eq!(out.trim(), "[REDACTED]");
    }

    #[test]
    fn pem_single_line_private_key_redacted() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----MIIBOgIBAAJBAKj-----END RSA PRIVATE KEY-----";
        let out = redact_secrets(pem);
        assert_eq!(out, "[REDACTED]");
    }

    #[test]
    fn pem_unterminated_private_key_fail_closed() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBsecretstuff\n";
        let out = redact_secrets(pem);
        assert_eq!(out, "[REDACTED]");
        assert!(!out.contains("MIIB"));
    }

    #[test]
    fn pem_end_certificate_does_not_close_private_key() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nABC\n-----END CERTIFICATE-----\nmore\n-----END RSA PRIVATE KEY-----\n";
        let out = redact_secrets(pem);
        assert_eq!(out.trim(), "[REDACTED]");
        assert!(!out.contains("ABC"));
        assert!(!out.contains("more"));
    }

    #[test]
    fn redact_secrets_noop_no_alloc_change() {
        assert_eq!(redact_secrets("plain text"), "plain text");
    }
}
