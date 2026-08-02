//! `RepairWorker` (id `repair`, kind `Analyze`) — RFC-0013 §9.1, rules
//! RW1–RW8.
//!
//! Given a goal (root node) or a predecessor failure body with rustc
//! diagnostics (post-verification generation), produce a `RepairPlanPayload`
//! describing a minimal, local, text-patchable fix. It never writes files
//! and never emits a diff (RW5): patch authorship belongs to `edit` alone.

use async_trait::async_trait;
use serde::Deserialize;

use crate::adapters::{CapabilityExecError, CapabilityOutcome};
use crate::context::AssembleInputs;
use crate::dag::{NodeInputPayload, NodeKind};
use crate::graph::{GraphEdgeKind, GraphNodeKind, GraphQuery};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::CapabilityId;
use crate::types::tools::{ToolName, ToolSelector};

use super::super::deps::{CapabilityContext, WorkerConfig};
use super::super::parse::is_jail_relative;
use super::super::payload::{
    clamp_string, RepairPlanPayload, RepairStep, MAX_PAYLOAD_STRING_BYTES, PAYLOAD_SCHEMA_VERSION,
};
use super::super::prompt::{fence_tool, repair_response_schema, REPAIR_SYSTEM};
use super::super::traits::{Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass};
use super::{
    diagnostics_from_payloads, finish_attempt, llm_exchange, load_pred_payloads, worker_span,
    Attempt, WorkerError, WorkerSuccess,
};

/// RW2: diagnostics presented to the model are capped at this many.
const MAX_DIAGNOSTICS: usize = 32;
/// RW4 (A-0011-5): distinct diagnostic codes the graph is asked about.
const MAX_FIX_CODES: usize = 4;
/// RW4 (A-0011-5): rows requested per code — the query's own `limit`.
const FIXES_PER_CODE: usize = 2;
/// RW4 (A-0011-5): total past-fix lines shown, across all codes.
const MAX_SIMILAR_FIXES: usize = 8;
/// RW2-style byte cap on the assembled past-fix note.
const MAX_SIMILAR_FIXES_BYTES: usize = 1024;
/// A-0012-1: distinct diagnosed paths resolved for caller impact.
const MAX_CALLER_PATHS: usize = 4;
/// A-0012-1: item anchors expanded from one resolved module (`Calls` edges
/// anchor on item nodes, so a resolved module node must be expanded to the
/// items it `Defines` before `Callers` can answer).
const MAX_CALLER_ITEMS: usize = 4;
/// A-0012-1: total caller lines shown, across all diagnosed paths.
const MAX_CALLER_LINES: usize = 8;
/// RW2-style byte cap on the assembled callers note.
const MAX_CALLERS_BYTES: usize = 1024;
/// §8.2: max `target_files` entries.
const MAX_TARGET_FILES: usize = 16;
/// §8.2: max bytes of one step rationale.
const MAX_RATIONALE_BYTES: usize = 512;

/// Model response schema (PS5: `deny_unknown_fields`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairProposal {
    summary: String,
    target_files: Vec<String>,
    steps: Vec<StepProposal>,
    #[serde(default)]
    needs_replan: bool,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepProposal {
    file: String,
    rationale: String,
    #[serde(default)]
    anchor_line: Option<u32>,
}

/// Diagnostic-analysis worker.
#[derive(Debug, Clone)]
pub struct RepairWorker {
    config: WorkerConfig,
}

impl RepairWorker {
    /// Construct with worker knobs.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
    }

    const VERSION: CapabilityVersion = CapabilityVersion::new(1, 0, 0);

    fn capability_id() -> CapabilityId {
        CapabilityId::new("repair").expect("static id")
    }
}

