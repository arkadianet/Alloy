//! [`DefaultContextEngine`]: the MVP assembler (RFC-0012 §5–§8).
//!
//! Assembly is a pure read + render (rule A14): validate, resolve pins,
//! allocate weighted allowances, build the three live domains, render the
//! fenced sections in A2 order, backstop-drop until the estimate fits
//! (B10), then write the manifest from the counters accumulated during
//! rendering (A11).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::graph::{GraphNode, GraphQuery, GraphViewHandle};
use crate::router::{ChatMessage, ChatRole, Citation, PromptPack};
use crate::storage::{ArtifactStore, EventStore};
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{ArtifactId, Digest, GraphVersion, SessionId, SummaryId};

use super::artifacts::{
    fetch_patch_body, fetch_unpinned, kind_admitted, render_meta_line, ArtifactCandidate,
};
use super::budget::{allowances, effective_budget};
use super::conversation::{fetch as fetch_conversation, sanitize_goal, ConversationRaw, EventLine};
use super::engine::ContextEngine;
use super::error::ContextError;
use super::estimator::{BytesPerTokenEstimator, TokenEstimator};
use super::profile::ContextProfile;
use super::render::{
    bound_bytes, degraded_marker, graph_truncated_marker, omitted_marker, relativize,
    sanitize_line, sanitize_untrusted, system_frame, truncated_marker, Section, SectionCitation,
};
use super::types::{
    AssembleInputs, AssembleRequest, CompactStrategy, ContextHandle, Degradation,
    DegradationReason, DomainId, EvictPolicy, EvictReport, FileExcerpt, GraphProjection,
    StaleReason, WorkingSet,
};
use super::working_set::{
    fetch_diagnostics_fallback, fetch_graph, order_diagnostics, order_files, primary_span_path,
    query_once_retry_busy, read_workspace_file, render_diagnostic, FileCandidate,
};
use super::{CONTEXT_FORMAT_VERSION, SYSTEM_FRAME_RESERVE_EST};

/// D3: the pinned goal is never truncated below its first 2 000 bytes.
const GOAL_MIN_BYTES: usize = 2_000;

// ---------------------------------------------------------------------
// Metrics (§3.9, OB2)
// ---------------------------------------------------------------------

/// Atomic counters, RFC-0004 snapshot shape (OB2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextMetricsSnapshot {
    /// Successful `assemble` calls.
    pub assembled: u64,
    /// `assemble` calls that returned `ContextError`.
    pub failed: u64,
    /// Sum of estimated input tokens across successful packs.
    pub tokens_est_total: u64,
    /// Items dropped by the budget (B6, B10).
    pub items_dropped: u64,
    /// Items rendered with a truncation marker (B7).
    pub items_truncated: u64,
    /// Graph queries issued.
    pub graph_queries: u64,
    /// Graph queries that degraded the domain (E2).
    pub graph_degradations: u64,
    /// Memo hits (§8.1).
    pub cache_hits: u64,
    /// Memo misses.
    pub cache_misses: u64,
    /// Entries evicted (§8.3).
    pub cache_evictions: u64,
}

#[derive(Debug, Default)]
struct Metrics {
    assembled: AtomicU64,
    failed: AtomicU64,
    tokens_est_total: AtomicU64,
    items_dropped: AtomicU64,
    items_truncated: AtomicU64,
    graph_queries: AtomicU64,
    graph_degradations: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    cache_evictions: AtomicU64,
}

impl Metrics {
    fn snapshot(&self) -> ContextMetricsSnapshot {
        ContextMetricsSnapshot {
            assembled: self.assembled.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            tokens_est_total: self.tokens_est_total.load(Ordering::Relaxed),
            items_dropped: self.items_dropped.load(Ordering::Relaxed),
            items_truncated: self.items_truncated.load(Ordering::Relaxed),
            graph_queries: self.graph_queries.load(Ordering::Relaxed),
            graph_degradations: self.graph_degradations.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            cache_evictions: self.cache_evictions.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------
// Memo (§8.1)
// ---------------------------------------------------------------------

/// Memo key: `(SessionId, NodeId, GraphVersion, seed-set digest, radius)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MemoKey {
    session: SessionId,
    node: crate::types::ids::NodeId,
    version: GraphVersion,
    /// Hex form of the seed-set digest (`Digest` itself is not `Ord`).
    seed_digest: String,
    radius: u8,
}

#[derive(Debug, Clone)]
struct MemoEntry {
    summary: SummaryId,
    projection: GraphProjection,
    /// Queries the memoized projection stands for; reported on hits so the
    /// manifest stays byte-identical across the memo (A1).
    queried: u64,
    /// Estimated tokens of the projection's rendered lines (B2 estimator).
    est: u64,
    /// Monotonic in-process use counter — never a wall clock (K4).
    last_used: u64,
}

#[derive(Debug, Default)]
struct Memo {
    entries: BTreeMap<MemoKey, MemoEntry>,
    use_seq: u64,
}

// ---------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------

/// The MVP assembler. One instance per host; shared behind `Arc`.
pub struct DefaultContextEngine {
    profile: ContextProfile,
    graph: GraphViewHandle,
    events: Arc<dyn EventStore>,
    artifacts: Arc<dyn ArtifactStore>,
    workspace_root: PathBuf,
    estimator: Arc<dyn TokenEstimator>,
    memo: Mutex<Memo>,
    metrics: Metrics,
}

impl std::fmt::Debug for DefaultContextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Opaque: never leaks the stores or the workspace path.
        f.write_str("DefaultContextEngine")
    }
}

impl DefaultContextEngine {
    /// Build from a profile and the seams the host already holds.
    #[must_use]
    pub fn new(
        profile: ContextProfile,
        graph: GraphViewHandle,
        events: Arc<dyn EventStore>,
        artifacts: Arc<dyn ArtifactStore>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            profile,
            graph,
            events,
            artifacts,
            workspace_root,
            estimator: Arc::new(BytesPerTokenEstimator::default()),
            memo: Mutex::new(Memo::default()),
            metrics: Metrics::default(),
        }
    }

    /// Override the estimator (tests, future tokenizer). Defaults to
    /// [`BytesPerTokenEstimator`].
    #[must_use]
    pub fn with_estimator(mut self, est: Arc<dyn TokenEstimator>) -> Self {
        self.estimator = est;
        self
    }

    /// Profile in use.
    #[must_use]
    pub fn profile(&self) -> &ContextProfile {
        &self.profile
    }

    /// Metrics snapshot (§11.2).
    #[must_use]
    pub fn metrics(&self) -> ContextMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Assemble with the host-side inputs of §3.5. `assemble` calls this
    /// with `AssembleInputs::default()`; the scheduler calls it directly.
    pub async fn assemble_with(
        &self,
        req: AssembleRequest,
        inputs: AssembleInputs,
    ) -> Result<PromptPack, ContextError> {
        let span = tracing::info_span!(
            "context.assemble",
            session = %req.session,
            node = %req.node,
            capability = %req.capability,
            budget_est = tracing::field::Empty,
            used_est = tracing::field::Empty,
            citations = tracing::field::Empty,
            degradations = tracing::field::Empty,
            graph_version = tracing::field::Empty,
        );
        let result = self
            .assemble_inner(&req, &inputs, span.clone())
            .instrument(span)
            .await;
        match &result {
            Ok(_) => self.metrics.assembled.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.metrics.failed.fetch_add(1, Ordering::Relaxed),
        };
        result
    }

    /// The WorkingSet alone, for tests and for RFC-0013 introspection.
    pub async fn working_set(&self, req: &AssembleRequest, inputs: &AssembleInputs) -> WorkingSet {
        let pins = self.resolve_pins(req, inputs).await.unwrap_or_default();
        let ws = self.fetch_working_set(req, inputs, &pins).await;
        let remainder = self
            .profile
            .total_token_budget
            .saturating_sub(SYSTEM_FRAME_RESERVE_EST);
        let allowance = allowances(&self.profile, remainder)
            .into_iter()
            .find(|(d, _)| *d == DomainId::WorkingSet)
            .map_or(0, |(_, a)| a);
        let state = self.clamp_working_set(&ws, allowance);
        self.working_set_payload(&ws, &state)
    }

    fn est(&self, s: &str) -> usize {
        self.estimator.estimate(s)
    }
}

// ---------------------------------------------------------------------
// Pin resolution (B11, E11)
// ---------------------------------------------------------------------

/// A resolved `must_include` item and its addendum entry.
#[derive(Debug, Clone)]
enum ResolvedPin {
    File(FileCandidate),
    Artifact(ArtifactCandidate),
    Diagnostic(DiagnosticEvent),
    Symbol(Vec<GraphNode>),
}

#[derive(Debug, Clone, Default)]
struct ResolvedPins {
    /// `(handle-kind, key, pin)` in request order.
    pins: Vec<(&'static str, String, ResolvedPin)>,
    /// Degradations discovered while resolving (e.g. a non-textual pinned
    /// artifact rendered by metadata).
    degradations: Vec<Degradation>,
    /// Graph queries issued for `Symbol` pins.
    queried: u64,
}

impl DefaultContextEngine {
    fn validate(&self, req: &AssembleRequest) -> Result<(), ContextError> {
        for handle in &req.must_include {
            match handle {
                ContextHandle::File { path, lines } => {
                    validate_pin_path(path)?;
                    if let Some((start, end)) = lines {
                        if *start == 0 || end < start {
                            return Err(ContextError::InvalidRequest(format!(
                                "bad line range {start}-{end} for {path}"
                            )));
                        }
                    }
                }
                ContextHandle::Symbol { path } => validate_pin_path(path)?,
                ContextHandle::Artifact(_) | ContextHandle::Diagnostic(_) => {}
            }
        }
        Ok(())
    }

