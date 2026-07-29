//! Budget and session-create shapes.

use serde::{Deserialize, Serialize};

use super::ids::{ArtifactId, LanguageId, ProfileId};

/// Run-level budget policy (V2 profile budgets).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetPolicy {
    /// USD limit for the run. MVP stores `f64` for V2 parity; do not rely on exact equality.
    pub max_usd_per_run: f64,
    /// Token ceiling for the run.
    pub max_tokens_per_run: u64,
    /// Max parallel DAG nodes (MVP: 1).
    pub max_parallel_nodes: u32,
    /// Max parallel cargo invocations (MVP: 1).
    pub max_parallel_cargo: u32,
    /// Max parallel edits (MVP: 1).
    pub max_parallel_edits: u32,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            max_usd_per_run: 5.0,
            max_tokens_per_run: 2_000_000,
            max_parallel_nodes: 1,
            max_parallel_cargo: 1,
            max_parallel_edits: 1,
        }
    }
}

/// Per-node token budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Max prompt/input tokens.
    pub max_input: u64,
    /// Max completion/output tokens.
    pub max_output: u64,
}

/// Spent budget snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    /// USD spent so far.
    pub usd_spent: f64,
    /// Input tokens consumed.
    pub tokens_in: u64,
    /// Output tokens consumed.
    pub tokens_out: u64,
}

/// Model cost/capability tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Highest capability tier.
    Premium,
    /// Default repair/edit tier.
    Standard,
    /// Cheaper review tier.
    Economy,
    /// Local/offline provider tier.
    Local,
}

/// Request to create a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSession {
    /// Workspace root path.
    pub workspace_root: std::path::PathBuf,
    /// Profile catalog id.
    pub profile: ProfileId,
    /// Budget policy for the session.
    pub budget: BudgetPolicy,
    /// Enabled language backends (MVP: `["rust"]`).
    pub language_backends: Vec<LanguageId>,
    /// Provenance and consent recorded at creation (research §7.11 item 4).
    /// `None` persists the fail-closed `SessionProvenance::unknown()` — no
    /// consent. Consent is write-once per session: granting it later means
    /// creating a new session, never mutating this one.
    #[serde(default)]
    pub provenance: Option<crate::types::provenance::SessionProvenance>,
}

/// User goal submitted to a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    /// Natural-language goal text.
    pub text: String,
    /// Hard constraints.
    pub constraints: Vec<Constraint>,
    /// Attached artifact ids.
    pub attachments: Vec<ArtifactId>,
}

/// Goal constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    /// Cap USD spend for this goal.
    MaxUsd(f64),
    /// Require cargo check before completion.
    RequireCargoCheck,
    /// Deny raw bash tool use.
    DenyRawBash,
    /// Extension point.
    Custom(String),
}
