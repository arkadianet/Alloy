//! Hidden semantic oracle for `e0499_scope_sequenced_borrow_02`.
//!
//! Every assertion here is derivable from the broken source alone. That source
//! clamps the newest element in place with `if *newest > cap { *newest = cap; }`
//! and then appends `*newest + delta`. Nothing else is asserted; in particular
//! the empty-series case is a stated caller precondition, not a behaviour.

use e0499_scope_sequenced_borrow_02::extend_clamped;

/// The clamp is written back in place, and the appended reading is computed
/// from the clamped value — both halves of `*newest = cap` followed by
/// `series.push(*newest + delta)`.
#[test]
fn clamps_the_newest_reading_in_place_and_appends_from_it() {
    let mut series = vec![10];
    extend_clamped(&mut series, 4, 1);
    assert_eq!(
        series,
        vec![4, 5],
        "the newest reading must be clamped in place and the append must use it"
    );
}

/// The guard is a strict `>`: a reading at or below `cap` is left exactly as
/// it was, so the append is the unclamped reading plus `delta`.
#[test]
fn leaves_the_newest_reading_alone_at_or_below_the_cap() {
    let mut below = vec![3];
    extend_clamped(&mut below, 10, 2);
    assert_eq!(below, vec![3, 5], "a reading below cap must not move");

    let mut at_cap = vec![5];
    extend_clamped(&mut at_cap, 5, 0);
    assert_eq!(at_cap, vec![5, 5], "a reading equal to cap must not move");
}

/// A single `push` runs, so the length grows by exactly one and every reading
/// before the newest one is untouched.
#[test]
fn appends_exactly_one_reading_and_preserves_the_earlier_ones() {
    let mut series = vec![1, 2, 3];
    extend_clamped(&mut series, 100, 5);
    assert_eq!(series.len(), 4, "exactly one reading must be appended");
    assert_eq!(
        &series[..3],
        &[1, 2, 3],
        "readings before the newest must be untouched"
    );
    assert_eq!(series[3], 8, "the appended reading is newest + delta");
}

/// `delta` is added, not otherwise combined, so a negative `delta` subtracts
/// and it is applied on top of a clamp that already fired.
#[test]
fn delta_offsets_the_clamped_reading_and_may_be_negative() {
    let mut series = vec![8];
    extend_clamped(&mut series, 6, -2);
    assert_eq!(
        series,
        vec![6, 4],
        "clamp to 6 first, then offset by delta = -2"
    );
}
