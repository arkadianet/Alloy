//! Token estimation without a tokenizer (RFC-0012 §3.6, §6.2).

/// Estimates prompt cost without a tokenizer. **Estimates only** (B2).
pub trait TokenEstimator: Send + Sync + std::fmt::Debug {
    /// Estimated input tokens for `s`. MUST be monotonic in `s.len()` (B13).
    fn estimate(&self, s: &str) -> usize;

    /// Stable identifier recorded in the domain manifest (§7.3).
    fn id(&self) -> &'static str;
}

/// Bytes-per-token heuristic. The only MVP implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytesPerTokenEstimator {
    /// Divisor applied to UTF-8 byte length. Default `4`.
    pub bytes_per_token: u32,
}

impl Default for BytesPerTokenEstimator {
    fn default() -> Self {
        Self { bytes_per_token: 4 }
    }
}

impl TokenEstimator for BytesPerTokenEstimator {
    /// `s.len().div_ceil(bytes_per_token)` over UTF-8 **bytes**, never chars.
    fn estimate(&self, s: &str) -> usize {
        s.len().div_ceil(self.bytes_per_token.max(1) as usize)
    }

    /// `"bytes_per_token_v1"`.
    fn id(&self) -> &'static str {
        "bytes_per_token_v1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T2a — B2.
    #[test]
    fn estimator_is_bytes_div_ceil_four() {
        let e = BytesPerTokenEstimator::default();
        assert_eq!(e.estimate(""), 0);
        assert_eq!(e.estimate("a"), 1);
        assert_eq!(e.estimate("abcd"), 1);
        assert_eq!(e.estimate("abcde"), 2);
        assert_eq!(e.estimate(&"x".repeat(4001)), 1001);
        assert_eq!(e.id(), "bytes_per_token_v1");
    }

    // T2b — B2: a 3-byte CJK char estimates as 1, not 0.
    #[test]
    fn estimator_counts_bytes_not_chars() {
        let e = BytesPerTokenEstimator::default();
        assert_eq!("好".len(), 3);
        assert_eq!(e.estimate("好"), 1);
        // Two CJK chars = 6 bytes → 2, though only 2 chars.
        assert_eq!(e.estimate("好好"), 2);
    }

    // T2c — B13.
    #[test]
    fn estimator_is_monotonic_in_length() {
        let e = BytesPerTokenEstimator::default();
        let mut prev = 0;
        for n in 0..64 {
            let est = e.estimate(&"y".repeat(n));
            assert!(est >= prev, "estimate must not shrink as input grows");
            prev = est;
        }
    }
}
