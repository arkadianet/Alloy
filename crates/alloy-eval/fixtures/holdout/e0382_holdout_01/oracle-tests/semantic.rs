use e0382_holdout_01::first_and_len;

#[test]
fn preserves_first_item_and_original_length() {
    let items = vec!["alpha".to_owned(), "beta".to_owned()];
    assert_eq!(first_and_len(items), ("alpha".to_owned(), 2));
}
