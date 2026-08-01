//! Hidden semantic oracle for `e0382_partial_move_field_01`.
//!
//! Every assertion is derivable from the broken source alone. `describe`
//! binds `name` from `command.name` and formats
//! `"{} takes {} arg(s)"` from `command.name` and `command.args.len()`,
//! returning `(name, summary)` in that order. Nothing else is asserted.

use e0382_partial_move_field_01::{describe, Command};

/// The returned pair is `(name, summary)` and the summary is exactly
/// `<name> takes <n> arg(s)` — read straight off the `format!` string.
#[test]
fn returns_the_name_and_the_formatted_summary() {
    let command = Command {
        name: String::from("deploy"),
        args: vec![String::from("--fast"), String::from("prod")],
    };
    let (name, summary) = describe(command);
    assert_eq!(name, "deploy", "first element is the command name");
    assert_eq!(summary, "deploy takes 2 arg(s)");
}

/// `<n>` is `args.len()` — the number of arguments, not their combined
/// length and not a fixed value. Two empty arguments still count as two.
#[test]
fn counts_arguments_not_their_contents() {
    let none = Command {
        name: String::from("status"),
        args: Vec::new(),
    };
    assert_eq!(describe(none).1, "status takes 0 arg(s)");

    let two_empty = Command {
        name: String::from("status"),
        args: vec![String::new(), String::new()],
    };
    assert_eq!(
        describe(two_empty).1,
        "status takes 2 arg(s)",
        "empty arguments still count"
    );
}

/// The name is carried through verbatim on both sides, including characters
/// a repair might trim, split on, or normalise.
#[test]
fn carries_the_name_verbatim() {
    let command = Command {
        name: String::from(" git commit "),
        args: vec![String::from("-m")],
    };
    let (name, summary) = describe(command);
    assert_eq!(name, " git commit ");
    assert_eq!(summary, " git commit  takes 1 arg(s)");
}

/// The two returned values agree: the summary begins with the returned name
/// followed by " takes ". Both are built from `command.name`, so any repair
/// that sources one of them from somewhere else breaks this.
#[test]
fn summary_is_prefixed_by_the_returned_name() {
    for (name, arg_count) in [("a", 0usize), ("build", 3), ("run-tests", 1)] {
        let command = Command {
            name: String::from(name),
            args: vec![String::from("x"); arg_count],
        };
        let (returned, summary) = describe(command);
        assert_eq!(
            summary,
            format!("{returned} takes {arg_count} arg(s)"),
            "summary must be built from the same name it returns"
        );
    }
}
