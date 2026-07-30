//! `EditWorker` (id `edit`, kind `Edit`) — RFC-0013 §9.2, rules EW1–EW11
//! plus the AM-0013-1 line-ops response form.
//!
//! Obtains either a unified diff or a line-ops array from the model,
//! converts it to a validated `PatchSet` **locally** (EW4 / AM-0013-1 —
//! ops are compiled against the CURRENT file content read via `fs_read`,
//! with each op's `expect` lines verified verbatim), persists the
//! canonical patch as `ArtifactKind::Patch` (EW9), and applies it through
//! the `apply_patch` builtin only (EW1: never a second write stack, never
//! a direct file write, never a checkpoint restore). Forward-only: no
//! re-apply, no compensation of a partial apply (EW10) — RFC-0008's
//! transaction is the unit of atomicity.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use std::collections::HashMap;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::{NodeInputPayload, NodeKind};
use crate::edit::{FilePatch, PatchSet};
use crate::storage::{ArtifactKind, ArtifactPut};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, RetryDisposition};
use crate::types::ids::{ArtifactId, CapabilityId, TransactionId};
use crate::types::tools::{ToolName, ToolSelector};

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::parse::{
    ops_to_patchset, parse_line_op, parse_model_diff, screen_line_ops, LineOp,
};
use super::super::payload::{
    clamp_string, EditAppliedPayload, MAX_PAYLOAD_STRING_BYTES, PAYLOAD_SCHEMA_VERSION,
};
use super::super::perms::WorkerToolClass;
use super::super::prompt::{edit_response_schema, fence_tool, EDIT_SYSTEM};
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{
    call_tool, finish_attempt, llm_exchange, load_pred_payloads, map_tool_result_error,
    worker_span, Attempt, WorkerError, WorkerSuccess,
};

/// EW5: mirrors RFC-0006's `MAX_ARGUMENT_BYTES` (64 KiB). The constant
/// lives in `alloy-tools`, which this crate MUST NOT depend on (C2), so the
/// bound is restated here and cross-checked by the RFC-0006 host anyway.
const MAX_PATCH_ARGUMENT_BYTES: usize = 64 * 1024;

/// Tools this worker may call (TL5).
const ALLOWED_TOOLS: [&str; 2] = ["fs_read", "apply_patch"];

/// Model response schema (EW3 + AM-0013-1, PS5: `deny_unknown_fields`):
/// exactly one of `patch` (a unified diff) or `ops` (line operations
/// against the numbered CURRENT file content) — never both, never neither.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchProposal {
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    ops: Option<Vec<serde_json::Value>>,
    summary: String,
    #[serde(default)]
    confidence: Option<f32>,
}

/// The locally validated body of one proposal: a parsed diff, or screened
/// ops that still need the current file content to compile (AM-0013-1).
enum ProposalBody {
    /// EW4: unified diff already parsed into a `PatchSet`.
    Patch(PatchSet),
    /// AM-0013-1: statically screened ops, compiled by [`EditWorker::compile_ops`].
    Ops(Vec<LineOp>),
}

/// Sanitized view of the patch builtin's success content (EW8: paths and
/// transaction id come from the tool outcome, never from the model).
#[derive(Debug, Deserialize)]
struct PatchOutcomeView {
    #[serde(default)]
    files_touched: Vec<String>,
    #[serde(default)]
    transaction_id: Option<TransactionId>,
}

/// Patch-authoring worker.
#[derive(Debug, Clone)]
pub struct EditWorker {
    config: WorkerConfig,
}

impl EditWorker {
    /// Construct with worker knobs.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
    }

    const VERSION: CapabilityVersion = CapabilityVersion::new(1, 0, 0);

    fn capability_id() -> CapabilityId {
        CapabilityId::new("edit").expect("static id")
    }
}

#[async_trait]
impl Capability for EditWorker {
    fn id(&self) -> CapabilityId {
        Self::capability_id()
    }

    fn version(&self) -> CapabilityVersion {
        Self::VERSION
    }

    fn describe(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: Self::capability_id(),
            version: Self::VERSION,
            summary: "Produce a minimal unified diff and apply it via the patch builtin".into(),
            uses_model: true,
            side_effects: SideEffectClass::WorkspaceWrite,
            kinds: vec![NodeKind::Edit],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        vec![
            ToolSelector::name(ToolName::new("fs_read").expect("static name")),
            ToolSelector::name(ToolName::new("apply_patch").expect("static name")),
        ]
    }

