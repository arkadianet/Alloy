//! Hidden semantic oracle for `e0277_display_bound_01`.
//!
//! Every assertion is read straight off the broken source: it starts from an
//! empty `String`, walks `items.iter().enumerate()`, and pushes exactly
//! `format!("{label}[{index}] = {item}\n")` per item. The signature is
//! `render_labeled<T>(&str, &[T]) -> String`, so `T` stays generic, and the
//! doc comment says items are shown "the way a user would read them, not as a
//! debug dump" — i.e. `Display`, not `Debug`.

use std::fmt;

use e0277_display_bound_01::render_labeled;

/// The literal line shape from the format string: label, bracketed index
/// starting at zero, ` = `, the item, then a newline.
#[test]
fn renders_one_label_index_item_line_per_element() {
    let rendered = render_labeled("row", &[10, 20, -3]);
    assert_eq!(rendered, "row[0] = 10\nrow[1] = 20\nrow[2] = -3\n");
}

/// `{item}` is a `Display` slot, so a string item is rendered bare. A repair
/// that switches to `{item:?}` would quote and escape it instead.
#[test]
fn shows_items_with_display_rather_than_debug() {
    let rendered = render_labeled("w", &["alpha", "beta"]);
    assert_eq!(rendered, "w[0] = alpha\nw[1] = beta\n");
    assert!(
        !rendered.contains('"'),
        "Display formatting must not quote string items"
    );
}

/// Renders whatever the caller's own displayable type prints.
#[derive(Clone, Copy)]
struct Celsius(i32);

impl fmt::Display for Celsius {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}C", self.0)
    }
}

/// `T` is an unconstrained type parameter in the broken signature, so the
/// function must stay generic over caller-defined item types rather than being
/// narrowed to any single concrete type.
#[test]
fn stays_generic_over_caller_defined_item_types() {
    let rendered = render_labeled("temp", &[Celsius(21), Celsius(-4)]);
    assert_eq!(rendered, "temp[0] = 21C\ntemp[1] = -4C\n");
}

/// The loop body never runs for an empty slice, so the initial empty `String`
/// is returned unchanged.
#[test]
fn an_empty_slice_renders_an_empty_string() {
    let rendered: String = render_labeled("row", &[] as &[u8]);
    assert_eq!(rendered, "");
}

/// One newline-terminated line per element, in slice order, including repeats:
/// the index comes from `enumerate`, never from the item's value.
#[test]
fn preserves_order_and_multiplicity_one_line_per_item() {
    let items = [7, 7, 7, 1];
    let rendered = render_labeled("k", &items);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), items.len(), "exactly one line per item");
    assert_eq!(lines, ["k[0] = 7", "k[1] = 7", "k[2] = 7", "k[3] = 1"]);
    assert!(rendered.ends_with('\n'), "every line is newline-terminated");
}
