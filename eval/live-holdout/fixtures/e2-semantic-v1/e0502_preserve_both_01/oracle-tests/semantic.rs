//! Hidden semantic oracle for `e0502_preserve_both_01`.
//!
//! Everything asserted here comes from the broken source alone: `Update`
//! documents `previous` as "the channel's value before the reading was
//! written" and `current` as the value after, `delta` is written as
//! `current - previous`, and `write_capped` is `reading.min(ceiling)`.

use e0502_preserve_both_01::{record, Update};

/// Both fields of the returned `Update` straddle the single write, and the
/// slice ends up holding the value reported as `current`.
#[test]
fn reports_both_sides_of_the_write() {
    let mut channels = [10];
    let update = record(&mut channels, 0, 25, 100);
    assert_eq!(
        update,
        Update {
            previous: 10,
            current: 25
        },
        "must report the value before and after the write"
    );
    assert_eq!(channels[0], 25, "the reading must have been stored");
}

/// `previous` is the value on entry and `current` the value on exit, so across
/// a run of recordings each call's `previous` is the last call's `current`.
/// A repair that samples the channel on the wrong side of the write breaks the
/// chain at the first step.
#[test]
fn previous_chains_onto_the_last_current() {
    let mut channels = [0];
    let mut last: Option<Update> = None;
    for reading in [4, 9, 2, 9] {
        let update = record(&mut channels, 0, reading, 100);
        if let Some(previous_update) = last {
            assert_eq!(
                update.previous, previous_update.current,
                "each call must enter at the value the last call left behind"
            );
        }
        assert_eq!(
            update.current, channels[0],
            "current must be the value left in the channel"
        );
        last = Some(update);
    }
    assert_eq!(channels[0], 9, "the final reading must be stored");
}

/// From `reading.min(ceiling)`: an over-range reading is capped, and `current`
/// reports the capped value that was stored rather than the raw reading.
#[test]
fn caps_the_reading_and_reports_the_capped_value() {
    let mut channels = [7];
    let update = record(&mut channels, 0, 150, 100);
    assert_eq!(update.previous, 7, "previous is the pre-write value");
    assert_eq!(update.current, 100, "current is the capped stored value");
    assert_eq!(channels[0], 100, "the capped value must be stored");
}

/// `delta` is `current - previous`, so it measures the movement this call
/// caused; re-recording the value a channel already holds moves it by zero.
#[test]
fn delta_measures_the_movement_this_call_caused() {
    let mut channels = [5];
    let rise = record(&mut channels, 0, 12, 100);
    assert_eq!(rise.delta(), 7, "delta must be current minus previous");

    let repeat = record(&mut channels, 0, 12, 100);
    assert_eq!(
        repeat.previous, repeat.current,
        "re-recording the stored value must not move the channel"
    );
    assert_eq!(repeat.delta(), 0, "an unchanged channel has zero delta");

    let fall = record(&mut channels, 0, 3, 100);
    assert_eq!(
        fall.delta(),
        -9,
        "delta must be negative when the reading drops"
    );
}

/// `write_capped` touches one index, so every other channel and the slice
/// length survive unchanged.
#[test]
fn leaves_other_channels_and_length_untouched() {
    let mut channels = [1, 2, 3, 4];
    let update = record(&mut channels, 2, 30, 100);
    assert_eq!(
        update.previous, 3,
        "previous is the addressed channel's old value"
    );
    assert_eq!(channels.len(), 4, "length must not change");
    assert_eq!(
        [channels[0], channels[1], channels[3]],
        [1, 2, 4],
        "unaddressed channels must keep their values"
    );
}
