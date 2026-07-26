pub fn broken_total() -> i32 {
    let mut total = 10;
    let writer = &mut total;
    let snapshot = &total;
    *writer += 5;
    *snapshot
}
