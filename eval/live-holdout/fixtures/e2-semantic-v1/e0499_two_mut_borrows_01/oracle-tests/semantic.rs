//! Hidden semantic oracle for `e0499_two_mut_borrows_01`.
//!
//! Every assertion here is derivable from the broken source alone: it writes
//! `*source -= amount` and `*destination += amount` against the balance slice
//! and touches nothing else. Nothing beyond that arithmetic is required.

use e0499_two_mut_borrows_01::transfer;

/// The two named endpoints move by exactly `amount`, in opposite directions.
#[test]
fn debits_the_source_and_credits_the_destination() {
    let mut balances = [100, 20];
    transfer(&mut balances, 0, 1, 30);
    assert_eq!(balances[0], 70, "source must be debited by amount");
    assert_eq!(balances[1], 50, "destination must be credited by amount");
}

/// The paired debit and credit conserve the total; a repair that only moves
/// one side, or that scales either side, breaks this invariant.
#[test]
fn conserves_the_total_across_the_slice() {
    let mut balances = [7, -3, 11, 0];
    let before: i64 = balances.iter().sum();
    transfer(&mut balances, 2, 3, 4);
    assert_eq!(
        balances.iter().sum::<i64>(),
        before,
        "total must be conserved"
    );
}

/// Only the two indexed balances are written; length and every other element
/// are untouched.
#[test]
fn leaves_unrelated_balances_and_length_untouched() {
    let mut balances = [1, 2, 3, 4, 5];
    transfer(&mut balances, 1, 3, 2);
    assert_eq!(balances.len(), 5, "length must not change");
    assert_eq!(
        [balances[0], balances[2], balances[4]],
        [1, 3, 5],
        "untouched indices must keep their values"
    );
}

/// `-= 0` then `+= 0` is a no-op, and the direction reverses when `from` and
/// `to` are swapped — both follow directly from the written arithmetic.
#[test]
fn zero_is_a_no_op_and_direction_follows_the_indices() {
    let mut balances = [9, 4];
    transfer(&mut balances, 0, 1, 0);
    assert_eq!(balances, [9, 4], "a zero transfer must change nothing");
    transfer(&mut balances, 1, 0, 4);
    assert_eq!(balances, [13, 0], "from is debited, to is credited");
}
