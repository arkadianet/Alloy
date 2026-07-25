//! Control-plane values persisted in [`crate::storage::RunRow::state`].

/// Control-plane run state (not the DAG state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunControlState {
    /// Row created by `submit_goal`; not yet accepted by `start`.
    Created,
    /// `start` wrote acceptance and emitted `RunAccepted`.
    Accepted,
    /// In-process / observable running marker after a non-terminal outcome.
    Running,
    /// Human gate outstanding.
    WaitingApproval,
    /// Cancel requested / in progress.
    Cancelling,
    /// Terminal cancel.
    Cancelled,
    /// Terminal success.
    Succeeded,
    /// Terminal failure.
    Failed,
    /// Replan requested; DAG mutation deferred to RFC-0009/0010.
    ReplanRequested,
}

impl RunControlState {
    /// Persisted snake_case vocabulary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::ReplanRequested => "replan_requested",
        }
    }

    /// Exact match on persisted vocabulary.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "accepted" => Some(Self::Accepted),
            "running" => Some(Self::Running),
            "waiting_approval" => Some(Self::WaitingApproval),
            "cancelling" => Some(Self::Cancelling),
            "cancelled" => Some(Self::Cancelled),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "replan_requested" => Some(Self::ReplanRequested),
            _ => None,
        }
    }

    /// Terminal control states.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Succeeded | Self::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_vocabulary() {
        for s in [
            RunControlState::Created,
            RunControlState::Accepted,
            RunControlState::Running,
            RunControlState::WaitingApproval,
            RunControlState::Cancelling,
            RunControlState::Cancelled,
            RunControlState::Succeeded,
            RunControlState::Failed,
            RunControlState::ReplanRequested,
        ] {
            assert_eq!(RunControlState::parse(s.as_str()), Some(s));
        }
        assert_eq!(RunControlState::parse("nope"), None);
    }
}
