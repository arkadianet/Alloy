//! Observability & cost metering (RFC-0004).
//!
//! Decision recording, process-local cost metering, hash/redaction helpers, and
//! EventStore query utilities. Persistence is the existing session event log only —
//! no parallel observability database and no OTLP in MVP.
//!
//! Dependency direction: `obs` → `runtime` / `session` / `storage` / `types`.
//! `session`, `storage`, and `runtime` MUST NOT depend on `obs`.

mod budget;
mod cost;
mod decision;
mod error;
mod hash;
mod meter_factory;
mod query;
mod recording;
mod redact;

pub use budget::maybe_signal_budget_warning;
pub use cost::{BudgetCheck, CostByTier, CostMeter, CostSnapshot, SharedCostMeter, TierCost};
pub use decision::{
    DecisionKind, DecisionLog, DecisionRecord, EventDecisionLog, ModelCallRecord, ModelUsdSource,
    ToolCallRecord,
};
pub use error::ObsError;
pub use hash::{hash_content, hash_prompt, hash_tool_body};
pub use meter_factory::{CostMeterFactory, ProcessCostMeterFactory};
pub use query::{
    list_decision_events, parse_decision_event, parse_model_call_event, parse_tool_call_event,
    reaccumulate_cost_from_events, DecisionPage,
};
pub use recording::RecordingDecisionLog;
/// Shared UTF-8-safe truncate used by obs prepare paths and the router.
pub(crate) use redact::truncate_utf8_bytes;
/// Body-size limit for router prompt hashing / retention (RFC-0007 §3.17).
pub(crate) use redact::BODY_MAX_BYTES as MODEL_PROMPT_BODY_MAX_BYTES;
pub use redact::{
    apply_prompt_retention, apply_tool_retention, redact_json_strings, redact_secrets,
    CapturePolicy, RetentionPolicy,
};
