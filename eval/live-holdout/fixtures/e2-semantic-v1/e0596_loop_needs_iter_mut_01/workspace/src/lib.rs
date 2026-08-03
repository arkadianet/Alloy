/// Appends `suffix` to every label in `labels`, in place.
///
/// The labels keep their order and the slice keeps its length; only the
/// contents of each label grow.
pub fn append_suffix(labels: &mut [String], suffix: &str) {
    for label in labels.iter() {
        label.push_str(suffix);
    }
}
