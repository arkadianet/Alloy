//! `EditWorker` (id `edit`, kind `Edit`) — RFC-0013 §9.2, rules EW1–EW11.
//!
//! Obtains a unified diff from the model, converts it to a validated
//! `PatchSet` **locally** (EW4), persists the canonical patch as
//! `ArtifactKind::Patch` (EW9), and applies it through the `apply_patch`
//! builtin only (EW1: never a second write stack, never a direct file
//! write, never a checkpoint restore). Forward-only: no re-apply, no
//! compensation of a partial apply (EW10) — RFC-0008's transaction is the
//! unit of atomicity.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::{NodeInputPayload, NodeKind};
use crate::edit::FilePatch;
use crate::storage::{ArtifactKind, ArtifactPut};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, RetryDisposition};
use crate::types::ids::{ArtifactId, CapabilityId, TransactionId};
use crate::types::tools::{ToolName, ToolSelector};

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::parse::parse_model_diff;
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

/// Model response schema (EW3, PS5: `deny_unknown_fields`): one wire form —
/// a unified diff — keeps the parser small and matches the tool backend.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchProposal {
    patch: String,
    summary: String,
    #[serde(default)]
    confidence: Option<f32>,
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
        let (candidate, patch_artifact) = loop {
            let candidate = self.author_patch(ctx, attempt, &inputs, &feedback).await?;

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

    /// One model turn producing a locally validated patch (EW3/EW4/EW5).
    async fn author_patch(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
        inputs: &AssembleInputs,
        feedback: &[String],
    ) -> Result<Candidate, WorkerError> {
        let ((proposal, patch_set), _pack) = llm_exchange(
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
                // EW4: local parse before any tool call — an unusable diff
                // never becomes a permission-denied tool error.
                let patch_set = parse_model_diff(&proposal.patch)?;
                Ok((proposal, patch_set))
            },
        )
        .await?;

        let canonical = serde_json::to_value(&patch_set).map_err(|e| {
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
