use e0308_holdout_01::retry_limit;

#[test]
fn returns_enabled_and_disabled_limits() {
    assert_eq!(retry_limit(true), 3);
    assert_eq!(retry_limit(false), 0);
}
