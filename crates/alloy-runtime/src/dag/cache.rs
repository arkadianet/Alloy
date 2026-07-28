//! Cache-key builder (RFC-0009 §5.8). Day-1 templates leave `cache_key = None`.
//!
//! `PlanContext` fingerprint fields (`policy_hash`, `tool_versions`,
//! `compiler_fingerprint`) are reserved for templates with `enable_cache = true`
//! (RFC-0010); day-1 instantiation never calls [`compute_cache_key`].

use crate::dag::types::{CacheKey, NodeKind};
use crate::types::ids::{CapabilityId, Digest};
use crate::types::toolchain::ToolchainRecord;

/// Materials for [`compute_cache_key`].
#[derive(Debug, Clone)]
pub struct CacheKeyMaterials<'a> {
    /// Node kind.
    pub kind: NodeKind,
    /// Optional capability.
    pub capability: Option<&'a CapabilityId>,
    /// Digest of **content-only** bytes — MUST NOT include dag_id/node_id/generation.
    pub content_digest: &'a Digest,
    /// Policy hash.
    pub policy_hash: &'a Digest,
    /// Tool versions digest.
    pub tool_versions: &'a Digest,
    /// Compiler fingerprint digest.
    pub compiler_fingerprint: &'a Digest,
}

fn kind_serde_snake_case(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Plan => "plan",
        NodeKind::Analyze => "analyze",
        NodeKind::Edit => "edit",
        NodeKind::VerifyCompile => "verify_compile",
        NodeKind::VerifyTest => "verify_test",
        NodeKind::Review => "review",
        NodeKind::GateHuman => "gate_human",
        NodeKind::Aggregate => "aggregate",
    }
}

/// Returns `CacheKey(Digest::sha256(canonical_bytes))` per §5.8 framing.
#[must_use]
pub fn compute_cache_key(m: CacheKeyMaterials<'_>) -> CacheKey {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"alloy.cache_key.v1");
    bytes.push(0x00);
    bytes.extend_from_slice(kind_serde_snake_case(m.kind).as_bytes());
    bytes.push(0x00);
    if let Some(cap) = m.capability {
        bytes.extend_from_slice(cap.as_str().as_bytes());
    }
    bytes.push(0x00);
    bytes.extend_from_slice(m.content_digest.as_hex().as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(m.policy_hash.as_hex().as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(m.tool_versions.as_hex().as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(m.compiler_fingerprint.as_hex().as_bytes());
    CacheKey(Digest::sha256(&bytes))
}

/// Tool-versions fingerprint derived from a captured [`ToolchainRecord`]
/// (research §7.11 item 3 — a real digest over real inputs, replacing the
/// former `mvp_tool_versions_digest()` constant).
#[must_use]
pub fn tool_versions_digest(toolchain: &ToolchainRecord) -> Digest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"alloy.tool_versions.v1");
    for field in [
        &toolchain.channel,
        &toolchain.rustc_version,
        &toolchain.cargo_version,
    ] {
        bytes.push(0x00);
        bytes.extend_from_slice(field.as_bytes());
    }
    Digest::sha256(&bytes)
}

/// Compiler fingerprint: the exact `rustc` plus the target triple it
/// compiles for. Two runs whose compile-pass labels came from different
/// compilers or targets must never share a cache row.
#[must_use]
pub fn compiler_fingerprint_digest(toolchain: &ToolchainRecord, target_triple: &str) -> Digest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"alloy.compiler_fingerprint.v1");
    bytes.push(0x00);
    bytes.extend_from_slice(toolchain.rustc_version.as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(target_triple.as_bytes());
    Digest::sha256(&bytes)
}

/// Policy hash over the effective profile identity and budget policy —
/// the knobs that change what a run was *allowed* to do, and therefore what
/// its outcome label means.
///
/// `BudgetPolicy` is plain serde data; serialization cannot fail for valid
/// values.
#[must_use]
pub fn policy_hash_digest(
    profile: &crate::types::ids::ProfileId,
    policy: &crate::types::budget::BudgetPolicy,
) -> Digest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"alloy.policy_hash.v1");
    bytes.push(0x00);
    bytes.extend_from_slice(profile.as_str().as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(&serde_json::to_vec(policy).expect("BudgetPolicy JSON serialization"));
    Digest::sha256(&bytes)
}

