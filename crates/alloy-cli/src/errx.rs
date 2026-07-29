//! RFC-0015 §9 — closed exit-code taxonomy and table-driven error mapping.
//!
//! Author: arkadianet

use alloy_runtime::{
    DagState, ErrorClass, RouterError, RunError, RuntimeError, SessionError, StoreError,
};

/// Closed exit-code set (RFC-0015 §9.2). No subcommand invents a code (EX1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `0` — success (operation completed and its subject succeeded, EX2).
    Ok,
    /// `1` — unexpected internal error.
    Internal,
    /// `2` — bad arguments (clap default).
    Usage,
    /// `3` — config missing or invalid.
    Config,
    /// `4` — sandbox unavailable / fails closed.
    Sandbox,
    /// `5` — the run itself failed.
    RunFailed,
    /// `6` — cancelled by signal or `alloy cancel`.
    Cancelled,
    /// `7` — a gate needs a human; none available.
    GateRequired,
    /// `8` — a human denied the gate.
    GateDenied,
    /// `9` — budget ceiling reached.
    Budget,
    /// `10` — run needs a replan; MVP does not auto-replan.
    Replan,
    /// `11` — run timeout elapsed.
    Timeout,
    /// `12` — session / run / gate not found.
    NotFound,
    /// `13` — profile forbids the operation.
    ProfileRefused,
    /// `14` — operation invalid for the current state.
    State,
    /// `15` — graph open / rebuild failed.
    Graph,
    /// `16` — a review completed and asked for changes (`alloy review`).
    ///
    /// Not a failure: the run succeeded and the verdict is the answer (VW4).
    /// It has its own code so CI can tell "the reviewer wants changes" from
    /// "the review could not be produced" (`EX_RUN_FAILED`).
    ReviewChanges,
}

impl Exit {
    /// Numeric process exit code.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Internal => 1,
            Exit::Usage => 2,
            Exit::Config => 3,
            Exit::Sandbox => 4,
            Exit::RunFailed => 5,
            Exit::Cancelled => 6,
            Exit::GateRequired => 7,
            Exit::GateDenied => 8,
            Exit::Budget => 9,
            Exit::Replan => 10,
            Exit::Timeout => 11,
            Exit::NotFound => 12,
            Exit::ProfileRefused => 13,
            Exit::State => 14,
            Exit::Graph => 15,
            Exit::ReviewChanges => 16,
        }
    }

    /// Taxonomy name (`EX_*`), for JSON and diagnostics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Exit::Ok => "EX_OK",
            Exit::Internal => "EX_INTERNAL",
            Exit::Usage => "EX_USAGE",
            Exit::Config => "EX_CONFIG",
            Exit::Sandbox => "EX_SANDBOX",
            Exit::RunFailed => "EX_RUN_FAILED",
            Exit::Cancelled => "EX_CANCELLED",
            Exit::GateRequired => "EX_GATE_REQUIRED",
            Exit::GateDenied => "EX_GATE_DENIED",
            Exit::Budget => "EX_BUDGET",
            Exit::Replan => "EX_REPLAN",
            Exit::Timeout => "EX_TIMEOUT",
            Exit::NotFound => "EX_NOT_FOUND",
            Exit::ProfileRefused => "EX_PROFILE_REFUSED",
            Exit::State => "EX_STATE",
            Exit::Graph => "EX_GRAPH",
            Exit::ReviewChanges => "EX_REVIEW_CHANGES",
        }
    }
}

/// A subcommand failure carrying its taxonomy exit and an actionable message
/// (EX3: name the file, variable, or id plus the next command).
#[derive(Debug)]
pub struct CliError {
    /// Taxonomy exit.
    pub exit: Exit,
    /// Human message (stderr; also echoed in the JSON envelope).
    pub message: String,
}

impl CliError {
    /// Construct from an exit and message.
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.exit.name())
    }
}

/// §9.3 — map `FailureIr.error_class` to an exit. Exhaustive over
/// [`ErrorClass`]: a new variant fails compilation here, not silently (EX5).
#[must_use]
pub fn exit_for_error_class(class: ErrorClass) -> Exit {
    match class {
        ErrorClass::Compile | ErrorClass::Test | ErrorClass::Tool | ErrorClass::Model => {
            Exit::RunFailed
        }
        ErrorClass::Budget => Exit::Budget,
        ErrorClass::Approval => Exit::GateDenied,
        ErrorClass::Timeout => Exit::Timeout,
        ErrorClass::Cancelled => Exit::Cancelled,
        ErrorClass::Internal => Exit::Internal,
    }
}

