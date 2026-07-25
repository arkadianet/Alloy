//! Secret redaction and retention helpers (RFC-0004 §5.6–5.7).

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

/// Redact secret-like substrings in `text` (API keys, Bearer tokens, env assignments, PEM).
///
/// Each match span is replaced with `[REDACTED]`. Leftmost-longest; non-overlapping.
#[must_use]
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = match_at(text, i) {
            out.push_str(REDACTED);
            i += len;
        } else {
            let ch = text[i..].chars().next().expect("valid utf-8 index");
            out.push(ch);
            i += ch.len_utf8();
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
    apply_body_retention(prompt, policy.retain_full_prompts, true)
}

/// Apply tool-body retention analogously using `retain_tool_bodies`.
pub fn apply_tool_retention(
    body: Option<&str>,
    policy: RetentionPolicy,
) -> Result<(Option<Digest>, Option<String>), ObsError> {
    apply_body_retention(body, policy.retain_tool_bodies, false)
}

fn apply_body_retention(
    raw: Option<&str>,
    retain: bool,
    is_prompt: bool,
) -> Result<(Option<Digest>, Option<String>), ObsError> {
    let Some(text) = raw else {
        return Ok((None, None));
    };
    let hash = if is_prompt {
        hash_prompt(text)
    } else {
        hash_tool_body(text)
    };
    if !retain {
        return Ok((Some(hash), None));
    }
    let redacted = redact_secrets(text);
    if path_deny_list_hit(&redacted) || path_deny_list_hit(text) {
        tracing::warn!(
            reason = "path_deny_list",
            "retention deny-list stripped body"
        );
        return Ok((Some(hash), None));
    }
    Ok((Some(hash), Some(redacted)))
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
            let ok_after = after >= text.len()
                || text.as_bytes()[after].is_ascii_whitespace()
                || matches!(
                    text.as_bytes()[after],
                    b'/' | b'\\' | b'"' | b'\'' | b')' | b']' | b',' | b';' | b':'
                );
            if ok_after {
                return true;
            }
            start = abs + 1;
        }
    }
    for part in text.split(|c: char| c == '/' || c == '\\' || c.is_whitespace()) {
        let trimmed = part.trim_matches(|c: char| {
            matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | ':')
        });
        if trimmed == ".env" {
            return true;
        }
    }
    false
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

fn match_pem(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest_ci_starts_with(rest, "-----BEGIN") {
        return None;
    }
    // Require PRIVATE KEY in the begin line before we treat it as sensitive.
    let begin_end = rest.find('\n').unwrap_or(rest.len());
    let header = &rest[..begin_end];
    if !contains_ci(header, "PRIVATE KEY") {
        return None;
    }
    let after_header = begin_end;
    let body = &rest[after_header..];
    // Find matching END line.
    let mut search = 0;
    while let Some(rel) = body[search..].find("-----END") {
        let abs = after_header + search + rel;
        let end_line_end = text[i + abs..]
            .find('\n')
            .map(|n| abs + n)
            .unwrap_or(rest.len());
        let end_line = &rest[abs..end_line_end];
        if contains_ci(end_line, "PRIVATE KEY") || contains_ci(end_line, "-----END") {
            // include through end of END line (without requiring trailing newline)
            let mut len = end_line_end;
            if len < rest.len() && rest.as_bytes()[len] == b'\n' {
                len += 1;
            }
            return Some(len);
        }
        search = rel + 8;
    }
    None
}

fn match_bearer(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest_ci_starts_with(rest, "authorization") {
        return None;
    }
    let mut pos = "authorization".len();
    // optional spaces, then ':'
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
    // Left boundary: start or non-ident char before i
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
    if pos >= rest.len() || rest.as_bytes()[pos] != b'=' {
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

fn match_sk_token(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest.starts_with("sk-") {
        return None;
    }
    // Left boundary: not alphanumeric continuation into sk-
    if i > 0 {
        let prev = text[..i].chars().next_back().unwrap();
        if prev.is_ascii_alphanumeric() || prev == '_' || prev == '-' {
            return None;
        }
    }
    let mut pos = 3;
    while pos < rest.len() && rest.as_bytes()[pos].is_ascii_alphanumeric() {
        pos += 1;
    }
    let alnum = pos - 3;
    if alnum < 8 {
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
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| {
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
    fn bearer_redacted() {
        let out = redact_secrets("Authorization: Bearer tokensecret99");
        assert_eq!(out, "[REDACTED]");
    }

    #[test]
    fn bare_sk_token_redacted() {
        assert_eq!(redact_secrets("key sk-abcdefgh ij"), "key [REDACTED] ij");
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
        // hash is of pre-redaction bytes
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
}
