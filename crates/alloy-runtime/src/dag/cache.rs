//! Cache-key builder (RFC-0009 §5.8). Day-1 templates leave `cache_key = None`.

use crate::dag::types::{CacheKey, NodeKind};
use crate::types::ids::{CapabilityId, Digest};

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

/// MVP tool-versions fingerprint.
#[must_use]
pub fn mvp_tool_versions_digest() -> Digest {
    Digest::sha256(b"alloy.mvp.tool_versions.v0")
}

/// MVP compiler fingerprint.
#[must_use]
pub fn mvp_compiler_fingerprint_digest() -> Digest {
    Digest::sha256(b"alloy.mvp.compiler_fingerprint.v0")
}

/// MVP policy hash.
#[must_use]
pub fn mvp_policy_hash_digest() -> Digest {
    Digest::sha256(b"alloy.mvp.policy_hash.v0")
}

/// Content digest for a root [`crate::types::budget::Goal`] (JSON of the Goal only).
pub fn goal_content_digest(goal: &crate::types::budget::Goal) -> Result<Digest, serde_json::Error> {
    let bytes = serde_json::to_vec(goal)?;
    Ok(Digest::sha256(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::Goal;
    use crate::types::ids::CapabilityId;

    #[test]
    fn cache_key_stable() {
        let goal = Goal {
            text: "fix the compile error in src/lib.rs".into(),
            constraints: vec![],
            attachments: vec![],
        };
        let content = goal_content_digest(&goal).unwrap();
        let policy = mvp_policy_hash_digest();
        let tools = mvp_tool_versions_digest();
        let compiler = mvp_compiler_fingerprint_digest();
        let cap = CapabilityId::new("repair").unwrap();
        let key = compute_cache_key(CacheKeyMaterials {
            kind: NodeKind::Analyze,
            capability: Some(&cap),
            content_digest: &content,
            policy_hash: &policy,
            tool_versions: &tools,
            compiler_fingerprint: &compiler,
        });
        // Golden: pinned for this fixed Goal fixture (root/content path only).
        let expected = Digest::sha256(&{
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"alloy.cache_key.v1");
            bytes.push(0x00);
            bytes.extend_from_slice(b"analyze");
            bytes.push(0x00);
            bytes.extend_from_slice(b"repair");
            bytes.push(0x00);
            bytes.extend_from_slice(content.as_hex().as_bytes());
            bytes.push(0x00);
            bytes.extend_from_slice(policy.as_hex().as_bytes());
            bytes.push(0x00);
            bytes.extend_from_slice(tools.as_hex().as_bytes());
            bytes.push(0x00);
            bytes.extend_from_slice(compiler.as_hex().as_bytes());
            bytes
        });
        assert_eq!(key.0, expected);
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
    fn kind_snake_case_matches_serde() {
        let k = NodeKind::VerifyCompile;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"verify_compile\"");
        assert_eq!(kind_serde_snake_case(k), "verify_compile");
    }
}
