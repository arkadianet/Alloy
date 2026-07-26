//! Offline RFC-0016 evaluation harness.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_runtime::Digest;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::cost_claim::CostClaimEnvelope;
use crate::driver;
use crate::error::{bound_message, EvalError, ReportError};
use crate::gate::{
    evaluate_gate, GateResult, GateThresholds, NaiveComparisonResult, NAIVE_BASELINE_LABEL,
};
use crate::manifest::{
    self, FixtureDriverKind, FixtureId, FixtureManifest, FixturePaths, FixtureSet,
    LoadedFixtureParts, ToolchainRecord,
};
use crate::metrics::{EvalMetrics, MetricField, MetricsAggregator};
use crate::recording::CargoJsonRecording;
use crate::report::{assemble_report_toolchain, EvalReport, FixtureOutcome, FixtureStatus};
use crate::scripted::ScriptedProvider;
use crate::trajectory::{
    sort_trajectories_stable, write_trajectory_artifacts, EvalTrajectoryRecord,
};

/// Maximum accepted fixture concurrency for the eval harness.
pub const EVAL_MAX_CONCURRENCY: usize = 1024;

/// Offline eval harness configuration.
#[derive(Debug, Clone)]
pub struct EvalHarnessConfig {
    /// Root directory containing `train/` and `holdout/` fixture sets.
    pub fixture_root: PathBuf,
    /// Gate thresholds evaluated against assembled reports.
    pub thresholds: GateThresholds,
    /// Maximum number of fixture tasks to run concurrently; valid range is `1..=1024`.
    pub max_concurrency: usize,
    /// Pinned Rust toolchain channel expected by recordings, e.g. `"1.97.1"`.
    pub pin_toolchain_channel: String,
    /// Optional cooperative cancellation token.
    pub cancel: Option<CancellationToken>,
    /// Optional trajectory artifact root; `None` keeps trajectories report-only.
    pub artifact_dir: Option<PathBuf>,
    /// Maximum retained artifact run directories.
    pub max_retained_runs: u32,
}

impl EvalHarnessConfig {
    /// Build the offline train/skeleton profile.
    #[must_use]
    pub fn skeleton(fixture_root: impl Into<PathBuf>) -> Self {
        Self {
            fixture_root: fixture_root.into(),
            thresholds: GateThresholds::skeleton_defaults(),
            max_concurrency: 4,
            pin_toolchain_channel: "1.97.1".to_owned(),
            cancel: None,
            artifact_dir: None,
            max_retained_runs: 32,
        }
    }

    /// Build the offline holdout profile requiring naive comparison.
    #[must_use]
    pub fn milestone_holdout(fixture_root: impl Into<PathBuf>) -> Self {
        Self {
            fixture_root: fixture_root.into(),
            thresholds: GateThresholds::milestone_holdout_defaults(),
            max_concurrency: 4,
            pin_toolchain_channel: "1.97.1".to_owned(),
            cancel: None,
            artifact_dir: None,
            max_retained_runs: 32,
        }
    }
}

/// Offline RFC-0016 evaluation harness.
///
/// Cloning is cheap: the validated configuration is shared behind an
/// [`Arc`] so every fixture task can own a handle without copying paths.
#[derive(Debug, Clone)]
pub struct EvalHarness {
    config: Arc<EvalHarnessConfig>,
}

/// Loaded, validated fixture and one-shot scripted provider.
pub struct LoadedFixture {
    pub(crate) manifest: FixtureManifest,
    pub(crate) root: PathBuf,
    pub(crate) paths: FixturePaths,
    pub(crate) pre_repair: CargoJsonRecording,
    pub(crate) post_repair: CargoJsonRecording,
    pub(crate) endpoint: alloy_runtime::ModelEndpoint,
    pub(crate) script_entries: Vec<(
        crate::fingerprint::RequestFingerprint,
        crate::scripted::ScriptOutcome,
    )>,
    pub(crate) scripts: Option<Arc<ScriptedProvider>>,
    #[cfg(test)]
    pub(crate) panic_after_dispatch: bool,
    #[cfg(test)]
    pub(crate) cancel_at_checkpoint: Option<&'static str>,
}

impl LoadedFixture {
    /// Validated fixture manifest.
    #[must_use]
    pub fn manifest(&self) -> &FixtureManifest {
        &self.manifest
    }

    /// Canonical fixture root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validated failing pre-repair cargo recording.
    #[must_use]
    pub fn pre_repair(&self) -> &CargoJsonRecording {
        &self.pre_repair
    }

    /// Validated passing post-repair cargo recording.
    #[must_use]
    pub fn post_repair(&self) -> &CargoJsonRecording {
        &self.post_repair
    }
}

