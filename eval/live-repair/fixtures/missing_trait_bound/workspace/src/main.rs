fn dup<T>(t: T) -> (T, T) {
    (t.clone(), t)
}

fn main() {
    let (a, b) = dup(5i32);
    println!("{a} {b}");
}