    async fn resolve_pins(
        &self,
        req: &AssembleRequest,
        inputs: &AssembleInputs,
    ) -> Result<ResolvedPins, ContextError> {
        let mut out = ResolvedPins::default();
        for handle in &req.must_include {
            match handle {
                ContextHandle::File { path, lines } => {
                    let candidate = self.pin_file(path, *lines).await?;
                    out.pins.push((
                        "file",
                        file_pin_key(path, *lines),
                        ResolvedPin::File(candidate),
                    ));
                }
                ContextHandle::Artifact(id) => {
                    let candidate = self.pin_artifact(*id, &mut out.degradations).await?;
                    out.pins
                        .push(("artifact", id.to_string(), ResolvedPin::Artifact(candidate)));
                }
                ContextHandle::Diagnostic(id) => {
                    let key = id.to_string();
                    let found = inputs.diagnostics.iter().find(|d| d.id == *id).cloned();
                    let found = match found {
                        Some(d) => d,
                        None => {
                            // Explicit exception to the only-when-empty rule
                            // (§4.3c), still within the A13 query bound.
                            out.queried += 1;
                            let view = query_once_retry_busy(
                                &self.graph,
                                GraphQuery::Diagnostics {
                                    crate_id: None,
                                    since: None,
                                },
                            )
                            .await;
                            view.ok()
                                .and_then(|v| v.diagnostics.into_iter().find(|d| d.id == *id))
                                .ok_or(ContextError::MustIncludeNotFound(key.clone()))?
                        }
                    };
                    out.pins
                        .push(("diagnostic", key, ResolvedPin::Diagnostic(found)));
                }
                ContextHandle::Symbol { path } => {
                    out.queried += 1;
                    let view = query_once_retry_busy(
                        &self.graph,
                        GraphQuery::Symbol { path: path.clone() },
                    )
                    .await;
                    match view {
                        Ok(v) if !v.nodes.is_empty() => {
                            out.pins
                                .push(("symbol", path.clone(), ResolvedPin::Symbol(v.nodes)));
                        }
                        // E11: graph-unavailable resolution degrades to a
                        // File pin when the path names a readable file.
                        _ => match self.pin_file(path, None).await {
                            Ok(candidate) => {
                                out.degradations.push(Degradation {
                                    domain: DomainId::WorkingSet,
                                    reason: DegradationReason::GraphEmpty,
                                    detail: format!(
                                        "symbol pin {} resolved as a file",
                                        bound_bytes(&sanitize_line(path), 150)
                                    ),
                                });
                                out.pins.push((
                                    "file",
                                    file_pin_key(path, None),
                                    ResolvedPin::File(candidate),
                                ));
                            }
                            Err(_) => {
                                return Err(ContextError::MustIncludeNotFound(path.clone()));
                            }
                        },
                    }
                }
            }
        }
        Ok(out)
    }

    async fn pin_file(
        &self,
        path: &str,
        lines: Option<(u32, u32)>,
    ) -> Result<FileCandidate, ContextError> {
        let file_lines = read_workspace_file(&self.workspace_root, path)
            .await
            .map_err(|_| ContextError::MustIncludeNotFound(path.to_owned()))?;
        if let Some((start, _)) = lines {
            if (start as usize) > file_lines.len() {
                return Err(ContextError::MustIncludeNotFound(format!(
                    "{path}: line {start} past end of file"
                )));
            }
        }
        Ok(FileCandidate {
            path: path.to_owned(),
            pinned: true,
            pin_range: lines,
            is_focus: true,
            has_diagnostic: false,
            centre_line: None,
            lines: file_lines,
            crate_id: None,
        })
    }

    async fn pin_artifact(
        &self,
        id: ArtifactId,
        degradations: &mut Vec<Degradation>,
    ) -> Result<ArtifactCandidate, ContextError> {
        let meta = self
            .artifacts
            .meta(id)
            .await
            .map_err(|_| ContextError::MustIncludeNotFound(id.to_string()))?;
        // D12 outranks B11: a pinned `PromptPack`- or `Other`-kind artifact
        // is `MustIncludeNotFound`, never rendered.
        if !kind_admitted(&meta.kind) {
            return Err(ContextError::MustIncludeNotFound(id.to_string()));
        }
        let blob = self
            .artifacts
            .get(id)
            .await
            .map_err(|_| ContextError::MustIncludeNotFound(id.to_string()))?;
        let body = if blob.bytes.iter().take(8 * 1024).any(|&b| b == 0) {
            None
        } else {
            String::from_utf8(blob.bytes)
                .ok()
                .map(|s| sanitize_untrusted(&s))
        };
        if body.is_none() {
            // The pin still appears — by metadata and digest (D7).
            degradations.push(Degradation {
                domain: DomainId::Artifacts,
                reason: DegradationReason::NotTextual,
                detail: id.to_string(),
            });
        }
        Ok(ArtifactCandidate {
            id,
            meta,
            pinned: true,
            body,
        })
    }
}

fn validate_pin_path(path: &str) -> Result<(), ContextError> {
    let looks_absolute = path.starts_with('/')
        || path.starts_with('\\')
        || (path.len() >= 3
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'/' | b'\\'));
    if path.is_empty() || looks_absolute || path.split(['/', '\\']).any(|c| c == "..") {
        return Err(ContextError::InvalidRequest(format!(
            "must_include path must be workspace-relative with `/` separators and no `..`: {path}"
        )));
    }
    Ok(())
}

fn file_pin_key(path: &str, lines: Option<(u32, u32)>) -> String {
    match lines {
        Some((start, end)) => format!("{path}#L{start}-L{end}"),
        None => path.to_owned(),
    }
}

// ---------------------------------------------------------------------
// Raw domain data and clamp states
// ---------------------------------------------------------------------

#[derive(Debug, Default)]
struct WsRaw {
    files: Vec<FileCandidate>,
    /// Files dropped by the `max_files` cap.
    file_cap_omitted: usize,
    graph: Option<GraphProjection>,
    pinned_node_ids: BTreeSet<crate::types::ids::GraphNodeId>,
    /// `(diagnostic, pinned)`, in D11 order.
    diagnostics: Vec<(DiagnosticEvent, bool)>,
    diag_cap_omitted: usize,
    degradations: Vec<Degradation>,
    /// Queries actually issued this call (metrics).
    queried: u64,
    /// Queries the rendered projection stands for (manifest; A1 across a
    /// memo hit).
    queried_repr: u64,
    degraded_queries: u64,
}

#[derive(Debug, Clone, Default)]
struct ConvState {
    /// The full pinned goal text (D3).
    goal_full: Option<String>,
    /// Kept goal byte prefix; equals the full length when untruncated.
    goal_kept_bytes: usize,
    /// `true` when the goal section carries a truncation marker.
    goal_truncated: bool,
    /// Number of admitted history lines — a suffix of the ascending list,
    /// admitted newest-first (D13).
    admitted: usize,
    omitted: usize,
    used_est: usize,
}

#[derive(Debug, Clone)]
struct FileRender {
    /// Index into `WsRaw.files`.
    idx: usize,
    start_line: u32,
    end_line: u32,
    truncated: bool,
    total_lines: usize,
}

#[derive(Debug, Clone, Default)]
struct WsState {
    files: Vec<FileRender>,
    files_omitted: usize,
    /// Kept flags over the merged (seeds + neighbourhood) node list.
    nodes_kept: Vec<bool>,
    /// Kept flags over the projection's edges.
    edges_kept: Vec<bool>,
    graph_omitted: usize,
    /// Kept flags over `WsRaw.diagnostics`.
    diags_kept: Vec<bool>,
    diags_omitted: usize,
    used_est: usize,
}

#[derive(Debug, Clone, Default)]
struct ArtState {
    /// Kept flags over the candidate list; second flag keeps the body.
    kept: Vec<(bool, bool)>,
    omitted: usize,
    used_est: usize,
}

// ---------------------------------------------------------------------
// Assembly (§5.2)
// ---------------------------------------------------------------------

