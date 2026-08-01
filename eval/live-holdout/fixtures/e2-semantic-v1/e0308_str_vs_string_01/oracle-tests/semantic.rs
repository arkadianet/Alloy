//! Hidden semantic oracle for `e0308_str_vs_string_01`.
//!
//! Every assertion is derivable from the broken source alone: the loop keeps
//! `ch.is_ascii_alphanumeric()` characters as `ch.to_ascii_lowercase()`, sets
//! `separator_pending` for anything else, and emits a single `-` only when a
//! kept character follows a pending separator and the slug is not empty. The
//! doc comment states the slug is freshly built and never starts or ends with a
//! separator.
//!
//! Comparisons are against string literals, which hold for an owned `String`
//! result and for a borrowed `&str` result alike; only the produced text is
//! asserted, never the chosen return type.

use e0308_str_vs_string_01::slugify;

/// Kept characters are ASCII alphanumerics, lower-cased; single separators
/// between them become one `-`.
#[test]
fn keeps_alphanumerics_in_lower_case() {
    assert_eq!(slugify("Hello World"), "hello-world", "words are separated");
    assert_eq!(
        slugify("Release 2 Beta"),
        "release-2-beta",
        "digits are kept"
    );
    assert_eq!(slugify("ABC"), "abc", "upper case is folded down");
    assert_eq!(slugify("abc123"), "abc123", "no separator is invented");
}

/// A run of non-alphanumeric characters collapses to exactly one `-`, because
/// `separator_pending` is a flag rather than a counter.
#[test]
fn collapses_runs_of_separators() {
    assert_eq!(slugify("a  b"), "a-b", "repeated spaces collapse");
    assert_eq!(slugify("a -_- b"), "a-b", "a mixed run collapses");
    assert_eq!(
        slugify("one, two; three"),
        "one-two-three",
        "punctuation plus space is still one separator"
    );
}

/// The `!slug.is_empty()` guard suppresses a leading separator, and a trailing
/// run emits nothing because no kept character follows it.
#[test]
fn never_starts_or_ends_with_a_separator() {
    assert_eq!(slugify("  Rust!  "), "rust", "surrounding noise is dropped");
    assert_eq!(slugify("---x---"), "x", "leading and trailing runs vanish");
    assert_eq!(slugify("!!!"), "", "nothing kept means an empty slug");
    assert_eq!(slugify(""), "", "an empty title yields an empty slug");
}

/// Non-ASCII characters are not `is_ascii_alphanumeric`, so they act as
/// separators; and because `-` is itself a separator, re-slugifying a slug
/// leaves it unchanged.
#[test]
fn non_ascii_separates_and_slugs_are_stable() {
    assert_eq!(
        slugify("café au lait"),
        "caf-au-lait",
        "non-ASCII separates"
    );
    let once = slugify("Hello, World! 2026");
    assert_eq!(once, "hello-world-2026", "baseline slug");
    assert_eq!(slugify(&once), once, "slugifying a slug changes nothing");
}
