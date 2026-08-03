//! Hidden semantic oracle for `e0382_borrow_after_move_01`.
//!
//! Every assertion is derivable from the broken source alone. `store` writes
//! `format!("{}: {} bytes", upload.name, bytes.len())` where `bytes` is the
//! payload returned by `into_bytes`, and `into_bytes` returns `self.bytes`.
//! Nothing beyond that is asserted.

use e0382_borrow_after_move_01::{store, Upload};

/// The receipt is exactly `<name>: <n> bytes` — read straight off the
/// `format!` string in `store`.
#[test]
fn renders_the_name_then_the_payload_length() {
    let upload = Upload {
        name: String::from("notes.txt"),
        bytes: vec![1, 2, 3, 4, 5],
    };
    assert_eq!(store(upload), "notes.txt: 5 bytes");
}

/// `<n>` is `bytes.len()`, not the length of the name and not a fixed value.
/// The two cases here differ only in payload length, so any repair that
/// derives the count from something else diverges on at least one of them.
#[test]
fn counts_payload_bytes_not_name_characters() {
    let empty = Upload {
        name: String::from("aaaaaaaa"),
        bytes: Vec::new(),
    };
    assert_eq!(
        store(empty),
        "aaaaaaaa: 0 bytes",
        "empty payload is 0 bytes"
    );

    let filled = Upload {
        name: String::from("aaaaaaaa"),
        bytes: vec![0u8; 12],
    };
    assert_eq!(
        store(filled),
        "aaaaaaaa: 12 bytes",
        "count must follow the payload"
    );
}

/// The name is reproduced verbatim through `Display`, including characters
/// that a repair might normalise or escape.
#[test]
fn reproduces_the_name_verbatim() {
    let upload = Upload {
        name: String::from("réport 2026: final.tar"),
        bytes: vec![7, 7],
    };
    assert_eq!(store(upload), "réport 2026: final.tar: 2 bytes");
}

/// `into_bytes` returns `self.bytes` — same contents, same order, nothing
/// dropped or reordered.
#[test]
fn into_bytes_hands_back_the_payload_unchanged() {
    let upload = Upload {
        name: String::from("blob.bin"),
        bytes: vec![9, 0, 255, 3],
    };
    assert_eq!(upload.into_bytes(), vec![9, 0, 255, 3]);

    let empty = Upload {
        name: String::from("empty.bin"),
        bytes: Vec::new(),
    };
    assert!(empty.into_bytes().is_empty(), "empty payload stays empty");
}
