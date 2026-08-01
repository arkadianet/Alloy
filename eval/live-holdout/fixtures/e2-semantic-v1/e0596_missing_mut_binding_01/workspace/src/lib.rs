/// Returns the running totals of `values`.
///
/// Element `i` of the result is the sum of `values[..=i]`, so the result
/// always has the same length as `values`.
pub fn running_totals(values: &[i64]) -> Vec<i64> {
    let totals = Vec::new();
    let mut sum: i64 = 0;
    for value in values {
        sum += value;
        totals.push(sum);
    }
    totals
}
