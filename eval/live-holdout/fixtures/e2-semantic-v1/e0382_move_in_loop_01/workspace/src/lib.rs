/// Labels every entry by putting `prefix` in front of it, returning one
/// `<prefix><entry>` string per entry, in the order the entries were given.
pub fn label_all(prefix: String, entries: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let mut line = prefix;
        line.push_str(entry);
        out.push(line);
    }
    out
}
