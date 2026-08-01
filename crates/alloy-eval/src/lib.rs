//! Offline evaluation harness and holdout gates for RFC-0016.
//!
//! The crate loads validated fixture manifests, replays scripted provider turns
//! without network access, aggregates eval metrics, evaluates gates, and can
//! compare holdout control runs against the naive single-turn baseline.
//!
//! Optional feature `stack-driver` enables the live ControlPlane **integration
//! smoke** path (scheduler + Landlock sandbox + MCP + EditEngine). Golden /
//! committed-recording patches are not thesis citation; the default build stays
//! offline.
//!
//! It additionally hosts the `live_repair` module, the **operator**
//! live-endpoint repair benchmark rooted at [`LiveRepairCorpus`] and
//! [`LiveRepairReport`]. That surface is deliberately disjoint from the holdout gates: a
//! different corpus root (`eval/live-repair/fixtures/`), a different manifest
//! file name (`live-manifest.toml`), a different report type
//! ([`LiveRepairReport`], always `offline = false`), and no path into
//! [`evaluate_gate`]. It stays pure — no process spawning and no network I/O —
//! so RFC-0016 §10.2's offline guarantee holds for the whole crate; the thin
//! wrapper at `eval/live-repair/run.sh` is what actually executes anything.

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
/// Strict live holdout oracle and arm comparison.
mod live_holdout;
/// One-shot, tool-free naive driver (E1 three-arm holdout, arm B).
#[cfg(feature = "live-naive")]
mod live_naive;
/// Live-endpoint repair benchmark (operator tooling; never a holdout gate).
mod live_repair;
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
pub use live_holdout::{
    compare as compare_live_holdout, load_observations as load_live_holdout_observations,
    oracle as inspect_live_holdout, score as score_live_holdout,
    target_path_text as live_holdout_target_path_text, telemetry as live_holdout_telemetry,
    Endpoint as LiveHoldoutEndpoint, HarnessIdentity as LiveHoldoutHarnessIdentity,
    LiveHoldoutDriver, MatrixComparison as LiveHoldoutMatrixComparison,
    OracleEvidence as LiveHoldoutOracleEvidence, RunTelemetry as LiveHoldoutRunTelemetry,
    StrictObservation as LiveHoldoutObservation,
    StrictObservationFields as LiveHoldoutObservationFields, StrictReport as LiveHoldoutReport,
    REPORT_SCHEMA_VERSION as LIVE_HOLDOUT_REPORT_VERSION,
};
#[cfg(feature = "live-naive")]
pub use live_naive::{
    build_naive_request, parse_replacement, resolve_target, write_replacement,
    write_resolved_replacement, NaiveReplacement, NaiveRunTelemetry, MAX_REPLACEMENT_BYTES,
    NAIVE_SCHEMA_NAME,
};
pub use live_repair::{
    parse_observations_jsonl, render_router_toml, wilson_interval, LiveRepairCorpus,
    LiveRepairEndpoint, LiveRepairExpectedOutcome, LiveRepairFixture, LiveRepairFixtureReport,
    LiveRepairGateApplicability, LiveRepairManifest, LiveRepairObservation, LiveRepairOutcome,
    LiveRepairPassRate, LiveRepairReport, WilsonInterval, LIVE_REPAIR_GOAL_MAX_BYTES,
    LIVE_REPAIR_MANIFEST_FILE, LIVE_REPAIR_MANIFEST_VERSION, LIVE_REPAIR_MAX_TAGS,
    LIVE_REPAIR_REPORT_VERSION, LIVE_REPAIR_REQUEST_TIMEOUT_MS, WILSON_Z_95,
};
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

/// Live stack-driver options (planner, context-profile weight arms, repair gens).
#[cfg(feature = "stack-driver")]
pub use driver::stack_live_options::StackLiveOptions;
