pub fn first_and_len(items: Vec<String>) -> (String, usize) {
    let first = items.into_iter().next().unwrap_or_default();
    (first, items.len())
}
