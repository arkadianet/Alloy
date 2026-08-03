//! Hidden semantic oracle for `e0502_preserve_old_01`.
//!
//! Everything asserted here is readable off the broken source alone: the doc
//! comment on `deposit` says it returns "the balance the account held before
//! this deposit was applied", and `credit` is written as
//! `balances[index] += amount`. No other behaviour is required of a repair.

use e0502_preserve_old_01::deposit;

/// From the doc comment ("returns the *opening* balance") plus `credit`'s
/// `+= amount`: the call reports the pre-call balance and leaves the
/// post-call balance raised by exactly `amount`.
#[test]
fn returns_the_opening_balance_and_credits_the_account() {
    let mut balances = [100, 20];
    let opening = deposit(&mut balances, 0, 30);
    assert_eq!(
        opening, 100,
        "must report the balance from before the deposit"
    );
    assert_eq!(balances[0], 130, "the account must be credited by amount");
}

/// The returned value is the *previous* balance, so a chain of deposits
/// reports the running total lagging one step behind. A repair that reads the
/// slot after mutating instead of before cannot produce this sequence.
#[test]
fn consecutive_deposits_report_the_previous_balance_each_time() {
    let mut balances = [0];
    let first = deposit(&mut balances, 0, 5);
    let second = deposit(&mut balances, 0, 7);
    let third = deposit(&mut balances, 0, 1);
    assert_eq!(
        [first, second, third],
        [0, 5, 12],
        "each call must report the balance as it stood on entry"
    );
    assert_eq!(balances[0], 13, "the deposits must all have been applied");
}

/// `credit` writes one index only, so every other account and the slice
/// length are untouched.
#[test]
fn leaves_other_accounts_and_length_untouched() {
    let mut balances = [1, 2, 3, 4];
    let opening = deposit(&mut balances, 2, 10);
    assert_eq!(
        opening, 3,
        "must report the addressed account's old balance"
    );
    assert_eq!(balances.len(), 4, "length must not change");
    assert_eq!(
        [balances[0], balances[1], balances[3]],
        [1, 2, 4],
        "unaddressed accounts must keep their values"
    );
}

/// Straight from `+= amount`: zero changes nothing, a negative amount debits,
/// and in both cases the reported value is still the opening balance.
#[test]
fn zero_and_negative_amounts_follow_the_written_arithmetic() {
    let mut balances = [40, -5];
    assert_eq!(
        deposit(&mut balances, 0, 0),
        40,
        "zero reports the opening balance"
    );
    assert_eq!(balances[0], 40, "a zero deposit must change nothing");
    assert_eq!(
        deposit(&mut balances, 1, -3),
        -5,
        "reports the opening balance"
    );
    assert_eq!(balances[1], -8, "a negative amount must debit the account");
}
