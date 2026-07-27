//! RFC-0008 conversion tables into [`EditError`].
//!
//! Author: arkadianet

use alloy_runtime::{EditError, EventSinkError, StoreError};

use crate::sandbox::{DenialReason, SandboxError};

/// Map a sandbox/broker failure into the edit error taxonomy.
#[must_use]
pub(crate) fn map_sandbox(err: SandboxError) -> EditError {
    match err {
        SandboxError::Denied(DenialReason::PathDenied(_)) => EditError::PathDenied {
            path: "<redacted>".into(),
            reason: "path denied".into(),
        },
        SandboxError::Denied(DenialReason::CwdOutsideJail) => {
            EditError::Git("cwd outside jail".into())
        }
        SandboxError::Denied(DenialReason::MissingExecGrant) => {
            EditError::MissingGrant("exec".into())
        }
        SandboxError::Denied(DenialReason::ExecNotAllowlisted) => {
            EditError::MissingGrant("exec:git".into())
        }
        SandboxError::Denied(DenialReason::ArgsNotAllowlisted) => {
            EditError::MissingGrant("exec:git args".into())
        }
        SandboxError::Denied(DenialReason::EnvDenied(_)) => EditError::MissingGrant("env".into()),
        SandboxError::Denied(DenialReason::NetworkDenied) => {
            EditError::MissingGrant("network".into())
        }
        SandboxError::Denied(DenialReason::QuarantineBlocked(_)) => {
            EditError::MissingGrant("quarantine".into())
        }
        SandboxError::TokenExpired => EditError::TokenExpired,
        SandboxError::Timeout(_) => EditError::Git("sandbox timeout".into()),
        SandboxError::Cancelled => EditError::Cancelled,
        SandboxError::BackendUnavailable { .. }
        | SandboxError::BackendCannotEnforce(_)
        | SandboxError::UnsupportedOs => {
            EditError::Environment("sandbox backend unavailable".into())
        }
        SandboxError::Invalid(_) => EditError::Git("sandbox: invalid request".into()),
        SandboxError::Io(_) => EditError::Io("sandbox io".into()),
        SandboxError::Internal(_) => EditError::Git("sandbox: internal error".into()),
    }
}

/// Map an artifact-store failure into edit storage failure.
#[must_use]
pub(crate) fn map_store(err: StoreError) -> EditError {
    EditError::Storage(err.to_string())
}

/// Map an event sink failure into edit event failure.
#[must_use]
pub(crate) fn map_event(err: EventSinkError) -> EditError {
    EditError::Event(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxBackend;

    #[test]
    fn sandbox_mapping_matches_rfc_table() {
        assert!(matches!(
            map_sandbox(SandboxError::Denied(DenialReason::ExecNotAllowlisted)),
            EditError::MissingGrant(ref g) if g == "exec:git"
        ));
        assert!(matches!(
            map_sandbox(SandboxError::Denied(DenialReason::ArgsNotAllowlisted)),
            EditError::MissingGrant(ref g) if g == "exec:git args"
        ));
        assert!(matches!(
            map_sandbox(SandboxError::Denied(DenialReason::PathDenied(
                "/tmp/secret".into()
            ))),
            EditError::PathDenied { ref path, .. } if path == "<redacted>"
        ));
        assert!(matches!(
            map_sandbox(SandboxError::BackendUnavailable {
                backend: SandboxBackend::Landlock,
                message: "/abs".into(),
            }),
            EditError::Environment(ref m) if m == "sandbox backend unavailable"
        ));
    }
}
