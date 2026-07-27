//! Absolute-path redaction shared by the MCP output boundary and EditEngine
//! error details.
//!
//! Operator- and model-visible strings must never carry jail locations
//! (RFC-0006 §5.9, RFC-0008 §5.4), and both subsystems need the same dialect so
//! a message redacted on one path is redacted identically on the other.
//!
//! Author: arkadianet

/// Replace absolute-path spans in `msg` with `<path>`.
///
/// Absolute Unix paths (`/…`) and Windows drive paths (`C:\…` / `C:/…`) are
/// replaced unless the preceding character is path-ish (alphanumeric, `.`, `-`,
/// `_`). That keeps relative mentions like `src/main.rs` intact while redacting
/// quoted and delimited forms such as `"/home/op/x"`, `path=/home/op/x`, and
/// `(C:\Users\op\y)`.
#[must_use]
pub(crate) fn redact_abs_paths(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    let mut prev_pathish = false;
    while !rest.is_empty() {
        if !prev_pathish && rest.starts_with('/') {
            out.push_str("<path>");
            rest = &rest[path_span_end(rest)..];
            prev_pathish = false;
            continue;
        }
        if !prev_pathish {
            if let Some(stripped) = strip_drive_path_prefix(rest) {
                out.push_str("<path>");
                rest = stripped;
                prev_pathish = false;
                continue;
            }
        }
        let ch = rest.chars().next().expect("rest non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
        prev_pathish = is_path_continuation(ch);
    }
    out
}

/// Redact, collapse whitespace, and cap `msg` at `max_bytes` on a char boundary.
///
/// Used for stderr snippets carried in error details, where the source is
/// arbitrary tool output rather than a message the host composed.
#[must_use]
pub(crate) fn redacted_snippet(msg: &str, max_bytes: usize) -> String {
    let collapsed = redact_abs_paths(msg)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.len() <= max_bytes {
        return collapsed;
    }
    let end = collapsed
        .char_indices()
        .take_while(|(i, ch)| i + ch.len_utf8() <= max_bytes)
        .map(|(i, ch)| i + ch.len_utf8())
        .last()
        .unwrap_or(0);
    format!("{}...", &collapsed[..end])
}

fn is_path_continuation(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')
}

/// End index of an absolute-path span starting at `s` (which begins with `/`
/// or a drive path). Stops before whitespace and before message delimiters so
/// closing quotes/parens are not swallowed into the redaction.
fn path_span_end(s: &str) -> usize {
    s.char_indices()
        .find(|&(_, ch)| ch.is_whitespace() || is_path_terminator(ch))
        .map_or(s.len(), |(i, _)| i)
}

fn is_path_terminator(ch: char) -> bool {
    matches!(ch, '"' | '\'' | ')' | ']' | '}' | ',' | ';' | '<' | '>')
}

/// If `s` begins with a Windows drive path (`X:\` or `X:/`), return the
/// remainder after the path span.
fn strip_drive_path_prefix(s: &str) -> Option<&str> {
    let mut chars = s.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if chars.next() != Some(':') {
        return None;
    }
    match chars.next() {
        Some('\\' | '/') => {}
        _ => return None,
    }
    Some(&s[path_span_end(s)..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_absolute_and_keeps_relative() {
        assert_eq!(
            redact_abs_paths("wrote /home/op/work/src/main.rs"),
            "wrote <path>"
        );
        assert_eq!(
            redact_abs_paths("at path=/home/op/x on C:\\Users\\op\\y"),
            "at path=<path> on <path>"
        );
        assert_eq!(
            redact_abs_paths("wrote src/main.rs ok"),
            "wrote src/main.rs ok"
        );
        assert_eq!(
            redact_abs_paths(r#"conflict in "/home/op/x" and (C:\Users\op\y)"#),
            r#"conflict in "<path>" and (<path>)"#
        );
    }

    #[test]
    fn snippet_collapses_and_truncates_on_char_boundary() {
        assert_eq!(
            redacted_snippet("fatal: could not open\n /home/op/.git/index", 200),
            "fatal: could not open <path>"
        );
        // The cap must never split the multi-byte char it lands inside.
        let snippet = redacted_snippet(&"é".repeat(40), 11);
        assert_eq!(snippet, format!("{}...", "é".repeat(5)));
    }
}
