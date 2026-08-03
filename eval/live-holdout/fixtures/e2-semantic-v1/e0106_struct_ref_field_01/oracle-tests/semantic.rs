//! Hidden semantic oracle for `e0106_struct_ref_field_01`.
//!
//! Every assertion is derivable from the broken source alone. The struct
//! declares `readings: &[i32]` — a borrow of the caller's slice, never a copy —
//! plus an owned `scale: i32`, and the three free functions spell out their own
//! arithmetic: `series.readings`, `map(|r| r * series.scale).sum()`, and
//! `series.readings.len()`.

use e0106_struct_ref_field_01::{count, readings, scaled_total, series};

/// `readings` returns `series.readings`, and that field is declared as a
/// borrow of the caller's slice — so the result must alias the very buffer
/// that was handed to `series`, not a copy of it.
#[test]
fn readings_alias_the_caller_buffer() {
    let buf: Vec<i32> = vec![4, -1, 6];
    let s = series(&buf, 2);
    let got = readings(&s);
    assert_eq!(got, &[4, -1, 6], "the readings must come back unchanged");
    assert_eq!(
        got.as_ptr(),
        buf.as_ptr(),
        "the series must borrow the caller's slice, not copy it"
    );
}

/// `scaled_total` is written as `map(|r| r * scale).sum()`.
#[test]
fn scaled_total_multiplies_every_reading_by_the_scale() {
    let buf = [4, -1, 6];
    assert_eq!(scaled_total(&series(&buf, 2)), 18, "(4 + -1 + 6) * 2");
    assert_eq!(scaled_total(&series(&buf, -3)), -27, "(4 + -1 + 6) * -3");
    assert_eq!(
        scaled_total(&series(&buf, 0)),
        0,
        "a zero scale zeroes the sum"
    );
}

/// `count` is `readings.len()`, which is independent of the scale; an empty
/// slice has nothing to sum.
#[test]
fn count_tracks_the_slice_length_and_ignores_the_scale() {
    let buf = [10, 20, 30, 40];
    assert_eq!(count(&series(&buf, 7)), 4, "count is the slice length");
    assert_eq!(count(&series(&buf, -7)), 4, "the scale cannot change count");

    let empty: [i32; 0] = [];
    assert_eq!(count(&series(&empty, 5)), 0, "an empty series counts zero");
    assert_eq!(
        scaled_total(&series(&empty, 5)),
        0,
        "an empty series sums to zero"
    );
}

/// Multiplying each reading by the scale makes the total linear in the scale:
/// a repair that adds the scale, or applies it to only one reading, breaks
/// this even when the single-case totals happen to line up.
#[test]
fn scaled_total_is_linear_in_the_scale() {
    let buf = [3, 5, -2, 9];
    let unit = scaled_total(&series(&buf, 1));
    assert_eq!(unit, 15, "a scale of one is the plain sum");
    for k in [-4, -1, 0, 1, 6] {
        assert_eq!(
            scaled_total(&series(&buf, k)),
            k * unit,
            "the total must scale linearly with the scale factor"
        );
    }
}