/// Trajectory-carrying fixture execution output.
pub(crate) struct FixtureRunOutput {
    pub outcome: FixtureOutcome,
    pub trajectories: Vec<EvalTrajectoryRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FixtureEntryKind {
    Dir,
    File,
    SymlinkDir,
    SymlinkOther,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FixtureDirEntryClass {
    ValidId(String),
    InvalidUtf8Name,
    InvalidFixtureId { name: String },
    SkipNonDir,
}

#[derive(Debug, Clone)]
enum BatchEntry {
    Fixture(FixtureId),
    Error(FixtureOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchDriverMode {
    Manifest,
    HoldoutControl,
    ForceNaive,
}

struct BatchOutputs {
    outcomes: Vec<FixtureOutcome>,
    trajectories: Vec<EvalTrajectoryRecord>,
    non_error_toolchains: Vec<ToolchainRecord>,
}

/// One fixture that survived the load phase, with its sidecar toolchain.
struct LoadedEntry {
    id: FixtureId,
    fixture: LoadedFixture,
    toolchain: ToolchainRecord,
}

/// Load phase result: survivors in sorted fixture-id order plus the terminal
/// outcomes of everything that failed, was cancelled, or was rejected.
struct LoadPhase {
    loaded: Vec<LoadedEntry>,
    outcomes: Vec<FixtureOutcome>,
}

/// Run phase result for the fixtures that survived load and preflight.
struct RunPhase {
    outcomes: Vec<FixtureOutcome>,
    trajectories: Vec<EvalTrajectoryRecord>,
    non_error_toolchains: Vec<ToolchainRecord>,
}

/// Result of one spawned fixture task.
enum TaskOutcome<T> {
    Ready(T),
    Cancelled,
    JoinFailed(String),
}

impl EvalHarness {
    /// Construct and validate a harness.
    ///
    /// # Errors
    ///
    /// Returns an error when thresholds or configuration bounds are invalid,
    /// or when the fixture root is missing or is not a directory.
    pub fn new(config: EvalHarnessConfig) -> Result<Self, EvalError> {
        config.thresholds.validate()?;
        if config.max_concurrency == 0 {
            return Err(EvalError::Manifest(
                "max_concurrency must be at least 1".to_owned(),
            ));
        }
        if config.max_concurrency > EVAL_MAX_CONCURRENCY {
            return Err(EvalError::Manifest(format!(
                "max_concurrency must be at most {EVAL_MAX_CONCURRENCY}"
            )));
        }
        if config.max_retained_runs == 0 {
            return Err(EvalError::Manifest(
                "max_retained_runs must be at least 1".to_owned(),
            ));
        }
        if config.pin_toolchain_channel.is_empty() {
            return Err(EvalError::Manifest(
                "pin_toolchain_channel must be non-empty".to_owned(),
            ));
        }
        let metadata = std::fs::metadata(&config.fixture_root)?;
        if !metadata.is_dir() {
            return Err(EvalError::Manifest(
                "fixture_root must be a directory".to_owned(),
            ));
        }
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Borrow the validated harness configuration.
    #[must_use]
    pub fn config(&self) -> &EvalHarnessConfig {
        &self.config
    }

    /// Load one manifest and its validated fixture artifacts.
    ///
    /// # Errors
    ///
    /// Returns an error when the fixture cannot be found, parsed, or validated.
    pub fn load_fixture(
        &self,
        set: FixtureSet,
        id: &FixtureId,
    ) -> Result<LoadedFixture, EvalError> {
        load_fixture_blocking(&self.config, set, id)
    }

    /// Run one fixture and return only its terminal outcome.
    pub async fn run_fixture(&self, fixture: &mut LoadedFixture) -> FixtureOutcome {
        self.run_fixture_collect(fixture).await.outcome
    }

    /// Run all fixtures in `set` and attach gate and optional artifacts.
    ///
    /// # Errors
    ///
    /// Returns an error when the fixture set cannot be enumerated, report
    /// assembly fails, or configured trajectory artifacts cannot be written.
    pub async fn run_batch(&self, set: FixtureSet) -> Result<EvalReport, EvalError> {
        let entries = self.enumerate_fixture_entries(set).await?;
        let fixture_count = entries.len();
        let span = batch_span(set, fixture_count);
        let batch = self
            .run_entries(
                set,
                entries,
                BatchDriverMode::Manifest,
                self.new_semaphore(fixture_count),
            )
            .instrument(span)
            .await;
        let mut report = self.assemble_report(batch)?;
        report.gate = Some(self.evaluate_gate(&report));
        self.write_trajectory_artifacts_blocking(&report).await?;
        Ok(report)
    }

    /// Evaluate this harness's configured gate thresholds against a report.
    #[must_use]
    pub fn evaluate_gate(&self, report: &EvalReport) -> GateResult {
        let require_beat_naive = self.config.thresholds.require_beat_naive;
        let span = tracing::info_span!(
            "alloy_eval.gate",
            passed = tracing::field::Empty,
            require_beat_naive
        );
        let _entered = span.enter();
        let result = evaluate_gate(&self.config.thresholds, report);
        span.record("passed", result.passed);
        if !result.passed {
            tracing::info!(
                run_id = %report.run_id,
                failures = result.failures.len(),
                require_beat_naive,
                "eval gate failed"
            );
        }
        result
    }

    /// Run holdout control and forced naive baseline, then compare and gate.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-holdout gate profile, fixture-set mismatch,
    /// report assembly failure, or trajectory artifact write failure.
    pub async fn run_holdout_with_naive(&self) -> Result<EvalReport, EvalError> {
        if self.config.thresholds.set != FixtureSet::Holdout {
            return Err(EvalError::Manifest(
                "run_holdout_with_naive requires thresholds.set = holdout".to_owned(),
            ));
        }

        let entries = self.enumerate_fixture_entries(FixtureSet::Holdout).await?;
        let fixture_count = entries.len();
        let span = batch_span(FixtureSet::Holdout, fixture_count);
        // Control and naive share one semaphore so the pair still honours
        // `max_concurrency` overall.
        let semaphore = self.new_semaphore(fixture_count);
        let (control, naive) = async {
            tokio::join!(
                self.run_entries(
                    FixtureSet::Holdout,
                    entries.clone(),
                    BatchDriverMode::HoldoutControl,
                    Arc::clone(&semaphore),
                ),
                self.run_entries(
                    FixtureSet::Holdout,
                    entries,
                    BatchDriverMode::ForceNaive,
                    semaphore,
                )
            )
        }
        .instrument(span)
        .await;
        ensure_same_fixture_id_set(&control.outcomes, &naive.outcomes)?;

        let control_metrics = MetricsAggregator::aggregate(&control.outcomes);
        let naive_metrics = MetricsAggregator::aggregate(&naive.outcomes);
        let comparison = naive_comparison(
            control_metrics.clone(),
            naive_metrics,
            self.config.thresholds.naive_epsilon,
        );
        let cost_claim = CostClaimEnvelope::uncalibrated(MetricsAggregator::internal_cost_usd_p50(
            &control.outcomes,
        ));

        let mut control_trajectories = control.trajectories;
        sort_trajectories_stable(&mut control_trajectories);
        let mut naive_trajectories = naive.trajectories;
        sort_trajectories_stable(&mut naive_trajectories);
        let toolchain = assemble_report_toolchain(
            self.config.pin_toolchain_channel.clone(),
            &control.non_error_toolchains,
        )?;

        let mut fixtures = control.outcomes;
        sort_outcomes(&mut fixtures);
        let mut naive_fixtures = naive.outcomes;
        sort_outcomes(&mut naive_fixtures);

        let mut report = EvalReport {
            schema_version: 1,
            run_id: uuid::Uuid::new_v4().to_string(),
            offline: true,
            toolchain,
            fixtures,
            trajectories: control_trajectories,
            naive_fixtures: Some(naive_fixtures),
            naive_trajectories: Some(naive_trajectories),
            metrics: control_metrics,
            cost_claim,
            gate: None,
            naive_comparison: Some(comparison),
        };
        report.gate = Some(self.evaluate_gate(&report));
        self.write_trajectory_artifacts_blocking(&report).await?;
        Ok(report)
    }

    /// Write and rotate the configured trajectory artifact.
    ///
    /// # Errors
    ///
    /// Returns an error when the report run id is invalid or artifact
    /// directories and files cannot be safely created, written, or rotated.
    pub fn write_trajectory_artifacts(&self, report: &EvalReport) -> Result<(), EvalError> {
        write_trajectory_artifacts(
            report,
            self.config.artifact_dir.as_deref(),
            self.config.max_retained_runs,
        )
    }

    async fn write_trajectory_artifacts_blocking(
        &self,
        report: &EvalReport,
    ) -> Result<(), EvalError> {
        let harness = self.clone();
        let report = report.clone();
        match tokio::task::spawn_blocking(move || harness.write_trajectory_artifacts(&report)).await
        {
            Ok(result) => result,
            Err(error) => Err(EvalError::Internal(join_failed_message(error))),
        }
    }

    pub(crate) async fn run_fixture_collect(
        &self,
        fixture: &mut LoadedFixture,
    ) -> FixtureRunOutput {
        self.run_loaded_fixture_collect(fixture, BatchDriverMode::Manifest)
            .await
    }

    async fn run_loaded_fixture_collect(
        &self,
        fixture: &mut LoadedFixture,
        mode: BatchDriverMode,
    ) -> FixtureRunOutput {
        let driver_kind = effective_driver(fixture.manifest.driver, mode);
        let span = tracing::info_span!(
            "alloy_eval.run_fixture",
            fixture_id = %fixture.manifest.id,
            driver = driver_label(driver_kind),
            status = tracing::field::Empty,
        );
        let output = self
            .dispatch_driver(fixture, driver_kind)
            .instrument(span.clone())
            .await;
        span.record("status", status_label(output.outcome.status));
        output
    }

    async fn dispatch_driver(
        &self,
        fixture: &mut LoadedFixture,
        driver_kind: FixtureDriverKind,
    ) -> FixtureRunOutput {
        let Some(provider) = fixture.scripts.take() else {
            return fixture_already_run_output(fixture);
        };
        match driver_kind {
            FixtureDriverKind::SkeletonReplay => {
                driver::skeleton::run(fixture, provider, self.config.cancel.clone()).await
            }
            FixtureDriverKind::NaiveBaseline => {
                driver::naive::run(fixture, provider, self.config.cancel.clone()).await
            }
            FixtureDriverKind::ControlPlane => driver::control_plane::run(fixture).await,
        }
    }

    /// Enumerate `<root>/<set>` off the runtime; `read_dir` plus `canonicalize`
    /// are blocking syscalls.
    async fn enumerate_fixture_entries(
        &self,
        set: FixtureSet,
    ) -> Result<Vec<BatchEntry>, EvalError> {
        let config = Arc::clone(&self.config);
        match tokio::task::spawn_blocking(move || enumerate_fixture_entries_blocking(&config, set))
            .await
        {
            Ok(entries) => entries,
            Err(error) => Err(EvalError::Internal(join_failed_message(error))),
        }
    }

    /// Load every enumerated fixture, apply the §3.7.2 cross-fixture toolchain
    /// preflight, then run the survivors. Both phases are bounded by
    /// `semaphore`, which carries `max_concurrency` permits.
    async fn run_entries(
        &self,
        set: FixtureSet,
        entries: Vec<BatchEntry>,
        mode: BatchDriverMode,
        semaphore: Arc<Semaphore>,
    ) -> BatchOutputs {
        let mut outcomes = Vec::new();
        let mut ids = Vec::new();
        for entry in entries {
            match entry {
                BatchEntry::Fixture(id) => ids.push(id),
                BatchEntry::Error(outcome) => outcomes.push(outcome),
            }
        }

        let load = self.load_entries(set, ids, mode, &semaphore).await;
        outcomes.extend(load.outcomes);
        let loaded = apply_toolchain_preflight(load.loaded, set, &mut outcomes);

        let mut run = self.run_loaded_entries(set, loaded, mode, &semaphore).await;
        outcomes.append(&mut run.outcomes);

        sort_outcomes(&mut outcomes);
        sort_trajectories_stable(&mut run.trajectories);
        BatchOutputs {
            outcomes,
            trajectories: run.trajectories,
            non_error_toolchains: run.non_error_toolchains,
        }
    }

    /// Load phase: one bounded task per fixture, results kept in sorted
    /// fixture-id order so the toolchain reference choice stays deterministic.
    async fn load_entries(
        &self,
        set: FixtureSet,
        ids: Vec<FixtureId>,
        mode: BatchDriverMode,
        semaphore: &Arc<Semaphore>,
    ) -> LoadPhase {
        let mut outcomes = Vec::new();
        let mut handles = Vec::with_capacity(ids.len());
        let mut unscheduled = Vec::new();
        let mut ids = ids.into_iter();
        for id in ids.by_ref() {
            if self.is_cancelled() {
                unscheduled.push(id);
                break;
            }
            let harness = self.clone();
            let semaphore = Arc::clone(semaphore);
            let task_id = id.clone();
            handles.push((
                id,
                tokio::spawn(async move { harness.load_one(semaphore, set, task_id).await }),
            ));
        }
        unscheduled.extend(ids);

        let mut loaded = Vec::with_capacity(handles.len());
        for (id, handle) in handles {
            let task = match handle.await {
                Ok(task) => task,
                Err(error) => TaskOutcome::JoinFailed(join_failed_message(error)),
            };
            match task {
                TaskOutcome::Ready(Ok(fixture)) => {
                    if mode == BatchDriverMode::HoldoutControl
                        && fixture.manifest.driver == FixtureDriverKind::NaiveBaseline
                    {
                        let error = EvalError::Manifest(
                            "holdout control fixture must not use naive_baseline driver".to_owned(),
                        );
                        outcomes.push(error_outcome(id, set, ReportError::from_eval(&error)));
                        continue;
                    }
                    let toolchain = fixture.manifest.toolchain.clone();
                    loaded.push(LoadedEntry {
                        id,
                        fixture,
                        toolchain,
                    });
                }
                TaskOutcome::Ready(Err(error)) => outcomes.push(load_error_outcome(id, set, error)),
                TaskOutcome::Cancelled => outcomes.push(cancelled_outcome(id, set)),
                TaskOutcome::JoinFailed(message) => {
                    outcomes.push(error_outcome(id, set, ReportError::join_failed(message)));
                }
            }
        }
        for id in unscheduled {
            outcomes.push(cancelled_outcome(id, set));
        }
        LoadPhase { loaded, outcomes }
    }

    async fn load_one(
        self,
        semaphore: Arc<Semaphore>,
        set: FixtureSet,
        id: FixtureId,
    ) -> TaskOutcome<Result<LoadedFixture, EvalError>> {
        let Some(_permit) = self.acquire_permit(semaphore).await else {
            return TaskOutcome::Cancelled;
        };
        if self.is_cancelled() {
            return TaskOutcome::Cancelled;
        }
        let config = Arc::clone(&self.config);
        match tokio::task::spawn_blocking(move || load_fixture_blocking(&config, set, &id)).await {
            Ok(result) => TaskOutcome::Ready(result),
            Err(error) => TaskOutcome::JoinFailed(join_failed_message(error)),
        }
    }

    async fn run_loaded_entries(
        &self,
        set: FixtureSet,
        entries: Vec<LoadedEntry>,
        mode: BatchDriverMode,
        semaphore: &Arc<Semaphore>,
    ) -> RunPhase {
        let mut outcomes = Vec::new();
        let mut trajectories = Vec::new();
        let mut non_error_toolchains = Vec::new();
        let mut handles = Vec::with_capacity(entries.len());
        let mut unscheduled = Vec::new();
        let mut entries = entries.into_iter();
        for entry in entries.by_ref() {
            if self.is_cancelled() {
                unscheduled.push(entry.id);
                break;
            }
            let LoadedEntry {
                id,
                mut fixture,
                toolchain,
            } = entry;
            let harness = self.clone();
            let semaphore = Arc::clone(semaphore);
            handles.push((
                id,
                toolchain,
                tokio::spawn(async move {
                    let Some(_permit) = harness.acquire_permit(semaphore).await else {
                        return TaskOutcome::Cancelled;
                    };
                    if harness.is_cancelled() {
                        return TaskOutcome::Cancelled;
                    }
                    TaskOutcome::Ready(harness.run_loaded_fixture_collect(&mut fixture, mode).await)
                }),
            ));
        }
        unscheduled.extend(entries.map(|entry| entry.id));

        for (id, toolchain, handle) in handles {
            let task = match handle.await {
                Ok(task) => task,
                Err(error) => TaskOutcome::JoinFailed(join_failed_message(error)),
            };
            match task {
                TaskOutcome::Ready(output) => {
                    if output.outcome.status != FixtureStatus::Error {
                        non_error_toolchains.push(toolchain);
                    }
                    trajectories.extend(output.trajectories);
                    outcomes.push(output.outcome);
                }
                TaskOutcome::Cancelled => outcomes.push(cancelled_outcome(id, set)),
                TaskOutcome::JoinFailed(message) => {
                    outcomes.push(error_outcome(id, set, ReportError::join_failed(message)));
                }
            }
        }
        for id in unscheduled {
            outcomes.push(cancelled_outcome(id, set));
        }
        RunPhase {
            outcomes,
            trajectories,
            non_error_toolchains,
        }
    }

    fn new_semaphore(&self, fixture_count: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(
            self.config.max_concurrency.min(fixture_count.max(1)),
        ))
    }

    /// Acquire one fixture permit, or `None` when cancellation wins the race.
    async fn acquire_permit(&self, semaphore: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
        match &self.config.cancel {
            Some(token) => tokio::select! {
                biased;
                () = token.cancelled() => None,
                permit = semaphore.acquire_owned() => permit.ok(),
            },
            None => semaphore.acquire_owned().await.ok(),
        }
    }

    fn assemble_report(&self, batch: BatchOutputs) -> Result<EvalReport, EvalError> {
        let metrics = MetricsAggregator::aggregate(&batch.outcomes);
        let cost_claim = CostClaimEnvelope::uncalibrated(MetricsAggregator::internal_cost_usd_p50(
            &batch.outcomes,
        ));
        let toolchain = assemble_report_toolchain(
            self.config.pin_toolchain_channel.clone(),
            &batch.non_error_toolchains,
        )?;
        Ok(EvalReport {
            schema_version: 1,
            run_id: uuid::Uuid::new_v4().to_string(),
            offline: true,
            toolchain,
            fixtures: batch.outcomes,
            trajectories: batch.trajectories,
            naive_fixtures: None,
            naive_trajectories: None,
            metrics,
            cost_claim,
            gate: None,
            naive_comparison: None,
        })
    }

    fn is_cancelled(&self) -> bool {
        self.config
            .cancel
            .as_ref()
            .map(CancellationToken::is_cancelled)
            .unwrap_or(false)
    }
}

impl LoadedFixture {
    fn from_parts(parts: LoadedFixtureParts) -> Result<Self, EvalError> {
        let provider = Arc::new(ScriptedProvider::new(
            parts.endpoint.provider.clone(),
            parts.endpoint.clone(),
        )?);
        Ok(Self {
            manifest: parts.manifest,
            root: parts.root,
            paths: parts.paths,
            pre_repair: parts.pre_repair,
            post_repair: parts.post_repair,
            endpoint: parts.endpoint,
            script_entries: parts.script_entries,
            scripts: Some(provider),
            #[cfg(test)]
            panic_after_dispatch: false,
            #[cfg(test)]
            cancel_at_checkpoint: None,
        })
    }
}

fn load_fixture_blocking(
    config: &EvalHarnessConfig,
    set: FixtureSet,
    id: &FixtureId,
) -> Result<LoadedFixture, EvalError> {
    LoadedFixture::from_parts(manifest::load_fixture(
        &config.fixture_root,
        set,
        id,
        &config.pin_toolchain_channel,
    )?)
}

fn enumerate_fixture_entries_blocking(
    config: &EvalHarnessConfig,
    set: FixtureSet,
) -> Result<Vec<BatchEntry>, EvalError> {
    let set_path = config.fixture_root.join(set_dir_name(set));
    let canonical_fixture_root = config.fixture_root.canonicalize()?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(set_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let (kind, metadata_error) = entry_kind(&path)?;
        match classify_fixture_dir_entry(&name, kind) {
            FixtureDirEntryClass::ValidId(raw) => {
                let id = FixtureId::new(raw).map_err(|error| {
                    EvalError::Internal(format!("classifier accepted invalid fixture id: {error}"))
                })?;
                if let Some(error) = metadata_error {
                    entries.push(BatchEntry::Error(error_outcome(
                        id,
                        set,
                        ReportError::from_eval(&EvalError::Io(error)),
                    )));
                    continue;
                }
                if kind == FixtureEntryKind::SymlinkDir {
                    match path.canonicalize() {
                        Ok(target) if target.starts_with(&canonical_fixture_root) => {}
                        Ok(_) | Err(_) => {
                            let error = EvalError::Manifest(bound_message(format!(
                                "path: {}",
                                path.display()
                            )));
                            entries.push(BatchEntry::Error(error_outcome(
                                id,
                                set,
                                ReportError::from_eval(&error),
                            )));
                            continue;
                        }
                    }
                }
                entries.push(BatchEntry::Fixture(id));
            }
            FixtureDirEntryClass::InvalidUtf8Name => {
                let id = invalid_path_id(&name);
                entries.push(BatchEntry::Error(error_outcome(
                    id,
                    set,
                    ReportError {
                        kind: "invalid_fixture_name".to_owned(),
                        message: bound_message("invalid_fixture_name: non-UTF-8 fixture name"),
                    },
                )));
            }
            FixtureDirEntryClass::InvalidFixtureId { name } => {
                let id = invalid_id(&name);
                entries.push(BatchEntry::Error(error_outcome(
                    id,
                    set,
                    ReportError {
                        kind: "invalid_fixture_id".to_owned(),
                        message: bound_message(format!("invalid_fixture_id: {name}")),
                    },
                )));
            }
            FixtureDirEntryClass::SkipNonDir => {}
        }
    }
    entries.sort_by(|left, right| batch_entry_id(left).cmp(batch_entry_id(right)));
    Ok(entries)
}

/// §3.7.2 cross-fixture preflight: the first successfully loaded fixture in
/// sorted fixture-id order pins the toolchain triplet for the batch. A later
/// disagreeing fixture becomes a `Manifest` Error instead of aborting the run.
fn apply_toolchain_preflight(
    loaded: Vec<LoadedEntry>,
    set: FixtureSet,
    outcomes: &mut Vec<FixtureOutcome>,
) -> Vec<LoadedEntry> {
    let mut reference: Option<ToolchainRecord> = None;
    let mut kept = Vec::with_capacity(loaded.len());
    for entry in loaded {
        match &reference {
            Some(expected) if *expected != entry.toolchain => {
                let error = EvalError::Manifest(bound_message(format!(
                    "cross-fixture toolchain disagreement: {} declares {} / {} / {}, batch reference is {} / {} / {}",
                    entry.id,
                    entry.toolchain.channel,
                    entry.toolchain.rustc_version,
                    entry.toolchain.cargo_version,
                    expected.channel,
                    expected.rustc_version,
                    expected.cargo_version,
                )));
                tracing::error!(
                    fixture_id = %entry.id,
                    %set,
                    "eval fixture toolchain disagrees with the batch reference"
                );
                outcomes.push(error_outcome(entry.id, set, ReportError::from_eval(&error)));
            }
            Some(_) => kept.push(entry),
            None => {
                reference = Some(entry.toolchain.clone());
                kept.push(entry);
            }
        }
    }
    kept
}

fn batch_span(set: FixtureSet, fixture_count: usize) -> tracing::Span {
    tracing::info_span!(
        "alloy_eval.run_batch",
        set = %set,
        fixture_count,
        offline = true
    )
}

fn effective_driver(declared: FixtureDriverKind, mode: BatchDriverMode) -> FixtureDriverKind {
    match mode {
        BatchDriverMode::ForceNaive => FixtureDriverKind::NaiveBaseline,
        BatchDriverMode::Manifest | BatchDriverMode::HoldoutControl => declared,
    }
}

fn driver_label(driver: FixtureDriverKind) -> &'static str {
    match driver {
        FixtureDriverKind::SkeletonReplay => "skeleton_replay",
        FixtureDriverKind::NaiveBaseline => "naive_baseline",
        FixtureDriverKind::ControlPlane => "control_plane",
    }
}

fn status_label(status: FixtureStatus) -> &'static str {
    match status {
        FixtureStatus::Pass => "pass",
        FixtureStatus::Fail => "fail",
        FixtureStatus::Error => "error",
    }
}

pub(crate) fn classify_fixture_dir_entry(
    name: &OsStr,
    entry_kind: FixtureEntryKind,
) -> FixtureDirEntryClass {
    let Some(name) = name.to_str() else {
        return FixtureDirEntryClass::InvalidUtf8Name;
    };
    if FixtureId::new(name.to_owned()).is_err() {
        return FixtureDirEntryClass::InvalidFixtureId {
            name: name.to_owned(),
        };
    }
    match entry_kind {
        FixtureEntryKind::Dir | FixtureEntryKind::SymlinkDir => {
            FixtureDirEntryClass::ValidId(name.to_owned())
        }
        FixtureEntryKind::File | FixtureEntryKind::SymlinkOther | FixtureEntryKind::Other => {
            FixtureDirEntryClass::SkipNonDir
        }
    }
}

fn entry_kind(path: &Path) -> Result<(FixtureEntryKind, Option<std::io::Error>), EvalError> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return match std::fs::metadata(path) {
            Ok(target) if target.is_dir() => Ok((FixtureEntryKind::SymlinkDir, None)),
            Ok(target) if target.is_file() => Ok((FixtureEntryKind::SymlinkOther, None)),
            Ok(_) => Ok((FixtureEntryKind::SymlinkOther, None)),
            Err(error) => Ok((FixtureEntryKind::SymlinkDir, Some(error))),
        };
    }
    if metadata.is_dir() {
        Ok((FixtureEntryKind::Dir, None))
    } else if metadata.is_file() {
        Ok((FixtureEntryKind::File, None))
    } else {
        Ok((FixtureEntryKind::Other, None))
    }
}