    fn preferred_tier(&self) -> ModelTier {
        ModelTier::Standard
    }

    fn accepts_kind(&self, kind: NodeKind) -> bool {
        kind == NodeKind::Edit
    }

    async fn execute(
        &self,
        ctx: &CapabilityContext<'_>,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        let span = worker_span(ctx);
        let mut attempt = Attempt::new(self.preferred_tier(), ctx.effective_tier);
        let result = {
            use tracing::Instrument;
            self.run(ctx, &mut attempt).instrument(span.clone()).await
        };
        finish_attempt(ctx, &self.describe(), &attempt, result, &span).await
    }
}

/// One validated patch candidate ready to be sent to the builtin.
struct Candidate {
    proposal: PatchProposal,
    canonical: serde_json::Value,
    bytes: u32,
    hunk_count: u32,
}

impl EditWorker {
    async fn run(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }

        // EW2: the first Analyze pred whose payload decodes as a
        // `RepairPlanPayload` is the plan; preds without any decodable plan
        // are an internal failure (FM10-adjacent, "edit node without a
        // repair plan"). A goal-rooted single-node DAG has no pred.
        let payloads = load_pred_payloads(ctx).await?;
        let has_preds = matches!(
            &ctx.input.payload,
            NodeInputPayload::FromPredecessors { .. }
        );
        let plan = payloads.iter().find_map(|(kind, payload)| {
            if *kind != NodeKind::Analyze {
                return None;
            }
            serde_json::from_value::<super::super::payload::RepairPlanPayload>(payload.clone()).ok()
        });
        if has_preds && plan.is_none() {
            return Err(WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                "edit node without a repair plan",
            ));
        }

        let focus_paths = plan
            .as_ref()
            .map(|p| p.target_files.clone())
            .unwrap_or_default();
        let inputs = AssembleInputs {
            run: Some(ctx.run),
            input: Some(ctx.input.clone()),
            diagnostics: Vec::new(),
            budget: Some(ctx.budget.clone()),
            focus_paths,
        };

        // §7.2 turn budget: the model turn(s), the EW6 dry-run, and the PS6
        // repair share one attempt; every loop iteration below consumes a
        // model turn through `llm_exchange`.
        let mut feedback: Vec<String> = Vec::new();
        let mut dry_run_repaired = false;
        let mut ops_repaired = false;
        let (candidate, patch_artifact) = loop {
            let (proposal, body) = self.author(ctx, attempt, &inputs, &feedback).await?;
            let patch_set = match body {
                ProposalBody::Patch(set) => set,
                // AM-0013-1: compile ops against the current files; a stale
                // or misanchored op is model-repairable feedback, exactly
                // like an EW6 dry-run failure.
                ProposalBody::Ops(ops) => match self.compile_ops(ctx, attempt, &ops).await? {
                    Ok(set) => set,
                    Err(reason) => {
                        if !ops_repaired && attempt.model_turns < self.config.max_model_turns {
                            ops_repaired = true;
                            feedback = vec![fence_tool(
                                "line_ops",
                                &reason,
                                self.config.max_tool_result_bytes,
                            )];
                            continue;
                        }
                        return Err(WorkerError::soft(
                            ErrorClass::Model,
                            RetryDisposition::Retryable,
                            format!("line ops rejected after repair turn: {reason}"),
                        ));
                    }
                },
            };
            let candidate = Self::candidate(proposal, &patch_set)?;

            // EW9: persist the canonical PatchSet before the apply call.
            let patch_artifact = self.persist_patch(ctx, &candidate).await?;

            if !self.config.validate_before_apply {
                break (candidate, patch_artifact);
            }
            // EW6: one dry run; on failure, one repair turn with the
            // sanitized tool error fed back, then re-validate.
            let dry = call_tool(
                ctx,
                attempt,
                &self.config,
                WorkerToolClass::Patch,
                &ALLOWED_TOOLS,
                "apply_patch",
                json!({ "patch": candidate.canonical, "dry_run": true }),
            )
            .await?;
            if !dry.is_error() {
                break (candidate, patch_artifact);
            }
            if !dry_run_repaired && attempt.model_turns < self.config.max_model_turns {
                dry_run_repaired = true;
                feedback = vec![fence_tool(
                    "apply_patch",
                    &dry.content.to_string(),
                    self.config.max_tool_result_bytes,
                )];
                continue;
            }
            // Second dry-run failure: FM3 disposition from the tool error.
            return Err(map_tool_result_error(&dry));
        };

        // EW7: the apply call is never a dry run; a validated-but-unapplied
        // patch is not success.
        let applied = call_tool(
            ctx,
            attempt,
            &self.config,
            WorkerToolClass::Patch,
            &ALLOWED_TOOLS,
            "apply_patch",
            json!({ "patch": candidate.canonical, "dry_run": false }),
        )
        .await?;
        if applied.is_error() {
            // EW10/EW11: no re-apply, no compensation; the disposition comes
            // from the tool error taxonomy.
            return Err(map_tool_result_error(&applied));
        }
        // EW8: backend-reported paths only.
        let outcome: PatchOutcomeView =
            serde_json::from_value(applied.content.clone()).map_err(|e| {
                WorkerError::soft(
                    ErrorClass::Internal,
                    RetryDisposition::NonRetryable,
                    format!("apply_patch content undecodable: {e}"),
                )
            })?;

        let confidence = candidate.proposal.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
        let mut summary = candidate.proposal.summary.clone();
        let summary_cut = clamp_string(&mut summary, MAX_PAYLOAD_STRING_BYTES);
        // OC7 vector bound on the backend-reported list.
        let mut files_touched = outcome.files_touched;
        let files_cut = super::super::payload::clamp_vec(
            &mut files_touched,
            super::super::payload::MAX_PAYLOAD_VEC_ENTRIES,
        );

        let mut payload = EditAppliedPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "edit".into(),
            files_touched,
            transaction_id: outcome.transaction_id,
            patch_artifact,
            hunk_count: candidate.hunk_count,
            bytes: candidate.bytes,
            dry_run: false, // EW7.
            summary,
            truncated: summary_cut || files_cut,
            confidence,
            citations: attempt.citations.clone(),
            artifacts: vec![patch_artifact],
            metrics: attempt.metrics(ctx, None),
        };
        let mut value = serde_json::to_value(&payload).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "payload serialization failed: {e}" // CW10.
            )))
        })?;
        // OC7 total bound: drop the largest list rather than a citation.
        if super::super::payload::exceeds_total_bound(&value) {
            payload.files_touched.clear();
            payload.summary.clear();
            payload.truncated = true;
            value = serde_json::to_value(&payload).map_err(|e| {
                WorkerError::Host(CapabilityExecError::Internal(format!(
                    "payload serialization failed: {e}"
                )))
            })?;
        }
        let payload = value;
        Ok(WorkerSuccess {
            payload,
            confidence,
        })
    }

    /// One model turn producing a locally validated proposal (EW3/EW4 plus
    /// the AM-0013-1 ops form: strict either/or, static screen here, file
    /// verification in [`Self::compile_ops`]).
    async fn author(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        inputs: &AssembleInputs,
        feedback: &[String],
    ) -> Result<(PatchProposal, ProposalBody), WorkerError> {
        let (authored, _pack) = llm_exchange(
            ctx,
            attempt,
            &self.config,
            EDIT_SYSTEM,
            Some(&edit_response_schema()),
            inputs,
            feedback,
            |value| {
                let proposal: PatchProposal =
                    serde_json::from_value(value.clone()).map_err(|e| format!("schema: {e}"))?;
                let body = match (&proposal.patch, &proposal.ops) {
                    (Some(_), Some(_)) => {
                        return Err("reply with either patch or ops, never both".into());
                    }
                    (None, None) => {
                        return Err("reply must carry a patch or an ops array".into());
                    }
                    // EW4: local parse before any tool call — an unusable
                    // diff never becomes a permission-denied tool error.
                    (Some(patch), None) => ProposalBody::Patch(parse_model_diff(patch)?),
                    (None, Some(raw_ops)) => {
                        let ops = raw_ops
                            .iter()
                            .map(parse_line_op)
                            .collect::<Result<Vec<_>, _>>()?;
                        screen_line_ops(&ops)?;
                        ProposalBody::Ops(ops)
                    }
                };
                Ok((proposal, body))
            },
        )
        .await?;
        Ok(authored)
    }

    /// AM-0013-1: read each distinct target file once through `fs_read` and
    /// compile the ops into a context-correct `PatchSet`. The outer `Err` is
    /// a host/tool fault; the inner `Err` is model-repairable feedback (a
    /// stale `expect`, an unreadable or truncated file, a bad range).
    async fn compile_ops(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        ops: &[LineOp],
    ) -> Result<Result<PatchSet, String>, WorkerError> {
        let mut files: HashMap<String, String> = HashMap::new();
        for op in ops {
            let path = op.path();
            if files.contains_key(path) {
                continue;
            }
            let result = call_tool(
                ctx,
                attempt,
                &self.config,
                WorkerToolClass::Read,
                &ALLOWED_TOOLS,
                "fs_read",
                json!({ "path": path }),
            )
            .await?;
            if result.is_error() {
                // A path the model named but the jail cannot read is the
                // model's mistake to repair, not a worker failure.
                return Ok(Err(format!(
                    "fs_read failed for {path}; check the path or emit a unified diff patch"
                )));
            }
            let text = result
                .content
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    WorkerError::soft(
                        ErrorClass::Internal,
                        RetryDisposition::NonRetryable,
                        "fs_read content undecodable: no text field",
                    )
                })?;
            if result
                .content
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                // Compiling against a partial read would fabricate context;
                // the honest fallback is the diff form.
                return Ok(Err(format!(
                    "{path} is too large to line-edit; reply with a unified diff patch instead"
                )));
            }
            files.insert(path.to_owned(), text.to_owned());
        }
        Ok(ops_to_patchset(ops, &files))
    }

    /// EW5 bounds over the compiled `PatchSet`, shared by both wire forms.
    fn candidate(proposal: PatchProposal, patch_set: &PatchSet) -> Result<Candidate, WorkerError> {
        let canonical = serde_json::to_value(patch_set).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "patch serialization failed: {e}"
            )))
        })?;
        // EW5/FM7: the serialized tool argument must fit the RFC-0006 cap;
        // chunking across nodes is a template concern (RFC-0010 AS2), not an
        // in-worker split.
        let args_len = serde_json::to_vec(&json!({ "patch": canonical, "dry_run": false }))
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if args_len > MAX_PATCH_ARGUMENT_BYTES {
            return Err(WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                "patch exceeds MAX_ARGUMENT_BYTES; split the repair",
            ));
        }
        let bytes = u32::try_from(serde_json::to_vec(&patch_set).map(|v| v.len()).unwrap_or(0))
            .unwrap_or(u32::MAX);
        let hunk_count = u32::try_from(
            patch_set
                .files
                .iter()
                .map(|f| match f {
                    FilePatch::Modify { hunks, .. } | FilePatch::Create { hunks, .. } => {
                        hunks.len()
                    }
                    FilePatch::Delete {
                        validation_hunks, ..
                    } => validation_hunks.len(),
                })
                .sum::<usize>(),
        )
        .unwrap_or(u32::MAX);
        Ok(Candidate {
            proposal,
            canonical,
            bytes,
            hunk_count,
        })
    }

    /// EW9: canonical `PatchSet` JSON into the CAS as `ArtifactKind::Patch`.
    /// An orphan after a failed apply is acceptable (RFC-0002 has no GC).
    async fn persist_patch(
        &self,
        ctx: &CapabilityContext<'_>,
        candidate: &Candidate,
    ) -> Result<ArtifactId, WorkerError> {
        let bytes = serde_json::to_vec(&candidate.canonical).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "patch serialization failed: {e}"
            )))
        })?;
        ctx.artifacts
            .put(ArtifactPut {
                bytes,
                kind: ArtifactKind::Patch,
                content_type: Some("application/json".into()),
                session_id: Some(ctx.session),
                run_id: Some(ctx.run),
                labels: serde_json::Map::new(),
            })
            .await
            .map_err(|e| {
                WorkerError::soft(
                    ErrorClass::Internal,
                    RetryDisposition::NonRetryable,
                    format!("patch artifact store failed: {e}"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// Minimal structural check matching `tests/worker_schemas.rs` for the
    /// subset `edit_response_schema` uses (incl. op-item `oneOf`).
    fn schema_validates(schema: &Value, value: &Value) -> bool {
        let obj = schema.as_object().expect("schema object");
        if let Some(alts) = obj.get("oneOf") {
            return alts
                .as_array()
                .expect("oneOf array")
                .iter()
                .any(|alt| schema_validates(alt, value));
        }
        if let Some(types) = obj.get("type") {
            let ok = match types {
                Value::String(ty) => match ty.as_str() {
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "string" => value.is_string(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "number" => value.is_number(),
                    "null" => value.is_null(),
                    _ => false,
                },
                Value::Array(list) => list
                    .iter()
                    .any(|ty| schema_validates(&json!({ "type": ty.clone() }), value)),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        if let Some(allowed) = obj.get("enum") {
            if !allowed.as_array().expect("enum").contains(value) {
                return false;
            }
        }
        if let Some(props) = obj.get("properties") {
            let props = props.as_object().expect("properties");
            let Some(map) = value.as_object() else {
                return false;
            };
            for (key, sub) in props {
                if let Some(v) = map.get(key) {
                    if !schema_validates(sub, v) {
                        return false;
                    }
                }
            }
            if obj.get("additionalProperties") == Some(&Value::Bool(false))
                && map.keys().any(|k| !props.contains_key(k))
            {
                return false;
            }
            if let Some(required) = obj.get("required") {
                for key in required.as_array().expect("required") {
                    if !map.contains_key(key.as_str().expect("required key")) {
                        return false;
                    }
                }
            }
        }
        if let Some(items) = obj.get("items") {
            if let Some(list) = value.as_array() {
                if !list.iter().all(|item| schema_validates(items, item)) {
                    return false;
                }
            }
        }
        true
    }

    /// A-0007-2 × AM-0013-1 reconciliation guard: the declared edit schema
    /// and the live `PatchProposal` parser must accept and reject the same
    /// surface. PR #64 widened the parser to exactly-one-of `patch` / `ops`;
    /// any future parser change must regenerate `edit_response_schema()` and
    /// this test in the same commit.
    #[test]
    fn edit_schema_matches_current_parser_surface() {
        let schema = edit_response_schema().schema;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required list")
            .iter()
            .map(|v| v.as_str().expect("required entry"))
            .collect();
        let properties: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();

        // The parser requires `summary`, admits exactly one of `patch` /
        // `ops` plus optional `confidence`, and denies unknown fields.
        // serde_json maps iterate sorted, so compare property sets
        // order-insensitively.
        assert_eq!(required, ["summary"]);
        assert_eq!(properties, ["confidence", "ops", "patch", "summary"]);

        // Complete schema-valid replace_lines (not the bare `{op}` stub).
        let ops_shape = json!({
            "ops": [{
                "op": "replace_lines",
                "path": "a.rs",
                "start": 1,
                "end": 1,
                "expect": ["x"],
                "new": ["y"]
            }],
            "summary": "s"
        });
        assert!(
            schema_validates(&schema, &ops_shape),
            "closed replace_lines shape must validate against edit_response_schema"
        );
        assert!(serde_json::from_value::<PatchProposal>(ops_shape.clone()).is_ok());

        let patch_shape = json!({ "patch": "--- a\n+++ b\n", "summary": "s" });
        assert!(schema_validates(&schema, &patch_shape));
        assert!(serde_json::from_value::<PatchProposal>(patch_shape).is_ok());

        // Incomplete / wrong-tag ops are schema-invalid even when they still
        // deserialize into the loose `Vec<Value>` ops field.
        let bare_op = json!({
            "ops": [{ "op": "replace_lines" }],
            "summary": "s"
        });
        assert!(
            !schema_validates(&schema, &bare_op),
            "bare {{op}} must not satisfy the closed op oneOf"
        );
        let wrong_tag = json!({
            "ops": [{
                "op": "replace",
                "path": "a.rs",
                "start": 1,
                "end": 1,
                "expect": ["x"],
                "new": ["y"]
            }],
            "summary": "s"
        });
        assert!(!schema_validates(&schema, &wrong_tag));

        // Unknown top-level fields are closed off by
        // `additionalProperties: false` / `deny_unknown_fields`.
        let mut unknown = ops_shape;
        unknown
            .as_object_mut()
            .expect("object")
            .insert("bogus".into(), json!(true));
        assert!(
            serde_json::from_value::<PatchProposal>(unknown.clone()).is_err(),
            "parser admits an unknown field; regenerate edit_response_schema() (AM-0013-1)"
        );
        assert!(!schema_validates(&schema, &unknown));
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "schema must stay closed while the parser is deny_unknown_fields"
        );
    }
}
