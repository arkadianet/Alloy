//! [`LinearScheduler`] — the serial ready-queue [`crate::Scheduler`] (RFC-0010).
//!
//! Non-`pub`: only the types re-exported from [`crate::scheduler`] escape this
//! module tree (RFC-0010 §4.1 rule M1).

mod checkpoint;
mod envelopes;
mod loop_;
mod metrics;
mod own;
mod ready;

pub use metrics::SchedulerMetrics;
pub use ready::{backoff_delay, derive_dag_state, promotable_nodes, ready_nodes, DeriveFlags};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use self::metrics::SchedulerCounters;
use self::own::OwnershipLock;
use crate::adapters::{
    CapabilityExecutor, GateHumanAdapter, VerifyCompileAdapter, VerifyTestAdapter,
};
use crate::dag::ValidateOpts;
use crate::error::SchedError;
use crate::obs::{CostMeterFactory, DecisionLog};
use crate::session::{RunController, SessionPlane};
use crate::storage::{ArtifactStore, DagStore, EventStore, SessionRows};
use crate::types::budget::BudgetPolicy;

/// Construction-time configuration for [`LinearScheduler`] (RFC-0010 §3.11).
///
/// Deliberately has no `Default`: a default would have to invent a `data_dir`.
#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// Absolute runtime data dir; owns `<data_dir>/scheduler.lock` (§4.5).
    pub data_dir: PathBuf,
    /// Run-side budget for abandoning an in-flight node after cancel (§5.12).
    pub cancel_drain_grace: Duration,
    /// Extra budget `cancel` allows the run for its forced C6 write (§5.12.3).
    pub cancel_write_grace: Duration,
    /// Upper bound on any single retry backoff sleep (§5.11.3).
    pub max_backoff: Duration,
    /// Host affirmation that every other parallelism knob is pinned to 1
    /// (MCP `max_in_flight` for cargo classes, edit path). MUST be `true`
    /// in production; only the crate-internal test constructor may relax it.
    pub host_parallel_honesty: bool,
    /// Re-validate every loaded DAG (§2.8). MUST be `true` in production;
    /// only the crate-internal test constructor may relax it.
    pub validate_on_load: bool,
    /// Options for the load-time validation.
    pub validate_opts: ValidateOpts,
}

impl SchedConfig {
    /// Defaults with the required `data_dir` (§3.11 defaults table).
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            cancel_drain_grace: Duration::from_secs(5),
            cancel_write_grace: Duration::from_secs(2),
            max_backoff: Duration::from_secs(60),
            host_parallel_honesty: true,
            validate_on_load: true,
            validate_opts: ValidateOpts {
                enforce_linear_mvp: true,
                require_gates: true,
            },
        }
    }
}

/// Everything [`LinearScheduler::new`] needs (RFC-0010 §3.10).
///
/// MUST NOT contain a `RuntimeHandle` (D1): phase coupling lives in the
/// control plane; the scheduler observes only `runtime_cancel` and its own
/// ownership state.
pub struct LinearSchedulerDeps {
    /// DAG blobs (CAS checkpoints).
    pub dags: Arc<dyn DagStore>,
    /// Envelopes, raw verify logs, failure IR.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Append **and** read: attempt rebuild, `ApprovalResolved` scan, meter
    /// rebuild, existence probes.
    pub events: Arc<dyn EventStore>,
    /// Run binding + session workspace/profile/budget.
    pub sessions: Arc<dyn SessionRows>,
    /// Control plane. `SessionPlane` is `Clone` (`Arc` inner) — store the
    /// value, not an `Arc`.
    pub session_plane: SessionPlane,
    /// Gate resolution / expiry. MUST equal `session_plane.runs()` (D6).
    pub runs: Arc<dyn RunController>,
    /// Compile verification adapter.
    pub verify_compile: Arc<dyn VerifyCompileAdapter>,
    /// Test verification adapter.
    pub verify_test: Arc<dyn VerifyTestAdapter>,
    /// Human gate adapter.
    pub gate_human: Arc<dyn GateHumanAdapter>,
    /// `UnavailableCapabilityExecutor` until RFC-0013.
    pub capabilities: Arc<dyn CapabilityExecutor>,
    /// Decision/retry/budget/gate record sink.
    pub decisions: Arc<dyn DecisionLog>,
    /// Run-scoped cost meter provider.
    pub cost_meters: Arc<dyn CostMeterFactory>,
    /// Process cancellation token.
    pub runtime_cancel: CancellationToken,
    /// Session budget ceilings; `max_parallel_*` MUST all be 1.
    pub budget_policy: BudgetPolicy,
    /// Wall-clock budget for one `run`, excluding gate waits (§5.19).
    pub run_timeout: Duration,
    /// Scheduler-only knobs (§3.11).
    pub config: SchedConfig,
}

