/// Appends one new reading to `series`.
///
/// The newest reading is first clamped down to `cap` when it exceeds `cap`,
/// and the appended reading is that (possibly clamped) newest reading plus
/// `delta`.
///
/// Callers must pass a non-empty `series`.
pub fn extend_clamped(series: &mut Vec<i64>, cap: i64, delta: i64) {
    let newest = series.last_mut().expect("series must not be empty");
    if *newest > cap {
        *newest = cap;
    }
    series.push(*newest + delta);
}
