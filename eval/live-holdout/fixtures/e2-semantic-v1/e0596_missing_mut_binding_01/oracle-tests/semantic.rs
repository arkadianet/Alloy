//! Hidden semantic oracle for `e0596_missing_mut_binding_01`.
//!
//! Every assertion is derivable from the broken source alone. That source
//! starts an accumulator at `sum = 0`, walks `values` in order, does
//! `sum += value` and then `totals.push(sum)` once per element, and returns
//! `totals`. Nothing beyond that loop is required of a repair.

use e0596_missing_mut_binding_01::running_totals;

/// One push per element, each carrying the sum of everything seen so far.
/// Read straight off `sum += value; totals.push(sum);`.
#[test]
fn produces_the_inclusive_prefix_sums() {
    assert_eq!(running_totals(&[3, -1, 4, -1, 5]), vec![3, 2, 6, 5, 10]);
}

/// Structural invariant of the same loop: exactly one output element per
/// input element, and consecutive outputs differ by the input that produced
/// them. A repair that drops the push, pushes the wrong value, or reorders
/// the walk cannot satisfy this.
#[test]
fn length_matches_and_differences_recover_the_input() {
    let values: [i64; 6] = [10, 0, -4, 7, 7, -20];
    let totals = running_totals(&values);
    assert_eq!(totals.len(), values.len(), "one total per input element");

    let mut previous: i64 = 0;
    for (index, value) in values.iter().enumerate() {
        assert_eq!(
            totals[index] - previous,
            *value,
            "step {index} must add exactly the input at that position"
        );
        previous = totals[index];
    }
    assert_eq!(
        *totals.last().unwrap(),
        values.iter().sum::<i64>(),
        "the final total is the sum of every input"
    );
}

/// With no elements the loop body never runs and the freshly built vec is
/// returned as-is.
#[test]
fn empty_input_yields_an_empty_result() {
    assert!(running_totals(&[]).is_empty());
}

/// Single element and all-zero cases: the accumulator starts at 0, so one
/// element maps to itself and zeros map to zeros without changing the shape.
#[test]
fn single_element_and_zeros_keep_their_shape() {
    assert_eq!(running_totals(&[-8]), vec![-8]);
    assert_eq!(running_totals(&[0, 0, 0]), vec![0, 0, 0]);
}
