//! Eval harness errors and serializable report boundary errors.

use thiserror::Error;

/// Operational failure from the eval harness.
///
/// Not `Clone`/`PartialEq`/`Serialize` because [`Io`](Self::Io) must preserve
/// the source [`std::io::Error`]. Reports store [`ReportError`] instead.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EvalError {
    /// Manifest schema or semantic validation failure.
    #[error("manifest: {0}")]
    Manifest(String),
    /// R17 license / provenance rejection.
    #[error("license forbidden: {0}")]
    LicenseForbidden(String),
    /// Recording toolchain channel disagrees with the harness pin.
    #[error("recording stale: {0}")]
    RecordingStale(String),
    /// Recording digest, NDJSON, or intra-fixture integrity failure.
    #[error("recording invalid: {0}")]
    RecordingInvalid(String),
    /// Offline harness reached a network-required seam.
    #[error("network required while offline: {0}")]
    NetworkRequired(String),
    /// Missing `<set>/<id>` directory or its `manifest.toml`.
    #[error("fixture not found: {0}")]
    FixtureNotFound(String),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse or trajectory serialization failure.
    #[error("json: {0}")]
    Json(String),
    /// Explicit deferred surface invoked.
    #[error("stub: {0}")]
    Stub(String),
    /// Live stack-driver could not obtain a usable sandbox backend.
    ///
    /// Report kind is always `stack_driver_sandbox_unavailable`.
    #[error("stack_driver_sandbox_unavailable: {0}")]
    SandboxUnavailable(String),
    /// Internal invariant failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// Stable, serializable representation of an [`EvalError`] for reports.
///
/// `EvalError` remains the operational API error and deliberately contains
/// `std::io::Error`, which is neither Clone nor serde data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportError {
    /// Stable error kind string (e.g. `"manifest"`, `"cancelled"`).
    pub kind: String,
    /// Bounded human-readable message.
    pub message: String,
}

impl ReportError {
    /// Convert an operational [`EvalError`] into report data.
    ///
    /// Ownership: borrows `error`; returns an owned serializable value.
    /// Failure semantics: infallible mapping; Io uses the inner `to_string()`.
    #[must_use]
    pub fn from_eval(error: &EvalError) -> Self {
        let (kind, message) = match error {
            EvalError::Manifest(_) => ("manifest", error.to_string()),
            EvalError::LicenseForbidden(_) => ("license_forbidden", error.to_string()),
            EvalError::RecordingStale(_) => ("recording_stale", error.to_string()),
            EvalError::RecordingInvalid(_) => ("recording_invalid", error.to_string()),
            EvalError::NetworkRequired(_) => ("network_required", error.to_string()),
            EvalError::FixtureNotFound(_) => ("fixture_not_found", error.to_string()),
            EvalError::Io(err) => ("io", err.to_string()),
            EvalError::Json(_) => ("json", error.to_string()),
            EvalError::Stub(_) => ("stub", error.to_string()),
            EvalError::SandboxUnavailable(detail) => (
                "stack_driver_sandbox_unavailable",
                format!("stack_driver_sandbox_unavailable: {detail}"),
            ),
            EvalError::Internal(_) => ("internal", error.to_string()),
        };
        Self {
            kind: kind.to_owned(),
            message: bound_message(message),
        }
    }

    /// Cancellation outcome for a fixture task.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            kind: "cancelled".to_owned(),
            message: "fixture cancelled".to_owned(),
        }
    }

    /// Join/panic failure for a fixture task.
    #[must_use]
    pub fn join_failed(message: impl Into<String>) -> Self {
        Self {
            kind: "join_failed".to_owned(),
            message: message.into(),
        }
    }
}

/// Maximum UTF-8 byte length for bounded eval messages (§5.2.3).
pub(crate) const EVAL_MESSAGE_MAX_BYTES: usize = 512;

/// Truncation suffix appended when bounding messages.
pub(crate) const EVAL_MESSAGE_TRUNCATE_SUFFIX: &str = "...";

/// Bound a UTF-8 message to ≤512 bytes on a code-point boundary (§5.2.3).
#[must_use]
pub(crate) fn bound_message(message: impl Into<String>) -> String {
    let message = message.into();
    if message.len() <= EVAL_MESSAGE_MAX_BYTES {
        return message;
    }
    let mut end = EVAL_MESSAGE_MAX_BYTES
        .saturating_sub(EVAL_MESSAGE_TRUNCATE_SUFFIX.len())
        .min(message.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = message[..end].to_owned();
    out.push_str(EVAL_MESSAGE_TRUNCATE_SUFFIX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_message_utf8_algorithm() {
        assert_eq!(bound_message("short"), "short");
        let ascii = "a".repeat(600);
        let bounded = bound_message(ascii);
        assert_eq!(bounded.len(), 512);
        assert!(bounded.ends_with("..."));

        // Multibyte character at the truncation boundary must not be split.
        let mut s = String::new();
        while s.len() < 508 {
            s.push('字');
        }
        // Ensure we land near the limit with a multibyte char that would be split.
        while s.len() < 511 {
            s.push('字');
        }
        s.push_str(&"x".repeat(20));
        let bounded = bound_message(s);
        assert!(bounded.len() <= 512);
        assert!(bounded.ends_with("..."));
        assert!(bounded.is_char_boundary(bounded.len() - 3));
    }

    #[test]
    fn report_error_io_mapping() {
        let err = EvalError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        let report = ReportError::from_eval(&err);
        assert_eq!(report.kind, "io");
        assert_eq!(report.message, "denied");
        assert!(!report.message.starts_with("io:"));
    }

    #[test]
    fn report_error_from_eval_bounds_messages() {
        let err = EvalError::Manifest("a".repeat(600));
        let report = ReportError::from_eval(&err);
        assert_eq!(report.kind, "manifest");
        assert!(report.message.len() <= EVAL_MESSAGE_MAX_BYTES);
        assert!(report.message.ends_with(EVAL_MESSAGE_TRUNCATE_SUFFIX));
    }
}
