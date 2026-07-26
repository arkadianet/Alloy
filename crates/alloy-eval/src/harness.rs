//! Offline RFC-0016 evaluation harness.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_runtime::Digest;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::cost_claim::CostClaimEnvelope;
use crate::driver;
use crate::error::{bound_message, EvalError, ReportError};
use crate::gate::{
    evaluate_gate, GateResult, GateThresholds, NaiveComparisonResult, NAIVE_BASELINE_LABEL,
};
use crate::manifest::{
    self, FixtureDriverKind, FixtureId, FixtureManifest, FixturePaths, FixtureSet,
    LoadedFixtureParts,
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
#[derive(Debug, Clone)]
pub struct EvalHarness {
    config: EvalHarnessConfig,
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
    non_error_toolchains: Vec<manifest::ToolchainRecord>,
}

impl EvalHarness {
    /// Construct and validate a harness.
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
        Ok(Self { config })
    }

    /// Load one manifest and its validated fixture artifacts.
    pub fn load_fixture(
        &self,
        set: FixtureSet,
        id: &FixtureId,
    ) -> Result<LoadedFixture, EvalError> {
        LoadedFixture::from_parts(manifest::load_fixture(
            &self.config.fixture_root,
            set,
            id,
            &self.config.pin_toolchain_channel,
        )?)
    }

    /// Run one fixture and return only its terminal outcome.
    pub async fn run_fixture(&self, fixture: &mut LoadedFixture) -> FixtureOutcome {
        self.run_fixture_collect(fixture).await.outcome
    }

    /// Run all fixtures in `set` and attach gate and optional artifacts.
    pub async fn run_batch(&self, set: FixtureSet) -> Result<EvalReport, EvalError> {
        let entries = self.enumerate_fixture_entries(set)?;
        let batch = self
            .run_entries(set, entries, BatchDriverMode::Manifest)
            .await?;
        let mut report = self.assemble_report(batch)?;
        report.gate = Some(self.evaluate_gate(&report));
        self.write_trajectory_artifacts(&report)?;
        Ok(report)
    }

    /// Evaluate this harness's configured gate thresholds against a report.
    #[must_use]
    pub fn evaluate_gate(&self, report: &EvalReport) -> GateResult {
        evaluate_gate(&self.config.thresholds, report)
    }

    /// Run holdout control and forced naive baseline, then compare and gate.
    pub async fn run_holdout_with_naive(&self) -> Result<EvalReport, EvalError> {
        if self.config.thresholds.set != FixtureSet::Holdout {
            return Err(EvalError::Manifest(
                "run_holdout_with_naive requires thresholds.set = holdout".to_owned(),
            ));
        }

        let entries = self.enumerate_fixture_entries(FixtureSet::Holdout)?;
        let control = self
            .run_entries(
                FixtureSet::Holdout,
                entries.clone(),
                BatchDriverMode::HoldoutControl,
            )
            .await?;
        let naive = self
            .run_entries(FixtureSet::Holdout, entries, BatchDriverMode::ForceNaive)
            .await?;
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
        self.write_trajectory_artifacts(&report)?;
        Ok(report)
    }

    /// Write and rotate the configured trajectory artifact.
    pub fn write_trajectory_artifacts(&self, report: &EvalReport) -> Result<(), EvalError> {
        write_trajectory_artifacts(
            report,
            self.config.artifact_dir.as_deref(),
            self.config.max_retained_runs,
        )
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
        let Some(provider) = fixture.scripts.take() else {
            return fixture_already_run_output(fixture);
        };
        match mode {
            BatchDriverMode::ForceNaive => {
                driver::naive::run(fixture, provider, self.config.cancel.clone()).await
            }
            BatchDriverMode::Manifest | BatchDriverMode::HoldoutControl => {
                match fixture.manifest.driver {
                    FixtureDriverKind::SkeletonReplay => {
                        driver::skeleton::run(fixture, provider, self.config.cancel.clone()).await
                    }
                    FixtureDriverKind::NaiveBaseline => {
                        driver::naive::run(fixture, provider, self.config.cancel.clone()).await
                    }
                    FixtureDriverKind::ControlPlane => driver::control_plane::run(fixture).await,
                }
            }
        }
    }

    fn enumerate_fixture_entries(&self, set: FixtureSet) -> Result<Vec<BatchEntry>, EvalError> {
        let set_path = self.config.fixture_root.join(set_dir_name(set));
        let canonical_fixture_root = self.config.fixture_root.canonicalize()?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(set_path)? {
            let entry = entry?;
            let name = entry.file_name();
            let path = entry.path();
            let (kind, metadata_error) = entry_kind(&path)?;
            match classify_fixture_dir_entry(&name, kind) {
                FixtureDirEntryClass::ValidId(raw) => {
                    let id = FixtureId::new(raw).map_err(|error| {
                        EvalError::Internal(format!(
                            "classifier accepted invalid fixture id: {error}"
                        ))
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

    async fn run_entries(
        &self,
        set: FixtureSet,
        entries: Vec<BatchEntry>,
        mode: BatchDriverMode,
    ) -> Result<BatchOutputs, EvalError> {
        let mut outcomes = Vec::new();
        let mut trajectories = Vec::new();
        let mut non_error_toolchains = Vec::new();
        let mut pending = VecDeque::new();

        for entry in entries {
            match entry {
                BatchEntry::Fixture(id) => pending.push_back(id),
                BatchEntry::Error(outcome) => outcomes.push(outcome),
            }
        }

        while !pending.is_empty() {
            if self.is_cancelled() {
                while let Some(id) = pending.pop_front() {
                    outcomes.push(cancelled_outcome(id, set));
                }
                break;
            }

            let limit = self.config.max_concurrency.min(pending.len());
            let mut handles = Vec::<(
                FixtureId,
                JoinHandle<(FixtureRunOutput, manifest::ToolchainRecord)>,
            )>::new();
            for _ in 0..limit {
                if self.is_cancelled() {
                    break;
                }
                let Some(id) = pending.pop_front() else {
                    break;
                };
                let mut fixture = match self.load_fixture(set, &id) {
                    Ok(fixture) => fixture,
                    Err(error) => {
                        outcomes.push(error_outcome(id, set, ReportError::from_eval(&error)));
                        continue;
                    }
                };
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
                let harness = self.clone();
                let handle = tokio::spawn(async move {
                    let output = harness.run_loaded_fixture_collect(&mut fixture, mode).await;
                    (output, toolchain)
                });
                handles.push((id, handle));
            }

            for (id, handle) in handles {
                match handle.await {
                    Ok((output, toolchain)) => {
                        if output.outcome.status != FixtureStatus::Error {
                            non_error_toolchains.push(toolchain);
                        }
                        trajectories.extend(output.trajectories);
                        outcomes.push(output.outcome);
                    }
                    Err(error) => outcomes.push(join_failed_outcome(id, set, error)),
                }
            }
        }

        sort_outcomes(&mut outcomes);
        sort_trajectories_stable(&mut trajectories);
        Ok(BatchOutputs {
            outcomes,
            trajectories,
            non_error_toolchains,
        })
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
        })
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

fn join_failed_outcome(fixture_id: FixtureId, set: FixtureSet, error: JoinError) -> FixtureOutcome {
    error_outcome(
        fixture_id,
        set,
        ReportError::join_failed(join_failed_message(error)),
    )
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
    use crate::manifest::{
        CargoRecordingRefs, ExpectedDiagnostic, FixtureTurnId, LicenseClass, LicenseMeta,
        NaivePatchMode, ScriptTurn, ScriptTurnOutcome, SuccessCriterion, ToolchainRecord,
        WorkspaceRef,
    };
    use alloy_runtime::{
        CapabilityId, ChatMessage, ChatRole, CompletionRequest, EndpointId, ModelEndpoint,
        ModelTier, ProviderId, ResponseFormat, ToolChoice, Usage,
    };

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
    fn config_validation_rejects_bad_bounds() {
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

    pub(crate) fn loaded_fixture_for_tests(id: &str, driver: FixtureDriverKind) -> LoadedFixture {
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
                outcome: ScriptTurnOutcome::Response {
                    text: Some("fixed".to_owned()),
                    structured: None,
                    usage: Usage {
                        input_tokens: Some(1),
                        output_tokens: Some(1),
                    },
                    provider_request_id: None,
                    finish_reason: None,
                },
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
                workspace_dir: PathBuf::new(),
                target: PathBuf::new(),
                golden: PathBuf::new(),
                pre_repair: PathBuf::new(),
                post_repair: PathBuf::new(),
            },
            pre_repair: recording(1),
            post_repair: recording(0),
            endpoint,
            script_entries: vec![],
            scripts: Some(provider),
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
