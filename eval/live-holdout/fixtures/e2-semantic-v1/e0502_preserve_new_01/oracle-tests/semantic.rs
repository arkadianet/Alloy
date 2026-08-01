//! Hidden semantic oracle for `e0502_preserve_new_01`.
//!
//! Every assertion is readable off the broken source alone. `bump`'s doc
//! comment states it "returns the value that is now stored in the counter" and
//! that the result "is never greater than `limit`"; `clamp_add` is written as
//! `counters[index] = (counters[index] + delta).min(limit)`. Nothing else is
//! required of a repair.

use e0502_preserve_new_01::bump;

/// The documented result is the post-update value, and `clamp_add` adds
/// `delta` when the sum stays under `limit`.
#[test]
fn reports_the_counter_value_after_the_update() {
    let mut counters = [5];
    let now = bump(&mut counters, 0, 3, 100);
    assert_eq!(
        now, 8,
        "must report the counter as it stands after the bump"
    );
    assert_eq!(counters[0], 8, "the bump must have been applied");
}

/// The central invariant of the doc comment: the reported value *is* the
/// stored value. Checked across a run of calls, including ones that clamp, so
/// a repair that reports the pre-update reading cannot slip through.
#[test]
fn the_reported_value_always_equals_the_stored_counter() {
    let mut counters = [0, 40];
    for (index, delta, limit) in [(0, 7, 50), (0, 7, 50), (1, 30, 50), (1, 5, 50), (0, 0, 50)] {
        let reported = bump(&mut counters, index, delta, limit);
        assert_eq!(
            reported, counters[index],
            "the returned value must be the value left in the counter"
        );
        assert!(
            reported <= limit,
            "the reported value must never exceed the limit"
        );
    }
}

/// Straight from `.min(limit)`: the sum saturates, and the saturated value is
/// what gets reported.
#[test]
fn saturates_at_the_limit() {
    let mut counters = [90];
    assert_eq!(
        bump(&mut counters, 0, 50, 100),
        100,
        "the sum must clamp to limit"
    );
    assert_eq!(counters[0], 100, "the clamped value must be stored");

    // A counter already above the limit is pulled down to it, even by a zero
    // delta, because the clamp applies to the whole sum.
    let mut over = [120];
    assert_eq!(
        bump(&mut over, 0, 0, 100),
        100,
        "an over-limit counter clamps down"
    );
    assert_eq!(over[0], 100, "the clamped value must be stored");
}

/// `clamp_add` writes one index only, so the other counters and the slice
/// length are untouched.
#[test]
fn leaves_other_counters_and_length_untouched() {
    let mut counters = [1, 2, 3, 4];
    let now = bump(&mut counters, 1, 5, 100);
    assert_eq!(
        now, 7,
        "the addressed counter must be reported after the bump"
    );
    assert_eq!(counters.len(), 4, "length must not change");
    assert_eq!(
        [counters[0], counters[2], counters[3]],
        [1, 3, 4],
        "unaddressed counters must keep their values"
    );
}
