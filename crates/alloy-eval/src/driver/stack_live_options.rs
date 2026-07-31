//! Live stack-driver options (RFC-0016 §5.9 / RFC-0012 weight arms).
//!
//! Kept additive so concurrent `stack.rs` work can merge against a thin
//! options surface. `context_profile: None` preserves the historical
//! `NullContextEngine` + keyed [`ScriptedProvider`] smoke path;
//! `Some(profile)` selects [`DefaultContextEngine`] and FIFO
//! [`RecordingModelProvider`] (fingerprints differ across weight arms).
//!
//! Author: arkadianet

use alloy_runtime::{ContextProfile, PlannerMode};

/// Options for [`super::stack::run_live_with_options`].
#[derive(Debug, Clone)]
pub struct StackLiveOptions {
    /// Planner mode (`Template` default; `Llm` is CapabilityPlanProposer +
    /// PlanningWorker smoke — not RFC-0017 §12.4 flip evidence).
    pub planner: PlannerMode,
    /// When `Some`, wire [`alloy_runtime::DefaultContextEngine`] with this
    /// profile (weight-measurement arms). When `None`, keep
    /// [`alloy_runtime::NullContextEngine`] (integration-smoke default).
    pub context_profile: Option<ContextProfile>,
    /// Bound forwarded to runtime config + [`alloy_runtime::GenerationPolicy`].
    pub max_repair_generations: u32,
}

impl Default for StackLiveOptions {
    fn default() -> Self {
        Self {
            planner: PlannerMode::Template,
            context_profile: None,
            max_repair_generations: 2,
        }
    }
}

impl StackLiveOptions {
    /// Template planner, null context (historical smoke default).
    #[must_use]
    pub fn template() -> Self {
        Self::default()
    }

    /// Template planner with an injectable context profile (weight arms).
    #[must_use]
    pub fn with_context_profile(profile: ContextProfile) -> Self {
        Self {
            context_profile: Some(profile),
            ..Self::default()
        }
    }

    /// Builder: set planner mode.
    #[must_use]
    pub fn planner(mut self, mode: PlannerMode) -> Self {
        self.planner = mode;
        self
    }

    /// Builder: set max repair generations.
    #[must_use]
    pub fn max_repair_generations(mut self, n: u32) -> Self {
        self.max_repair_generations = n;
        self
    }
}
