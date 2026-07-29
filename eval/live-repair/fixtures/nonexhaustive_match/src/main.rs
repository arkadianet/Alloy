enum Dir {
    North,
    South,
    East,
    West,
}

fn name(d: Dir) -> &'static str {
    match d {
        Dir::North => "n",
        Dir::South => "s",
    }
}

fn main() {
    println!("{}", name(Dir::East));
}