impl DefaultContextEngine {
    #[allow(clippy::too_many_lines)]
    async fn assemble_inner(
        &self,
        req: &AssembleRequest,
        inputs: &AssembleInputs,
        span: tracing::Span,
    ) -> Result<PromptPack, ContextError> {
        // 1. Validate (§5.2 step 1).
        self.validate(req)?;
        let effective = effective_budget(req.token_budget, &self.profile, inputs);
        if effective == 0 {
            return Err(ContextError::BudgetTooSmall { needed: 1, have: 0 });
        }
        span.record("budget_est", effective);

        // 2. Conversation fetch — also supplies the pinned goal and the
        // artifact references (§4.2, §4.4).
        let conv = fetch_conversation(
            self.events.as_ref(),
            req.session,
            self.profile.max_conversation_events,
        )
        .instrument(tracing::info_span!("context.domain.conversation"))
        .await;
        let goal_text = goal_from_inputs(inputs).or_else(|| conv.goal_from_events.clone());

        // 3. Resolve must_include (B11, E11).
        let pins = self.resolve_pins(req, inputs).await?;
        self.metrics
            .graph_queries
            .fetch_add(pins.queried, Ordering::Relaxed);

        // 4. Budget floors (B3, E5, E6).
        let goal_min_est = goal_text
            .as_ref()
            .map(|g| self.goal_section_min_est(g))
            .unwrap_or(0);
        let mut mi_est = 0usize;
        for (kind, key, pin) in &pins.pins {
            let item_est = self.pin_est(pin) + self.addendum_est(kind, key);
            if item_est > effective.saturating_sub(SYSTEM_FRAME_RESERVE_EST) {
                return Err(ContextError::MustIncludeTooLarge(key.clone()));
            }
            mi_est += item_est;
        }
        let floor = SYSTEM_FRAME_RESERVE_EST + goal_min_est + mi_est;
        if effective <= floor {
            return Err(ContextError::BudgetTooSmall {
                needed: floor + 1,
                have: effective,
            });
        }

        // 5. WorkingSet fetch (files + graph + diagnostics).
        let ws = self
            .fetch_working_set(req, inputs, &pins)
            .instrument(tracing::info_span!("context.domain.working_set"))
            .await;

        // 6. Artifacts fetch.
        let pinned_ids: Vec<ArtifactId> = pins
            .pins
            .iter()
            .filter_map(|(_, _, p)| match p {
                ResolvedPin::Artifact(c) => Some(c.id),
                _ => None,
            })
            .collect();
        let predecessor_ids = predecessor_output_ids(inputs);
        let mut art = fetch_unpinned(
            self.artifacts.as_ref(),
            &conv.artifact_refs,
            &predecessor_ids,
            &pinned_ids,
        )
        .instrument(tracing::info_span!("context.domain.artifacts"))
        .await;
        // Pins render first, in request order (A10).
        let mut art_candidates: Vec<ArtifactCandidate> = pins
            .pins
            .iter()
            .filter_map(|(_, _, p)| match p {
                ResolvedPin::Artifact(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        let cap_left = self
            .profile
            .max_artifacts
            .saturating_sub(art_candidates.len());
        let art_cap_omitted = art.candidates.len().saturating_sub(cap_left);
        art.candidates.truncate(cap_left);
        // Pre-fetch unpinned Patch bodies so clamping stays pure (§4.4).
        for c in &mut art.candidates {
            if matches!(c.meta.kind, crate::storage::ArtifactKind::Patch) {
                c.body = fetch_patch_body(self.artifacts.as_ref(), c.id).await;
            }
        }
        art_candidates.extend(art.candidates.clone());

        // 7. Allowances (B4) over the remainder.
        let remainder = effective - SYSTEM_FRAME_RESERVE_EST - mi_est;
        let base = allowances(&self.profile, remainder);

        // 8. Build domains clamped to their allowances (B6), then
        // redistribute unused allowance exactly once in LIVE order (B5).
        let mut conv_state = self.clamp_conversation(
            &conv,
            goal_text.as_deref(),
            allowance_of(&base, DomainId::Conversation),
        );
        let mut ws_state = self.clamp_working_set(&ws, allowance_of(&base, DomainId::WorkingSet));
        let mut art_state =
            self.clamp_artifacts(&art_candidates, allowance_of(&base, DomainId::Artifacts));

        let mut leftover = allowance_of(&base, DomainId::Conversation)
            .saturating_sub(conv_state.used_est)
            + allowance_of(&base, DomainId::WorkingSet).saturating_sub(ws_state.used_est)
            + allowance_of(&base, DomainId::Artifacts).saturating_sub(art_state.used_est);
        for domain in DomainId::LIVE {
            if leftover == 0 {
                break;
            }
            match domain {
                DomainId::Conversation => {
                    let expanded = self.clamp_conversation(
                        &conv,
                        goal_text.as_deref(),
                        allowance_of(&base, domain) + leftover,
                    );
                    leftover -= expanded
                        .used_est
                        .saturating_sub(conv_state.used_est)
                        .min(leftover);
                    conv_state = expanded;
                }
                DomainId::WorkingSet => {
                    let expanded =
                        self.clamp_working_set(&ws, allowance_of(&base, domain) + leftover);
                    leftover -= expanded
                        .used_est
                        .saturating_sub(ws_state.used_est)
                        .min(leftover);
                    ws_state = expanded;
                }
                DomainId::Artifacts => {
                    let expanded = self
                        .clamp_artifacts(&art_candidates, allowance_of(&base, domain) + leftover);
                    leftover -= expanded
                        .used_est
                        .saturating_sub(art_state.used_est)
                        .min(leftover);
                    art_state = expanded;
                }
                _ => {}
            }
        }

        // 9. Render + backstop (B10): drop the lowest-ranked droppable item
        // in ascending-weight domain order until the estimate fits.
        let mut rendered = self.render_all(
            req,
            &conv,
            &conv_state,
            &ws,
            &ws_state,
            &art_candidates,
            &art_state,
            &art.degradations,
            &pins,
            art_cap_omitted + art.unresolved,
        );
        while rendered.total_est > effective {
            if !self.backstop_drop_one(
                &mut conv_state,
                &ws,
                &mut ws_state,
                &mut art_state,
                &art_candidates,
            ) {
                // Last resort permitted by D3: cut the goal to its 2 000-byte
                // floor. E5 guaranteed that floor fits.
                let can_cut_goal = conv_state
                    .goal_full
                    .as_ref()
                    .is_some_and(|g| conv_state.goal_kept_bytes > GOAL_MIN_BYTES.min(g.len()));
                if can_cut_goal {
                    let goal = conv_state.goal_full.as_deref().unwrap_or_default();
                    conv_state.goal_kept_bytes = bound_bytes(goal, GOAL_MIN_BYTES).len();
                    conv_state.goal_truncated = conv_state.goal_kept_bytes < goal.len();
                } else {
                    return Err(ContextError::Internal(format!(
                        "assembled estimate {} exceeds effective budget {effective} \
                         with nothing left to drop",
                        rendered.total_est
                    )));
                }
            }
            rendered = self.render_all(
                req,
                &conv,
                &conv_state,
                &ws,
                &ws_state,
                &art_candidates,
                &art_state,
                &art.degradations,
                &pins,
                art_cap_omitted + art.unresolved,
            );
        }

        // 10. B12 assertion, in release builds too.
        if rendered.total_est > effective {
            return Err(ContextError::Internal(format!(
                "assembled estimate {} exceeds effective budget {effective}",
                rendered.total_est
            )));
        }
        if rendered.messages.iter().all(|m| m.role != ChatRole::User) {
            return Err(ContextError::EmptyPrompt);
        }

        // OB5: warn once per distinct degradation reason.
        let mut warned: BTreeSet<(DomainId, DegradationReason)> = BTreeSet::new();
        for d in &rendered.degradations {
            if warned.insert((d.domain, d.reason)) {
                tracing::warn!(
                    domain = d.domain.label(),
                    reason = d.reason.label(),
                    "context domain degraded"
                );
            }
        }

        span.record("used_est", rendered.total_est);
        span.record("citations", rendered.citations.len());
        span.record("degradations", rendered.degradations.len());
        span.record("graph_version", rendered.graph_version.0);
        self.metrics
            .tokens_est_total
            .fetch_add(rendered.total_est as u64, Ordering::Relaxed);
        self.metrics
            .items_dropped
            .fetch_add(rendered.omitted_total as u64, Ordering::Relaxed);
        self.metrics
            .items_truncated
            .fetch_add(rendered.truncated_total as u64, Ordering::Relaxed);

        // 11. Manifest last, from the render counters (A11).
        let manifest = self.manifest(effective, &rendered, &ws);
        Ok(PromptPack {
            messages: rendered.messages,
            citations: rendered.citations,
            domains: Some(manifest),
        })
    }

    /// Fetch files + graph projection + diagnostics (§4.3), memoized by
    /// `GraphVersion` (K1).
    async fn fetch_working_set(
        &self,
        req: &AssembleRequest,
        inputs: &AssembleInputs,
        pins: &ResolvedPins,
    ) -> WsRaw {
        let mut ws = WsRaw::default();
        ws.degradations.extend(pins.degradations.clone());

        // Diagnostics: the caller's first, the recorded log as fallback.
        let mut diagnostics: Vec<DiagnosticEvent> = inputs.diagnostics.clone();
        if diagnostics.is_empty() {
            let (found, degr, queried, degraded) = fetch_diagnostics_fallback(&self.graph).await;
            diagnostics = found;
            ws.degradations.extend(degr);
            ws.queried += queried;
            ws.queried_repr += queried;
            ws.degraded_queries += degraded;
        }
        // Pinned diagnostics join the list (deduplicated by id).
        for (_, _, pin) in &pins.pins {
            if let ResolvedPin::Diagnostic(d) = pin {
                if !diagnostics.iter().any(|x| x.id == d.id) {
                    diagnostics.push(d.clone());
                }
            }
        }
        order_diagnostics(&mut diagnostics, &self.workspace_root);
        let pinned_diag_ids: BTreeSet<_> = pins
            .pins
            .iter()
            .filter_map(|(_, _, p)| match p {
                ResolvedPin::Diagnostic(d) => Some(d.id),
                _ => None,
            })
            .collect();
        let mut kept: Vec<(DiagnosticEvent, bool)> = Vec::new();
        for d in diagnostics {
            let pinned = pinned_diag_ids.contains(&d.id);
            if pinned || kept.len() < self.profile.max_diagnostics {
                kept.push((d, pinned));
            } else {
                ws.diag_cap_omitted += 1;
            }
        }
        ws.diagnostics = kept;

        // Graph seeds (D9): pinned Symbol nodes + file-pin and diagnostic
        // primary-span paths, deduplicated and sorted.
        let mut pinned_nodes: Vec<GraphNode> = Vec::new();
        for (_, _, pin) in &pins.pins {
            if let ResolvedPin::Symbol(nodes) = pin {
                pinned_nodes.extend(nodes.clone());
            }
        }
        ws.pinned_node_ids = pinned_nodes.iter().map(|n| n.id).collect();
        let mut seed_paths: BTreeSet<String> = BTreeSet::new();
        for (_, _, pin) in &pins.pins {
            if let ResolvedPin::File(f) = pin {
                seed_paths.insert(f.path.clone());
            }
        }
        for (d, _) in &ws.diagnostics {
            if let Some(p) = primary_span_path(d, &self.workspace_root) {
                seed_paths.insert(p);
            }
        }
        // A13 bound: at most `must_include.len() + max_files` Symbol queries.
        while seed_paths.len() > self.profile.max_files {
            let last = seed_paths.iter().next_back().cloned();
            if let Some(last) = last {
                seed_paths.remove(&last);
            }
        }

        // Memo lookup (K1/K3): keyed by the current GraphVersion.
        let seed_digest = seed_set_digest(&pinned_nodes, &seed_paths, self.profile.graph_radius)
            .as_hex()
            .to_owned();
        let version = self.graph.version().await;
        let mut projection: Option<GraphProjection> = None;
        let mut served_from_memo = false;
        if let Ok(current) = &version {
            let key = MemoKey {
                session: req.session,
                node: req.node,
                version: *current,
                seed_digest: seed_digest.clone(),
                radius: self.profile.graph_radius,
            };
            let mut memo = self
                .memo
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Evict stale-version entries for the same request shape (K1).
            let stale: Vec<MemoKey> = memo
                .entries
                .keys()
                .filter(|k| {
                    k.session == key.session
                        && k.node == key.node
                        && k.seed_digest == key.seed_digest
                        && k.radius == key.radius
                        && k.version != key.version
                })
                .cloned()
                .collect();
            for k in stale {
                if let Some(entry) = memo.entries.remove(&k) {
                    self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        summary = %entry.summary,
                        was = k.version.0,
                        now = key.version.0,
                        "memo entry stale: graph version changed"
                    );
                }
            }
            memo.use_seq += 1;
            let seq = memo.use_seq;
            if let Some(entry) = memo.entries.get_mut(&key) {
                entry.last_used = seq;
                projection = Some(entry.projection.clone());
                ws.queried_repr += entry.queried;
                served_from_memo = true;
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // K3: a failed version lookup is a miss; evict this request's
            // entries and rebuild.
            let mut memo = self
                .memo
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mine: Vec<MemoKey> = memo
                .entries
                .keys()
                .filter(|k| k.session == req.session && k.node == req.node)
                .cloned()
                .collect();
            for k in mine {
                memo.entries.remove(&k);
                self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        if served_from_memo {
            ws.graph = projection;
        } else {
            self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
            let fetch = fetch_graph(
                &self.graph,
                &pinned_nodes,
                &seed_paths,
                self.profile.graph_radius,
            )
            .await;
            ws.queried += fetch.queried;
            ws.queried_repr += fetch.queried;
            ws.degraded_queries += fetch.degraded_queries;
            ws.degradations.extend(fetch.degradations);
            if let Some(projection) = &fetch.projection {
                let est = self.projection_est(projection) as u64;
                let mut memo = self
                    .memo
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                memo.use_seq += 1;
                let seq = memo.use_seq;
                let key = MemoKey {
                    session: req.session,
                    node: req.node,
                    version: projection.version,
                    seed_digest,
                    radius: self.profile.graph_radius,
                };
                memo.entries.insert(
                    key,
                    MemoEntry {
                        summary: SummaryId::new(),
                        projection: projection.clone(),
                        queried: fetch.queried,
                        est,
                        last_used: seq,
                    },
                );
                // LRU capacity (K4).
                while memo.entries.len() > self.profile.cache_capacity.max(1) {
                    if let Some(victim) = lru_victim(&memo) {
                        memo.entries.remove(&victim);
                        self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
                    } else {
                        break;
                    }
                }
            }
            ws.graph = fetch.projection;
        }
        self.metrics
            .graph_queries
            .fetch_add(ws.queried, Ordering::Relaxed);
        self.metrics
            .graph_degradations
            .fetch_add(ws.degraded_queries, Ordering::Relaxed);

        // Files (§4.3a): pins, focus paths, diagnostic paths, seed files.
        let mut candidate_paths: Vec<(String, bool, bool)> = Vec::new(); // (path, focus, diag)
        for p in &inputs.focus_paths {
            if let Some(rel) = relativize(&self.workspace_root, p) {
                candidate_paths.push((rel, true, false));
            } else {
                ws.degradations.push(Degradation {
                    domain: DomainId::WorkingSet,
                    reason: DegradationReason::FileUnreadable,
                    detail: "focus path outside the workspace".into(),
                });
            }
        }
        let mut diag_lines: BTreeMap<String, u32> = BTreeMap::new();
        for (d, _) in &ws.diagnostics {
            if let Some(p) = primary_span_path(d, &self.workspace_root) {
                let line = d.spans.first().map_or(1, |s| s.start_line);
                diag_lines.entry(p.clone()).or_insert(line);
                candidate_paths.push((p, false, true));
            }
        }
        if let Some(projection) = &ws.graph {
            for node in projection.seeds.iter() {
                if let Some(file) = &node.file {
                    candidate_paths.push((file.clone(), false, false));
                }
            }
        }

        let mut files: Vec<FileCandidate> = pins
            .pins
            .iter()
            .filter_map(|(_, _, p)| match p {
                ResolvedPin::File(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        for f in &mut files {
            if let Some(line) = diag_lines.get(&f.path) {
                f.has_diagnostic = true;
                f.centre_line = Some(*line);
            }
        }
        let mut seen: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
        // Merge duplicate candidates so focus/diagnostic flags accumulate.
        let mut merged: BTreeMap<String, (bool, bool)> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for (path, focus, diag) in candidate_paths {
            if seen.contains(&path) {
                continue;
            }
            let entry = merged.entry(path.clone()).or_insert_with(|| {
                order.push(path.clone());
                (false, false)
            });
            entry.0 |= focus;
            entry.1 |= diag;
        }
        for path in order {
            if files.len() >= self.profile.max_files {
                ws.file_cap_omitted += 1;
                continue;
            }
            let (is_focus, has_diagnostic) = merged[&path];
            match read_workspace_file(&self.workspace_root, &path).await {
                Ok(lines) => {
                    seen.insert(path.clone());
                    let crate_id = ws.graph.as_ref().and_then(|g| {
                        g.seeds
                            .iter()
                            .chain(g.neighbourhood.iter())
                            .find(|n| n.file.as_deref() == Some(path.as_str()))
                            .and_then(|n| n.crate_id.clone())
                    });
                    files.push(FileCandidate {
                        centre_line: diag_lines.get(&path).copied(),
                        path,
                        pinned: false,
                        pin_range: None,
                        is_focus,
                        has_diagnostic,
                        lines,
                        crate_id,
                    });
                }
                Err(reason) => ws.degradations.push(Degradation {
                    domain: DomainId::WorkingSet,
                    reason,
                    detail: bound_bytes(&sanitize_line(&path), 200),
                }),
            }
        }
        order_files(&mut files);
        ws.files = files;
        dedupe_degradations(&mut ws.degradations);
        ws
    }
}

fn allowance_of(base: &[(DomainId, usize); 3], domain: DomainId) -> usize {
    base.iter()
        .find(|(d, _)| *d == domain)
        .map_or(0, |(_, a)| *a)
}

fn goal_from_inputs(inputs: &AssembleInputs) -> Option<String> {
    match &inputs.input {
        Some(envelope) => match &envelope.payload {
            crate::dag::NodeInputPayload::Goal(goal) => Some(sanitize_goal(&goal.text)),
            crate::dag::NodeInputPayload::FromPredecessors { .. } => None,
        },
        None => None,
    }
}

fn predecessor_output_ids(inputs: &AssembleInputs) -> Vec<ArtifactId> {
    match &inputs.input {
        Some(envelope) => match &envelope.payload {
            crate::dag::NodeInputPayload::FromPredecessors { preds } => {
                preds.iter().map(|p| p.output_ref).collect()
            }
            crate::dag::NodeInputPayload::Goal(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

fn seed_set_digest(
    pinned_nodes: &[GraphNode],
    seed_paths: &BTreeSet<String>,
    radius: u8,
) -> Digest {
    let mut hasher = crate::types::ids::DigestHasher::new();
    for node in pinned_nodes {
        hasher.update(node.path.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\x01");
    for path in seed_paths {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&[radius]);
    hasher.finish()
}

fn lru_victim(memo: &Memo) -> Option<MemoKey> {
    memo.entries
        .iter()
        .min_by_key(|(_, e)| (e.last_used, e.summary))
        .map(|(k, _)| k.clone())
}

fn dedupe_degradations(degradations: &mut Vec<Degradation>) {
    degradations
        .sort_by(|a, b| (a.domain, a.reason, &a.detail).cmp(&(b.domain, b.reason, &b.detail)));
    degradations.dedup();
}

// ---------------------------------------------------------------------
// Section builders (§5.3) and estimates
// ---------------------------------------------------------------------

impl DefaultContextEngine {
    fn section_est(&self, section: &Section) -> usize {
        self.est(&section.render())
    }

    fn goal_section(&self, body: String) -> Section {
        Section {
            domain_label: DomainId::Conversation.label(),
            kind: "goal",
            key: String::new(),
            body,
            fidelity: None,
            citations: vec![SectionCitation {
                source: "alloy://conversation/goal".into(),
                bytes: None,
            }],
        }
    }

    fn goal_section_min_est(&self, goal: &str) -> usize {
        let mut body = bound_bytes(goal, GOAL_MIN_BYTES);
        if body.len() < goal.len() {
            let kept = body.split('\n').count().saturating_sub(1);
            let total = goal.split('\n').count();
            body.push('\n');
            body.push_str(&truncated_marker(kept, total));
        }
        self.section_est(&self.goal_section(body))
    }

    fn history_section(&self, lines: &[&EventLine], omitted: usize) -> Section {
        let first = lines.first().map_or(0, |l| l.seq.0);
        let last = lines.last().map_or(0, |l| l.seq.0);
        let mut body = lines
            .iter()
            .map(|l| format!("#{} {}", l.seq.0, l.line))
            .collect::<Vec<_>>()
            .join("\n");
        if omitted > 0 {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&omitted_marker(omitted, "events"));
        }
        Section {
            domain_label: DomainId::Conversation.label(),
            kind: "history",
            key: String::new(),
            body,
            fidelity: None,
            citations: vec![SectionCitation {
                source: format!("alloy://conversation/events/{first}-{last}"),
                bytes: None,
            }],
        }
    }

    fn file_body(cand: &FileCandidate, fr: &FileRender) -> String {
        let start = fr.start_line as usize;
        let end = fr.end_line as usize;
        let mut out = String::new();
        for (offset, line) in cand
            .lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .map(|(i, l)| (i + 1, l))
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{offset:>4} | {line}"));
        }
        if fr.truncated {
            let kept = end.saturating_sub(start) + 1;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&truncated_marker(kept, fr.total_lines));
        }
        out
    }

    fn file_key_and_source(cand: &FileCandidate, fr: &FileRender) -> (String, String) {
        let whole = fr.start_line == 1 && (fr.end_line as usize) >= fr.total_lines && !fr.truncated;
        if whole {
            (
                cand.path.clone(),
                format!("alloy://working_set/file/{}", cand.path),
            )
        } else {
            let key = format!("{}#L{}-L{}", cand.path, fr.start_line, fr.end_line);
            (key.clone(), format!("alloy://working_set/file/{key}"))
        }
    }

    fn file_section(cand: &FileCandidate, fr: &FileRender) -> Section {
        let body = Self::file_body(cand, fr);
        let (key, source) = Self::file_key_and_source(cand, fr);
        Section {
            domain_label: DomainId::WorkingSet.label(),
            kind: "file",
            key,
            body,
            fidelity: None,
            citations: vec![SectionCitation {
                source,
                bytes: None,
            }],
        }
    }

    fn node_line(node: &GraphNode) -> String {
        let kind = node.kind.as_str();
        let path = sanitize_line(&node.path);
        match &node.file {
            Some(file) => format!("{kind}  {path}  {}", sanitize_line(file)),
            None => format!("{kind}  {path}"),
        }
    }

    fn graph_section(&self, projection: &GraphProjection, state: &WsState) -> Option<Section> {
        let merged: Vec<&GraphNode> = projection
            .seeds
            .iter()
            .chain(projection.neighbourhood.iter())
            .collect();
        let mut lines: Vec<String> = Vec::new();
        let mut citations: Vec<SectionCitation> = Vec::new();
        let mut kept_ids: BTreeSet<crate::types::ids::GraphNodeId> = BTreeSet::new();
        for (i, node) in merged.iter().enumerate() {
            if !state.nodes_kept.get(i).copied().unwrap_or(false) {
                continue;
            }
            kept_ids.insert(node.id);
            let line = Self::node_line(node);
            citations.push(SectionCitation {
                source: format!(
                    "alloy://working_set/graph/{}/{}",
                    projection.version.0,
                    sanitize_line(&node.path)
                ),
                bytes: Some(line.clone()),
            });
            lines.push(line);
        }
        if lines.is_empty() {
            return None;
        }
        let path_of: BTreeMap<_, _> = merged.iter().map(|n| (n.id, n.path.as_str())).collect();
        for (i, edge) in projection.edges.iter().enumerate() {
            if !state.edges_kept.get(i).copied().unwrap_or(false) {
                continue;
            }
            if !kept_ids.contains(&edge.from) || !kept_ids.contains(&edge.to) {
                continue;
            }
            let (Some(from), Some(to)) = (path_of.get(&edge.from), path_of.get(&edge.to)) else {
                continue;
            };
            lines.push(format!(
                "{} {} -> {}",
                edge.kind.as_str(),
                sanitize_line(from),
                sanitize_line(to)
            ));
        }
        if projection.truncated {
            lines.push(graph_truncated_marker().to_owned());
        }
        if state.graph_omitted > 0 {
            lines.push(omitted_marker(state.graph_omitted, "graph items"));
        }
        let key = projection
            .seeds
            .first()
            .map(|n| sanitize_line(&n.path))
            .unwrap_or_default();
        Some(Section {
            domain_label: DomainId::WorkingSet.label(),
            kind: "graph",
            key: bound_bytes(&key, 120),
            body: lines.join("\n"),
            fidelity: Some(projection.fidelity),
            citations,
        })
    }

    fn diag_section(&self, d: &DiagnosticEvent) -> Section {
        let code = d
            .code
            .as_deref()
            .map(sanitize_line)
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "_".into());
        Section {
            domain_label: DomainId::WorkingSet.label(),
            kind: "diagnostics",
            key: code.clone(),
            body: render_diagnostic(d, &self.workspace_root),
            fidelity: None,
            citations: vec![SectionCitation {
                source: format!("alloy://working_set/diagnostics/{code}/{}", d.id),
                bytes: None,
            }],
        }
    }

    fn artifact_section(c: &ArtifactCandidate, with_body: bool) -> Section {
        let mut body = render_meta_line(c);
        if with_body {
            if let Some(text) = &c.body {
                body.push('\n');
                body.push_str(text);
            }
        }
        Section {
            domain_label: DomainId::Artifacts.label(),
            kind: "artifact",
            key: c.id.to_string(),
            body,
            fidelity: None,
            citations: vec![SectionCitation {
                source: format!("alloy://artifacts/{}", c.id),
                bytes: None,
            }],
        }
    }

    fn addendum_section(kind: &'static str, key: &str) -> Section {
        Section {
            domain_label: "must_include",
            kind,
            key: key.to_owned(),
            body: "(pinned above)".into(),
            fidelity: None,
            citations: vec![SectionCitation {
                source: format!("alloy://must_include/{kind}/{key}"),
                bytes: None,
            }],
        }
    }

    fn addendum_est(&self, kind: &'static str, key: &str) -> usize {
        self.section_est(&Self::addendum_section(kind, key))
    }

    fn pin_est(&self, pin: &ResolvedPin) -> usize {
        match pin {
            ResolvedPin::File(cand) => {
                let fr = pin_file_render(cand);
                self.section_est(&Self::file_section(cand, &fr))
            }
            ResolvedPin::Artifact(c) => self.section_est(&Self::artifact_section(c, true)),
            ResolvedPin::Diagnostic(d) => self.section_est(&self.diag_section(d)),
            ResolvedPin::Symbol(nodes) => {
                let lines: Vec<String> = nodes.iter().map(Self::node_line).collect();
                // Charged as if the graph fence stood alone (conservative).
                self.est(&lines.join("\n")) + 24
            }
        }
    }

    fn projection_est(&self, projection: &GraphProjection) -> usize {
        let lines: Vec<String> = projection
            .seeds
            .iter()
            .chain(projection.neighbourhood.iter())
            .map(Self::node_line)
            .collect();
        self.est(&lines.join("\n"))
    }
}

/// Render window for a pinned file (B11: exactly the pinned range, or the
/// whole file, never truncated).
fn pin_file_render(cand: &FileCandidate) -> FileRender {
    let total = cand.lines.len();
    let (start, end) = match cand.pin_range {
        Some((s, e)) => (s, e.min(total as u32).max(s)),
        None => (1, total.max(1) as u32),
    };
    FileRender {
        idx: usize::MAX,
        start_line: start,
        end_line: end,
        truncated: false,
        total_lines: total,
    }
}

// ---------------------------------------------------------------------
// Clamping (B6, B9)
// ---------------------------------------------------------------------

impl DefaultContextEngine {
    fn clamp_conversation(
        &self,
        conv: &ConversationRaw,
        goal: Option<&str>,
        allowance: usize,
    ) -> ConvState {
        let mut state = ConvState::default();
        if let Some(goal) = goal {
            let full = self.goal_section(goal.to_owned());
            let full_est = self.section_est(&full);
            if full_est <= allowance {
                state.goal_full = Some(goal.to_owned());
                state.goal_kept_bytes = goal.len();
                state.used_est += full_est;
            } else {
                // Largest byte prefix that fits, floored at 2 000 bytes (D3).
                let mut lo = GOAL_MIN_BYTES.min(goal.len());
                let mut hi = goal.len();
                while lo < hi {
                    let mid = (lo + hi).div_ceil(2);
                    let body = truncated_goal_body(goal, mid);
                    if self.section_est(&self.goal_section(body)) <= allowance {
                        lo = mid;
                    } else {
                        hi = mid - 1;
                    }
                }
                let body = truncated_goal_body(goal, lo);
                let est = self.section_est(&self.goal_section(body));
                let kept = bound_bytes(goal, lo);
                state.goal_full = Some(goal.to_owned());
                state.goal_kept_bytes = kept.len();
                state.goal_truncated = kept.len() < goal.len();
                state.used_est += est;
            }
        }
        // History: newest-first admission (D13), oldest-first rendering.
        let remaining = allowance.saturating_sub(state.used_est);
        let overhead = 32; // history fence lines + a possible omitted marker
        let mut used = overhead;
        let mut admitted = 0usize;
        for line in conv.events.iter().rev() {
            let cost = self.est(&format!("#{} {}\n", line.seq.0, line.line));
            if used + cost > remaining {
                break;
            }
            used += cost;
            admitted += 1;
        }
        state.admitted = admitted;
        state.omitted = conv.events.len() - admitted;
        if admitted > 0 {
            state.used_est += used;
        }
        state
    }

    fn clamp_working_set(&self, ws: &WsRaw, allowance: usize) -> WsState {
        let mut state = WsState::default();
        let mut remaining = allowance;

        // Files (D8 order). Pinned files are pre-charged (B11).
        for (idx, cand) in ws.files.iter().enumerate() {
            let total = cand.lines.len();
            if cand.pinned {
                let mut fr = pin_file_render(cand);
                fr.idx = idx;
                state.files.push(fr);
                continue;
            }
            let max_lines = self.profile.max_file_lines.max(1) as usize;
            let (win_start, win_end) = window_around(cand.centre_line, total, max_lines);
            let mut fr = FileRender {
                idx,
                start_line: win_start as u32,
                end_line: win_end as u32,
                truncated: win_end - win_start + 1 < total,
                total_lines: total,
            };
            let est = self.section_est(&Self::file_section(cand, &fr));
            if est <= remaining {
                remaining -= est;
                state.files.push(fr);
                continue;
            }
            // B9: truncate at a line boundary; drop whole if even one line
            // plus the marker cannot fit.
            let mut lo = 0usize;
            let mut hi = win_end - win_start + 1;
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                let probe = FileRender {
                    idx,
                    start_line: win_start as u32,
                    end_line: (win_start + mid - 1) as u32,
                    truncated: true,
                    total_lines: total,
                };
                if self.section_est(&Self::file_section(cand, &probe)) <= remaining {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if lo == 0 {
                state.files_omitted += 1;
                continue;
            }
            fr.end_line = (win_start + lo - 1) as u32;
            fr.truncated = true;
            let est = self.section_est(&Self::file_section(cand, &fr));
            remaining = remaining.saturating_sub(est);
            state.files.push(fr);
        }

        // Graph: node lines in order, then edges (D10 result, Q8 order).
        if let Some(projection) = &ws.graph {
            let merged: Vec<&GraphNode> = projection
                .seeds
                .iter()
                .chain(projection.neighbourhood.iter())
                .collect();
            let overhead = 40; // graph fence lines with the fidelity label
            let mut used = overhead;
            state.nodes_kept = vec![false; merged.len()];
            state.edges_kept = vec![false; projection.edges.len()];
            for (i, node) in merged.iter().enumerate() {
                let pinned = ws.pinned_node_ids.contains(&node.id);
                let cost = self.est(&Self::node_line(node)) + 1;
                if pinned {
                    state.nodes_kept[i] = true;
                    continue; // pre-charged (B11)
                }
                if used + cost <= remaining {
                    used += cost;
                    state.nodes_kept[i] = true;
                } else {
                    state.graph_omitted += 1;
                }
            }
            for (i, edge) in projection.edges.iter().enumerate() {
                let cost = self.est(&format!("{} x -> y\n", edge.kind.as_str())) + 8;
                if used + cost <= remaining {
                    used += cost;
                    state.edges_kept[i] = true;
                } else {
                    state.graph_omitted += 1;
                }
            }
            if state.nodes_kept.iter().any(|&k| k) {
                remaining = remaining.saturating_sub(used);
            }
        }

        // Diagnostics (D11 order). Pinned are pre-charged.
        state.diags_kept = vec![false; ws.diagnostics.len()];
        for (i, (d, pinned)) in ws.diagnostics.iter().enumerate() {
            if *pinned {
                state.diags_kept[i] = true;
                continue;
            }
            let est = self.section_est(&self.diag_section(d));
            if est <= remaining {
                remaining -= est;
                state.diags_kept[i] = true;
            } else {
                state.diags_omitted += 1;
            }
        }
        state.used_est = allowance - remaining.min(allowance);
        state
    }

    fn clamp_artifacts(&self, candidates: &[ArtifactCandidate], allowance: usize) -> ArtState {
        let mut state = ArtState {
            kept: vec![(false, false); candidates.len()],
            ..ArtState::default()
        };
        let mut remaining = allowance;
        for (i, c) in candidates.iter().enumerate() {
            if c.pinned {
                state.kept[i] = (true, true); // pre-charged (B11)
                continue;
            }
            let meta_est = self.section_est(&Self::artifact_section(c, false));
            if meta_est > remaining {
                state.omitted += 1;
                continue;
            }
            // Bodies only for `Patch` kind, and only when they fit (§4.4).
            let with_body = matches!(c.meta.kind, crate::storage::ArtifactKind::Patch)
                && c.body.is_some()
                && self.section_est(&Self::artifact_section(c, true)) <= remaining;
            let est = self.section_est(&Self::artifact_section(c, with_body));
            remaining -= est;
            state.kept[i] = (true, with_body);
        }
        state.used_est = allowance - remaining;
        state
    }

    /// One backstop drop (B10): ascending domain weight, ties broken by
    /// reverse `DomainId::LIVE` order; within a domain the exact reverse of
    /// the D5 inclusion order. Returns `false` when nothing droppable is
    /// left.
    fn backstop_drop_one(
        &self,
        conv: &mut ConvState,
        ws_raw: &WsRaw,
        ws: &mut WsState,
        art: &mut ArtState,
        art_candidates: &[ArtifactCandidate],
    ) -> bool {
        let mut order: Vec<(f32, usize, DomainId)> = DomainId::LIVE
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    self.profile.weights.weight_of(*d),
                    DomainId::LIVE.len() - i,
                    *d,
                )
            })
            .collect();
        order.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        for (_, _, domain) in order {
            let dropped = match domain {
                DomainId::Conversation => {
                    // Inclusion is newest-first; its reverse drops the
                    // oldest admitted line.
                    if conv.admitted > 0 {
                        conv.admitted -= 1;
                        conv.omitted += 1;
                        true
                    } else {
                        false
                    }
                }
                DomainId::Artifacts => {
                    let victim = art
                        .kept
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(i, (kept, _))| *kept && !art_candidates[*i].pinned)
                        .map(|(i, _)| i);
                    if let Some(i) = victim {
                        art.kept[i] = (false, false);
                        art.omitted += 1;
                        true
                    } else {
                        false
                    }
                }
                DomainId::WorkingSet => Self::drop_one_working_set(ws_raw, ws),
                _ => false,
            };
            if dropped {
                return true;
            }
        }
        false
    }

    /// B10 within the WorkingSet: reverse inclusion order — diagnostics,
    /// then graph edges, then graph nodes, then files. Pinned items (B11)
    /// are never droppable.
    fn drop_one_working_set(ws_raw: &WsRaw, ws: &mut WsState) -> bool {
        let diag_victim = ws
            .diags_kept
            .iter()
            .enumerate()
            .rev()
            .find(|(i, &kept)| kept && !ws_raw.diagnostics.get(*i).is_some_and(|(_, p)| *p))
            .map(|(i, _)| i);
        if let Some(i) = diag_victim {
            ws.diags_kept[i] = false;
            ws.diags_omitted += 1;
            return true;
        }
        if let Some(i) = ws.edges_kept.iter().rposition(|&k| k) {
            ws.edges_kept[i] = false;
            ws.graph_omitted += 1;
            return true;
        }
        let merged_pinned = |i: usize| {
            ws_raw.graph.as_ref().is_some_and(|p| {
                p.seeds
                    .iter()
                    .chain(p.neighbourhood.iter())
                    .nth(i)
                    .is_some_and(|n| ws_raw.pinned_node_ids.contains(&n.id))
            })
        };
        let node_victim = ws
            .nodes_kept
            .iter()
            .enumerate()
            .rev()
            .find(|(i, &kept)| kept && !merged_pinned(*i))
            .map(|(i, _)| i);
        if let Some(i) = node_victim {
            ws.nodes_kept[i] = false;
            ws.graph_omitted += 1;
            return true;
        }
        let file_victim = ws
            .files
            .iter()
            .enumerate()
            .rev()
            .find(|(_, fr)| !ws_raw.files.get(fr.idx).is_some_and(|c| c.pinned))
            .map(|(i, _)| i);
        if let Some(i) = file_victim {
            ws.files.remove(i);
            ws.files_omitted += 1;
            return true;
        }
        false
    }
}

/// Truncated goal body with its marker (D3, B7).
fn truncated_goal_body(goal: &str, bytes: usize) -> String {
    let kept = bound_bytes(goal, bytes);
    let kept_lines = kept.split('\n').count().saturating_sub(1).max(1);
    let total_lines = goal.split('\n').count();
    format!("{kept}\n{}", truncated_marker(kept_lines, total_lines))
}

/// Retained window centred on `centre` (§4.3a), 1-based inclusive.
fn window_around(centre: Option<u32>, total: usize, max_lines: usize) -> (usize, usize) {
    if total == 0 {
        return (1, 1);
    }
    if total <= max_lines {
        return (1, total);
    }
    match centre {
        Some(c) => {
            let c = (c as usize).clamp(1, total);
            let half = max_lines / 2;
            let start = c.saturating_sub(half).max(1);
            let end = (start + max_lines - 1).min(total);
            let start = end.saturating_sub(max_lines - 1).max(1);
            (start, end)
        }
        None => (1, max_lines),
    }
}

// ---------------------------------------------------------------------
// Rendering (§5.2 step 7, A2) and the manifest (§7.3)
// ---------------------------------------------------------------------

/// Per-domain manifest counters, accumulated during rendering (A11).
#[derive(Debug, Clone, Copy, Default)]
struct DomainStats {
    items: usize,
    tokens_est: usize,
    truncated: usize,
    omitted: usize,
}

#[derive(Debug)]
struct RenderedPack {
    messages: Vec<ChatMessage>,
    citations: Vec<Citation>,
    stats: [DomainStats; 3],
    degradations: Vec<Degradation>,
    total_est: usize,
    graph_version: GraphVersion,
    graph_fidelity: Option<crate::graph::GraphFidelity>,
    graph_queried: u64,
    graph_degraded: bool,
    omitted_total: usize,
    truncated_total: usize,
}

impl DefaultContextEngine {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn render_all(
        &self,
        req: &AssembleRequest,
        conv: &ConversationRaw,
        conv_state: &ConvState,
        ws: &WsRaw,
        ws_state: &WsState,
        art_candidates: &[ArtifactCandidate],
        art_state: &ArtState,
        art_degradations: &[Degradation],
        pins: &ResolvedPins,
        art_extra_omitted: usize,
    ) -> RenderedPack {
        // Degradations: fetch-time plus budget exhaustion (B6).
        let mut degradations: Vec<Degradation> = Vec::new();
        degradations.extend(conv.degradations.iter().cloned());
        degradations.extend(ws.degradations.iter().cloned());
        degradations.extend(art_degradations.iter().cloned());
        let budget_omitted = [
            (DomainId::Conversation, conv_state.omitted),
            (
                DomainId::WorkingSet,
                ws_state.files_omitted + ws_state.graph_omitted + ws_state.diags_omitted,
            ),
            (DomainId::Artifacts, art_state.omitted),
        ];
        for (domain, n) in budget_omitted {
            if n > 0 {
                degradations.push(Degradation {
                    domain,
                    reason: DegradationReason::BudgetExhausted,
                    detail: String::new(),
                });
            }
        }
        dedupe_degradations(&mut degradations);

        // Distinct degradation reasons per domain, for the E3 marker in the
        // domain's first rendered section.
        let mut marker_lines: BTreeMap<DomainId, Vec<String>> = BTreeMap::new();
        for d in &degradations {
            let lines = marker_lines.entry(d.domain).or_default();
            let marker = degraded_marker(d.domain, d.reason);
            if !lines.contains(&marker) {
                lines.push(marker);
            }
        }
        let append_markers = |section: &mut Section, lines: Option<&Vec<String>>| {
            if let Some(lines) = lines {
                for line in lines {
                    section.body.push('\n');
                    section.body.push_str(line);
                }
            }
        };

        // Conversation sections (A2: goal, then history).
        let mut conv_sections: Vec<Section> = Vec::new();
        if let Some(goal) = &conv_state.goal_full {
            let body = if conv_state.goal_truncated {
                truncated_goal_body(goal, conv_state.goal_kept_bytes)
            } else {
                goal.clone()
            };
            conv_sections.push(self.goal_section(body));
        }
        let mut history_lines: Vec<&EventLine> = Vec::new();
        if conv_state.admitted > 0 {
            let start = conv.events.len() - conv_state.admitted;
            history_lines = conv.events[start..].iter().collect();
        }
        if !history_lines.is_empty() {
            conv_sections.push(self.history_section(&history_lines, conv_state.omitted));
        }
        if let Some(first) = conv_sections.first_mut() {
            append_markers(first, marker_lines.get(&DomainId::Conversation));
        }

        // WorkingSet sections (A2: files, graph, diagnostics).
        let mut file_sections: Vec<Section> = Vec::new();
        for fr in &ws_state.files {
            let cand = &ws.files[fr.idx];
            file_sections.push(Self::file_section(cand, fr));
        }
        if ws_state.files_omitted > 0 {
            if let Some(last) = file_sections.last_mut() {
                last.body.push('\n');
                last.body
                    .push_str(&omitted_marker(ws_state.files_omitted, "files"));
            }
        }
        let mut graph_sections: Vec<Section> = Vec::new();
        if let Some(projection) = &ws.graph {
            if let Some(section) = self.graph_section(projection, ws_state) {
                graph_sections.push(section);
            }
        }
        let mut diag_sections: Vec<Section> = Vec::new();
        for (i, (d, _)) in ws.diagnostics.iter().enumerate() {
            if ws_state.diags_kept.get(i).copied().unwrap_or(false) {
                diag_sections.push(self.diag_section(d));
            }
        }
        let omitted_diags = ws_state.diags_omitted + ws.diag_cap_omitted;
        if omitted_diags > 0 {
            if let Some(last) = diag_sections.last_mut() {
                last.body.push('\n');
                last.body
                    .push_str(&omitted_marker(omitted_diags, "diagnostics"));
            }
        }
        if let Some(first) = file_sections
            .first_mut()
            .or(graph_sections.first_mut())
            .or(diag_sections.first_mut())
        {
            append_markers(first, marker_lines.get(&DomainId::WorkingSet));
        }

        // Artifacts sections.
        let mut art_sections: Vec<Section> = Vec::new();
        for (i, c) in art_candidates.iter().enumerate() {
            let (kept, with_body) = art_state.kept.get(i).copied().unwrap_or((false, false));
            if kept {
                art_sections.push(Self::artifact_section(c, with_body));
            }
        }
        let art_omitted = art_state.omitted + art_extra_omitted;
        if art_omitted > 0 {
            if let Some(last) = art_sections.last_mut() {
                last.body.push('\n');
                last.body
                    .push_str(&omitted_marker(art_omitted, "artifacts"));
            }
        }
        if let Some(first) = art_sections.first_mut() {
            append_markers(first, marker_lines.get(&DomainId::Artifacts));
        }

        // MustInclude addendum (A10): one section per pin, request order.
        let addendum_sections: Vec<Section> = pins
            .pins
            .iter()
            .map(|(kind, key, _)| Self::addendum_section(kind, key))
            .collect();

        // Messages in A2 order; empty sections are never emitted.
        let system = system_frame(req.capability.as_str());
        let mut messages = vec![ChatMessage {
            role: ChatRole::System,
            content: system,
        }];
        let mut citations: Vec<Citation> = Vec::new();
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut push_group = |messages: &mut Vec<ChatMessage>,
                              citations: &mut Vec<Citation>,
                              sections: &[Section]| {
            if sections.is_empty() {
                return 0usize;
            }
            let texts: Vec<String> = sections.iter().map(Section::render).collect();
            for section in sections {
                for citation in section.resolved_citations() {
                    let key = (
                        citation.source.clone(),
                        citation
                            .digest
                            .as_ref()
                            .map(|d| d.as_hex().to_owned())
                            .unwrap_or_default(),
                    );
                    // CIT5: no duplicate (source, digest) pair.
                    if seen.insert(key) {
                        citations.push(citation);
                    }
                }
            }
            let msg = super::render::user_message(&texts);
            let est = self.est(&msg.content);
            messages.push(msg);
            est
        };

        let mut conv_tokens = 0usize;
        for section in &conv_sections {
            // Goal and history are separate messages (§4.2).
            conv_tokens += push_group(&mut messages, &mut citations, std::slice::from_ref(section));
        }
        let mut ws_tokens = 0usize;
        ws_tokens += push_group(&mut messages, &mut citations, &file_sections);
        ws_tokens += push_group(&mut messages, &mut citations, &graph_sections);
        ws_tokens += push_group(&mut messages, &mut citations, &diag_sections);
        let art_tokens = push_group(&mut messages, &mut citations, &art_sections);
        push_group(&mut messages, &mut citations, &addendum_sections);

        let total_est: usize = messages.iter().map(|m| self.est(&m.content)).sum();

        // Counters (A11): items, truncated, omitted per live domain.
        let kept_nodes = ws_state.nodes_kept.iter().filter(|&&k| k).count();
        let kept_edges = ws_state.edges_kept.iter().filter(|&&k| k).count();
        let kept_diags = ws_state.diags_kept.iter().filter(|&&k| k).count();
        let graph_truncated = ws
            .graph
            .as_ref()
            .is_some_and(|p| p.truncated && !graph_sections.is_empty());
        let stats = [
            DomainStats {
                items: usize::from(conv_state.goal_full.is_some()) + conv_state.admitted,
                tokens_est: conv_tokens,
                truncated: usize::from(conv_state.goal_truncated),
                omitted: conv_state.omitted + conv.skipped_malformed,
            },
            DomainStats {
                items: ws_state.files.len() + kept_nodes + kept_edges + kept_diags,
                tokens_est: ws_tokens,
                truncated: ws_state.files.iter().filter(|f| f.truncated).count()
                    + usize::from(graph_truncated),
                omitted: ws_state.files_omitted
                    + ws.file_cap_omitted
                    + ws_state.graph_omitted
                    + ws_state.diags_omitted
                    + ws.diag_cap_omitted,
            },
            DomainStats {
                items: art_state.kept.iter().filter(|(k, _)| *k).count(),
                tokens_est: art_tokens,
                truncated: 0,
                omitted: art_state.omitted + art_extra_omitted,
            },
        ];
        let graph_degraded = degradations.iter().any(|d| {
            matches!(
                d.reason,
                DegradationReason::GraphDisabled
                    | DegradationReason::GraphBusy
                    | DegradationReason::GraphUnavailable
                    | DegradationReason::GraphEmpty
            )
        });

        RenderedPack {
            messages,
            citations,
            stats,
            degradations,
            total_est,
            graph_version: ws.graph.as_ref().map_or(GraphVersion(0), |p| p.version),
            graph_fidelity: ws.graph.as_ref().map(|p| p.fidelity),
            graph_queried: ws.queried_repr + pins.queried,
            graph_degraded,
            omitted_total: stats.iter().map(|s| s.omitted).sum(),
            truncated_total: stats.iter().map(|s| s.truncated).sum(),
        }
    }

    /// The `domains` manifest (§7.3), written last from the render counters
    /// (A11). Lists all eight domains (CIT8).
    fn manifest(
        &self,
        effective: usize,
        rendered: &RenderedPack,
        _ws: &WsRaw,
    ) -> serde_json::Value {
        let mut domains = serde_json::Map::new();
        for domain in DomainId::ALL {
            let entry = if domain.is_live() {
                let stats = rendered.stats[DomainId::LIVE
                    .iter()
                    .position(|d| *d == domain)
                    .unwrap_or(0)];
                serde_json::json!({
                    "live": true,
                    "items": stats.items,
                    "tokens_est": stats.tokens_est,
                    "truncated": stats.truncated,
                    "omitted": stats.omitted,
                })
            } else {
                serde_json::json!({ "live": false, "items": 0 })
            };
            domains.insert(domain.label().to_owned(), entry);
        }
        let degradations: Vec<serde_json::Value> = rendered
            .degradations
            .iter()
            .map(|d| {
                serde_json::json!({
                    "domain": d.domain.label(),
                    "reason": d.reason.label(),
                    "detail": d.detail,
                })
            })
            .collect();
        serde_json::json!({
            "format_version": CONTEXT_FORMAT_VERSION,
            "engine": "alloy-runtime::context/DefaultContextEngine",
            "estimator": self.estimator.id(),
            "budget": {
                "effective_est": effective,
                "used_est": rendered.total_est,
                "reserve_est": SYSTEM_FRAME_RESERVE_EST,
            },
            "graph": {
                "version": rendered.graph_version.0,
                "fidelity": rendered
                    .graph_fidelity
                    .map_or("manifest", super::render::fidelity_tag),
                "queried": rendered.graph_queried,
                "degraded": rendered.graph_degraded,
            },
            "domains": domains,
            "degradations": degradations,
        })
    }

    /// Convert the clamped WorkingSet into the public payload (§3.4).
    fn working_set_payload(&self, ws: &WsRaw, state: &WsState) -> WorkingSet {
        let mut files = Vec::new();
        for fr in &state.files {
            let cand = &ws.files[fr.idx];
            let text = Self::file_body(cand, fr);
            files.push(FileExcerpt {
                path: cand.path.clone(),
                start_line: fr.start_line,
                digest: Digest::sha256(text.as_bytes()),
                text,
                truncated: fr.truncated,
                crate_id: cand.crate_id.clone(),
            });
        }
        let diagnostics = ws
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(i, _)| state.diags_kept.get(*i).copied().unwrap_or(false))
            .map(|(_, (d, _))| d.clone())
            .collect();
        WorkingSet {
            files,
            graph: ws.graph.clone(),
            diagnostics,
            degradations: ws.degradations.clone(),
        }
    }
}

// ---------------------------------------------------------------------
// ContextEngine impl (§3.3) — compact Stub (A12), evict (§8.3),
// mark_stale (§8.2)
// ---------------------------------------------------------------------

#[async_trait]
impl ContextEngine for DefaultContextEngine {
    async fn assemble(&self, req: AssembleRequest) -> Result<PromptPack, ContextError> {
        DefaultContextEngine::assemble_with(self, req, AssembleInputs::default()).await
    }

    // Trait-object entry for RFC-0013 workers: delegates to the inherent
    // implementation so `Arc<dyn ContextEngine>` callers get the real
    // host-input-aware assembly, not the ignore-inputs trait default.
    async fn assemble_with(
        &self,
        req: AssembleRequest,
        inputs: AssembleInputs,
    ) -> Result<PromptPack, ContextError> {
        DefaultContextEngine::assemble_with(self, req, inputs).await
    }

    async fn compact(
        &self,
        domain: DomainId,
        _strategy: CompactStrategy,
    ) -> Result<(), ContextError> {
        if !domain.is_live() {
            return Err(ContextError::DomainNotLive(domain));
        }
        // Stub (A12): drop the memoized projection, summarise nothing. Only
        // the WorkingSet holds a memo; the other live domains are re-read
        // every call already.
        if domain == DomainId::WorkingSet {
            let mut memo = self
                .memo
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let n = memo.entries.len() as u64;
            memo.entries.clear();
            self.metrics.cache_evictions.fetch_add(n, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn evict(&self, policy: EvictPolicy) -> Result<EvictReport, ContextError> {
        let _span = tracing::info_span!("context.evict").entered();
        let mut memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let victims: Vec<MemoKey> = match policy {
            EvictPolicy::All => memo.entries.keys().cloned().collect(),
            EvictPolicy::StaleGraphVersion { current } => memo
                .entries
                .keys()
                .filter(|k| k.version != current)
                .cloned()
                .collect(),
            EvictPolicy::Session(session) => memo
                .entries
                .keys()
                .filter(|k| k.session == session)
                .cloned()
                .collect(),
            EvictPolicy::Lru { keep } => {
                // Deterministic (K4): ascending (last_used_seq, SummaryId),
                // never a wall clock.
                let mut by_age: Vec<(u64, SummaryId, MemoKey)> = memo
                    .entries
                    .iter()
                    .map(|(k, e)| (e.last_used, e.summary, k.clone()))
                    .collect();
                by_age.sort_by_key(|(used, summary, _)| (*used, *summary));
                let evict_n = memo.entries.len().saturating_sub(keep);
                by_age
                    .into_iter()
                    .take(evict_n)
                    .map(|(_, _, k)| k)
                    .collect()
            }
        };
        let mut freed = 0u64;
        let mut evicted = 0u32;
        for key in victims {
            if let Some(entry) = memo.entries.remove(&key) {
                freed += entry.est;
                evicted += 1;
            }
        }
        self.metrics
            .cache_evictions
            .fetch_add(u64::from(evicted), Ordering::Relaxed);
        Ok(EvictReport {
            evicted,
            retained: memo.entries.len() as u32,
            freed_tokens_est: freed,
        })
    }

    async fn mark_stale(
        &self,
        summary_id: SummaryId,
        reason: StaleReason,
    ) -> Result<(), ContextError> {
        let mut memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = memo
            .entries
            .iter()
            .find(|(_, e)| e.summary == summary_id)
            .map(|(k, _)| k.clone());
        match key {
            Some(key) => {
                memo.entries.remove(&key);
                self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(summary = %summary_id, ?reason, "projection marked stale");
                Ok(())
            }
            // K6: never silently succeed on a miss.
            None => Err(ContextError::SummaryNotFound(summary_id)),
        }
    }
}
