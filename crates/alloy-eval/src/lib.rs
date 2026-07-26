//! Alloy evaluation crate.
//!
//! Stub surface for RFC-0016 (Eval Harness & Holdout Gates).

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// Cost claim envelope and USD derivation.
pub mod cost_claim;
/// Eval harness error types.
pub mod error;
/// Request fingerprinting helpers.
pub mod fingerprint;
/// Pure holdout gate evaluation.
pub mod gate;
/// R17 license validation.
pub mod license;
/// Strict fixture manifest loading.
pub mod manifest;
/// Eval metric aggregation.
pub mod metrics;
/// Recorded cargo JSON replay.
pub mod recording;
/// Serializable eval reports.
pub mod report;
/// Offline scripted model provider.
pub mod scripted;
/// Eval-local trajectory retention.
pub mod trajectory;

pub use cost_claim::{CostClaimEnvelope, CostClaimGrade, COST_DISCLAIMER};
pub use error::{EvalError, ReportError};
pub use fingerprint::RequestFingerprint;
pub use gate::{
    evaluate_gate, GateFailure, GateResult, GateThresholds, NaiveComparisonResult,
    NAIVE_BASELINE_LABEL,
};
pub use manifest::{
    CargoRecordingRefs, EndpointPrices, ExpectedDiagnostic, FixtureDriverKind, FixtureId,
    FixtureManifest, FixtureSet, FixtureTurnId, LicenseClass, LicenseMeta, NaivePatchMode,
    ScriptTurn, ScriptTurnOutcome, SuccessCriterion, ToolchainRecord, WorkspaceRef,
    FIXTURE_MANIFEST_VERSION, PERMITTED_SPDX,
};
pub use metrics::{EvalMetrics, MetricField, MetricsAggregator, UnmeasuredReason};
pub use report::{
    assemble_report_toolchain, CriterionResult, EvalReport, FixtureOutcome, FixtureStatus,
};
pub use scripted::{ScriptOutcome, ScriptedInvocation, ScriptedProvider, ScriptedProviderError};
pub use trajectory::{sort_trajectories_stable, write_trajectory_artifacts, EvalTrajectoryRecord};
