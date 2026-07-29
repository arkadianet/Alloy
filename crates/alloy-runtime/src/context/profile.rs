//! Profile-driven configuration (`[context]`) — RFC-0012 §3.5, §4.6.
//!
//! RFC-0015 owns the profile file and its layering; this module owns the
//! `[context]` schema, its V2 Appendix B defaults, and the D2/D19
//! validation.

use super::error::ContextError;
use super::types::DomainId;

/// Fixed domain weights (V2 §8.1 "Fixed weights").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DomainWeights {
    /// Conversation share (default `0.20`).
    pub conversation: f32,
    /// WorkingSet share (default `0.55`).
    pub working_set: f32,
    /// Artifacts share (default `0.25`).
    pub artifacts: f32,
}

impl DomainWeights {
    /// V2 Appendix B defaults.
    #[must_use]
    pub const fn v2_defaults() -> Self {
        Self {
            conversation: 0.20,
            working_set: 0.55,
            artifacts: 0.25,
        }
    }

    /// Reject non-finite, negative, or all-zero weights (rule D2).
    pub fn validate(&self) -> Result<(), ContextError> {
        for (name, w) in [
            ("conversation", self.conversation),
            ("working_set", self.working_set),
            ("artifacts", self.artifacts),
        ] {
            if !w.is_finite() {
                return Err(ContextError::InvalidProfile(format!(
                    "weights.{name} must be finite"
                )));
            }
            if w < 0.0 {
                return Err(ContextError::InvalidProfile(format!(
                    "weights.{name} must not be negative"
                )));
            }
        }
        if self.conversation == 0.0 && self.working_set == 0.0 && self.artifacts == 0.0 {
            return Err(ContextError::InvalidProfile(
                "weights must not be all zero".into(),
            ));
        }
        Ok(())
    }

    /// Share for `domain`; `0.0` for every reserved domain (D1).
    #[must_use]
    pub fn weight_of(&self, domain: DomainId) -> f32 {
        match domain {
            DomainId::Conversation => self.conversation,
            DomainId::WorkingSet => self.working_set,
            DomainId::Artifacts => self.artifacts,
            _ => 0.0,
        }
    }

    /// Sum over the three live weights (rule B4 denominator).
    #[must_use]
    pub(super) fn live_sum(&self) -> f32 {
        self.conversation + self.working_set + self.artifacts
    }
}

/// Profile-driven configuration, parsed from `[context]` by RFC-0015.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextProfile {
    /// V2 Appendix B default: `32_000`.
    pub total_token_budget: usize,
    /// V2 Appendix B defaults: 0.20 / 0.55 / 0.25 (rule D2).
    pub weights: DomainWeights,
    /// Per-file rendered-line cap (default `400`).
    pub max_file_lines: u32,
    /// Max files admitted to the WorkingSet (default `12`).
    pub max_files: usize,
    /// Max diagnostics admitted (default `20`).
    pub max_diagnostics: usize,
    /// Max artifacts admitted (default `8`).
    pub max_artifacts: usize,
    /// Max conversation events scanned backwards (default `200`).
    pub max_conversation_events: usize,
    /// `GraphQuery::Subgraph` radius (default `1`, clamped to `0..=3`).
    pub graph_radius: u8,
    /// Memo capacity (default `32`).
    pub cache_capacity: usize,
    /// Seeds asked for `Callers`/`Refs` impact, in seed order (default `4`;
    /// `0` disables the impact reads entirely) (A-0012-1d).
    pub max_impact_seeds: usize,
    /// Total impact entries admitted to the projection (default `8`)
    /// (A-0012-1d).
    pub max_impact_nodes: usize,
}

impl ContextProfile {
    /// V2 Appendix B defaults.
    #[must_use]
    pub fn v2_defaults() -> Self {
        Self {
            total_token_budget: 32_000,
            weights: DomainWeights::v2_defaults(),
            max_file_lines: 400,
            max_files: 12,
            max_diagnostics: 20,
            max_artifacts: 8,
            max_conversation_events: 200,
            graph_radius: 1,
            cache_capacity: 32,
            max_impact_seeds: 4,
            max_impact_nodes: 8,
        }
    }

