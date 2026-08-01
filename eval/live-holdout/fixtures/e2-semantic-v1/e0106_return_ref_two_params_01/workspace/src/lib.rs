/// Returns whichever of the two names sorts first, borrowed from the argument
/// it came from.
///
/// Ties are broken in favour of `left`.
pub fn first_alphabetically(left: &str, right: &str) -> &str {
    if right < left {
        right
    } else {
        left
    }
}
