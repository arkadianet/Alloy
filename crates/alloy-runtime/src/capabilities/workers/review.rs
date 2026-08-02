//! `ReviewWorker` (id `review`, kind `Review`) — RFC-0013 §9.3, rules
//! VW1–VW5. Optional (registered iff `WorkerConfig.enable_review`) and
//! unreached by the MVP template; exercised by unit/integration tests only.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::NodeKind;
use crate::types::budget::ModelTier;
use crate::types::ids::CapabilityId;
use crate::types::tools::{ToolName, ToolSelector};

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::parse::is_jail_relative;
use super::super::payload::{
    clamp_string, clamp_vec, ReviewFinding, ReviewPayload, ReviewSeverity, ReviewVerdict,
    MAX_PAYLOAD_STRING_BYTES, PAYLOAD_SCHEMA_VERSION,
};
use super::super::perms::WorkerToolClass;
use super::super::prompt::{fence_tool, review_response_schema, REVIEW_SYSTEM};
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{
    call_tool, finish_attempt, llm_exchange, load_pred_payloads, worker_span, Attempt, WorkerError,
    WorkerSuccess,
};

/// §8.4: findings cap.
const MAX_FINDINGS: usize = 64;

/// VW6: ceiling on one goal attachment's fenced body. A host bounds its own
/// input (`alloy review` cuts at 128 KiB); this is the worker's independent
/// backstop so an arbitrary host cannot push a 10 MiB blob into a prompt.
const MAX_ATTACHMENT_BYTES: usize = 256 * 1024;

/// VW6: how many goal attachments one review may read.
const MAX_ATTACHMENTS: usize = 4;

/// Tools this worker may call (TL5, VW5: the patch builtin is excluded).
const ALLOWED_TOOLS: [&str; 1] = ["fs_read"];