#[async_trait]
impl Capability for RepairWorker {
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
            summary: "Analyse Rust diagnostics and propose a minimal repair plan".into(),
            uses_model: true,
            side_effects: SideEffectClass::ReadOnly,
            kinds: vec![NodeKind::Analyze],
        }
    }

    fn required_tools(&self) -> Vec<ToolSelector> {
        vec![ToolSelector::name(
            ToolName::new("fs_read").expect("static name"),
        )]
    }

    fn preferred_tier(&self) -> ModelTier {
        ModelTier::Standard
    }

    fn accepts_kind(&self, kind: NodeKind) -> bool {
        kind == NodeKind::Analyze
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

impl RepairWorker {
    /// One attempt (§9.1 sequence): cancel check → load predecessor
    /// artifacts → graph read (best effort) → assemble → route → complete →
    /// extract/validate → payload.
    async fn run(
        &self,
        ctx: &CapabilityContext<'_>,
        attempt: &mut Attempt,
    ) -> Result<WorkerSuccess, WorkerError> {
        if ctx.is_cancelled() {
            return Err(WorkerError::cancelled());
        }

        // RW1: goal or predecessor decode.
        let goal_text = match &ctx.input.payload {
            NodeInputPayload::Goal(goal) => goal.text.clone(),
            NodeInputPayload::FromPredecessors { .. } => String::new(),
        };
        let payloads = load_pred_payloads(ctx).await?;
        let mut diagnostics = diagnostics_from_payloads(&payloads);

        // RW4: graph read is best effort; an empty view is normal (M7 thin,
        // CX7) and an error degrades to "no graph input", never a failure.
        if let Ok(view) = ctx
            .graph
            .query(GraphQuery::Diagnostics {
                crate_id: None,
                since: None,
            })
            .await
        {
            diagnostics.extend(view.diagnostics);
        }

        // RW2: dedupe by fingerprint, sort by (path, start_line, code), cap.
        diagnostics.sort_by_key(sort_key);
        let mut seen = std::collections::BTreeSet::new();
        diagnostics.retain(|d| seen.insert(d.fingerprint.as_hex().to_owned()));
        let truncated = diagnostics.len() > MAX_DIAGNOSTICS;
        diagnostics.truncate(MAX_DIAGNOSTICS);

        let diag_paths: Vec<String> = diagnostics
            .iter()
            .flat_map(|d| d.spans.iter().map(|s| s.path.clone()))
            .collect();

        // PR3: node-local material rides the shipped `AssembleInputs`
        // fields, never a worker-concatenated message body.
        let inputs = AssembleInputs {
            run: Some(ctx.run),
            input: Some(ctx.input.clone()),
            diagnostics: diagnostics.clone(),
            budget: Some(ctx.budget.clone()),
            focus_paths: diag_paths.clone(),
            prior_failure: None,
        };

        // RW4 (A-0011-5): what the graph remembers about these codes. Best
        // effort like every other graph read; the note is advisory prose on
        // the PR11 User-role seam, never a substitute for the diagnostics.
        let mut notes = similar_fix_notes(ctx, &diagnostics).await;

        // A-0012-1 (mirroring A-0011-5c): who calls into the diagnosed
        // items. Same posture — bounded, read-only, best effort; caller
        // files additionally widen the RW6 target set, since a caller is a
        // workspace observation of impact.
        let (caller_notes, caller_paths) = caller_hints(ctx, &diagnostics).await;
        notes.extend(caller_notes);

        let (proposal, _pack) = llm_exchange(
            ctx,
            attempt,
            &self.config,
            REPAIR_SYSTEM,
            Some(&repair_response_schema()),
            &inputs,
            &notes,
            |value| {
                let proposal: RepairProposal =
                    serde_json::from_value(value.clone()).map_err(|e| format!("schema: {e}"))?;
                validate_proposal(&proposal, &diag_paths, &caller_paths, &goal_text)?;
                Ok(proposal)
            },
        )
        .await?;

        // RW7: model confidence clamped; absent ⇒ 0.5.
        let confidence = proposal.confidence.unwrap_or(0.5).clamp(0.0, 1.0);

        let mut summary = proposal.summary;
        let summary_cut = clamp_string(&mut summary, MAX_PAYLOAD_STRING_BYTES);
        let mut steps: Vec<RepairStep> = proposal
            .steps
            .into_iter()
            .map(|s| {
                let mut rationale = s.rationale;
                clamp_string(&mut rationale, MAX_RATIONALE_BYTES);
                RepairStep {
                    file: s.file,
                    rationale,
                    anchor_line: s.anchor_line,
                }
            })
            .collect();
        // OC7 vector bound.
        let steps_cut = super::super::payload::clamp_vec(
            &mut steps,
            super::super::payload::MAX_PAYLOAD_VEC_ENTRIES,
        );

        let mut payload = RepairPlanPayload {
            schema_version: PAYLOAD_SCHEMA_VERSION,
            capability: "repair".into(),
            summary,
            target_files: proposal.target_files,
            steps,
            diagnostics_addressed: diagnostics.iter().map(|d| d.fingerprint.clone()).collect(),
            needs_replan: proposal.needs_replan, // RW8: still a success.
            truncated: truncated || summary_cut || steps_cut,
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
        // OC7 total bound: drop the largest list rather than truncate a
        // citation (PR4 keeps citations intact).
        if super::super::payload::exceeds_total_bound(&value) {
            payload.steps.clear();
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

/// Ask the graph what has been fixed before for the codes in hand and
/// render at most one compact fenced note (amendment A-0011-5).
///
/// Returns an empty vector when there is nothing to say: no codes, no
/// rows, or a graph that errored — an empty fence would only spend tokens
/// claiming ignorance.
async fn similar_fix_notes(
    ctx: &CapabilityContext<'_>,
    diagnostics: &[DiagnosticEvent],
) -> Vec<String> {
    let mut codes: Vec<&str> = Vec::new();
    for d in diagnostics {
        if let Some(code) = d.code.as_deref() {
            if !codes.contains(&code) {
                codes.push(code);
            }
            if codes.len() == MAX_FIX_CODES {
                break;
            }
        }
    }

    let mut lines: Vec<String> = Vec::new();
    for code in codes {
        if lines.len() >= MAX_SIMILAR_FIXES {
            break;
        }
        let Ok(view) = ctx
            .graph
            .query(GraphQuery::SimilarFixes {
                diagnostic_code: code.to_owned(),
                limit: FIXES_PER_CODE,
            })
            .await
        else {
            continue; // A graph error is "no priors", never a failure (RW4).
        };
        for f in view.fixes {
            if !f.verified || lines.len() >= MAX_SIMILAR_FIXES {
                continue;
            }
            let package = f
                .crate_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |c| c.as_str().to_owned());
            let patch = f
                .patch_artifact
                .map_or_else(|| "-".to_owned(), |a| a.to_string());
            lines.push(format!(
                "{code} in {package}: verified on {}, patch {patch}",
                f.recorded_at.0.date()
            ));
        }
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let body = format!(
        "Past repairs of these diagnostic codes in this workspace that a \
         later verification accepted. Advisory precedent only — the \
         patches are not shown and may not apply here.\n{}",
        lines.join("\n")
    );
    vec![fence_tool("similar_fixes", &body, MAX_SIMILAR_FIXES_BYTES)]
}

/// Ask the graph who calls into the diagnosed items and render at most one
/// compact fenced note plus the caller-file target hints (A-0012-1,
/// mirroring A-0011-5c's posture).
///
/// Bounded like every other graph read: at most [`MAX_CALLER_PATHS`]
/// `Symbol` resolutions, one `Subgraph` per resolved module, one `Callers`
/// query per item anchor (at most [`MAX_CALLER_ITEMS`] per path), and
/// [`MAX_CALLER_LINES`] rendered lines. The anchor chain follows the
/// `alloy-index` store: a file-path `Symbol` resolves to the file's
/// **module** node (`query.rs::symbol`, via `graph_files.module_id`), but
/// `Calls` edges anchor exclusively on **item** nodes
/// (`lang/rust/pass.rs`), so the module is expanded to the items it
/// `Defines` before `Callers` is asked. A graph error or an empty view —
/// today's store, whose `Callers` stub returns empty — is "no known
/// callers": no fence, no error, never a failure (RW4). Read-only via the
/// `GraphViewHandle`; PW/SEC posture unchanged.
async fn caller_hints(
    ctx: &CapabilityContext<'_>,
    diagnostics: &[DiagnosticEvent],
) -> (Vec<String>, Vec<String>) {
    let mut paths: Vec<&str> = Vec::new();
    for d in diagnostics {
        if let Some(span) = d.spans.first() {
            let path = span.path.as_str();
            if !paths.contains(&path) {
                paths.push(path);
            }
            if paths.len() == MAX_CALLER_PATHS {
                break;
            }
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut caller_paths: Vec<String> = Vec::new();
    'outer: for path in paths {
        let Ok(view) = ctx
            .graph
            .query(GraphQuery::Symbol {
                path: path.to_owned(),
            })
            .await
        else {
            continue; // A graph error is "no callers known", never a failure.
        };
        let Some(node) = view.nodes.first() else {
            continue;
        };
        // Expand a module node to the items it `Defines`: the only nodes
        // the store anchors `Calls` edges on. An item resolution (a
        // rust-path `Symbol`) anchors itself.
        let anchors: Vec<crate::graph::GraphNode> = if node.kind == GraphNodeKind::Item {
            vec![node.clone()]
        } else {
            let Ok(sub) = ctx
                .graph
                .query(GraphQuery::Subgraph {
                    seeds: vec![node.id],
                    radius: 1,
                })
                .await
            else {
                continue;
            };
            let item_ids: Vec<_> = sub
                .edges
                .iter()
                .filter(|e| e.kind == GraphEdgeKind::Defines && e.from == node.id)
                .map(|e| e.to)
                .collect();
            sub.nodes
                .into_iter()
                .filter(|n| n.kind == GraphNodeKind::Item && item_ids.contains(&n.id))
                .take(MAX_CALLER_ITEMS)
                .collect()
        };
        for anchor in anchors {
            let Ok(callers) = ctx
                .graph
                .query(GraphQuery::Callers { fn_node: anchor.id })
                .await
            else {
                continue 'outer;
            };
            for caller in callers.nodes {
                if lines.len() >= MAX_CALLER_LINES {
                    break 'outer;
                }
                if caller.id == anchor.id {
                    continue;
                }
                let file = caller.file.as_deref().unwrap_or("-");
                let line = format!("{} ({file}) calls into {path}", caller.path);
                if lines.contains(&line) {
                    continue;
                }
                lines.push(line);
                if let Some(f) = caller.file {
                    if is_jail_relative(&f) && !caller_paths.contains(&f) {
                        caller_paths.push(f);
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let body = format!(
        "Known callers of the diagnosed items, from the project graph. \
         These files consume what you are changing: treat them as \
         additional target-file hints and keep their call sites \
         compiling.\n{}",
        lines.join("\n")
    );
    (
        vec![fence_tool("callers", &body, MAX_CALLERS_BYTES)],
        caller_paths,
    )
}

fn sort_key(d: &DiagnosticEvent) -> (String, u32, Option<String>) {
    let (path, line) = d
        .spans
        .first()
        .map_or((String::new(), 0), |s| (s.path.clone(), s.start_line));
    (path, line, d.code.clone())
}

/// PS5 + RW5 + RW6 semantic validation. `caller_paths` are graph-observed
/// caller files (A-0012-1): legitimate impact targets alongside the
/// diagnostic- and goal-named ones.
fn validate_proposal(
    proposal: &RepairProposal,
    diag_paths: &[String],
    caller_paths: &[String],
    goal_text: &str,
) -> Result<(), String> {
    // RW5: prose only — a unified-diff header anywhere is a violation.
    for text in std::iter::once(proposal.summary.as_str())
        .chain(proposal.steps.iter().map(|s| s.rationale.as_str()))
    {
        if contains_diff_marker(text) {
            return Err("diff content in prose field (patch authorship belongs to edit)".into());
        }
    }
    // RW6.
    if !proposal.needs_replan && proposal.target_files.is_empty() {
        return Err("target_files must be non-empty unless needs_replan".into());
    }
    if proposal.target_files.len() > MAX_TARGET_FILES {
        return Err(format!("more than {MAX_TARGET_FILES} target files"));
    }
    for path in &proposal.target_files {
        if !is_jail_relative(path) {
            return Err(format!("target path is not jail-relative: {path:.120}"));
        }
        // RW6: a target must trace to a workspace observation — a
        // diagnostic span or a graph-recorded caller (A-0012-1) — or be
        // named by the goal (a Create target).
        let known = diag_paths.iter().any(|p| p == path)
            || caller_paths.iter().any(|p| p == path)
            || goal_text.contains(path.as_str());
        if !known {
            return Err(format!(
                "target file named by neither a diagnostic, a recorded caller, \
                 nor the goal: {path:.120}"
            ));
        }
    }
    for step in &proposal.steps {
        if !is_jail_relative(&step.file) {
            return Err(format!(
                "step path is not jail-relative: {:.120}",
                step.file
            ));
        }
    }
    Ok(())
}

fn contains_diff_marker(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("@@"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(files: Vec<&str>, rationale: &str, needs_replan: bool) -> RepairProposal {
        RepairProposal {
            summary: "fix it".into(),
            target_files: files.into_iter().map(String::from).collect(),
            steps: vec![StepProposal {
                file: "src/lib.rs".into(),
                rationale: rationale.into(),
                anchor_line: None,
            }],
            needs_replan,
            confidence: None,
        }
    }

    #[test]
    fn repair_worker_rejects_diff_in_rationale() {
        // RW5.
        let p = proposal(vec!["src/lib.rs"], "@@ -1 +1 @@ change this", false);
        assert!(validate_proposal(&p, &["src/lib.rs".into()], &[], "").is_err());
        let ok = proposal(vec!["src/lib.rs"], "clone before the borrow", false);
        assert!(validate_proposal(&ok, &["src/lib.rs".into()], &[], "").is_ok());
    }

    #[test]
    fn repair_worker_requires_target_files_unless_needs_replan() {
        // RW6.
        assert!(validate_proposal(&proposal(vec![], "x", false), &[], &[], "").is_err());
        assert!(validate_proposal(&proposal(vec![], "x", true), &[], &[], "").is_ok());
    }

    #[test]
    fn repair_worker_rejects_unknown_and_escaping_targets() {
        // RW6 + PS5.
        assert!(validate_proposal(&proposal(vec!["../x.rs"], "x", false), &[], &[], "").is_err());
        assert!(
            validate_proposal(&proposal(vec!["src/other.rs"], "x", false), &[], &[], "").is_err()
        );
        // Named by the goal ⇒ acceptable Create target.
        assert!(validate_proposal(
            &proposal(vec!["src/other.rs"], "x", false),
            &[],
            &[],
            "create src/other.rs"
        )
        .is_ok());
        // A graph-recorded caller file is a workspace observation too
        // (A-0012-1): a legitimate impact target.
        assert!(validate_proposal(
            &proposal(vec!["src/other.rs"], "x", false),
            &[],
            &["src/other.rs".into()],
            ""
        )
        .is_ok());
    }
}
