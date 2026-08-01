use e0502_holdout_01::broken_total;

#[test]
fn preserves_snapshot_and_updated_total() {
    assert_eq!(broken_total(), 25);
}
