//! Hidden semantic oracle for `e0596_shared_self_method_01`.
//!
//! Every assertion is derivable from the broken source alone. That source
//! declares an append-only log whose `record` body is
//! `self.entries.push(entry.to_string())`, with `len`, `is_empty` and
//! `entries` reading the same vec. Nothing beyond that is required.

use e0596_shared_self_method_01::EventLog;

fn as_strs(log: &EventLog) -> Vec<&str> {
    log.entries().iter().map(String::as_str).collect()
}

/// `new` builds the struct from `Vec::new()`, and the source derives
/// `Default`; both start with nothing recorded.
#[test]
fn a_fresh_log_is_empty() {
    let log = EventLog::new();
    assert_eq!(log.len(), 0, "a new log has no entries");
    assert!(log.is_empty(), "a new log reports is_empty");
    assert!(as_strs(&log).is_empty(), "a new log exposes no entries");
    assert_eq!(EventLog::default().len(), 0, "default is also empty");
}

/// `record` pushes onto the end, so entries come back in call order and each
/// call is observable. A repair that compiles but never performs the push
/// fails here.
#[test]
fn record_appends_each_entry_in_call_order() {
    let mut log = EventLog::new();
    log.record("boot");
    log.record("connect");
    log.record("shutdown");
    assert_eq!(as_strs(&log), ["boot", "connect", "shutdown"]);
    assert_eq!(log.len(), 3, "one entry per record call");
    assert!(!log.is_empty(), "a log with entries is not empty");
}

/// `Vec::push` never deduplicates and stores the string verbatim, so repeats
/// and the empty string are all retained. Catches a repair that swaps the vec
/// for a set or filters what it stores.
#[test]
fn repeats_and_empty_entries_are_retained_verbatim() {
    let mut log = EventLog::new();
    log.record("tick");
    log.record("tick");
    log.record("");
    log.record("tick");
    assert_eq!(as_strs(&log), ["tick", "tick", "", "tick"]);
    assert_eq!(log.len(), 4, "duplicates are not collapsed");
}

/// The three readers all view the same vec, so they must agree after every
/// single record. Catches a repair that updates a counter without storing the
/// entry, or stores without counting.
#[test]
fn readers_stay_consistent_after_every_record() {
    let mut log = EventLog::new();
    for (index, name) in ["one", "two", "three", "four"].iter().enumerate() {
        log.record(name);
        assert_eq!(log.len(), index + 1, "len tracks the number of records");
        assert_eq!(
            log.entries().len(),
            log.len(),
            "entries and len must agree at step {index}"
        );
        assert_eq!(log.is_empty(), log.len() == 0, "is_empty must match len");
        assert_eq!(
            log.entries()[index],
            *name,
            "the newest entry is appended last"
        );
    }
}
