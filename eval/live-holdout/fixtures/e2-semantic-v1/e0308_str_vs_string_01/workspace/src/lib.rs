/// Builds a slug for `title`: ASCII letters and digits are kept in lower case,
/// and every run of other characters becomes a single `-` separator.
///
/// The slug is freshly built rather than borrowed out of `title`, and it never
/// starts or ends with a separator.
pub fn slugify(title: &str) -> &str {
    let mut slug = String::with_capacity(title.len());
    let mut separator_pending = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator_pending && !slug.is_empty() {
                slug.push('-');
            }
            separator_pending = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            separator_pending = true;
        }
    }
    slug
}
