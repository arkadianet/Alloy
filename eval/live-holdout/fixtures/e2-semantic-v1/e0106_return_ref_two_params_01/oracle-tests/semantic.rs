//! Hidden semantic oracle for `e0106_return_ref_two_params_01`.
//!
//! Every assertion is derivable from the broken source alone. The body is
//! `if right < left { right } else { left }`, and the doc comment states that
//! the result is "borrowed from the argument it came from" with ties going to
//! `left`. Nothing beyond that comparison and that borrow is asserted here.

use e0106_return_ref_two_params_01::first_alphabetically;

/// `if right < left { right } else { left }` selects the smaller of the two,
/// whichever side it is on.
#[test]
fn picks_the_name_that_sorts_first() {
    assert_eq!(first_alphabetically("kiwi", "apple"), "apple");
    assert_eq!(first_alphabetically("apple", "kiwi"), "apple");
    assert_eq!(first_alphabetically("Zeta", "alpha"), "Zeta");
}

/// The comparison is `right < left`, so equal names fall through to the
/// `else` arm: the doc comment's "ties are broken in favour of `left`".
/// Two separately allocated strings with the same text make the choice
/// observable.
#[test]
fn ties_return_the_left_argument() {
    let left = String::from("delta");
    let right = String::from("delta");
    let got = first_alphabetically(left.as_str(), right.as_str());
    assert_eq!(got, "delta", "a tie still yields the shared text");
    assert_eq!(
        got.as_ptr(),
        left.as_str().as_ptr(),
        "a tie must return the left argument itself"
    );
}

/// The result is "borrowed from the argument it came from", so it must be the
/// winning argument itself and not a fresh string with the same contents.
#[test]
fn the_result_borrows_the_winning_argument() {
    let early = String::from("amber");
    let late = String::from("violet");

    let got = first_alphabetically(early.as_str(), late.as_str());
    assert_eq!(
        got.as_ptr(),
        early.as_str().as_ptr(),
        "the left argument won, so the result must borrow it"
    );

    let got = first_alphabetically(late.as_str(), early.as_str());
    assert_eq!(
        got.as_ptr(),
        early.as_str().as_ptr(),
        "the right argument won, so the result must borrow it"
    );
}

/// Whichever side wins, it was chosen for being no greater than the other, so
/// the result can never exceed either argument — and it is always one of them
/// verbatim, never a prefix or a merged value.
#[test]
fn the_result_never_exceeds_either_argument() {
    let pairs = [
        ("app", "apple"),
        ("apple", "app"),
        ("", "anything"),
        ("anything", ""),
        ("mango", "mango"),
        ("beta", "alpha"),
    ];
    for (left, right) in pairs {
        let got = first_alphabetically(left, right);
        assert!(got <= left, "{got:?} must not sort after {left:?}");
        assert!(got <= right, "{got:?} must not sort after {right:?}");
        assert!(
            got == left || got == right,
            "{got:?} must be one of the two arguments verbatim"
        );
    }
}
