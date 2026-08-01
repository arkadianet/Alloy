pub fn normalized() -> i32 {
    let mut base = 40;
    let read = &base;
    let writer = &mut base;
    *writer += 2;
    *read
}
