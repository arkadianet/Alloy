//! Planner knobs (RFC-0017 §3.3, profile `[planner]` table §7.1).
//!
//! Author: arkadianet

use serde::{Deserialize, Serialize};

use crate::types::budget::TokenBudget;

/// Which plan service the composition root constructs (AM-0009-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerMode {
    /// Closed template catalog (`PlannerConfig::new` default; shipped
    /// `readonly` stays here).
    Template,
    /// LLM proposal path, fail-closed onto templates. Shipped `default` /
    /// `autonomous` after the RFC-0017 §12.4 holdout flip; `readonly`
    /// profiles reject it at assembly.
    Llm,
}

/// Validated planner knobs (profile `[planner]` table, RFC-0017 §7.1).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannerConfig {
    /// Plan source selection. Default [`PlannerMode::Template`].
    pub mode: PlannerMode,
    /// PC4 ceiling. Default 8; accepted `2..=16`.
    pub max_proposed_nodes: u32,
    /// Cap on the raw proposal bytes (PC2). Default 16_384; accepted
    /// `1_024..=32_768`.
    ///
    /// **Hard ceiling rationale (OC7).** The proposal rides inside
    /// `PlanningProposalPayload`, and RFC-0013 OC7 bounds the *total
    /// serialized worker payload* at `MAX_PAYLOAD_TOTAL_BYTES = 64 KiB`,
    /// enforced fail-closed inside the worker. A 32 KiB ceiling leaves the
    /// payload's other fields and JSON framing a full 32 KiB of headroom;
    /// values above it are not merely unwise, they are unreachable.
    pub proposal_max_bytes: u32,
    /// Token budget for the planning capability call. Default
    /// `{ max_input: 16_384, max_output: 4_096 }`.
    pub planning_budget: TokenBudget,
    /// Planning call timeout. Default 120_000; must be > 0.
    pub planning_timeout_ms: u64,
}

impl PlannerConfig {
    /// RFC-0017 §3.3 defaults. Out-of-range values are a construction error
    /// at config resolution (fail closed, no clamping-to-valid) — see
    /// [`Self::validate`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: PlannerMode::Template,
            max_proposed_nodes: 8,
            proposal_max_bytes: 16_384,
            planning_budget: TokenBudget {
                max_input: 16_384,
                max_output: 4_096,
            },
            planning_timeout_ms: 120_000,
        }
    }

    /// Range validation (§3.3 / §7.1): first violation named. No clamping.
    pub fn validate(&self) -> Result<(), String> {
        if !(2..=16).contains(&self.max_proposed_nodes) {
            return Err(format!(
                "[planner].max_proposed_nodes must be in 2..=16, got {}",
                self.max_proposed_nodes
            ));
        }
        if !(1_024..=32_768).contains(&self.proposal_max_bytes) {
            return Err(format!(
                "[planner].proposal_max_bytes must be in 1024..=32768 (OC7 headroom), got {}",
                self.proposal_max_bytes
            ));
        }
        if self.planning_timeout_ms == 0 {
            return Err("[planner].planning_timeout_ms must be > 0".into());
        }
        Ok(())
    }
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self::new()
    }
}
