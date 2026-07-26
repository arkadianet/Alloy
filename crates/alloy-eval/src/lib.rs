//! Alloy evaluation crate.
//!
//! Stub surface for RFC-0016 (Eval Harness & Holdout Gates).

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// Eval harness error types.
pub mod error;
/// Request fingerprinting helpers.
pub mod fingerprint;
/// R17 license validation.
pub mod license;
/// Strict fixture manifest loading.
pub mod manifest;
/// Recorded cargo JSON replay.
pub mod recording;
/// Offline scripted model provider.
pub mod scripted;
