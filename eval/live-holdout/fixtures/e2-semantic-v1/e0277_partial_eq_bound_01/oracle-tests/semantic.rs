//! Hidden semantic oracle for `e0277_partial_eq_bound_01`.
//!
//! Every assertion is read straight off the broken source: `allowed` starts
//! empty, the loop walks `items` in order, and an item is cloned into the
//! output exactly when `blocked.contains(item)` is false. `contains` is a
//! by-value membership test over the whole `blocked` slice. The signature is
//! `retain_allowed<T: Clone>(&[T], &[T]) -> Vec<T>`, so `T` stays generic, and
//! the doc comment states the result "keeps their original relative order" and
//! that repeats are kept: "it never sorts and never de-duplicates".

use e0277_partial_eq_bound_01::retain_allowed;

/// Drops exactly the blocked values and keeps everything else in slice order.
#[test]
fn removes_blocked_items_and_keeps_the_rest_in_order() {
    let kept = retain_allowed(&[3, 1, 4, 1, 5], &[1]);
    assert_eq!(kept, vec![3, 4, 5]);
}

/// The loop pushes once per surviving element and never reorders, so repeats
/// survive and an unsorted input stays unsorted.
#[test]
fn keeps_duplicates_and_never_sorts() {
    let kept = retain_allowed(&["pear", "apple", "fig", "apple"], &["fig"]);
    assert_eq!(kept, vec!["pear", "apple", "apple"]);

    let numbers = retain_allowed(&[9, 2, 9, 5, 2], &[]);
    assert_eq!(numbers, vec![9, 2, 9, 5, 2]);
}

/// `contains` scans all of `blocked`, not just its first entry, and matches by
/// value wherever the item appears in `items`.
#[test]
fn blocks_on_any_entry_of_the_blocked_slice() {
    let kept = retain_allowed(&[1, 2, 3, 4, 5, 2], &[5, 2, 8]);
    assert_eq!(kept, vec![1, 3, 4]);
}

/// Degenerate inputs follow directly from the loop: nothing blocked means a
/// full copy, and no items means the initial empty vector.
#[test]
fn empty_inputs_behave_as_the_loop_implies() {
    assert_eq!(retain_allowed(&[7, 8], &[]), vec![7, 8]);
    assert_eq!(retain_allowed(&[] as &[i32], &[7]), Vec::<i32>::new());
    assert_eq!(retain_allowed(&[4, 4], &[4]), Vec::<i32>::new());
}

/// `T` is a type parameter in the broken signature, so the function must stay
/// generic over caller-defined item types instead of being narrowed to a
/// concrete one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Sku(u32);

#[test]
fn stays_generic_over_caller_defined_item_types() {
    let items = [Sku(30), Sku(10), Sku(30), Sku(20)];
    let kept = retain_allowed(&items, &[Sku(20)]);
    assert_eq!(kept, vec![Sku(30), Sku(10), Sku(30)]);
}