fn set_dir_name(set: FixtureSet) -> &'static str {
    match set {
        FixtureSet::Train => "train",
        FixtureSet::Holdout => "holdout",
    }
}

fn batch_entry_id(entry: &BatchEntry) -> &str {
    match entry {
        BatchEntry::Fixture(id) => id.as_str(),
        BatchEntry::Error(outcome) => outcome.fixture_id.as_str(),
    }
}

fn sort_outcomes(outcomes: &mut [FixtureOutcome]) {
    outcomes.sort_by(|left, right| left.fixture_id.as_str().cmp(right.fixture_id.as_str()));
}

fn error_outcome(fixture_id: FixtureId, set: FixtureSet, error: ReportError) -> FixtureOutcome {
    FixtureOutcome {
        fixture_id,
        set,
        status: FixtureStatus::Error,
        criteria: vec![],
        wall_ms: 0,
        model_calls: 0,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
        retry_count: None,
        human_interventions: None,
        unsafe_introduced: None,
        compile_clean: None,
        error: Some(error),
    }
}

fn cancelled_outcome(fixture_id: FixtureId, set: FixtureSet) -> FixtureOutcome {
    error_outcome(fixture_id, set, ReportError::cancelled())
}

fn fixture_already_run_output(fixture: &LoadedFixture) -> FixtureRunOutput {
    FixtureRunOutput {
        outcome: error_outcome(
            fixture.manifest.id.clone(),
            fixture.manifest.set,
            ReportError {
                kind: "fixture_already_run".to_owned(),
                message: "fixture already run".to_owned(),
            },
        ),
        trajectories: vec![],
    }
}