/// Model response schema (PS5: `deny_unknown_fields`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewProposal {
    verdict: VerdictProposal,
    #[serde(default)]
    findings: Vec<FindingProposal>,
    summary: String,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerdictProposal {
    Approve,
    RequestChanges,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingProposal {
    severity: SeverityProposal,
    file: String,
    #[serde(default)]
    line: Option<u32>,
    message: String,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SeverityProposal {
    Info,
    Warning,
    Blocker,
}

/// Advisory diff-review worker.
#[derive(Debug, Clone)]
pub struct ReviewWorker {
    config: WorkerConfig,
}

impl ReviewWorker {
    /// Construct with worker knobs.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
    }

    const VERSION: CapabilityVersion = CapabilityVersion::new(1, 0, 0);

    fn capability_id() -> CapabilityId {
        CapabilityId::new("review").expect("static id")
    }
}

#[async_trait]
impl Capability for ReviewWorker {
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
            summary: "Review an applied diff for correctness and risk (advisory)".into(),
            uses_model: true,
            side_effects: SideEffectClass::ReadOnly, // VW5.
            kinds: vec![NodeKind::Review],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        vec![ToolSelector::name(
            ToolName::new("fs_read").expect("static name"),
        )]
    }

    fn preferred_tier(&self) -> ModelTier {
        ModelTier::Economy
    }

    fn accepts_kind(&self, kind: NodeKind) -> bool {
        kind == NodeKind::Review
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

impl ReviewWorker {
    /// Fence every goal attachment for the prompt (VW6).
    ///
    /// Bytes are passed through verbatim apart from the two transformations
    /// fencing always owes the prompt: secret redaction (SEC) and escaping a
    /// literal `</workspace>` so untrusted content cannot close its own
    /// fence (PR12). Neither touches leading whitespace, line structure, or
    /// trailing whitespace, so a unified diff survives byte for byte.
    ///
    /// An attachment that does not decode as UTF-8, or that is missing from
    /// the store, is skipped with a note rather than failing the review: the
    /// model is told what it is *not* seeing.
    async fn fenced_attachments(&self, ctx: &CapabilityContext<'_>) -> Vec<String> {
        let crate::dag::NodeInputPayload::Goal(goal) = &ctx.input.payload else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for id in goal.attachments.iter().take(MAX_ATTACHMENTS) {
            let blob = match ctx.artifacts.get(*id).await {
                Ok(blob) => blob,
                Err(e) => {
                    tracing::warn!(artifact = %id, error = %e, "review attachment unreadable");
                    out.push(super::super::prompt::fence_workspace(
                        "diff",
                        &format!("[alloy: omitted — attachment {id} could not be read]"),
                    ));
                    continue;
                }
            };
            let total = blob.bytes.len();
            let Ok(text) = String::from_utf8(blob.bytes) else {
                out.push(super::super::prompt::fence_workspace(
                    "diff",
                    &format!("[alloy: omitted — attachment {id} is not UTF-8 text]"),
                ));
                continue;
            };
            let body = if total > MAX_ATTACHMENT_BYTES {
                let kept = crate::obs::truncate_utf8_bytes(&text, MAX_ATTACHMENT_BYTES);
                let marker = super::super::prompt::truncation_marker(kept.len(), total);
                format!("{kept}\n{marker}")
            } else {
                text
            };
            out.push(super::super::prompt::fence_workspace("diff", &body));
        }
        out
    }

    async fn run(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }

        // VW2: the Edit predecessor's payload names the changed files (never
        // re-derived from git).
        let payloads = load_pred_payloads(ctx).await?;
        let changed: Vec<String> = payloads
            .iter()
            .filter(|(kind, _)| *kind == NodeKind::Edit)
            .find_map(|(_, payload)| {
                serde_json::from_value::<super::super::payload::EditAppliedPayload>(payload.clone())
                    .ok()
                    .map(|p| p.files_touched)
            })
            .unwrap_or_default();

        // VW6: the material under review — the diff — travels out of band as
        // a goal attachment in the artifact CAS, never as goal *text*. Goal
        // text is sanitised for prompt injection on its way through the
        // context engine (`sanitize_untrusted`: per-line `trim_end`, fence
        // marker stripping), which silently reshapes a whitespace-sensitive
        // patch. Attachment bytes go straight into a `<workspace>` fence, so
        // the model reads exactly what the host handed us.
        let mut feedback: Vec<String> = self.fenced_attachments(ctx).await;
        for path in changed
            .iter()
            .filter(|p| is_jail_relative(p))
            .take(usize::from(self.config.max_tool_calls))
        {
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
                // A missing file is a finding for the model, not a failure.
                feedback.push(fence_tool(
                    "fs_read",
                    &format!("read failed for {path}"),
                    self.config.max_tool_result_bytes,
                ));
                continue;
            }
            // PR6: bound the tool result before it re-enters the prompt;
            // PR12: workspace content rides a `<workspace>` fence.
            let text = result
                .content
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| result.content.to_string(), str::to_owned);
            let bounded = crate::obs::truncate_utf8_bytes(&text, self.config.max_tool_result_bytes);
            feedback.push(super::super::prompt::fence_workspace(path, &bounded));
        }

        let inputs = AssembleInputs {
            run: Some(ctx.run),
            input: Some(ctx.input.clone()),
            diagnostics: Vec::new(),
            budget: Some(ctx.budget.clone()),
            focus_paths: changed.clone(),
            prior_failure: None,
        };

        let (proposal, _pack) = llm_exchange(
            ctx,
            attempt,
            &self.config,
            REVIEW_SYSTEM,
            Some(&review_response_schema()),
            &inputs,
            &feedback,
            |value| {
                let proposal: ReviewProposal =
                    serde_json::from_value(value.clone()).map_err(|e| format!("schema: {e}"))?;
                for finding in &proposal.findings {
                    if !is_jail_relative(&finding.file) {
                        return Err(format!(
                            "finding path is not jail-relative: {:.120}",
                            finding.file
                        ));
                    }
                }
                Ok(proposal)
            },
        )
        .await?;

        let confidence = proposal.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
        let mut summary = proposal.summary;
        let summary_cut = clamp_string(&mut summary, MAX_PAYLOAD_STRING_BYTES);
        let mut findings: Vec<ReviewFinding> = proposal
            .findings
            .into_iter()
            .map(|f| {
                let mut message = f.message;
                clamp_string(&mut message, MAX_PAYLOAD_STRING_BYTES);
                ReviewFinding {
                    severity: match f.severity {
                        SeverityProposal::Info => ReviewSeverity::Info,
                        SeverityProposal::Warning => ReviewSeverity::Warning,
                        SeverityProposal::Blocker => ReviewSeverity::Blocker,
                    },
                    file: f.file,
                    line: f.line,
                    message,
                }
            })
            .collect();
        let findings_cut = clamp_vec(&mut findings, MAX_FINDINGS);

        // VW4: `RequestChanges` is a success, not a failure.
        let mut payload = ReviewPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "review".into(),
            verdict: match proposal.verdict {
                VerdictProposal::Approve => ReviewVerdict::Approve,
                VerdictProposal::RequestChanges => ReviewVerdict::RequestChanges,
            },
            findings,
            summary,
            truncated: summary_cut || findings_cut,
            confidence,
            citations: attempt.citations.clone(),
            artifacts: vec![],
            metrics: attempt.metrics(ctx, None),
        };
        let mut value = serde_json::to_value(&payload).map_err(|e| {
            WorkerError::Host(CapabilityExecError::Internal(format!(
                "payload serialization failed: {e}" // CW10.
            )))
        })?;
        // OC7 total bound: drop the findings list rather than a citation;
        // the verdict itself always survives.
        if super::super::payload::exceeds_total_bound(&value) {
            payload.findings.clear();
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
}
