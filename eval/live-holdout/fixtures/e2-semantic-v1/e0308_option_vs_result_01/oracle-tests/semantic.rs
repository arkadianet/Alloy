//! Hidden semantic oracle for `e0308_option_vs_result_01`.
//!
//! Every assertion is derivable from the broken source alone: the body is
//! `entries.iter().find(|(entry_key, _)| *entry_key == key).map(|(_, value)|
//! *value)`, the signature is `Result<i32, LookupError>`, and the doc comment
//! states that a lookup with no matching entry reports
//! `Err(LookupError::MissingKey)`. Both branches are pinned, so a repair that
//! collapses the miss path into an `Ok` (or the hit path into an `Err`) fails.

use e0308_option_vs_result_01::{lookup, LookupError};

/// The hit branch: `find` matched, so `map` yields the paired value and the
/// caller sees it as `Ok`.
#[test]
fn present_key_reports_its_value() {
    let entries = [("alpha", 1), ("beta", 2), ("gamma", -7)];
    assert_eq!(lookup(&entries, "alpha"), Ok(1), "first entry");
    assert_eq!(lookup(&entries, "beta"), Ok(2), "middle entry");
    assert_eq!(
        lookup(&entries, "gamma"),
        Ok(-7),
        "negative values pass through unchanged"
    );
}

/// The miss branch: `find` matched nothing, which the doc comment defines as
/// `Err(LookupError::MissingKey)`. This must hold for a non-empty slice and for
/// an empty one.
#[test]
fn absent_key_reports_missing_key() {
    let entries = [("alpha", 1), ("beta", 2)];
    assert_eq!(
        lookup(&entries, "delta"),
        Err(LookupError::MissingKey),
        "an unmatched key is an error, never a substituted value"
    );
    let empty: [(&str, i32); 0] = [];
    assert_eq!(
        lookup(&empty, "alpha"),
        Err(LookupError::MissingKey),
        "an empty table matches nothing"
    );
}

/// `find` stops at the first match, so a duplicated key reports the earlier
/// value and never the later one.
#[test]
fn first_matching_entry_wins() {
    let entries = [("dup", 10), ("other", 20), ("dup", 30)];
    assert_eq!(
        lookup(&entries, "dup"),
        Ok(10),
        "earliest match is reported"
    );
    assert_eq!(lookup(&entries, "other"), Ok(20), "later keys still match");
}

/// The comparison is `*entry_key == key`, i.e. exact string equality: no
/// prefix, suffix, or case-insensitive matching.
#[test]
fn keys_match_exactly() {
    let entries = [("alpha", 1)];
    assert_eq!(
        lookup(&entries, "alph"),
        Err(LookupError::MissingKey),
        "a prefix is not a match"
    );
    assert_eq!(
        lookup(&entries, "alphabet"),
        Err(LookupError::MissingKey),
        "an extension is not a match"
    );
    assert_eq!(
        lookup(&entries, "Alpha"),
        Err(LookupError::MissingKey),
        "matching is case sensitive"
    );
    assert_eq!(
        lookup(&entries, ""),
        Err(LookupError::MissingKey),
        "the empty key matches no entry here"
    );
}
