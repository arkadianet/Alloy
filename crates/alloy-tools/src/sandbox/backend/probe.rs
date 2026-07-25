//! Backend capability probes (cached at broker construction).

use crate::sandbox::types::{BackendStatus, SandboxCapabilities};

/// Probe all backends for the current host.
pub fn probe_all() -> SandboxCapabilities {
    SandboxCapabilities {
        landlock: probe_landlock(),
        seatbelt: probe_seatbelt(),
        container: probe_container(),
    }
}

/// Probe Landlock (+ userns + netns on Linux).
pub fn probe_landlock() -> BackendStatus {
    #[cfg(target_os = "linux")]
    {
        match crate::sandbox::backend::linux::probe_landlock_sync() {
            Ok(detail) => BackendStatus::Available { detail },
            Err(reason) => BackendStatus::Unavailable { reason },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        BackendStatus::NotApplicable
    }
}

/// Probe Seatbelt on macOS.
pub fn probe_seatbelt() -> BackendStatus {
    #[cfg(target_os = "macos")]
    {
        match crate::sandbox::backend::macos::probe_seatbelt_sync() {
            Ok(detail) => BackendStatus::Available { detail },
            Err(reason) => BackendStatus::Unavailable { reason },
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        BackendStatus::NotApplicable
    }
}

/// Probe container runtime.
pub fn probe_container() -> BackendStatus {
    match crate::sandbox::backend::container::probe_container_sync() {
        Ok(detail) => BackendStatus::Available { detail },
        Err(reason) => BackendStatus::Unavailable { reason },
    }
}
