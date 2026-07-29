//! The `Capability` contract (RFC-0013 §3.2, V2 §9.2 with AM-V2-1/2).
//!
//! A capability is a contract, not a persona (V2 §9.1): JSON in, JSON out,
//! stateless across attempts. Everything attempt-specific arrives through
//! [`CapabilityContext`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::dag::NodeKind;
use crate::types::budget::ModelTier;
use crate::types::ids::CapabilityId;
use crate::types::tools::ToolSelector;

use super::deps::CapabilityContext;

/// One capability contract. Implementations are stateless across attempts
/// (rule CW3): everything attempt-specific arrives in [`CapabilityContext`].
#[async_trait]
pub trait Capability: Send + Sync {
    /// Catalog id. MUST be a member of [`super::CAPABILITY_CATALOG`] (RG2).
    fn id(&self) -> CapabilityId;

    /// Contract version (AM-V2-2). Bumped when a payload schema changes.
    fn version(&self) -> CapabilityVersion;

    /// Static description used for disclosure and the decision log.
    fn describe(&self) -> CapabilityDescriptor;

    /// Tool selectors for RFC-0006 lazy disclosure. MUST be a subset of the
    /// registered builtins (RG6).
    fn required_tools(&self) -> Vec<ToolSelector>;

    /// Tier hint. Advisory only: `ctx.effective_tier` wins (MR2).
    fn preferred_tier(&self) -> ModelTier;

    /// Node kinds this capability may be dispatched for. MUST agree with
    /// the RFC-0009 kind ↔ capability validation map (RG3).
    fn accepts_kind(&self, kind: NodeKind) -> bool;

    /// Execute exactly one attempt (rules CW1–CW10).
    async fn execute(
        &self,
        ctx: &CapabilityContext<'_>,
    ) -> Result<CapabilityOutcome, CapabilityExecError>;
}

/// Local semantic version (AM-V2-2 — no `semver` dependency, rule C1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CapabilityVersion {
    /// Major: incompatible payload schema change.
    pub major: u16,
    /// Minor: additive contract change.
    pub minor: u16,
    /// Patch: behaviour-preserving fix.
    pub patch: u16,
}

impl CapabilityVersion {
    /// Construct a version triple.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for CapabilityVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Static description of one capability implementation (§3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    /// Catalog id.
    pub id: CapabilityId,
    /// Contract version.
    pub version: CapabilityVersion,
    /// One-line contract description. Never a persona (V2 §9.1).
    pub summary: String,
    /// Whether this capability performs a model completion (RG1 counts these).
    pub uses_model: bool,
    /// Coarsest side effect this capability may cause.
    pub side_effects: SideEffectClass,
    /// Node kinds it accepts.
    pub kinds: Vec<NodeKind>,
}

/// Side-effect class (V2 §9.2). Ordered least → most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    /// No tool call, no model call.
    Pure,
    /// Model completion and read-only tools only.
    ReadOnly,
    /// May mutate the workspace through the patch builtin.
    WorkspaceWrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_orders_and_displays() {
        let a = CapabilityVersion::new(1, 0, 0);
        let b = CapabilityVersion::new(1, 2, 3);
        assert!(a < b);
        assert_eq!(b.to_string(), "1.2.3");
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<CapabilityVersion>(&json).unwrap(), b);
    }

    #[test]
    fn side_effect_class_orders_least_to_most_privileged() {
        assert!(SideEffectClass::Pure < SideEffectClass::ReadOnly);
        assert!(SideEffectClass::ReadOnly < SideEffectClass::WorkspaceWrite);
        assert_eq!(
            serde_json::to_string(&SideEffectClass::WorkspaceWrite).unwrap(),
            "\"workspace_write\""
        );
    }
}