/// §9.3 — map a terminal [`DagState`] to an exit. Exhaustive (EX5).
///
/// `gate_outstanding` disambiguates a non-terminal state from a blocking
/// `start`: with a gate pending it is `EX_GATE_REQUIRED`, else a bug.
#[must_use]
pub fn exit_for_dag_state(state: DagState, gate_outstanding: bool) -> Exit {
    match state {
        DagState::Succeeded => Exit::Ok,
        DagState::Failed => Exit::RunFailed,
        DagState::Cancelled => Exit::Cancelled,
        DagState::ReplanRequired => Exit::Replan,
        DagState::Pending | DagState::Running | DagState::WaitingApproval => {
            if gate_outstanding {
                Exit::GateRequired
            } else {
                Exit::Internal
            }
        }
    }
}

/// §9.3 — control-plane [`SessionError`] mapping. Exhaustive (EX5).
#[must_use]
pub fn exit_for_session_error(err: &SessionError) -> Exit {
    match err {
        SessionError::NotFound(_) => Exit::NotFound,
        SessionError::Invalid(_) => Exit::Usage,
        SessionError::Internal(_) => Exit::Internal,
    }
}

/// §9.3 — control-plane [`RunError`] mapping.
///
/// [`RunError`] is `#[non_exhaustive]` upstream, so a compile-breaking match
/// is impossible from this crate; the wildcard maps any future variant to
/// `EX_INTERNAL` and the `exit_code_table_is_exhaustive` test pins today's
/// variant list.
#[must_use]
pub fn exit_for_run_error(err: &RunError) -> Exit {
    match err {
        RunError::NotFound(_) | RunError::UnknownGate(_) => Exit::NotFound,
        RunError::InvalidPhase(_) | RunError::AlreadyStarted(_) => Exit::State,
        // CR12: the CLI installed the scheduler; seeing this means assembly failed.
        RunError::SchedulerUnavailable | RunError::Internal(_) => Exit::Internal,
        _ => Exit::Internal,
    }
}

/// §9.3 — [`RuntimeError`] mapping (config vs. internal).
#[must_use]
pub fn exit_for_runtime_error(err: &RuntimeError) -> Exit {
    match err {
        RuntimeError::Config(_) => Exit::Config,
        // The CLI owns phase ordering, so InvalidPhase is the CLI's bug.
        _ => Exit::Internal,
    }
}

/// Parse a payload `error_class` string back into [`ErrorClass`] (the wire
/// form is serde snake_case).
#[must_use]
pub fn parse_error_class(s: &str) -> Option<ErrorClass> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

impl From<SessionError> for CliError {
    fn from(e: SessionError) -> Self {
        CliError::new(exit_for_session_error(&e), e.to_string())
    }
}

impl From<RunError> for CliError {
    fn from(e: RunError) -> Self {
        CliError::new(exit_for_run_error(&e), e.to_string())
    }
}

impl From<RuntimeError> for CliError {
    fn from(e: RuntimeError) -> Self {
        CliError::new(exit_for_runtime_error(&e), e.to_string())
    }
}

impl From<StoreError> for CliError {
    fn from(e: StoreError) -> Self {
        let message = match &e {
            StoreError::Busy => format!("{e} (consider raising ALLOY_SQLITE_BUSY_TIMEOUT_MS)"),
            _ => e.to_string(),
        };
        let exit = match &e {
            StoreError::NotFound(_) => Exit::NotFound,
            _ => Exit::Internal,
        };
        CliError::new(exit, message)
    }
}

impl From<RouterError> for CliError {
    fn from(e: RouterError) -> Self {
        let exit = match &e {
            RouterError::Config(_) => Exit::Config,
            RouterError::BudgetDenied(_) => Exit::Budget,
            RouterError::Cancelled => Exit::Cancelled,
            _ => Exit::Internal,
        };
        CliError::new(exit, e.to_string())
    }
}

impl From<alloy_runtime::ObsError> for CliError {
    fn from(e: alloy_runtime::ObsError) -> Self {
        CliError::new(Exit::Internal, e.to_string())
    }
}

impl From<alloy_runtime::GraphError> for CliError {
    fn from(e: alloy_runtime::GraphError) -> Self {
        CliError::new(Exit::Graph, e.to_string())
    }
}

