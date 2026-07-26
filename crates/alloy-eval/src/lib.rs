//! Offline evaluation harness and holdout gates for RFC-0016.
//!
//! The crate loads validated fixture manifests, replays scripted provider turns
//! without network access, aggregates eval metrics, evaluates gates, and can
//! compare holdout control runs against the naive single-turn baseline.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// Cost claim envelope and USD derivation.
pub mod cost_claim;
mod driver;
/// Eval harness error types.
pub mod error;
/// Request fingerprinting helpers.
pub mod fingerprint;
/// Pure holdout gate evaluation.
pub mod gate;
/// Offline eval harness and fixture runner.
pub mod harness;
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