/// Serial ready-queue scheduler (RFC-0010).
///
/// Execution is strictly serial per DAG: at most one node in
/// [`crate::NodeState::Running`] at a time. See [`crate::scheduler`] module
/// docs for the merged [`crate::Scheduler`] trait this implements.
pub struct LinearScheduler {
    deps: LinearSchedulerDeps,
    /// Held for the process lifetime; released on `Drop`.
    _lock: OwnershipLock,
    metrics: Arc<SchedulerCounters>,
    /// DAG-level ownership: one in-process `run` per [`crate::DagId`]
    /// (§4.5). Minimal insert-if-absent set for R4 / `AlreadyOwned`. The
    /// full `OwnedDag`/`OwnedGuard` race-free `Notify`-based cancel wait
    /// (§4.3-4.4) lands in P8; this set only tracks membership.
    pub(super) owned: std::sync::Mutex<std::collections::HashSet<crate::types::ids::DagId>>,
    /// Cancels observed for DAGs this process does not (yet) own, or whose
    /// live loop has not reached its next L2 check yet (§5.12.1). Full
    /// `pending_cancels` + forced-C6-after-grace semantics (§5.12.3) land in
    /// P8; for now this only drives the loop's own L1/L2 cancel path.
    pub(super) pending_cancels:
        std::sync::Mutex<std::collections::HashSet<crate::types::ids::DagId>>,
}

impl std::fmt::Debug for LinearScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearScheduler").finish_non_exhaustive()
    }
}

/// Construction checks, in order (§3.12). The first failure wins.
fn validate_construction(
    deps: &LinearSchedulerDeps,
    relax_test_only: bool,
) -> Result<(), SchedError> {
    // N1
    if deps.config.data_dir.as_os_str().is_empty() {
        return Err(SchedError::Config("data_dir must not be empty".into()));
    }
    // N2
    if !deps.config.data_dir.is_absolute() {
        return Err(SchedError::Config(format!(
            "data_dir must be absolute: {}",
            deps.config.data_dir.display()
        )));
    }
    // N3-N5: unconditional, never relaxed by new_for_test.
    if deps.budget_policy.max_parallel_nodes != 1 {
        return Err(SchedError::Config(
            "max_parallel_nodes must be 1 (serial scheduler)".into(),
        ));
    }
    if deps.budget_policy.max_parallel_cargo != 1 {
        return Err(SchedError::Config("max_parallel_cargo must be 1".into()));
    }
    if deps.budget_policy.max_parallel_edits != 1 {
        return Err(SchedError::Config("max_parallel_edits must be 1".into()));
    }
    // N6, N7: relaxed only by new_for_test.
    if !relax_test_only && !deps.config.host_parallel_honesty {
        return Err(SchedError::Config(
            "host_parallel_honesty must be true".into(),
        ));
    }
    if !relax_test_only && !deps.config.validate_on_load {
        return Err(SchedError::Config(
            "validate_on_load must be true in production".into(),
        ));
    }
    // N8
    if deps.config.max_backoff == Duration::ZERO {
        return Err(SchedError::Config("max_backoff must be > 0".into()));
    }
    // N9
    if deps.config.cancel_drain_grace == Duration::ZERO {
        return Err(SchedError::Config("cancel_drain_grace must be > 0".into()));
    }
    // N10
    if deps.run_timeout == Duration::ZERO {
        return Err(SchedError::Config("run_timeout must be > 0".into()));
    }
    // D6
    if !Arc::ptr_eq(&deps.runs, &deps.session_plane.runs()) {
        return Err(SchedError::Config(
            "runs must be session_plane.runs()".into(),
        ));
    }
    Ok(())
}

