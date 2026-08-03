//! Hidden semantic oracle for `e0277_collect_wrong_container_01`.
//!
//! Every assertion is read straight off the broken source: the pipeline walks
//! `config.lines()`, keeps only the lines that `split_once('=')` accepts, and
//! collects into the declared `BTreeMap<String, String>`. The doc comment
//! fixes the rest — key before the first `=`, value after it, both trimmed,
//! lines without `=` skipped, last duplicate key wins. No case folding, no
//! sorting of values and no other rewriting appears anywhere in the source.

use std::collections::BTreeMap;

use e0277_collect_wrong_container_01::parse_config;

/// Each `key=value` line becomes one entry, with both halves trimmed and
/// otherwise passed through verbatim (no case folding).
#[test]
fn maps_each_key_to_its_trimmed_value() {
    let parsed = parse_config("  Host = example.org  \nport=8080\n");
    assert_eq!(
        parsed,
        BTreeMap::from([
            ("Host".to_string(), "example.org".to_string()),
            ("port".to_string(), "8080".to_string()),
        ])
    );
}

/// `filter_map(split_once)` drops every line without an `=`, so blanks and
/// comment lines contribute no entries at all.
#[test]
fn skips_lines_without_an_equals_sign() {
    let parsed = parse_config("# a comment\n\nmode=fast\njust-a-word\n   \n");
    assert_eq!(parsed.len(), 1, "only the one `=` line becomes an entry");
    assert_eq!(parsed.get("mode"), Some(&"fast".to_string()));
}

/// `split_once` splits at the FIRST `=`, so any further `=` stays inside the
/// value.
#[test]
fn splits_at_the_first_equals_only() {
    let parsed = parse_config("cmd=a=b=c");
    assert_eq!(parsed.get("cmd"), Some(&"a=b=c".to_string()));
    assert_eq!(parsed.len(), 1);
}

/// Collecting into a map inserts in line order, so a repeated key ends up
/// holding the value from its last line.
#[test]
fn the_last_line_wins_for_a_repeated_key() {
    let parsed = parse_config("level=1\nother=x\nlevel=3\n");
    assert_eq!(parsed.get("level"), Some(&"3".to_string()));
    assert_eq!(parsed.len(), 2, "a repeat replaces, it does not add");
}

/// Degenerate inputs follow directly from the pipeline: nothing to iterate
/// gives an empty map, and a trailing `=` gives an empty value.
#[test]
fn empty_input_and_empty_values() {
    assert_eq!(parse_config(""), BTreeMap::new());
    assert_eq!(
        parse_config("key="),
        BTreeMap::from([("key".to_string(), String::new())])
    );
}
