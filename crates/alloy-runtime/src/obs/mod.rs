//! Observability & cost metering (RFC-0004).
//!
//! Decision recording, process-local cost metering, hash/redaction helpers, and
//! EventStore query utilities. Persistence is the existing session event log only —
//! no parallel observability database and no OTLP in MVP.
//!
//! # Module layout
//!
//! - [`error`] — [`ObsError`]
//! - [`decision`] — [`DecisionLog`], records, [`EventDecisionLog`]
//! - [`recording`] — [`RecordingDecisionLog`] test double
//! - [`cost`] — [`CostMeter`], [`SharedCostMeter`], [`BudgetCheck`]
//! - [`hash`] — content hashing via [`crate::Digest`]
//! - [`redact`] — secret redaction + [`RetentionPolicy`]
//! - [`query`] — [`list_decision_events`], parse helpers, reaccumulate
//! - [`budget`] — [`maybe_signal_budget_warning`]
//!
//! Dependency direction: `obs` → `runtime` / `session` / `storage` / `types`.
//! `session`, `storage`, and `runtime` MUST NOT depend on `obs`.

pub mod budget;
pub mod cost;
pub mod decision;
pub mod error;
pub mod hash;
pub mod query;
pub mod recording;
pub mod redact;

pub use budget::maybe_signal_budget_warning;
pub use cost::{BudgetCheck, CostByTier, CostMeter, CostSnapshot, SharedCostMeter, TierCost};
pub use decision::{
    DecisionKind, DecisionLog, DecisionRecord, EventDecisionLog, ModelCallRecord, ToolCallRecord,
};
pub use error::ObsError;
pub use hash::{hash_content, hash_prompt, hash_tool_body};
pub use query::{
    list_decision_events, parse_decision_event, parse_model_call_event, parse_tool_call_event,
    reaccumulate_cost_from_events, DecisionPage,
};
pub use recording::RecordingDecisionLog;
pub use redact::{
    apply_prompt_retention, apply_tool_retention, redact_json_strings, redact_secrets,
    RetentionPolicy,
};
