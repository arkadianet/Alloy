//! Toolchain identity (lifted from `alloy-eval` — research §7.11 item 3).
//!
//! A compile-pass label is meaningless without the compiler that produced
//! it, and the identity cannot be backfilled after the fact. This type is
//! the seam: the composition root captures it (`rustc -V` / `cargo -V`
//! belong to the CLI/tools layer, never to this crate), the planner and
//! cache derive fingerprint digests from it, and `alloy-eval` records it in
//! fixtures.

use serde::{Deserialize, Serialize};

/// Captured Rust and Cargo toolchain identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainRecord {
    /// Toolchain channel, e.g. `1.97.1`.
    pub channel: String,
    /// `rustc -V` output captured with the recording.
    pub rustc_version: String,
    /// `cargo -V` output captured with the recording.
    pub cargo_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_and_deny_unknown_fields() {
        let record = ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1 (abcdef 2026-06-01)".into(),
            cargo_version: "cargo 1.97.1 (123456 2026-06-01)".into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: ToolchainRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        let with_extra = r#"{"channel":"x","rustc_version":"y","cargo_version":"z","zzz":1}"#;
        assert!(serde_json::from_str::<ToolchainRecord>(with_extra).is_err());
    }
}
