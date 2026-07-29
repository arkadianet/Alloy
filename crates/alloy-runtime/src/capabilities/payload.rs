//! Versioned success payload schemas (RFC-0013 §8).
//!
//! Every payload is a JSON object written verbatim into the node output
//! envelope by RFC-0010's C4 checkpoint and decoded by successors through
//! `deny_unknown_fields` structs (OC5). Serde-stable at
//! [`PAYLOAD_SCHEMA_VERSION`]; consumers MUST reject an unknown version
//! rather than guess (OC1).

use serde::{Deserialize, Serialize};

use crate::router::Citation;
use crate::types::ids::{ArtifactId, Digest, TransactionId};
use crate::types::metrics::WorkerMetrics;

/// OC1: current payload schema version for all four capabilities.
pub const PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// OC7: max bytes for `notes`/`summary`-class strings.
pub(crate) const MAX_PAYLOAD_STRING_BYTES: usize = 4 * 1024;
/// OC7: max entries per payload vector.
pub(crate) const MAX_PAYLOAD_VEC_ENTRIES: usize = 256;
/// OC7: max total serialized payload bytes.
pub(crate) const MAX_PAYLOAD_TOTAL_BYTES: usize = 64 * 1024;

/// `repair` success payload (§8.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairPlanPayload {
    /// Always [`PAYLOAD_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always `"repair"`.
    pub capability: String,
    /// One-paragraph explanation of the failure and the intended fix.
    pub summary: String,
    /// Jail-relative files the edit step is expected to touch (≤ 16).
    pub target_files: Vec<String>,
    /// Ordered, human-readable steps. No code, no diffs (RW5).
    pub steps: Vec<RepairStep>,
    /// Fingerprints of the diagnostics this plan addresses.
    pub diagnostics_addressed: Vec<Digest>,
    /// `true` when the worker believes no local text patch can fix this
    /// (RW8) — still a success.
    pub needs_replan: bool,
    /// OC7 truncation marker.
    pub truncated: bool,
    /// Model-reported confidence clamped to `[0,1]`; `0.5` when absent (RW7).
    pub confidence: f32,
    /// Citations copied unmodified from the assembled pack (OC4/PR4).
    pub citations: Vec<Citation>,
    /// Produced artifacts (OC3).
    pub artifacts: Vec<ArtifactId>,
    /// Attempt metrics (CW7); never pushed into the meter (BG2).
    pub metrics: WorkerMetrics,
}

/// One ordered repair step (§8.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepairStep {
    /// Jail-relative file.
    pub file: String,
    /// Prose rationale, ≤ 512 bytes.
    pub rationale: String,
    /// Optional 1-based anchor line; advisory only.
    pub anchor_line: Option<u32>,
}

/// `edit` success payload (§8.3).
///
/// Named `EditAppliedPayload` by the RFC; access it module-qualified — the
/// crate root already exports RFC-0008's session-event payload of the same
/// name, so this one is deliberately not re-exported at the root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EditAppliedPayload {
    /// Always [`PAYLOAD_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always `"edit"`.
    pub capability: String,
    /// Jail-relative paths reported by the patch builtin (never
    /// worker-invented, EW8).
    pub files_touched: Vec<String>,
    /// Transaction id when the patch backend created one.
    pub transaction_id: Option<TransactionId>,
    /// CAS id of the canonical `PatchSet` JSON (`ArtifactKind::Patch`, EW9).
    pub patch_artifact: ArtifactId,
    /// Hunks in the applied patch.
    pub hunk_count: u32,
    /// Canonical patch bytes.
    pub bytes: u32,
    /// Always `false` for a successful node: a dry run alone is not success
    /// (EW7).
    pub dry_run: bool,
    /// Model-provided change summary (bounded).
    pub summary: String,
    /// OC7 truncation marker.
    pub truncated: bool,
    /// Confidence (RW7 semantics).
    pub confidence: f32,
    /// Citations copied unmodified from the assembled pack (OC4).
    pub citations: Vec<Citation>,
    /// Produced artifacts, including `patch_artifact` (OC3).
    pub artifacts: Vec<ArtifactId>,
    /// Attempt metrics (CW7).
    pub metrics: WorkerMetrics,
}

