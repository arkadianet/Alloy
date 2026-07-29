//! Wilson score interval for the live-repair operator benchmark.
//!
//! Ported from `eval/live-repair/score.py::wilson`; the algebra, the `z`
//! default, and the `n == 0` degenerate case are reproduced exactly so the
//! Rust scorer and the retired Python scorer agree bit-for-bit.

use serde::{Deserialize, Serialize};

/// Two-sided 95% `z` multiplier used by the live-repair scorer.
pub const WILSON_Z_95: f64 = 1.96;

/// Wilson score interval for a binomial pass rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WilsonInterval {
    /// Lower bound.
    pub low: f64,
    /// Upper bound.
    pub high: f64,
}

impl WilsonInterval {
    /// Render as `[low,high]` with six decimal places, matching the
    /// six-decimal metric rendering used by the offline report summary.
    #[must_use]
    pub fn render(&self) -> String {
        format!("[{:.6},{:.6}]", self.low, self.high)
    }
}

/// Compute the Wilson score interval for `passed` successes out of `n` trials.
///
/// `n == 0` returns `[0.0, 0.0]`, exactly like the Python original: an empty
/// sample has no interval, and the caller is expected to surface that as
/// [`crate::MetricField::Unmeasured`] rather than as a measured zero.
///
/// Ownership: takes plain scalars and returns an owned interval.
/// Failure semantics: infallible; `passed > n` is clamped to `n`.
#[must_use]
pub fn wilson_interval(passed: u32, n: u32, z: f64) -> WilsonInterval {
    if n == 0 {
        return WilsonInterval {
            low: 0.0,
            high: 0.0,
        };
    }
    let n_f = f64::from(n);
    let p = f64::from(passed.min(n)) / n_f;
    let denominator = 1.0 + z * z / n_f;
    let centre = p + z * z / (2.0 * n_f);
    let margin = z * (p * (1.0 - p) / n_f + z * z / (4.0 * n_f * n_f)).sqrt();
    WilsonInterval {
        low: (centre - margin) / denominator,
        high: (centre + margin) / denominator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values produced by running `eval/live-repair/score.py`'s
    /// `wilson()` before it was retired, at its default `z = 1.96`.
    const PYTHON_REFERENCE: &[(u32, u32, f64, f64)] = &[
        (0, 0, 0.0, 0.0),
        (0, 10, -2.005_228_847_505_267_7e-17, 0.277_540_168_766_616_5),
        (1, 3, 0.061_490_315_276_160_515, 0.792_345_044_873_512_1),
        (5, 10, 0.236_589_593_615_487_3, 0.763_410_406_384_512_7),
        (7, 10, 0.396_773_219_979_565_2, 0.892_210_712_513_788),
        (10, 10, 0.722_459_831_233_383_4, 1.0),
        (100, 100, 0.963_005_192_523_998, 0.999_999_999_999_999_8),
        (3, 100, 0.010_254_338_223_414_811, 0.084_520_780_804_026_99),
    ];

    #[test]
    fn wilson_matches_ported_python_cases() {
        for &(passed, n, low, high) in PYTHON_REFERENCE {
            let actual = wilson_interval(passed, n, WILSON_Z_95);
            assert!(
                (actual.low - low).abs() < 1e-12,
                "low mismatch for {passed}/{n}: {} vs {low}",
                actual.low
            );
            assert!(
                (actual.high - high).abs() < 1e-12,
                "high mismatch for {passed}/{n}: {} vs {high}",
                actual.high
            );
        }
    }

    #[test]
    fn wilson_empty_sample_is_degenerate() {
        let interval = wilson_interval(0, 0, WILSON_Z_95);
        assert_eq!(interval.low, 0.0);
        assert_eq!(interval.high, 0.0);
    }

    #[test]
    fn wilson_bounds_are_ordered_and_finite() {
        for n in 1_u32..=64 {
            for passed in 0..=n {
                let interval = wilson_interval(passed, n, WILSON_Z_95);
                assert!(interval.low.is_finite() && interval.high.is_finite());
                assert!(interval.low <= interval.high, "{passed}/{n}");
                assert!(interval.high <= 1.0 + 1e-12, "{passed}/{n}");
            }
        }
    }

    #[test]
    fn wilson_clamps_impossible_pass_count() {
        assert_eq!(
            wilson_interval(20, 10, WILSON_Z_95),
            wilson_interval(10, 10, WILSON_Z_95)
        );
    }

    #[test]
    fn wilson_render_is_six_decimals() {
        assert_eq!(
            wilson_interval(10, 10, WILSON_Z_95).render(),
            "[0.722460,1.000000]"
        );
    }
}
