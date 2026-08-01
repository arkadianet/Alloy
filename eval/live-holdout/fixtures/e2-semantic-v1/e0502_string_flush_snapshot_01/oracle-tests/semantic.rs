//! Hidden semantic oracle for `e0502_string_flush_snapshot_01`.
//!
//! Every assertion is read straight off the broken source: it snapshots the
//! log's text, calls `log.clear()`, builds `pending.to_string()` and then
//! `push_str(terminator)`. Nothing beyond that is asserted.

use e0502_string_flush_snapshot_01::flush;

/// From `let mut flushed = pending.to_string(); flushed.push_str(terminator)`:
/// the return is the log's pre-call text followed by the terminator.
#[test]
fn returns_the_pending_text_with_the_terminator_appended() {
    let mut log = String::from("alpha beta");
    let flushed = flush(&mut log, ";\n");
    assert_eq!(
        flushed, "alpha beta;\n",
        "return is pending text + terminator"
    );
}

/// From the unconditional `log.clear()`: the log is empty when `flush` returns.
#[test]
fn empties_the_log() {
    let mut log = String::from("alpha beta");
    flush(&mut log, ";");
    assert_eq!(log, "", "log must be emptied by the flush");
    assert!(log.is_empty(), "log must report itself empty");
}

/// The terminator is appended to the returned text only — it is never written
/// back into the log, so a second flush sees an empty `pending`.
#[test]
fn a_second_flush_yields_only_the_terminator() {
    let mut log = String::from("first");
    assert_eq!(
        flush(&mut log, "!"),
        "first!",
        "first flush drains the text"
    );
    assert_eq!(
        flush(&mut log, "!"),
        "!",
        "second flush has nothing pending"
    );
    assert_eq!(log, "", "log stays empty across flushes");
}

/// With an empty terminator the `push_str` contributes nothing, so the return
/// is the pending text verbatim — multi-byte characters included.
#[test]
fn empty_terminator_returns_the_pending_text_verbatim() {
    let mut log = String::from("h\u{e9}llo \u{2014} ok");
    let flushed = flush(&mut log, "");
    assert_eq!(
        flushed, "h\u{e9}llo \u{2014} ok",
        "text is returned unaltered"
    );
    assert_eq!(log, "", "log is still emptied when the terminator is empty");
}

/// `push_str` appends: order is pending-then-terminator, not the reverse, and
/// the terminator is not interleaved with the text.
#[test]
fn terminator_is_appended_not_prepended() {
    let mut log = String::from("body");
    assert_eq!(flush(&mut log, "END"), "bodyEND", "terminator goes last");
}