impl From<alloy_runtime::PlanError> for CliError {
    fn from(e: alloy_runtime::PlanError) -> Self {
        CliError::new(Exit::Internal, format!("plan: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{GateId, RunId, SessionId};

    /// EX5 — the mapping is table-driven and every source variant lands on a
    /// pinned code. `ErrorClass`, `DagState`, and `SessionError` matches are
    /// compile-time exhaustive; `RunError` is `#[non_exhaustive]` upstream,
    /// so its variant list is pinned here instead.
    #[test]
    fn exit_code_table_is_exhaustive() {
        // ErrorClass — all nine variants.
        let classes = [
            (ErrorClass::Compile, Exit::RunFailed),
            (ErrorClass::Test, Exit::RunFailed),
            (ErrorClass::Tool, Exit::RunFailed),
            (ErrorClass::Model, Exit::RunFailed),
            (ErrorClass::Budget, Exit::Budget),
            (ErrorClass::Approval, Exit::GateDenied),
            (ErrorClass::Timeout, Exit::Timeout),
            (ErrorClass::Cancelled, Exit::Cancelled),
            (ErrorClass::Internal, Exit::Internal),
        ];
        for (class, exit) in classes {
            assert_eq!(exit_for_error_class(class), exit, "{class:?}");
        }

        // DagState — all seven variants.
        assert_eq!(exit_for_dag_state(DagState::Succeeded, false), Exit::Ok);
        assert_eq!(exit_for_dag_state(DagState::Failed, false), Exit::RunFailed);
        assert_eq!(
            exit_for_dag_state(DagState::Cancelled, false),
            Exit::Cancelled
        );
        assert_eq!(
            exit_for_dag_state(DagState::ReplanRequired, false),
            Exit::Replan
        );
        for pending in [
            DagState::Pending,
            DagState::Running,
            DagState::WaitingApproval,
        ] {
            assert_eq!(exit_for_dag_state(pending, true), Exit::GateRequired);
            assert_eq!(exit_for_dag_state(pending, false), Exit::Internal);
        }

        // SessionError — all three variants.
        assert_eq!(
            exit_for_session_error(&SessionError::NotFound(SessionId::new())),
            Exit::NotFound
        );
        assert_eq!(
            exit_for_session_error(&SessionError::Invalid("x".into())),
            Exit::Usage
        );
        assert_eq!(
            exit_for_session_error(&SessionError::Internal("x".into())),
            Exit::Internal
        );

        // RunError — the six variants merged today (§9.3 table).
        let run = RunId::new();
        assert_eq!(exit_for_run_error(&RunError::NotFound(run)), Exit::NotFound);
        assert_eq!(
            exit_for_run_error(&RunError::UnknownGate(GateId::new())),
            Exit::NotFound
        );
        assert_eq!(
            exit_for_run_error(&RunError::InvalidPhase("terminal".into())),
            Exit::State
        );
        assert_eq!(
            exit_for_run_error(&RunError::AlreadyStarted(run)),
            Exit::State
        );
        assert_eq!(
            exit_for_run_error(&RunError::SchedulerUnavailable),
            Exit::Internal
        );
        assert_eq!(
            exit_for_run_error(&RunError::Internal("x".into())),
            Exit::Internal
        );
    }

    /// §9.2 — the taxonomy is closed: exactly seventeen codes, 0..=16, each
    /// with a unique number and name.
    #[test]
    fn taxonomy_is_closed_and_dense() {
        let all = [
            Exit::Ok,
            Exit::Internal,
            Exit::Usage,
            Exit::Config,
            Exit::Sandbox,
            Exit::RunFailed,
            Exit::Cancelled,
            Exit::GateRequired,
            Exit::GateDenied,
            Exit::Budget,
            Exit::Replan,
            Exit::Timeout,
            Exit::NotFound,
            Exit::ProfileRefused,
            Exit::State,
            Exit::Graph,
            Exit::ReviewChanges,
        ];
        for (i, e) in all.iter().enumerate() {
            assert_eq!(u8::try_from(i).unwrap(), e.code());
            assert!(e.name().starts_with("EX_"));
        }
    }

    #[test]
    fn error_class_payload_strings_round_trip() {
        assert_eq!(parse_error_class("approval"), Some(ErrorClass::Approval));
        assert_eq!(parse_error_class("compile"), Some(ErrorClass::Compile));
        assert_eq!(parse_error_class("nonsense"), None);
    }
}