/// `review` success payload (§8.4). `RequestChanges` is a success, not a
/// node failure (VW4); turning a verdict into a decision is template policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewPayload {
    /// Always [`PAYLOAD_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always `"review"`.
    pub capability: String,
    /// Verdict.
    pub verdict: ReviewVerdict,
    /// Findings, ≤ 64. Advisory (VW3).
    pub findings: Vec<ReviewFinding>,
    /// Bounded summary.
    pub summary: String,
    /// OC7 truncation marker.
    pub truncated: bool,
    /// Confidence (RW7 semantics).
    pub confidence: f32,
    /// Citations copied unmodified from the assembled pack (OC4).
    pub citations: Vec<Citation>,
    /// Produced artifacts (OC3).
    pub artifacts: Vec<ArtifactId>,
    /// Attempt metrics (CW7).
    pub metrics: WorkerMetrics,
}

/// Review verdict (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// The diff looks correct.
    Approve,
    /// The diff needs changes — still a node success (VW4).
    RequestChanges,
}

/// One review finding (§8.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    /// Severity.
    pub severity: ReviewSeverity,
    /// Jail-relative file.
    pub file: String,
    /// Optional 1-based line.
    pub line: Option<u32>,
    /// Bounded message.
    pub message: String,
}

/// Finding severity (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    /// Informational.
    Info,
    /// Worth a look.
    Warning,
    /// Should not merge as-is.
    Blocker,
}

/// `planning` success payload (§8.5) — deterministic selection proposal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanningProposalPayload {
    /// Always [`PAYLOAD_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always `"planning"`.
    pub capability: String,
    /// Wire name of the selected template (`TemplateId::as_str`).
    pub template_id: String,
    /// Deterministic reason for the selection.
    pub rationale: String,
    /// Always `false` in MVP: a worker never requests topology change (PW2).
    pub replan_requested: bool,
    /// Model-proposed chain when the worker ran its model branch (RFC-0017
    /// AM-0013-2). Absent ⇒ deterministic template selection — the pre-0017
    /// wire shape decodes unchanged. The worker performs **no clamping**;
    /// containment is the proposal compiler's (SEC5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<crate::dag::ProposedDagManifest>,
    /// OC7 truncation marker.
    pub truncated: bool,
    /// Always `1.0` — deterministic selection.
    pub confidence: f32,
    /// Always empty — no prompt was assembled.
    pub citations: Vec<Citation>,
    /// Always empty.
    pub artifacts: Vec<ArtifactId>,
    /// Attempt metrics; tokens `None`, `tool_calls` 0.
    pub metrics: WorkerMetrics,
}

