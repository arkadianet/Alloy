pub fn bump_first() -> i32 {
    let mut scores = vec![1, 2, 3];
    let head = &scores[0];
    scores.push(4);
    *head + scores.len() as i32
}
