//! Hidden semantic oracle for `e0596_loop_needs_iter_mut_01`.
//!
//! Every assertion is derivable from the broken source alone. That source
//! walks every element of the caller's `&mut [String]` and calls
//! `label.push_str(suffix)` on it, and its doc comment states the labels keep
//! their order and the slice keeps its length. Nothing else is required.

use e0596_loop_needs_iter_mut_01::append_suffix;

/// `push_str` appends to the existing contents, so each label keeps its
/// original text and gains the suffix at the end, in place, in order.
#[test]
fn appends_the_suffix_to_every_label_in_place() {
    let mut labels = vec!["alpha".to_string(), "beta".to_string()];
    append_suffix(&mut labels, "-v2");
    assert_eq!(labels, vec!["alpha-v2".to_string(), "beta-v2".to_string()]);
}

/// The loop visits every element, not just the first or last: with repeated
/// values each occurrence must grow by exactly `suffix.len()` bytes, and the
/// slice length is unchanged. Catches a repair that mutates a copy, or only
/// one element.
#[test]
fn every_element_grows_by_the_suffix_and_the_length_is_unchanged() {
    let originals = ["x".to_string(), "x".to_string(), String::new()];
    let mut labels = originals.to_vec();
    append_suffix(&mut labels, "!!");
    assert_eq!(labels.len(), originals.len(), "length must not change");
    for (index, original) in originals.iter().enumerate() {
        assert_eq!(
            labels[index].len(),
            original.len() + 2,
            "label {index} must grow by exactly the suffix"
        );
        assert!(
            labels[index].starts_with(original.as_str()),
            "label {index} must keep its original text"
        );
        assert!(
            labels[index].ends_with("!!"),
            "label {index} must end with the suffix"
        );
    }
    assert_eq!(labels[2], "!!", "an empty label becomes just the suffix");
}

/// Boundary cases written into the loop itself: an empty slice never enters
/// the body, and pushing an empty suffix changes nothing.
#[test]
fn empty_slice_and_empty_suffix_are_no_ops() {
    let mut empty: Vec<String> = Vec::new();
    append_suffix(&mut empty, "-tag");
    assert!(empty.is_empty(), "an empty slice stays empty");

    let mut labels = vec!["kept".to_string()];
    append_suffix(&mut labels, "");
    assert_eq!(
        labels,
        vec!["kept".to_string()],
        "empty suffix changes nothing"
    );
}

/// The mutation accumulates because it appends rather than replaces, so two
/// calls leave two suffixes. Catches a repair that assigns the suffix over
/// the label instead of appending.
#[test]
fn repeated_calls_accumulate_suffixes() {
    let mut labels = vec!["log".to_string(), "run".to_string()];
    append_suffix(&mut labels, ".");
    append_suffix(&mut labels, ".");
    assert_eq!(labels, vec!["log..".to_string(), "run..".to_string()]);
}
