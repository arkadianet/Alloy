//! Offline evaluation harness and holdout gates for RFC-0016.
//!
//! The crate loads validated fixture manifests, replays scripted provider turns
//! without network access, aggregates eval metrics, evaluates gates, and can
//! compare holdout control runs against the naive single-turn baseline.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// Cost claim envelope and USD derivation.
mod cost_claim;
mod driver;
/// Eval harness error types.
mod error;
/// Request fingerprinting helpers.
mod fingerprint;
/// Pure holdout gate evaluation.
mod gate;
/// Offline eval harness and fixture runner.
mod harness;
/// R17 license validation.
mod license;
/// Strict fixture manifest loading.
mod manifest;
/// Eval metric aggregation.
mod metrics;
/// Recorded cargo JSON replay.
mod recording;
/// Serializable eval reports.
mod report;
/// Offline scripted model provider.
mod scripted;
/// Eval-local trajectory retention.
mod trajectory;

pub use cost_claim::{CostClaimEnvelope, CostClaimGrade, COST_DISCLAIMER};
pub use error::{EvalError, ReportError};
pub use fingerprint::RequestFingerprint;
pub use gate::{
    evaluate_gate, GateFailure, GateResult, GateThresholds, NaiveComparisonResult,
    NAIVE_BASELINE_LABEL,
};
pub use harness::{EvalHarness, EvalHarnessConfig, LoadedFixture, EVAL_MAX_CONCURRENCY};
pub use manifest::{
    CargoRecordingRefs, EndpointPrices, ExpectedDiagnostic, FixtureDriverKind, FixtureId,
    FixtureManifest, FixtureSet, FixtureTurnId, LicenseClass, LicenseMeta, NaivePatchMode,
    ScriptTurn, ScriptTurnOutcome, SuccessCriterion, ToolchainRecord, WorkspaceRef,
    FIXTURE_MANIFEST_VERSION, PERMITTED_SPDX,
};
pub use metrics::{EvalMetrics, MetricField, UnmeasuredReason};
pub use recording::{CargoJsonRecording, RecordedDiagnostic, CARGO_RECORDING_FORMAT_VERSION};
pub use report::{CriterionResult, EvalReport, FixtureOutcome, FixtureStatus};
pub use scripted::{ScriptOutcome, ScriptedInvocation, ScriptedProvider, ScriptedProviderError};
pub use trajectory::EvalTrajectoryRecord;