/// Convert a load failure into a fixture Error, logging the §9.2 events that
/// must be visible at `error` level.
fn load_error_outcome(fixture_id: FixtureId, set: FixtureSet, error: EvalError) -> FixtureOutcome {
    match &error {
        EvalError::LicenseForbidden(_) => tracing::error!(
            fixture_id = %fixture_id,
            %set,
            reason = %error,
            "eval fixture rejected by R17 license validation"
        ),
        EvalError::RecordingStale(_) => tracing::error!(
            fixture_id = %fixture_id,
            %set,
            reason = %error,
            "eval fixture recording is stale for the pinned toolchain"
        ),
        _ => {}
    }
    error_outcome(fixture_id, set, ReportError::from_eval(&error))
}

fn join_failed_message(error: JoinError) -> String {
    let message = if error.is_panic() {
        let payload = error.into_panic();
        if let Some(value) = payload.downcast_ref::<&str>() {
            format!("join_failed: {value:?}")
        } else if let Some(value) = payload.downcast_ref::<String>() {
            format!("join_failed: {value:?}")
        } else {
            "join_failed: opaque".to_owned()
        }
    } else {
        format!("join_failed: {error:?}")
    };
    bound_message(message)
}

fn invalid_path_id(name: &OsStr) -> FixtureId {
    let digest = Digest::sha256(&os_str_bytes(name));
    FixtureId::new(format!("invalid-path-{}", digest.as_hex()))
        .expect("invalid-path synthetic id is valid")
}

