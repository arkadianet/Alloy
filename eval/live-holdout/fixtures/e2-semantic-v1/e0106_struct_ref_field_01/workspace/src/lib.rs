/// A borrowed run of sensor readings together with the factor they scale by.
///
/// A `Series` never owns its readings: it borrows the caller's slice for as
/// long as the series is alive.
pub struct Series {
    readings: &[i32],
    scale: i32,
}

/// Builds a series that borrows `readings` and scales each one by `scale`.
pub fn series(readings: &[i32], scale: i32) -> Series {
    Series { readings, scale }
}

/// The borrowed readings, exactly as they were supplied.
pub fn readings(series: &Series) -> &[i32] {
    series.readings
}

/// Sum of every reading multiplied by the series scale.
pub fn scaled_total(series: &Series) -> i32 {
    series.readings.iter().map(|r| r * series.scale).sum()
}

/// How many readings the series covers.
pub fn count(series: &Series) -> usize {
    series.readings.len()
}