/// Truncate a payload string to `max` bytes on a UTF-8 boundary (OC7).
/// Returns `true` when truncation happened.
pub(crate) fn clamp_string(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

/// Truncate a payload vector to `max` entries (OC7). Returns `true` when
/// truncation happened.
pub(crate) fn clamp_vec<T>(v: &mut Vec<T>, max: usize) -> bool {
    if v.len() <= max {
        return false;
    }
    v.truncate(max);
    true
}

/// OC7 total-size check on a serialized payload.
pub(crate) fn exceeds_total_bound(value: &serde_json::Value) -> bool {
    serde_json::to_vec(value).map_or(true, |v| v.len() > MAX_PAYLOAD_TOTAL_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::ModelTier;
    use crate::types::ids::ProviderId;

    pub(crate) fn metrics() -> WorkerMetrics {
        WorkerMetrics {
            model_tier_used: ModelTier::Standard,
            provider_id: ProviderId::new("provider").unwrap(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            tool_calls: 0,
            cache_hits: 0,
            duration_ms: 7,
            confidence: None,
            error_class: None,
        }
    }

    fn repair_payload() -> RepairPlanPayload {
        RepairPlanPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "repair".into(),
            summary: "fix the borrow".into(),
            target_files: vec!["src/lib.rs".into()],
            steps: vec![RepairStep {
                file: "src/lib.rs".into(),
                rationale: "clone before the mutable borrow".into(),
                anchor_line: Some(14),
            }],
            diagnostics_addressed: vec![Digest::sha256(b"d")],
            needs_replan: false,
            truncated: false,
            confidence: 0.7,
            citations: vec![Citation {
                source: "alloy://conversation/goal".into(),
                digest: None,
            }],
            artifacts: vec![],
            metrics: metrics(),
        }
    }

    #[test]
    fn payload_roundtrip_is_serde_stable_for_all_four_schemas() {
        // OC1–OC5.
        let repair = repair_payload();
        let json = serde_json::to_value(&repair).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["capability"], "repair");
        assert_eq!(
            serde_json::from_value::<RepairPlanPayload>(json).unwrap(),
            repair
        );

        let edit = EditAppliedPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "edit".into(),
            files_touched: vec!["src/lib.rs".into()],
            transaction_id: Some(TransactionId::new()),
            patch_artifact: ArtifactId::new(),
            hunk_count: 1,
            bytes: 120,
            dry_run: false,
            summary: "applied".into(),
            truncated: false,
            confidence: 0.6,
            citations: vec![],
            artifacts: vec![],
            metrics: metrics(),
        };
        let json = serde_json::to_value(&edit).unwrap();
        assert_eq!(
            serde_json::from_value::<EditAppliedPayload>(json).unwrap(),
            edit
        );

        let review = ReviewPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "review".into(),
            verdict: ReviewVerdict::RequestChanges,
            findings: vec![ReviewFinding {
                severity: ReviewSeverity::Warning,
                file: "src/lib.rs".into(),
                line: Some(3),
                message: "consider a slice".into(),
            }],
            summary: "one warning".into(),
            truncated: false,
            confidence: 0.5,
            citations: vec![],
            artifacts: vec![],
            metrics: metrics(),
        };
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(json["verdict"], "request_changes");
        assert_eq!(
            serde_json::from_value::<ReviewPayload>(json).unwrap(),
            review
        );

        let planning = PlanningProposalPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "planning".into(),
            template_id: "repair_local_diagnostic".into(),
            rationale: "sole MVP template".into(),
            replan_requested: false,
            truncated: false,
            confidence: 1.0,
            citations: vec![],
            artifacts: vec![],
            metrics: metrics(),
            proposal: None,
        };
        let json = serde_json::to_value(&planning).unwrap();
        assert_eq!(
            serde_json::from_value::<PlanningProposalPayload>(json).unwrap(),
            planning
        );
    }

    #[test]
    fn payload_read_side_rejects_unknown_fields_and_topology_names() {
        // OC5/OC6: an extra field — topology-shaped or otherwise — fails
        // decode instead of being silently dropped.
        let mut json = serde_json::to_value(repair_payload()).unwrap();
        json["unknown"] = serde_json::json!(1);
        assert!(serde_json::from_value::<RepairPlanPayload>(json.clone()).is_err());
        json.as_object_mut().unwrap().remove("unknown");
        json["extra_nodes"] = serde_json::json!([]);
        assert!(serde_json::from_value::<RepairPlanPayload>(json).is_err());
    }

    #[test]
    fn payload_truncation_sets_truncated_and_bounds_size() {
        // OC7.
        let mut s = "é".repeat(4000);
        assert!(clamp_string(&mut s, MAX_PAYLOAD_STRING_BYTES));
        assert!(s.len() <= MAX_PAYLOAD_STRING_BYTES);
        assert!(s.is_char_boundary(s.len()));

        let mut v: Vec<u32> = (0..500).collect();
        assert!(clamp_vec(&mut v, MAX_PAYLOAD_VEC_ENTRIES));
        assert_eq!(v.len(), MAX_PAYLOAD_VEC_ENTRIES);

        let mut small = String::from("ok");
        assert!(!clamp_string(&mut small, MAX_PAYLOAD_STRING_BYTES));
    }
}
