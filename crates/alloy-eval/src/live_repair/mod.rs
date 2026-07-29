//! Live-endpoint repair benchmark support (operator tooling — **not** a gate).
//!
//! # Separation from the RFC-0016 offline harness
//!
//! RFC-0016 makes `alloy-eval`'s harness offline by construction: no HTTP
//! client, no live-endpoint provider feature, no HTTP crate in the dependency
//! graph, and no process spawning anywhere under `crates/alloy-eval/src`. This
//! module does not change any of that. It contains **only** pure, offline
//! logic:
//!
//! * loading and validating `live-manifest.toml` fixtures,
//! * rendering a `router.toml` document as a string,
//! * scoring already-collected observations (Wilson intervals).
//!
//! Executing the real `alloy` binary against a live endpoint is done by the
//! thin shell wrapper at `eval/live-repair/run.sh`, which is the only
//! component that spawns a process or touches the network. The library and the
//! `alloy-eval-live-repair` binary never do either.
//!
//! Everything here is named `live_repair` / `LiveRepair*` /
//! `alloy-eval-live-repair`, lives in a corpus outside
//! `crates/alloy-eval/fixtures/`, and produces a [`LiveRepairReport`] that
//! cannot be passed to [`crate::evaluate_gate`], always carries
//! `offline = false`, and always renders `holdout_gate=not_applicable`.

pub(crate) mod manifest;
pub(crate) mod report;
pub(crate) mod router;
pub(crate) mod score;

pub use manifest::{
    LiveRepairCorpus, LiveRepairExpectedOutcome, LiveRepairFixture, LiveRepairManifest,
    LIVE_REPAIR_GOAL_MAX_BYTES, LIVE_REPAIR_MANIFEST_FILE, LIVE_REPAIR_MANIFEST_VERSION,
    LIVE_REPAIR_MAX_TAGS,
};
pub use report::{
    parse_observations_jsonl, LiveRepairEndpoint, LiveRepairFixtureReport,
    LiveRepairGateApplicability, LiveRepairObservation, LiveRepairOutcome, LiveRepairPassRate,
    LiveRepairReport, LIVE_REPAIR_REPORT_VERSION,
};
pub use router::{render_router_toml, LIVE_REPAIR_REQUEST_TIMEOUT_MS};
pub use score::{wilson_interval, WilsonInterval, WILSON_Z_95};