    /// Parse the `[context]` table; unknown keys are rejected (RFC-0015).
    pub fn from_toml_table(t: &toml::Table) -> Result<Self, ContextError> {
        let mut profile = Self::v2_defaults();
        for (key, value) in t {
            match key.as_str() {
                "total_token_budget" => {
                    profile.total_token_budget = usize_key(key, value)?;
                }
                "weights" => profile.weights = weights_from_toml(value)?,
                "max_file_lines" => {
                    profile.max_file_lines = u32::try_from(usize_key(key, value)?)
                        .map_err(|_| invalid(key, "out of range"))?;
                }
                "max_files" => profile.max_files = usize_key(key, value)?,
                "max_diagnostics" => profile.max_diagnostics = usize_key(key, value)?,
                "max_artifacts" => profile.max_artifacts = usize_key(key, value)?,
                "max_conversation_events" => {
                    profile.max_conversation_events = usize_key(key, value)?;
                }
                "graph_radius" => {
                    // Clamped to RFC-0011 Q7's 0..=3, not rejected (§3.5).
                    let raw = usize_key(key, value)?;
                    profile.graph_radius = u8::try_from(raw.min(3)).expect("clamped to 3");
                }
                "cache_capacity" => profile.cache_capacity = usize_key(key, value)?,
                "max_impact_seeds" => profile.max_impact_seeds = usize_key(key, value)?,
                "max_impact_nodes" => profile.max_impact_nodes = usize_key(key, value)?,
                other => {
                    return Err(ContextError::InvalidProfile(format!(
                        "unknown [context] key: {other}"
                    )));
                }
            }
        }
        if profile.total_token_budget == 0 {
            return Err(invalid("total_token_budget", "must be >= 1"));
        }
        profile.weights.validate()?;
        Ok(profile)
    }
}

fn invalid(key: &str, why: &str) -> ContextError {
    ContextError::InvalidProfile(format!("{key}: {why}"))
}

fn usize_key(key: &str, value: &toml::Value) -> Result<usize, ContextError> {
    match value {
        toml::Value::Integer(i) if *i >= 0 => {
            usize::try_from(*i).map_err(|_| invalid(key, "out of range"))
        }
        toml::Value::Integer(_) => Err(invalid(key, "must not be negative")),
        _ => Err(invalid(key, "must be an integer")),
    }
}

fn f32_key(key: &str, value: &toml::Value) -> Result<f32, ContextError> {
    #[allow(clippy::cast_possible_truncation)] // profile weights fit f32 by construction
    match value {
        toml::Value::Float(f) => Ok(*f as f32),
        toml::Value::Integer(i) => Ok(*i as f32),
        _ => Err(invalid(key, "must be a number")),
    }
}

/// Parse `weights = { conversation = .., working_set = .., artifacts = .. }`.
///
/// Rule D19: exactly the three live keys; anything else is `InvalidProfile`,
/// so a profile cannot silently pretend to enable a reserved domain.
fn weights_from_toml(value: &toml::Value) -> Result<DomainWeights, ContextError> {
    let table = value
        .as_table()
        .ok_or_else(|| invalid("weights", "must be a table"))?;
    let mut weights = DomainWeights::v2_defaults();
    let mut seen = [false; 3];
    for (key, v) in table {
        match key.as_str() {
            "conversation" => {
                weights.conversation = f32_key("weights.conversation", v)?;
                seen[0] = true;
            }
            "working_set" => {
                weights.working_set = f32_key("weights.working_set", v)?;
                seen[1] = true;
            }
            "artifacts" => {
                weights.artifacts = f32_key("weights.artifacts", v)?;
                seen[2] = true;
            }
            other => {
                return Err(ContextError::InvalidProfile(format!(
                    "unknown weights key: {other} (only the three live domains are weighted)"
                )));
            }
        }
    }
    if seen != [true; 3] {
        return Err(invalid(
            "weights",
            "must name exactly conversation, working_set and artifacts",
        ));
    }
    weights.validate()?;
    Ok(weights)
}