/// Content digest for a root [`crate::types::budget::Goal`] (JSON of the Goal only).
///
/// `Goal` is plain serde data; serialization cannot fail for valid values.
#[must_use]
pub fn goal_content_digest(goal: &crate::types::budget::Goal) -> Digest {
    let bytes = serde_json::to_vec(goal).expect("Goal JSON serialization");
    Digest::sha256(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::{BudgetPolicy, Goal};
    use crate::types::ids::{CapabilityId, ProfileId};

    fn toolchain() -> ToolchainRecord {
        ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1 (abcdef 2026-06-01)".into(),
            cargo_version: "cargo 1.97.1 (123456 2026-06-01)".into(),
        }
    }

    /// Fixed Goal fixture — digests pinned offline for AC 20 / §5.8.
    const GOLDEN_GOAL_CONTENT: &str =
        "400fa50ee08a1e97765ebb6157d7c71f18dbbc5dc979c5b4554f6f6f272d84d6";
    const GOLDEN_CACHE_KEY: &str =
        "108879d360b11bd4c3b7ef44690ce02b6b647e57e41a9fa13eb7c52d948eb2ef";

    #[test]
    fn cache_key_stable() {
        let goal = Goal {
            text: "fix the compile error in src/lib.rs".into(),
            constraints: vec![],
            attachments: vec![],
        };
        let content = goal_content_digest(&goal);
        assert_eq!(content.as_hex(), GOLDEN_GOAL_CONTENT);
        // Arbitrary fixed digests (the former mvp_* constants, inlined): the
        // golden pins the *key formula*, not any fingerprint derivation.
        let policy = Digest::sha256(b"alloy.mvp.policy_hash.v0");
        let tools = Digest::sha256(b"alloy.mvp.tool_versions.v0");
        let compiler = Digest::sha256(b"alloy.mvp.compiler_fingerprint.v0");
        let cap = CapabilityId::new("repair").unwrap();
        let key = compute_cache_key(CacheKeyMaterials {
            kind: NodeKind::Analyze,
            capability: Some(&cap),
            content_digest: &content,
            policy_hash: &policy,
            tool_versions: &tools,
            compiler_fingerprint: &compiler,
        });
        assert_eq!(key.0.as_hex(), GOLDEN_CACHE_KEY);
        // Identity fields excluded: same content → same key regardless of dag/node.
        let key2 = compute_cache_key(CacheKeyMaterials {
            kind: NodeKind::Analyze,
            capability: Some(&cap),
            content_digest: &content,
            policy_hash: &policy,
            tool_versions: &tools,
            compiler_fingerprint: &compiler,
        });
        assert_eq!(key, key2);
    }

    #[test]
    fn kind_snake_case_matches_serde_all_variants() {
        for kind in [
            NodeKind::Plan,
            NodeKind::Analyze,
            NodeKind::Edit,
            NodeKind::VerifyCompile,
            NodeKind::VerifyTest,
            NodeKind::Review,
            NodeKind::GateHuman,
            NodeKind::Aggregate,
        ] {
            let s = serde_json::to_string(&kind).unwrap();
            let expected = format!("\"{}\"", kind_serde_snake_case(kind));
            assert_eq!(s, expected, "mismatch for {kind:?}");
        }
    }

    /// §7.11 item 3: fingerprints are content-derived — same inputs agree,
    /// any changed input disagrees.
    #[test]
    fn fingerprints_are_real_and_input_sensitive() {
        let tc = toolchain();
        assert_eq!(tool_versions_digest(&tc), tool_versions_digest(&tc));
        let mut newer = tc.clone();
        newer.rustc_version = "rustc 1.98.0 (fedcba 2026-09-01)".into();
        assert_ne!(tool_versions_digest(&tc), tool_versions_digest(&newer));

        let a = compiler_fingerprint_digest(&tc, "x86_64-unknown-linux-gnu");
        let b = compiler_fingerprint_digest(&tc, "aarch64-apple-darwin");
        assert_ne!(a, b, "target triple is part of the compiler identity");
        assert_ne!(
            compiler_fingerprint_digest(&newer, "x86_64-unknown-linux-gnu"),
            a
        );

        let default_profile = ProfileId::new("default").unwrap();
        let autonomous = ProfileId::new("autonomous").unwrap();
        let policy = BudgetPolicy::default();
        assert_eq!(
            policy_hash_digest(&default_profile, &policy),
            policy_hash_digest(&default_profile, &policy)
        );
        assert_ne!(
            policy_hash_digest(&default_profile, &policy),
            policy_hash_digest(&autonomous, &policy)
        );
    }
}
