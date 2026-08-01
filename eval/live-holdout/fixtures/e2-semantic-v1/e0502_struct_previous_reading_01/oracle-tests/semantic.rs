//! Hidden semantic oracle for `e0502_struct_previous_reading_01`.
//!
//! Every assertion is read straight off the broken source: `record` snapshots
//! `self.history.last()`, pushes the new reading, and returns the snapshot;
//! `history()` exposes the whole vector. Nothing beyond that is asserted.

use e0502_struct_previous_reading_01::{Reading, Sensor};

fn reading(label: &str, value: i64) -> Reading {
    Reading {
        label: label.to_string(),
        value,
    }
}

/// From `self.history.last()` on an empty history: the first record has no
/// predecessor, and the reading is still stored.
#[test]
fn the_first_record_has_no_previous() {
    let mut sensor = Sensor::new();
    let previous = sensor.record(reading("t0", 7));
    assert_eq!(previous, None, "there is no reading before the first one");
    assert_eq!(
        sensor.history(),
        &[reading("t0", 7)],
        "the first reading is recorded"
    );
}

/// The snapshot is taken before the push, so the second call reports the
/// first reading rather than the one being recorded.
#[test]
fn the_second_record_returns_the_first_reading() {
    let mut sensor = Sensor::new();
    sensor.record(reading("t0", 7));
    let previous = sensor.record(reading("t1", 9));
    assert_eq!(
        previous,
        Some(reading("t0", 7)),
        "previous is the earlier reading, not the one just recorded"
    );
}

/// From `self.history.push(reading)`: history accumulates every reading in
/// insertion order and grows by exactly one per call.
#[test]
fn history_keeps_every_reading_in_insertion_order() {
    let mut sensor = Sensor::new();
    sensor.record(reading("t0", 1));
    sensor.record(reading("t1", 2));
    sensor.record(reading("t2", 3));
    assert_eq!(
        sensor.history(),
        &[reading("t0", 1), reading("t1", 2), reading("t2", 3)],
        "history is oldest first with no drops or reordering"
    );
    assert_eq!(sensor.history().len(), 3, "one entry per record call");
}

/// `last()` names the most recent reading, not the oldest: after three
/// records the third call reports the second reading.
#[test]
fn previous_is_the_immediately_preceding_reading_not_the_oldest() {
    let mut sensor = Sensor::new();
    sensor.record(reading("t0", 1));
    sensor.record(reading("t1", 2));
    let previous = sensor.record(reading("t2", 3));
    assert_eq!(
        previous,
        Some(reading("t1", 2)),
        "previous is the latest prior reading"
    );
}

/// Readings are stored as given: both fields survive, and duplicate labels or
/// repeated values are neither merged nor deduplicated.
#[test]
fn readings_are_stored_verbatim_including_duplicates() {
    let mut sensor = Sensor::new();
    sensor.record(reading("dup", -5));
    let previous = sensor.record(reading("dup", -5));
    assert_eq!(
        previous,
        Some(reading("dup", -5)),
        "label and value both survive the round trip"
    );
    assert_eq!(
        sensor.history(),
        &[reading("dup", -5), reading("dup", -5)],
        "identical readings are kept separately"
    );
}
