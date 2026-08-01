use e0502_holdout_02::normalized;

#[test]
fn returns_the_original_value() {
    assert_eq!(normalized(), 40);
}