fn invalid_id(name: &str) -> FixtureId {
    let digest = Digest::sha256(name.as_bytes());
    FixtureId::new(format!("invalid-id-{}", digest.as_hex()))
        .expect("invalid-id synthetic id is valid")
}

fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().as_bytes().to_vec()
    }
}

fn ensure_same_fixture_id_set(
    control: &[FixtureOutcome],
    naive: &[FixtureOutcome],
) -> Result<(), EvalError> {
    let mut control_ids = fixture_ids(control);
    let mut naive_ids = fixture_ids(naive);
    control_ids.sort();
    naive_ids.sort();
    if has_duplicates(&control_ids) || has_duplicates(&naive_ids) || control_ids != naive_ids {
        return Err(EvalError::Internal(
            "control/naive fixture id mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn fixture_ids(outcomes: &[FixtureOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .map(|outcome| outcome.fixture_id.as_str().to_owned())
        .collect()
}

fn has_duplicates(ids: &[String]) -> bool {
    ids.windows(2).any(|window| window[0] == window[1])
}

fn naive_comparison(
    control: EvalMetrics,
    naive: EvalMetrics,
    epsilon: f64,
) -> NaiveComparisonResult {
    let control_compile = measured_rate(&control.compile_success_rate);
    let naive_compile = measured_rate(&naive.compile_success_rate);
    let control_meets_or_beats_naive = matches!((control_compile, naive_compile), (Some(control), Some(naive)) if control + epsilon >= naive);
    NaiveComparisonResult {
        control,
        naive,
        control_meets_or_beats_naive,
        detail: format!("{NAIVE_BASELINE_LABEL}: compile_success_rate comparison"),
    }
}

fn measured_rate(metric: &MetricField<f64>) -> Option<f64> {
    match metric {
        MetricField::Measured(value) => Some(*value),
        MetricField::Unmeasured { .. } => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fingerprint::RequestFingerprint;
    use crate::manifest::{
        CargoRecordingRefs, ExpectedDiagnostic, FixtureTurnId, LicenseClass, LicenseMeta,
        NaivePatchMode, ScriptTurn, ScriptTurnOutcome, SuccessCriterion, WorkspaceRef,
    };
    use crate::scripted::ScriptOutcome;
    use alloy_runtime::{
        CapabilityId, ChatMessage, ChatRole, CompletionRequest, EndpointId, ModelEndpoint,
        ModelTier, ProviderId, ResponseFormat, ToolChoice, Usage,
    };
    use std::fs;

    #[test]
    fn classify_fixture_dir_entry_is_pure() {
        assert_eq!(
            classify_fixture_dir_entry(OsStr::new("valid-id_1.2"), FixtureEntryKind::Dir),
            FixtureDirEntryClass::ValidId("valid-id_1.2".to_owned())
        );
        assert_eq!(
            classify_fixture_dir_entry(OsStr::new("valid"), FixtureEntryKind::File),
            FixtureDirEntryClass::SkipNonDir
        );
        assert!(matches!(
            classify_fixture_dir_entry(OsStr::new(".."), FixtureEntryKind::Dir),
            FixtureDirEntryClass::InvalidFixtureId { .. }
        ));
        assert!(matches!(
            classify_fixture_dir_entry(OsStr::new("Bad Name"), FixtureEntryKind::Other),
            FixtureDirEntryClass::InvalidFixtureId { .. }
        ));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let name = OsStr::from_bytes(b"\xff");
            assert_eq!(
                classify_fixture_dir_entry(name, FixtureEntryKind::Dir),
                FixtureDirEntryClass::InvalidUtf8Name
            );
        }
    }

    #[test]
    fn config_validation_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.max_concurrency = 0;
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(_))
        ));

        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.max_concurrency = EVAL_MAX_CONCURRENCY + 1;
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(_))
        ));

        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.max_retained_runs = 0;
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(_))
        ));

        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.pin_toolchain_channel.clear();
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(message)) if message == "pin_toolchain_channel must be non-empty"
        ));

        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.thresholds.min_success_rate = f64::NAN;
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(message)) if message == "min_success_rate must be finite"
        ));

        let file = dir.path().join("not-a-directory");
        fs::write(&file, "fixture root must be a directory").unwrap();
        assert!(matches!(
            EvalHarness::new(EvalHarnessConfig::skeleton(&file)),
            Err(EvalError::Manifest(message)) if message == "fixture_root must be a directory"
        ));

        let missing = dir.path().join("missing");
        assert!(matches!(
            EvalHarness::new(EvalHarnessConfig::skeleton(missing)),
            Err(EvalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn config_rejects_excess_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.max_concurrency = EVAL_MAX_CONCURRENCY;
        assert!(EvalHarness::new(config).is_ok());

        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.max_concurrency = EVAL_MAX_CONCURRENCY + 1;
        assert!(matches!(
            EvalHarness::new(config),
            Err(EvalError::Manifest(message)) if message.contains("at most")
        ));
    }

    #[tokio::test]
    async fn loaded_fixture_is_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();
        let mut fixture = loaded_fixture_for_tests("once", FixtureDriverKind::ControlPlane);
        let _ = harness.run_fixture(&mut fixture).await;
        let second = harness.run_fixture(&mut fixture).await;
        assert_eq!(second.error.unwrap().kind, "fixture_already_run");
    }

    #[tokio::test]
    async fn run_holdout_with_naive_requires_holdout_thresholds() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("holdout")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();
        assert!(matches!(
            harness.run_holdout_with_naive().await,
            Err(EvalError::Manifest(message)) if message.contains("thresholds.set = holdout")
        ));
    }

    #[tokio::test]
    async fn empty_batch_returns_report() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("train")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();
        let report = harness.run_batch(FixtureSet::Train).await.unwrap();
        assert!(report.fixtures.is_empty());
        assert!(report.trajectories.is_empty());
        assert_eq!(report.toolchain.rustc_version, "none");
        assert_eq!(report.toolchain.cargo_version, "none");
        assert_eq!(report.toolchain.channel, "1.97.1");
    }

    #[tokio::test]
    async fn directory_enumeration_failure_is_batch_error() {
        let dir = tempfile::tempdir().unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        assert!(matches!(
            harness.run_batch(FixtureSet::Train).await,
            Err(EvalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[tokio::test]
    async fn malformed_fixture_does_not_abort_siblings() {
        let dir = tempfile::tempdir().unwrap();
        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "valid",
            "skeleton_replay",
            &toolchain(),
        );
        fs::create_dir_all(dir.path().join("train/malformed")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        let report = harness.run_batch(FixtureSet::Train).await.unwrap();

        assert_eq!(report.fixtures.len(), 2);
        let malformed = report
            .fixtures
            .iter()
            .find(|outcome| outcome.fixture_id.as_str() == "malformed")
            .unwrap();
        assert_eq!(malformed.status, FixtureStatus::Error);
        assert_eq!(malformed.error.as_ref().unwrap().kind, "fixture_not_found");
        let valid = report
            .fixtures
            .iter()
            .find(|outcome| outcome.fixture_id.as_str() == "valid")
            .unwrap();
        assert_eq!(valid.status, FixtureStatus::Pass, "{valid:?}");
    }

    #[tokio::test]
    async fn reports_always_attach_gate() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("train")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();
        let empty = harness.run_batch(FixtureSet::Train).await.unwrap();
        assert!(empty.gate.is_some());
        assert!(!empty.gate.unwrap().passed);

        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "populated",
            "skeleton_replay",
            &toolchain(),
        );
        let populated = harness.run_batch(FixtureSet::Train).await.unwrap();
        assert!(populated.gate.is_some());
    }

    #[tokio::test]
    async fn cancel_before_batch_marks_all() {
        let dir = tempfile::tempdir().unwrap();
        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "alpha",
            "skeleton_replay",
            &toolchain(),
        );
        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "beta",
            "skeleton_replay",
            &toolchain(),
        );
        let token = CancellationToken::new();
        token.cancel();
        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.cancel = Some(token);
        let harness = EvalHarness::new(config).unwrap();

        let report = harness.run_batch(FixtureSet::Train).await.unwrap();
        assert_eq!(report.fixtures.len(), 2);
        for outcome in &report.fixtures {
            assert_eq!(outcome.status, FixtureStatus::Error);
            assert_eq!(outcome.error.as_ref().unwrap().kind, "cancelled");
            assert_eq!(outcome.model_calls, 0);
        }
        assert!(report.trajectories.is_empty());
    }

    #[tokio::test]
    async fn cancel_during_batch_marks_pending() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let mut config = EvalHarnessConfig::skeleton(dir.path());
        config.cancel = Some(token.clone());
        let harness = EvalHarness::new(config).unwrap();
        let entries: Vec<LoadedEntry> = ["alpha", "beta"]
            .into_iter()
            .map(|id| {
                let fixture = loaded_fixture_for_tests(id, FixtureDriverKind::SkeletonReplay);
                LoadedEntry {
                    id: fixture.manifest.id.clone(),
                    toolchain: fixture.manifest.toolchain.clone(),
                    fixture,
                }
            })
            .collect();
        let semaphore = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&semaphore).acquire_owned().await.unwrap();

        let (phase, ()) = tokio::join!(
            harness.run_loaded_entries(
                FixtureSet::Train,
                entries,
                BatchDriverMode::Manifest,
                &semaphore,
            ),
            async {
                tokio::task::yield_now().await;
                token.cancel();
                drop(held);
            }
        );

        assert_eq!(phase.outcomes.len(), 2);
        assert!(phase.outcomes.iter().all(|outcome| {
            outcome.status == FixtureStatus::Error
                && outcome.error.as_ref().map(|error| error.kind.as_str()) == Some("cancelled")
                && outcome.model_calls == 0
        }));
        assert!(phase.trajectories.is_empty());
    }

    #[tokio::test]
    async fn cross_fixture_toolchain_disagreement_is_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "aaa-reference",
            "skeleton_replay",
            &toolchain(),
        );
        let divergent = ToolchainRecord {
            channel: "1.97.1".to_owned(),
            rustc_version: "rustc 1.97.1 (divergent)".to_owned(),
            cargo_version: "cargo 1.97.1 (divergent)".to_owned(),
        };
        write_test_fixture(
            dir.path(),
            FixtureSet::Train,
            "zzz-divergent",
            "skeleton_replay",
            &divergent,
        );
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        let report = harness.run_batch(FixtureSet::Train).await.unwrap();
        let reference = &report.fixtures[0];
        assert_eq!(reference.fixture_id.as_str(), "aaa-reference");
        assert_eq!(reference.status, FixtureStatus::Pass, "{reference:?}");

        let divergent_outcome = &report.fixtures[1];
        assert_eq!(divergent_outcome.fixture_id.as_str(), "zzz-divergent");
        assert_eq!(divergent_outcome.status, FixtureStatus::Error);
        let error = divergent_outcome.error.as_ref().unwrap();
        assert_eq!(error.kind, "manifest");
        assert!(
            error
                .message
                .contains("cross-fixture toolchain disagreement"),
            "{error:?}"
        );
        // The reference fixture, not the divergent one, defines report versions.
        assert_eq!(report.toolchain.rustc_version, toolchain().rustc_version);
    }

    #[tokio::test]
    async fn holdout_control_rejects_naive_driver() {
        let dir = tempfile::tempdir().unwrap();
        write_test_fixture(
            dir.path(),
            FixtureSet::Holdout,
            "naive-control",
            "naive_baseline",
            &toolchain(),
        );
        let harness = EvalHarness::new(EvalHarnessConfig::milestone_holdout(dir.path())).unwrap();
        let entries = harness
            .enumerate_fixture_entries(FixtureSet::Holdout)
            .await
            .unwrap();
        let batch = harness
            .run_entries(
                FixtureSet::Holdout,
                entries,
                BatchDriverMode::HoldoutControl,
                harness.new_semaphore(1),
            )
            .await;
        assert_eq!(batch.outcomes.len(), 1);
        let error = batch.outcomes[0].error.as_ref().unwrap();
        assert_eq!(error.kind, "manifest");
        assert!(error.message.contains("naive_baseline"), "{error:?}");
        assert!(batch.non_error_toolchains.is_empty());
    }

    #[tokio::test]
    async fn enumeration_reports_invalid_fixture_ids() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("train/Bad Name")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        let report = harness.run_batch(FixtureSet::Train).await.unwrap();

        assert_eq!(report.fixtures.len(), 1);
        let outcome = &report.fixtures[0];
        assert_eq!(outcome.status, FixtureStatus::Error);
        assert_eq!(outcome.model_calls, 0);
        assert_eq!(outcome.error.as_ref().unwrap().kind, "invalid_fixture_id");
        assert!(report.trajectories.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enumeration_reports_invalid_fixture_entries() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let train = dir.path().join("train");
        fs::create_dir_all(&train).unwrap();
        fs::create_dir(train.join(std::ffi::OsString::from_vec(vec![0xff]))).unwrap();
        symlink(outside.path(), train.join("escaping-symlink")).unwrap();
        fs::create_dir(train.join("fixture-like-but-malformed")).unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        let report = harness.run_batch(FixtureSet::Train).await.unwrap();

        assert_eq!(report.fixtures.len(), 3);
        let error_kinds: std::collections::BTreeSet<&str> = report
            .fixtures
            .iter()
            .map(|outcome| outcome.error.as_ref().unwrap().kind.as_str())
            .collect();
        assert_eq!(
            error_kinds,
            std::collections::BTreeSet::from([
                "fixture_not_found",
                "invalid_fixture_name",
                "manifest",
            ])
        );
        assert!(report
            .fixtures
            .iter()
            .all(|outcome| outcome.status == FixtureStatus::Error));
    }

    #[test]
    fn naive_fixture_id_mismatch_is_batch_error() {
        let outcome = |id| {
            error_outcome(
                FixtureId::new(id).unwrap(),
                FixtureSet::Holdout,
                ReportError::cancelled(),
            )
        };

        for (control, naive) in [
            (vec![outcome("a")], vec![outcome("b")]),
            (
                vec![outcome("duplicate"), outcome("duplicate")],
                vec![outcome("duplicate")],
            ),
        ] {
            assert!(matches!(
                ensure_same_fixture_id_set(&control, &naive),
                Err(EvalError::Internal(message))
                    if message == "control/naive fixture id mismatch"
            ));
        }
    }

    #[test]
    fn error_outcome_fields_canonical() {
        let load = load_error_outcome(
            FixtureId::new("load").unwrap(),
            FixtureSet::Train,
            EvalError::Manifest("broken".to_owned()),
        );
        let cancelled = cancelled_outcome(FixtureId::new("cancelled").unwrap(), FixtureSet::Train);
        let joined = error_outcome(
            FixtureId::new("joined").unwrap(),
            FixtureSet::Train,
            ReportError::join_failed("join_failed: test"),
        );

        for outcome in [load, cancelled, joined] {
            assert_eq!(outcome.status, FixtureStatus::Error);
            assert!(outcome.criteria.is_empty());
            assert_eq!(outcome.wall_ms, 0);
            assert_eq!(outcome.model_calls, 0);
            assert_eq!(outcome.tokens_in, None);
            assert_eq!(outcome.tokens_out, None);
            assert_eq!(outcome.cost_usd, None);
            assert_eq!(outcome.retry_count, None);
            assert_eq!(outcome.human_interventions, None);
            assert_eq!(outcome.unsafe_introduced, None);
            assert_eq!(outcome.compile_clean, None);
            assert!(outcome.error.is_some());
        }
    }

    #[tokio::test]
    async fn join_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        fs::write(&golden, "fixed").unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();

        let mut panicking =
            loaded_fixture_for_tests("panicking", FixtureDriverKind::SkeletonReplay);
        panicking.paths.golden = golden.clone();
        panicking.panic_after_dispatch = true;
        let mut sibling = loaded_fixture_for_tests("sibling", FixtureDriverKind::SkeletonReplay);
        sibling.paths.golden = golden;
        let entries = [panicking, sibling]
            .into_iter()
            .map(|fixture| LoadedEntry {
                id: fixture.manifest.id.clone(),
                toolchain: fixture.manifest.toolchain.clone(),
                fixture,
            })
            .collect();

        let phase = harness
            .run_loaded_entries(
                FixtureSet::Train,
                entries,
                BatchDriverMode::Manifest,
                &harness.new_semaphore(2),
            )
            .await;

        assert_eq!(phase.outcomes.len(), 2);
        let panicking = phase
            .outcomes
            .iter()
            .find(|outcome| outcome.fixture_id.as_str() == "panicking")
            .unwrap();
        assert_eq!(panicking.status, FixtureStatus::Error);
        assert_eq!(panicking.model_calls, 0);
        assert_eq!(panicking.error.as_ref().unwrap().kind, "join_failed");
        let sibling = phase
            .outcomes
            .iter()
            .find(|outcome| outcome.fixture_id.as_str() == "sibling")
            .unwrap();
        assert_eq!(sibling.status, FixtureStatus::Pass, "{sibling:?}");
    }

    #[tokio::test]
    async fn panic_drops_fixture_trajectory_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let harness = EvalHarness::new(EvalHarnessConfig::skeleton(dir.path())).unwrap();
        let mut fixture =
            loaded_fixture_for_tests("panic-buffer", FixtureDriverKind::SkeletonReplay);
        fixture.panic_after_dispatch = true;
        let entry = LoadedEntry {
            id: fixture.manifest.id.clone(),
            toolchain: fixture.manifest.toolchain.clone(),
            fixture,
        };

        let phase = harness
            .run_loaded_entries(
                FixtureSet::Train,
                vec![entry],
                BatchDriverMode::Manifest,
                &harness.new_semaphore(1),
            )
            .await;

        assert_eq!(phase.outcomes.len(), 1);
        let outcome = &phase.outcomes[0];
        assert_eq!(outcome.status, FixtureStatus::Error);
        assert_eq!(outcome.model_calls, 0);
        assert_eq!(outcome.error.as_ref().unwrap().kind, "join_failed");
        assert!(phase.trajectories.is_empty());
    }

    /// Write a complete, loadable fixture tree under `root`.
    fn write_test_fixture(
        root: &Path,
        set: FixtureSet,
        id: &str,
        driver: &str,
        toolchain: &ToolchainRecord,
    ) {
        let dir = root.join(set.to_string()).join(id);
        fs::create_dir_all(dir.join("workspace/src")).unwrap();
        fs::create_dir_all(dir.join("recordings")).unwrap();
        fs::write(dir.join("LICENSE"), "MIT license text\n").unwrap();
        fs::write(dir.join("workspace/src/lib.rs"), "pub fn broken() {}\n").unwrap();
        fs::write(dir.join("workspace/src/lib.rs.post"), "pub fn fixed() {}\n").unwrap();
        write_test_recording(
            &dir.join("recordings/pre.json"),
            toolchain,
            1,
            vec![serde_json::json!({
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "message": "cannot borrow x",
                    "code": { "code": "E0502" }
                }
            })
            .to_string()],
        );
        write_test_recording(
            &dir.join("recordings/post.json"),
            toolchain,
            0,
            vec![serde_json::json!({ "reason": "build-finished" }).to_string()],
        );
        fs::write(
            dir.join("manifest.toml"),
            test_manifest_toml(id, set, driver, toolchain),
        )
        .unwrap();
    }

    fn write_test_recording(
        path: &Path,
        toolchain: &ToolchainRecord,
        exit_code: i32,
        stdout_lines: Vec<String>,
    ) {
        let digest = Digest::sha256(stdout_lines.join("\n").as_bytes());
        fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "recording_format_version": crate::recording::CARGO_RECORDING_FORMAT_VERSION,
                "toolchain": toolchain,
                "argv": ["cargo", "check", "--message-format=json"],
                "exit_code": exit_code,
                "stdout_lines": stdout_lines,
                "stderr": "",
                "content_digest": digest,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn test_manifest_toml(
        id: &str,
        set: FixtureSet,
        driver: &str,
        toolchain: &ToolchainRecord,
    ) -> String {
        let channel = &toolchain.channel;
        let rustc_version = &toolchain.rustc_version;
        let cargo_version = &toolchain.cargo_version;
        format!(
            r#"
manifest_version = 1
id = "{id}"
set = "{set}"
naive_target_path = "src/lib.rs"
naive_patch_mode = "full_file_replace"
success_criteria = ["compile_clean"]
driver = "{driver}"

[license]
class = "permitted"
spdx = "MIT"
source_note = "harness unit test fixture"

[toolchain]
channel = "{channel}"
rustc_version = "{rustc_version}"
cargo_version = "{cargo_version}"

[workspace]
path = "workspace"
package = "fixture"

[[expected_diagnostics]]
code = "E0502"
message_contains = "borrow"

[cargo_recordings]
pre_repair = "recordings/pre.json"
post_repair = "recordings/post.json"
recording_format_version = 1

[[turns]]

[turns.turn_id]
capability = "repair"
ordinal = 0

[turns.request]
messages = [{{ role = "user", content = "repair" }}]

[turns.outcome]
type = "response"
text = "pub fn fixed() {{}}\n"

[turns.outcome.usage]
input_tokens = 3
output_tokens = 5
"#
        )
    }

    pub(crate) fn loaded_fixture_for_tests(id: &str, driver: FixtureDriverKind) -> LoadedFixture {
        loaded_fixture_with_outcome(id, driver, response_outcome(Some("fixed")))
    }

    /// Successful scripted response with complete usage on both sides.
    pub(crate) fn response_outcome(text: Option<&str>) -> ScriptTurnOutcome {
        ScriptTurnOutcome::Response {
            text: text.map(str::to_owned),
            structured: None,
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
            },
            provider_request_id: None,
            finish_reason: None,
        }
    }

    pub(crate) fn loaded_fixture_with_outcome(
        id: &str,
        driver: FixtureDriverKind,
        turn_outcome: ScriptTurnOutcome,
    ) -> LoadedFixture {
        let manifest = FixtureManifest {
            manifest_version: manifest::FIXTURE_MANIFEST_VERSION,
            id: FixtureId::new(id).unwrap(),
            set: FixtureSet::Train,
            license: LicenseMeta {
                class: LicenseClass::Permitted,
                spdx: "MIT".to_owned(),
                source_note: "test".to_owned(),
            },
            toolchain: toolchain(),
            workspace: WorkspaceRef {
                path: "workspace".to_owned(),
                package: "fixture".to_owned(),
            },
            naive_target_path: "src/lib.rs".to_owned(),
            naive_patch_mode: NaivePatchMode::FullFileReplace,
            endpoint_prices: None,
            expected_diagnostics: vec![ExpectedDiagnostic {
                code: "E0001".to_owned(),
                message_contains: "broken".to_owned(),
            }],
            turns: vec![ScriptTurn {
                turn_id: FixtureTurnId {
                    capability: CapabilityId::new("repair").unwrap(),
                    node: None,
                    ordinal: 0,
                },
                request: request(),
                request_fingerprint: None,
                outcome: turn_outcome.clone(),
            }],
            cargo_recordings: CargoRecordingRefs {
                pre_repair: "pre.json".to_owned(),
                post_repair: "post.json".to_owned(),
                recording_format_version: crate::recording::CARGO_RECORDING_FORMAT_VERSION,
            },
            success_criteria: vec![SuccessCriterion::CompileClean],
            require_consume_all: true,
            driver,
        };
        let endpoint = endpoint();
        let provider =
            Arc::new(ScriptedProvider::new(endpoint.provider.clone(), endpoint.clone()).unwrap());
        LoadedFixture {
            manifest,
            root: PathBuf::new(),
            paths: FixturePaths {
                target: PathBuf::new(),
                golden: PathBuf::new(),
                pre_repair: PathBuf::new(),
                post_repair: PathBuf::new(),
            },
            pre_repair: recording(1),
            post_repair: recording(0),
            endpoint,
            script_entries: vec![(
                RequestFingerprint::of(&request()),
                ScriptOutcome::from(turn_outcome),
            )],
            scripts: Some(provider),
            panic_after_dispatch: false,
            cancel_at_checkpoint: None,
        }
    }

    fn endpoint() -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("eval-script").unwrap(),
            provider: ProviderId::new("eval-script").unwrap(),
            display_name: "eval-script".to_owned(),
            model: "scripted".to_owned(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            max_context: 8192,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "repair".to_owned(),
            }],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format: ResponseFormat::Text,
            temperature: None,
            max_output_tokens: None,
        }
    }

    fn recording(exit_code: i32) -> CargoJsonRecording {
        CargoJsonRecording {
            recording_format_version: crate::recording::CARGO_RECORDING_FORMAT_VERSION,
            toolchain: toolchain(),
            argv: vec!["cargo".to_owned(), "check".to_owned()],
            exit_code,
            stdout_lines: vec![],
            stderr: String::new(),
            content_digest: Digest::sha256(b""),
        }
    }

    fn toolchain() -> ToolchainRecord {
        ToolchainRecord {
            channel: "1.97.1".to_owned(),
            rustc_version: "rustc 1.97.1".to_owned(),
            cargo_version: "cargo 1.97.1".to_owned(),
        }
    }
}
