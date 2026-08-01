//! Hidden semantic oracle for `e0382_move_in_loop_01`.
//!
//! Every assertion is derivable from the broken source alone. The loop body
//! starts each `line` from `prefix`, appends the current `entry`, and pushes
//! it — once per entry, in iteration order. Nothing else is asserted.

use e0382_move_in_loop_01::label_all;

/// Each output is `<prefix><entry>`, one per entry, in input order.
#[test]
fn prefixes_every_entry_in_order() {
    let labelled = label_all(String::from("id-"), &["a", "b", "c"]);
    assert_eq!(labelled, vec!["id-a", "id-b", "id-c"]);
}

/// Exactly one output per input: the loop pushes once per iteration, so an
/// empty input yields an empty vector and the lengths always agree.
#[test]
fn produces_one_output_per_entry() {
    assert!(
        label_all(String::from("id-"), &[]).is_empty(),
        "no entries means no outputs"
    );
    for count in [1usize, 2, 5] {
        let entries = vec!["x"; count];
        assert_eq!(
            label_all(String::from("id-"), &entries).len(),
            count,
            "one output per entry"
        );
    }
}

/// The prefix is prepended and nothing is inserted between it and the entry:
/// an empty prefix leaves the entries verbatim, and an empty entry leaves the
/// prefix alone.
#[test]
fn joins_prefix_and_entry_with_nothing_between_them() {
    assert_eq!(
        label_all(String::new(), &["alpha", "", "beta"]),
        vec!["alpha", "", "beta"],
        "an empty prefix must not alter the entries"
    );
    assert_eq!(
        label_all(String::from("p"), &["", "q"]),
        vec!["p", "pq"],
        "an empty entry yields the bare prefix"
    );
}

/// Each iteration starts a fresh line from `prefix`, so results do not
/// accumulate: labelling the whole slice at once equals labelling each entry
/// on its own. This is what the per-iteration `let mut line` expresses.
#[test]
fn entries_are_labelled_independently() {
    let entries = ["one", "two", "three", "four"];
    let together = label_all(String::from(">> "), &entries);
    for (index, entry) in entries.iter().enumerate() {
        let alone = label_all(String::from(">> "), &[entry]);
        assert_eq!(
            together[index], alone[0],
            "entry {index} must not depend on the entries before it"
        );
    }
}
