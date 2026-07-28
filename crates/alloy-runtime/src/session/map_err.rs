//! Error mapping helpers at Session/RunController boundaries.

use crate::error::{RunError, RuntimeError, SchedError, SessionError};
use crate::storage::StoreError;

/// Map [`StoreError`] into [`RunError`] for run APIs.
///
/// Typed [`RunError::NotFound`] requires a [`crate::types::ids::RunId`]; callers map
/// `get_run` → `Ok(None)` themselves.
#[must_use]
pub fn store_to_run(e: StoreError) -> RunError {
    match e {
        StoreError::NotFound(s) => RunError::Internal(format!("store not found: {s}")),
        StoreError::Conflict(s) => RunError::InvalidPhase(s),
        StoreError::Corrupt(s) | StoreError::Migration(s) => RunError::Internal(s),
        StoreError::Busy => RunError::Internal("store busy".into()),
        StoreError::Closed => RunError::Internal("store closed".into()),
        StoreError::DigestMismatch => RunError::Internal("digest mismatch".into()),
        StoreError::Io(s) | StoreError::Internal(s) => RunError::Internal(s),
    }
}

/// Map [`RuntimeError`] into [`RunError`].
///
/// `SchedError::Cancelled` from `run_dag` is a **success** path in
/// [`super::run_controller`] — callers MUST match that arm before invoking this helper.
#[must_use]
pub fn runtime_to_run(e: RuntimeError) -> RunError {
    match e {
        RuntimeError::SchedulerUnavailable => RunError::SchedulerUnavailable,
        RuntimeError::SchedulerBusy => RunError::InvalidPhase("scheduler busy".into()),
        RuntimeError::InvalidPhase { current, op } => {
            RunError::InvalidPhase(format!("{op} in phase {current:?}"))
        }
        RuntimeError::Scheduler(SchedError::Cancelled) => RunError::Internal(
            "bug: SchedError::Cancelled must be handled by start success path (§6.3)".into(),
        ),
        RuntimeError::Scheduler(SchedError::DagNotFound(id)) => {
            RunError::InvalidPhase(format!("dag not found: {id}"))
        }
        RuntimeError::Scheduler(SchedError::Unavailable) => RunError::SchedulerUnavailable,
        RuntimeError::Scheduler(SchedError::Internal(s)) => RunError::Internal(s),
        RuntimeError::Scheduler(SchedError::Config(m)) => {
            RunError::Internal(format!("scheduler config: {m}"))
        }
        RuntimeError::Scheduler(SchedError::Conflict { dag_id }) => {
            RunError::InvalidPhase(format!("dag generation conflict: {dag_id}"))
        }
        RuntimeError::Scheduler(SchedError::Invariant(m)) => {
            RunError::Internal(format!("scheduler invariant: {m}"))
        }
        RuntimeError::Scheduler(SchedError::Store(m)) => {
            RunError::Internal(format!("scheduler store: {m}"))
        }
        RuntimeError::Scheduler(SchedError::AlreadyOwned(id)) => {
            RunError::InvalidPhase(format!("dag already owned: {id}"))
        }
        RuntimeError::Scheduler(SchedError::RunBindingMissing(id)) => {
            RunError::Internal(format!("no run bound to dag {id}"))
        }
        RuntimeError::Scheduler(SchedError::Ownership(m)) => {
            RunError::Internal(format!("scheduler ownership: {m}"))
        }
        // `SchedError` is `#[non_exhaustive]`: this crate defines it, so the match
        // above must stay exhaustive over every named variant — a trailing `_` here
        // would be unreachable under `-D warnings` (RFC-0010 amendment A3).
        RuntimeError::EventSinkBusy => RunError::Internal("event sink busy".into()),
        RuntimeError::EventSink(e) => RunError::Internal(e.to_string()),
        RuntimeError::AlreadyStopped => RunError::InvalidPhase("runtime stopped".into()),
        RuntimeError::Config(s) | RuntimeError::Internal(s) => RunError::Internal(s),
        RuntimeError::Io(e) => RunError::Internal(e.to_string()),
    }
}

/// Map [`RunError`] into [`SessionError`].
///
/// `SessionService::resume` finalizes run-control rows through the
/// [`super::run_controller`] helpers, so their errors have to cross the trait boundary.
#[must_use]
pub fn run_to_session(e: RunError) -> SessionError {
    match e {
        RunError::NotFound(run) => SessionError::Internal(format!("run row vanished: {run}")),
        RunError::InvalidPhase(m) => SessionError::Invalid(m),
        other => SessionError::Internal(other.to_string()),
    }
}

/// Map [`RuntimeError`] into [`SessionError`].
#[must_use]
pub fn runtime_to_session(e: RuntimeError) -> SessionError {
    match e {
        RuntimeError::InvalidPhase { current, op } => {
            SessionError::Invalid(format!("{op} in phase {current:?}"))
        }
        RuntimeError::EventSinkBusy => SessionError::Internal("event sink busy".into()),
        RuntimeError::EventSink(e) => SessionError::Internal(e.to_string()),
        RuntimeError::AlreadyStopped => SessionError::Invalid("runtime stopped".into()),
        other => SessionError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimePhase;

    #[test]
    fn store_corrupt_is_internal() {
        assert!(matches!(
            store_to_run(StoreError::Corrupt("x".into())),
            RunError::Internal(_)
        ));
        assert!(matches!(
            store_to_run(StoreError::Migration("x".into())),
            RunError::Internal(_)
        ));
        assert!(matches!(
            store_to_run(StoreError::Conflict("x".into())),
            RunError::InvalidPhase(_)
        ));
    }

    #[test]
    fn cancelled_is_bug_internal() {
        let e = runtime_to_run(RuntimeError::Scheduler(SchedError::Cancelled));
        assert!(matches!(e, RunError::Internal(_)));
    }

    #[test]
    fn invalid_phase_preserved() {
        let e = runtime_to_run(RuntimeError::InvalidPhase {
            current: RuntimePhase::Draining,
            op: "run",
        });
        assert!(matches!(e, RunError::InvalidPhase(_)));
    }

    #[test]
    fn rfc0010_sched_error_variants_map_per_boundary_table() {
        use crate::types::ids::DagId;

        let dag_id = DagId::new();
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::Config("x".into()))),
            RunError::Internal(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::Conflict { dag_id })),
            RunError::InvalidPhase(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::Invariant("x".into()))),
            RunError::Internal(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::Store("x".into()))),
            RunError::Internal(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::AlreadyOwned(dag_id))),
            RunError::InvalidPhase(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::RunBindingMissing(
                dag_id
            ))),
            RunError::Internal(_)
        ));
        assert!(matches!(
            runtime_to_run(RuntimeError::Scheduler(SchedError::Ownership("x".into()))),
            RunError::Internal(_)
        ));
    }
}