impl LinearScheduler {
    fn construct(deps: LinearSchedulerDeps, relax_test_only: bool) -> Result<Self, SchedError> {
        validate_construction(&deps, relax_test_only)?;
        // N11
        let lock = OwnershipLock::acquire(&deps.config.data_dir)?;
        Ok(Self {
            deps,
            _lock: lock,
            metrics: Arc::new(SchedulerCounters::new()),
            owned: std::sync::Mutex::new(std::collections::HashSet::new()),
            pending_cancels: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Validate deps, then acquire the process ownership lock (§4.5).
    pub fn new(deps: LinearSchedulerDeps) -> Result<Self, SchedError> {
        Self::construct(deps, false)
    }

    /// Test-only relaxation: permits `validate_on_load = false` and
    /// `host_parallel_honesty = false`. Serial invariants (N3-N5) still hold.
    #[cfg(test)]
    pub(crate) fn new_for_test(deps: LinearSchedulerDeps) -> Result<Self, SchedError> {
        Self::construct(deps, true)
    }

    /// Debug/test counters (§9.3).
    #[must_use]
    pub fn metrics(&self) -> SchedulerMetrics {
        self.metrics.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::{LinearScheduler, LinearSchedulerDeps, SchedConfig};
    use crate::adapters::{
        UnavailableCapabilityExecutor, UnavailableGateHuman, UnavailableVerifyCompile,
        UnavailableVerifyTest,
    };
    use crate::error::SchedError;
    use crate::obs::{ProcessCostMeterFactory, RecordingDecisionLog, RetentionPolicy};
    use crate::runtime::AlloyRuntime;
    use crate::session::SessionPlane;
    use crate::storage::{install_sqlite_event_sink, AlloyStorage, StorageOpenOptions};
    use crate::types::budget::BudgetPolicy;

    /// Everything `LinearScheduler::new` needs, over a fresh temp storage dir.
    struct Fixture {
        _dir: tempfile::TempDir,
        _rt: AlloyRuntime,
        storage: Arc<AlloyStorage>,
        plane: SessionPlane,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut rt = AlloyRuntime::new();
            rt.configure(crate::config::RuntimeConfig {
                data_dir: dir.path().join("runtime"),
                data_dir_rule: "test",
                profile_path: dir.path().join("profiles/default.toml"),
                router_path: dir.path().join("router.toml"),
                env_file_hint: dir.path().join("example.env"),
                retain_full_prompts: false,
                retain_tool_bodies: false,
                run_timeout: Duration::from_secs(30),
                budget_policy: BudgetPolicy::default(),
            })
            .unwrap();
            let handle = rt.start().await.unwrap();
            let storage = install_sqlite_event_sink(
                &handle,
                Some(StorageOpenOptions::for_data_dir(dir.path().join("storage"))),
            )
            .await
            .unwrap();
            let plane = SessionPlane::new(handle, Arc::clone(&storage));
            Self {
                _dir: dir,
                _rt: rt,
                storage,
                plane,
            }
        }

        /// Fresh, otherwise-valid deps. `sched_dir` is a *different* directory
        /// from storage's, so each test's OS lock lives in its own temp path (TD3).
        fn deps(&self, sched_dir: std::path::PathBuf) -> LinearSchedulerDeps {
            LinearSchedulerDeps {
                dags: self.storage.dags(),
                artifacts: self.storage.artifacts(),
                events: self.storage.events(),
                sessions: self.storage.sessions(),
                session_plane: self.plane.clone(),
                runs: self.plane.runs(),
                verify_compile: Arc::new(UnavailableVerifyCompile),
                verify_test: Arc::new(UnavailableVerifyTest),
                gate_human: Arc::new(UnavailableGateHuman),
                capabilities: Arc::new(UnavailableCapabilityExecutor),
                decisions: Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults())),
                cost_meters: Arc::new(ProcessCostMeterFactory::new()),
                runtime_cancel: CancellationToken::new(),
                budget_policy: BudgetPolicy::default(),
                run_timeout: Duration::from_secs(30),
                config: SchedConfig::new(sched_dir),
            }
        }
    }

    fn sched_dir(base: &std::path::Path, name: &str) -> std::path::PathBuf {
        base.join(name)
    }

    impl Fixture {
        async fn close(self) {
            self.storage.close().await.unwrap();
            self._rt.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn new_rejects_non_unit_max_parallel_nodes() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s1"));
        deps.budget_policy.max_parallel_nodes = 2;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_non_unit_max_parallel_cargo() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s2"));
        deps.budget_policy.max_parallel_cargo = 2;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_non_unit_max_parallel_edits() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s3"));
        deps.budget_policy.max_parallel_edits = 2;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_empty_data_dir() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s4"));
        deps.config.data_dir = std::path::PathBuf::new();
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(m) if m.contains("must not be empty")));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_relative_data_dir() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s5"));
        deps.config.data_dir = std::path::PathBuf::from("relative/path");
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(m) if m.contains("must be absolute")));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_validate_on_load_false() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s6"));
        deps.config.validate_on_load = false;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_host_parallel_honesty_false() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s7"));
        deps.config.host_parallel_honesty = false;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_zero_max_backoff() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s8"));
        deps.config.max_backoff = Duration::ZERO;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_rejects_zero_run_timeout() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s9"));
        deps.run_timeout = Duration::ZERO;
        let err = LinearScheduler::new(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn new_for_test_relaxes_only_validate_and_honesty() {
        let fx = Fixture::new().await;
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s10"));
        deps.config.validate_on_load = false;
        deps.config.host_parallel_honesty = false;
        LinearScheduler::new_for_test(deps).expect("relaxed knobs must be accepted");

        // N3-N5 stay unconditional even under new_for_test.
        let mut deps = fx.deps(sched_dir(fx._dir.path(), "s10b"));
        deps.config.validate_on_load = false;
        deps.config.host_parallel_honesty = false;
        deps.budget_policy.max_parallel_nodes = 2;
        let err = LinearScheduler::new_for_test(deps).unwrap_err();
        assert!(matches!(err, SchedError::Config(_)));
        fx.close().await;
    }

    #[tokio::test]
    async fn second_scheduler_same_data_dir_fails_ownership_then_succeeds_after_drop() {
        let fx = Fixture::new().await;
        let dir = sched_dir(fx._dir.path(), "s11");

        let first = LinearScheduler::new(fx.deps(dir.clone())).unwrap();
        let err = LinearScheduler::new(fx.deps(dir.clone())).unwrap_err();
        assert!(matches!(err, SchedError::Ownership(_)));

        drop(first);
        LinearScheduler::new(fx.deps(dir)).expect("lock released after drop");
        fx.close().await;
    }

    #[tokio::test]
    async fn lock_file_exists_after_construction_and_is_not_deleted_on_drop() {
        let fx = Fixture::new().await;
        let dir = sched_dir(fx._dir.path(), "s12");
        let lock_path = dir.join("scheduler.lock");

        let sched = LinearScheduler::new(fx.deps(dir)).unwrap();
        assert!(lock_path.exists());
        drop(sched);
        assert!(lock_path.exists(), "lock file must not be deleted on drop");
        fx.close().await;
    }
}
