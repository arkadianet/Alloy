/// Error reported when a lookup finds no matching entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupError {
    MissingKey,
}

/// Reports the value stored beside the first entry whose key equals `key`.
///
/// Returns `Err(LookupError::MissingKey)` when no entry matches.
pub fn lookup(entries: &[(&str, i32)], key: &str) -> Result<i32, LookupError> {
    entries
        .iter()
        .find(|(entry_key, _)| *entry_key == key)
        .map(|(_, value)| *value)
}
