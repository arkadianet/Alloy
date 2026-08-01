//! Hidden semantic oracle for `e0106_impl_lifetime_param_01`.
//!
//! Every assertion is derivable from the broken source alone. `new` stores the
//! caller's slice with `pos: 0`, `remaining` is `&self.bytes[self.pos..]`,
//! `advance` is `self.pos += n`, `position` is `self.pos`, and `is_done` is
//! `self.pos == self.bytes.len()`. The struct doc states the reader borrows the
//! caller's buffer rather than owning it.

use e0106_impl_lifetime_param_01::Reader;

/// `new` sets `pos: 0` and stores the caller's slice, so the fresh reader
/// exposes the whole buffer — and it must be *that* buffer, not a copy.
#[test]
fn new_starts_at_zero_and_borrows_the_caller_buffer() {
    let buf: Vec<u8> = vec![10, 20, 30, 40, 50];
    let reader = Reader::new(&buf);
    assert_eq!(reader.position(), 0, "a fresh reader has consumed nothing");
    assert_eq!(
        reader.remaining(),
        &buf[..],
        "everything is still remaining"
    );
    assert_eq!(
        reader.remaining().as_ptr(),
        buf.as_ptr(),
        "the reader must borrow the caller's buffer, not copy it"
    );
    assert!(
        !reader.is_done(),
        "a non-empty buffer is not done at the start"
    );
}

/// `remaining` slices from `pos`, so advancing moves the window forward over
/// the same buffer rather than producing a detached copy.
#[test]
fn advance_moves_the_window_forward_over_the_same_buffer() {
    let buf: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
    let mut reader = Reader::new(&buf);
    reader.advance(2);
    assert_eq!(reader.position(), 2, "position is the number consumed");
    assert_eq!(
        reader.remaining(),
        &buf[2..],
        "the first two bytes are gone"
    );
    assert_eq!(
        reader.remaining().as_ptr(),
        buf[2..].as_ptr(),
        "the remainder must still point into the caller's buffer"
    );
}

/// `advance` is `pos += n`, so successive calls accumulate and `advance(0)`
/// changes nothing.
#[test]
fn advances_accumulate_and_zero_is_a_no_op() {
    let buf: Vec<u8> = vec![7, 8, 9, 10, 11];
    let mut reader = Reader::new(&buf);
    reader.advance(1);
    reader.advance(0);
    reader.advance(2);
    assert_eq!(reader.position(), 3, "1 + 0 + 2 bytes consumed");
    assert_eq!(reader.remaining(), &buf[3..], "two bytes are left");

    let mut untouched = Reader::new(&buf);
    untouched.advance(0);
    assert_eq!(
        untouched.position(),
        0,
        "advancing by zero consumes nothing"
    );
    assert_eq!(
        untouched.remaining(),
        &buf[..],
        "and leaves the window whole"
    );
}

/// `is_done` compares `pos` against the buffer length, so it flips only when
/// the last byte has been consumed — and an empty buffer is done immediately.
#[test]
fn is_done_only_once_every_byte_is_consumed() {
    let buf: Vec<u8> = vec![3, 6, 9];
    let mut reader = Reader::new(&buf);
    reader.advance(2);
    assert!(!reader.is_done(), "one byte still remains");
    reader.advance(1);
    assert!(reader.is_done(), "the whole buffer has been consumed");
    assert_eq!(reader.position(), 3, "position ends at the buffer length");
    assert!(reader.remaining().is_empty(), "nothing is left to read");

    let empty: [u8; 0] = [];
    let done = Reader::new(&empty);
    assert!(done.is_done(), "an empty buffer is done before any advance");
}
