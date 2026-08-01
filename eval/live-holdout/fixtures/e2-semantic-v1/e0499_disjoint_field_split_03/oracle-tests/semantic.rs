//! Hidden semantic oracle for `e0499_disjoint_field_split_03`.
//!
//! Every assertion here is derivable from the broken source alone: `adjust`
//! writes `*counter += delta` at `self.counters[index]` and records the same
//! `delta` through `push_audit`, whose body is `self.audit.push(delta)`. The
//! in-range index is a stated caller precondition, so it is not tested.

use e0499_disjoint_field_split_03::Ledger;

/// `*counter += delta` lands on the indexed counter and on no other one.
#[test]
fn adjust_applies_delta_to_the_indexed_counter_only() {
    let mut ledger = Ledger::new(vec![10, 20, 30]);
    ledger.adjust(1, -5);
    assert_eq!(
        ledger.counters(),
        &[10, 15, 30],
        "only the indexed counter may move, and by exactly delta"
    );
}

/// Each `adjust` records its own `delta` once, oldest first — the audit log is
/// an append-only trail, not a summary.
#[test]
fn adjust_records_one_audit_entry_per_call_in_order() {
    let mut ledger = Ledger::new(vec![0, 0]);
    ledger.adjust(0, 3);
    ledger.adjust(1, 7);
    ledger.adjust(0, -1);
    assert_eq!(
        ledger.audit(),
        &[3, 7, -1],
        "every adjustment must be recorded exactly once, in call order"
    );
}

/// Repeated adjustments accumulate on the counter, and the set of counters
/// never grows or shrinks — `adjust` only writes through an existing index.
#[test]
fn repeated_adjustments_accumulate_without_resizing_the_counters() {
    let mut ledger = Ledger::new(vec![1, 1]);
    ledger.adjust(0, 4);
    ledger.adjust(0, 4);
    ledger.adjust(0, 0);
    assert_eq!(ledger.counters().len(), 2, "counter count must not change");
    assert_eq!(ledger.counters()[0], 9, "deltas must accumulate");
    assert_eq!(ledger.counters()[1], 1, "the other counter is untouched");
}

/// A fresh ledger has an empty log, and `push_audit` on its own appends to the
/// log while leaving every counter alone.
#[test]
fn push_audit_records_without_touching_the_counters() {
    let mut ledger = Ledger::new(vec![5, 6]);
    assert_eq!(ledger.audit(), &[] as &[i64], "a new ledger logs nothing");
    ledger.push_audit(42);
    assert_eq!(ledger.audit(), &[42], "push_audit appends its argument");
    assert_eq!(
        ledger.counters(),
        &[5, 6],
        "push_audit must not change any counter"
    );
}
