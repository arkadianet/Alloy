//! Metrics shapes (writers in RFC-0004).

use serde::{Deserialize, Serialize};

use super::budget::ModelTier;
use super::diagnostic::ErrorClass;
use super::ids::ProviderId;

/// Per-worker execution metrics (published for later RFCs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerMetrics {
    /// Tier actually used.
    pub model_tier_used: ModelTier,
    /// Provider id.
    pub provider_id: ProviderId,
    /// Input tokens when reported; `None` means unknown / not reported.
    pub input_tokens: Option<u64>,
    /// Output tokens when reported; `None` means unknown / not reported.
    pub output_tokens: Option<u64>,
    /// Tool call count.
    pub tool_calls: u32,
    /// Cache hit count.
    pub cache_hits: u32,
    /// Wall duration in milliseconds.
    pub duration_ms: u64,
    /// Model confidence when the provider supplies one; `None` if unavailable.
    pub confidence: Option<f32>,
    /// Optional error class.
    pub error_class: Option<ErrorClass>,
}

/// Process-local runtime counters (snapshot view).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeMetrics {
    /// Successful phase transitions.
    pub phase_transitions: u64,
    /// `run` invocations started.
    pub runs_started: u64,
    /// `run` invocations completed successfully.
    pub runs_completed: u64,
    /// `run` invocations that failed.
    pub runs_failed: u64,
    /// Successful shutdowns.
    pub shutdowns: u64,
}
