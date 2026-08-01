//! Hidden semantic oracle for `e0308_int_width_return_01`.
//!
//! Every assertion is derivable from the broken source alone: it accumulates
//! `u64::from(len)` over `frame_lengths` into a `u64` and reports that total,
//! and its doc comment states the total is reported as a 64-bit count so a long
//! run of maximum-size frames cannot overflow it. Nothing else is asserted.
//!
//! The expected values are written as unsuffixed integer literals so that any
//! integer type wide enough to hold the stated total satisfies the oracle; only
//! a repair that narrows the reported total is rejected.

use e0308_int_width_return_01::total_bytes;

/// `total` starts at 0 and the loop body never runs for an empty slice.
#[test]
fn empty_input_totals_zero() {
    let frames: [u32; 0] = [];
    assert_eq!(total_bytes(&frames), 0, "no frames means no bytes");
}

/// Straight readback of `total += u64::from(len)`: every element contributes
/// its own value once, including repeats.
#[test]
fn sums_every_frame_exactly_once() {
    assert_eq!(total_bytes(&[1, 2, 3]), 6, "each length is added once");
    assert_eq!(total_bytes(&[3, 3]), 6, "repeated lengths are both counted");
    assert_eq!(total_bytes(&[u32::MAX]), 4_294_967_295, "single max frame");
}

/// The documented reason the accumulator is `u64`: a run of maximum-size frames
/// must be reported exactly, not wrapped or truncated into 32 bits.
#[test]
fn wide_totals_are_reported_without_truncation() {
    assert_eq!(
        total_bytes(&[u32::MAX, u32::MAX]),
        8_589_934_590,
        "a total above u32::MAX must survive intact"
    );
    assert_eq!(
        total_bytes(&[u32::MAX, 1]),
        4_294_967_296,
        "crossing the 32-bit boundary must not wrap to 0"
    );
}

/// Addition is associative and commutative, so splitting or reordering the
/// slice cannot change the reported total.
#[test]
fn total_is_additive_and_order_independent() {
    let head = [7u32, 11, 13];
    let tail = [u32::MAX, 5];
    let all = [7u32, 11, 13, u32::MAX, 5];
    let reversed = [5u32, u32::MAX, 13, 11, 7];
    assert_eq!(
        total_bytes(&all),
        total_bytes(&head) + total_bytes(&tail),
        "splitting the slice must not change the total"
    );
    assert_eq!(
        total_bytes(&all),
        total_bytes(&reversed),
        "order must not change the total"
    );
}
