pub fn broken() -> i32 {
    let mut x = 1;
    let r = &mut x;
    let y = &x;
    *r += 1;
    *y
}
