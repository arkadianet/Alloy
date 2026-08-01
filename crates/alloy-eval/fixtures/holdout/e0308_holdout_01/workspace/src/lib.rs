pub fn retry_limit(enabled: bool) -> u32 {
    if enabled {
        3
    } else {
        "0"
    }
}
