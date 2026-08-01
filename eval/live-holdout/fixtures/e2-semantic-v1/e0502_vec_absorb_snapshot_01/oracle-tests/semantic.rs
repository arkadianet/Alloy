//! Hidden semantic oracle for `e0502_vec_absorb_snapshot_01`.
//!
//! Every assertion is read straight off the broken source: it snapshots the
//! queue, calls `queue.extend_from_slice(incoming)`, and returns the snapshot.
//! Nothing beyond that is asserted.

use e0502_vec_absorb_snapshot_01::absorb;

fn tasks(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// From `let previous = &*queue; ... previous.clone()`: the return is the
/// queue's contents from before the extend, in their original order.
#[test]
fn returns_the_queue_contents_from_before_the_extend() {
    let mut queue = tasks(&["build", "test"]);
    let previous = absorb(&mut queue, &tasks(&["ship"]));
    assert_eq!(previous, tasks(&["build", "test"]), "snapshot is pre-call");
}

/// From `extend_from_slice`: incoming lands after the existing entries and
/// the queue grows by exactly `incoming.len()`.
#[test]
fn appends_incoming_after_the_existing_entries() {
    let mut queue = tasks(&["build", "test"]);
    absorb(&mut queue, &tasks(&["ship", "announce"]));
    assert_eq!(
        queue,
        tasks(&["build", "test", "ship", "announce"]),
        "incoming is appended in order, existing entries keep their places"
    );
    assert_eq!(queue.len(), 4, "queue grows by exactly incoming.len()");
}

/// Extending by an empty slice writes nothing, so the queue is unchanged and
/// the snapshot still reports every entry.
#[test]
fn empty_incoming_leaves_the_queue_unchanged() {
    let mut queue = tasks(&["only"]);
    let previous = absorb(&mut queue, &[]);
    assert_eq!(queue, tasks(&["only"]), "empty incoming is a no-op");
    assert_eq!(previous, tasks(&["only"]), "snapshot still sees the entry");
}

/// The snapshot is taken fresh on each call, so successive absorbs report the
/// growing prefix rather than the original queue.
#[test]
fn repeated_absorb_reports_the_growing_prefix() {
    let mut queue = tasks(&["a"]);
    let first = absorb(&mut queue, &tasks(&["b"]));
    let second = absorb(&mut queue, &tasks(&["c"]));
    assert_eq!(first, tasks(&["a"]), "first snapshot is the original queue");
    assert_eq!(second, tasks(&["a", "b"]), "second snapshot includes b");
    assert_eq!(queue, tasks(&["a", "b", "c"]), "queue accumulates in order");
}

/// An empty queue snapshots as empty, and the incoming tasks are stored
/// verbatim — values are copied, not renamed or deduplicated.
#[test]
fn absorbing_into_an_empty_queue_stores_the_tasks_verbatim() {
    let mut queue: Vec<String> = Vec::new();
    let previous = absorb(&mut queue, &tasks(&["dup", "dup", ""]));
    assert!(previous.is_empty(), "an empty queue snapshots as empty");
    assert_eq!(
        queue,
        tasks(&["dup", "dup", ""]),
        "tasks are stored verbatim, duplicates and empties included"
    );
}
