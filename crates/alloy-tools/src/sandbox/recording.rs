//! FIFO recording test-double for [`SandboxBroker`] (RFC-0005 §3.7).

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::sandbox::policy_digest::compute_policy_digest;
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{
    BackendStatus, SandboxBackend, SandboxBroker, SandboxCapabilities, SandboxError,
    SandboxExecRequest, SandboxExecResult,
};

/// FIFO canned responses for tests.
pub struct RecordingSandboxBroker {
    profile: SandboxProfile,
    capabilities: SandboxCapabilities,
    scripts: Mutex<VecDeque<Result<SandboxExecResult, SandboxError>>>,
    recorded: Mutex<Vec<SandboxExecRequest>>,
}

impl RecordingSandboxBroker {
    /// Create with default "recording" capabilities for all backends.
    #[must_use]
    pub fn new(profile: SandboxProfile) -> Self {
        let capabilities = SandboxCapabilities {
            landlock: BackendStatus::Available {
                detail: "recording".into(),
            },
            seatbelt: BackendStatus::Available {
                detail: "recording".into(),
            },
            container: BackendStatus::Available {
                detail: "recording".into(),
            },
        };
        Self {
            profile,
            capabilities,
            scripts: Mutex::new(VecDeque::new()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// Push a canned outcome (FIFO).
    pub fn push(&self, outcome: Result<SandboxExecResult, SandboxError>) {
        self.scripts.lock().unwrap().push_back(outcome);
    }

    /// Recorded requests in order.
    #[must_use]
    pub fn recorded(&self) -> Vec<SandboxExecRequest> {
        self.recorded.lock().unwrap().clone()
    }

    /// Override capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, caps: SandboxCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Helper: push a successful synthetic exit.
    pub fn push_ok_exit(&self, code: i32, backend: SandboxBackend) {
        let digest = compute_policy_digest(&self.profile);
        self.push(Ok(SandboxExecResult::synthetic(
            Some(code),
            None,
            backend,
            digest,
        )));
    }
}

#[async_trait]
impl SandboxBroker for RecordingSandboxBroker {
    async fn exec(&self, req: SandboxExecRequest) -> Result<SandboxExecResult, SandboxError> {
        self.recorded.lock().unwrap().push(req);
        self.scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(SandboxError::Internal("recording exhausted".into())))
    }

    fn profile(&self) -> &SandboxProfile {
        &self.profile
    }

    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::types::ExecClass;
    use alloy_runtime::{ExecAllow, Grant, PermissionToken, ProfileId, RunId};

    #[tokio::test]
    async fn recording_broker_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let profile = SandboxProfile::default_for_jail(dir.path().to_path_buf()).unwrap();
        let broker = RecordingSandboxBroker::new(profile);
        broker.push_ok_exit(0, SandboxBackend::Landlock);
        broker.push(Err(SandboxError::Cancelled));

        let req = SandboxExecRequest::new(
            vec!["true".into()],
            dir.path().to_path_buf(),
            PermissionToken {
                profile: ProfileId::new("default").unwrap(),
                grants: vec![Grant::Exec(ExecAllow {
                    binary: "true".into(),
                    args_glob: None,
                })],
                expires: None,
                run_id: RunId::new(),
            },
            ExecClass::Check,
        );
        let r1 = broker.exec(req.clone()).await.unwrap();
        assert_eq!(r1.exit_code, Some(0));
        let r2 = broker.exec(req.clone()).await.unwrap_err();
        assert!(matches!(r2, SandboxError::Cancelled));
        let r3 = broker.exec(req).await.unwrap_err();
        assert!(matches!(r3, SandboxError::Internal(ref m) if m.contains("exhausted")));
        assert_eq!(broker.recorded().len(), 3);
    }
}
