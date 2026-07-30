//! WorkingSet domain builder: files + graph projection + diagnostics
//! (RFC-0012 §4.3). Each sub-part independently degrades to empty (E2).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::graph::{
    GraphEdge, GraphEdgeKind, GraphError, GraphNode, GraphNodeKind, GraphQuery, GraphView,
    GraphViewHandle,
};
use crate::types::diagnostic::{DiagnosticEvent, DiagnosticLevel};
use crate::types::ids::CrateId;

use super::render::{bound_bytes, is_safe_rel_path, relativize, sanitize_line, sanitize_untrusted};
use super::types::{
    Degradation, DegradationReason, DomainId, GraphProjection, ImpactEntry, ImpactRelation,
};

// ---------------------------------------------------------------------
// Files (§4.3a)
// ---------------------------------------------------------------------

/// One candidate file, read and sanitised but not yet clamped.
#[derive(Debug, Clone)]
pub(super) struct FileCandidate {
    /// Workspace-relative path with `/` separators.
    pub path: String,
    /// Pinned via `must_include` (B11): never dropped or truncated.
    pub pinned: bool,
    /// Pin line range (1-based inclusive), `None` for the whole file.
    pub pin_range: Option<(u32, u32)>,
    /// Listed in `AssembleInputs.focus_paths` (D8).
    pub is_focus: bool,
    /// A diagnostic primary span points into this file (D8).
    pub has_diagnostic: bool,
    /// Centre line for the retained window, from the diagnostic span.
    pub centre_line: Option<u32>,
    /// Sanitised lines of the whole file.
    pub lines: Vec<String>,
    /// Owning package when a graph seed named it.
    pub crate_id: Option<CrateId>,
}

/// Read one workspace file with the SEC9 posture. `Err` is the degradation
/// reason — never an assembly error (E1).
pub(super) async fn read_workspace_file(
    workspace_root: &Path,
    rel_path: &str,
) -> Result<Vec<String>, DegradationReason> {
    if !is_safe_rel_path(rel_path) {
        return Err(DegradationReason::FileUnreadable);
    }
    let joined = workspace_root.join(rel_path);
    // Symlink containment: the resolved file must stay under the resolved
    // root (SEC9).
    let canon_root = tokio::fs::canonicalize(workspace_root)
        .await
        .map_err(|_| DegradationReason::FileUnreadable)?;
    let canon = tokio::fs::canonicalize(&joined)
        .await
        .map_err(|_| DegradationReason::FileUnreadable)?;
    if !canon.starts_with(&canon_root) {
        return Err(DegradationReason::FileUnreadable);
    }
    let bytes = tokio::fs::read(&canon)
        .await
        .map_err(|_| DegradationReason::FileUnreadable)?;
    // Binary guard (D7): NUL in the first 8 KiB, or invalid UTF-8.
    if bytes.iter().take(8 * 1024).any(|&b| b == 0) {
        return Err(DegradationReason::NotTextual);
    }
    let text = String::from_utf8(bytes).map_err(|_| DegradationReason::NotTextual)?;
    let mut lines: Vec<String> = sanitize_untrusted(&text)
        .split('\n')
        .map(str::to_owned)
        .collect();
    // A trailing newline is not an extra (empty) line.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    Ok(lines)
}

/// Rule D8 comparison key: `(is_focus DESC, has_diagnostic DESC, path ASC)`.
/// Pinned files sort first, in pin order, and are handled by the caller.
pub(super) fn order_files(files: &mut [FileCandidate]) {
    files.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.is_focus.cmp(&a.is_focus))
            .then(b.has_diagnostic.cmp(&a.has_diagnostic))
            .then(a.path.cmp(&b.path))
    });
}

// ---------------------------------------------------------------------
// Graph projection (§4.3b)
// ---------------------------------------------------------------------

/// Outcome of the graph fetch: a projection or degradations, never an
/// error (E1, E2).
#[derive(Debug, Default)]
pub(super) struct GraphFetch {
    /// The projection, `None` when unavailable or empty.
    pub projection: Option<GraphProjection>,
    /// Degradations recorded while querying.
    pub degradations: Vec<Degradation>,
    /// Queries issued (manifest `graph.queried`).
    pub queried: u64,
    /// Queries that degraded (metrics).
    pub degraded_queries: u64,
}

/// Map a `GraphError` to its degradation reason (rule E2).
pub(super) fn map_graph_error(e: &GraphError) -> DegradationReason {
    match e {
        GraphError::Disabled => DegradationReason::GraphDisabled,
        GraphError::Busy => DegradationReason::GraphBusy,
        _ => DegradationReason::GraphUnavailable,
    }
}

/// Issue one query, retrying `Busy` exactly once with no sleep (rule E4).
pub(super) async fn query_once_retry_busy(
    handle: &GraphViewHandle,
    query: GraphQuery,
) -> Result<GraphView, GraphError> {
    use tracing::Instrument;
    async {
        match handle.query(query.clone()).await {
            Err(GraphError::Busy) => handle.query(query).await,
            other => other,
        }
    }
    .instrument(tracing::info_span!("context.graph_query"))
    .await
}

/// Build the graph slice: `Symbol` per seed path, one `Subgraph` for all
/// seeds (D10), then bounded `Callers`/`Refs` impact reads over the item
/// anchors derived from the seeds (A-0012-1a). Only these query kinds plus
/// `Diagnostics` exist in this module (D14 as amended).
pub(super) async fn fetch_graph(
    handle: &GraphViewHandle,
    pinned_seed_nodes: &[GraphNode],
    seed_paths: &BTreeSet<String>,
    radius: u8,
    max_impact_seeds: usize,
    max_impact_nodes: usize,
) -> GraphFetch {
    let mut fetch = GraphFetch::default();
    let mut seeds: Vec<GraphNode> = pinned_seed_nodes.to_vec();
    let mut graph_dead = false;

    for path in seed_paths {
        fetch.queried += 1;
        match query_once_retry_busy(handle, GraphQuery::Symbol { path: path.clone() }).await {
            Ok(view) => seeds.extend(view.nodes),
            Err(e) => {
                fetch.degraded_queries += 1;
                push_degradation(&mut fetch.degradations, map_graph_error(&e), &e.to_string());
                graph_dead = true;
                break;
            }
        }
    }

    // Deduplicate and sort seeds (D9 / RFC-0011 Q8 order).
    seeds.sort_by(|a, b| (a.kind, &a.path, a.id).cmp(&(b.kind, &b.path, b.id)));
    seeds.dedup_by(|a, b| a.id == b.id);

    if !graph_dead && !seeds.is_empty() {
        fetch.queried += 1;
        let seed_ids = seeds.iter().map(|n| n.id).collect();
        match query_once_retry_busy(
            handle,
            GraphQuery::Subgraph {
                seeds: seed_ids,
                radius,
            },
        )
        .await
        {
            Ok(view) => {
                let seed_id_set: BTreeSet<_> = seeds.iter().map(|n| n.id).collect();
                // A-0012-1: bounded impact reads after the neighbourhood,
                // anchored on **item** nodes. A file-path seed resolves to
                // its module node (`alloy-index` Q2 resolves file paths
                // through `graph_files.module_id`), but the store anchors
                // `Calls`/`References` edges exclusively on item nodes, so
                // each module seed is first expanded to the items it
                // `Defines` in the D10 subgraph view.
                let anchors = impact_anchors(&seeds, &view.nodes, &view.edges, max_impact_seeds);
                let neighbourhood: Vec<GraphNode> = view
                    .nodes
                    .into_iter()
                    .filter(|n| !seed_id_set.contains(&n.id))
                    .collect();
                let impact = fetch_impact(handle, &mut fetch, &anchors, max_impact_nodes).await;
                fetch.projection = Some(GraphProjection {
                    version: view.version,
                    fidelity: view.fidelity,
                    seeds: seeds.clone(),
                    neighbourhood,
                    edges: view.edges,
                    truncated: view.truncated || impact.truncated,
                    impact: impact.entries,
                    impact_omitted: impact.omitted,
                });
            }
            Err(e) => {
                fetch.degraded_queries += 1;
                push_degradation(&mut fetch.degradations, map_graph_error(&e), &e.to_string());
                graph_dead = true;
            }
        }
    }

    if fetch.projection.is_none() && !graph_dead {
        // Queries succeeded but returned nothing: the normal M7 state
        // (RFC-0011 Q10). Null handles are indistinguishable from an
        // unbuilt graph, so they land here too.
        push_degradation(&mut fetch.degradations, DegradationReason::GraphEmpty, "");
    }

    fetch
}

/// Outcome of the bounded impact fetch (A-0012-1).
#[derive(Debug, Default)]
struct ImpactFetch {
    /// D5-ordered, deduplicated, capped entries.
    entries: Vec<ImpactEntry>,
    /// Entries dropped by the `max_impact_nodes` cap (B7/B8).
    omitted: usize,
    /// A non-empty impact view was capped by the index (RFC-0011 Q9).
    truncated: bool,
}

/// Derive the impact anchors (A-0012-1a): item seeds anchor themselves;
/// module seeds expand to the **item** nodes they `Defines` in the D10
/// subgraph view, in seed order then view order. At most
/// `max_impact_seeds` anchors, so the A13 query bound is unchanged.
///
/// This is the shape the `alloy-index` store dictates: `Symbol` on a file
/// path resolves to the file's module node (`query.rs::symbol`,
/// `graph_files.module_id`), while `Calls`/`References` edges anchor only
/// on item nodes (`lang/rust/pass.rs`), and `Callers`/`Refs` answer with
/// `to_id = anchor` lookups (`query.rs::neighbours`) — so a module-anchored
/// impact query can only ever return empty.
fn impact_anchors(
    seeds: &[GraphNode],
    view_nodes: &[GraphNode],
    view_edges: &[GraphEdge],
    max_impact_seeds: usize,
) -> Vec<GraphNode> {
    let by_id: BTreeMap<_, _> = view_nodes.iter().map(|n| (n.id, n)).collect();
    let mut seen = BTreeSet::new();
    let mut anchors: Vec<GraphNode> = Vec::new();
    'seeds: for seed in seeds {
        let children = view_edges
            .iter()
            .filter(|e| e.kind == GraphEdgeKind::Defines && e.from == seed.id)
            .filter_map(|e| by_id.get(&e.to).copied())
            .filter(|n| n.kind == GraphNodeKind::Item);
        let own = std::iter::once(seed).filter(|s| s.kind == GraphNodeKind::Item);
        for anchor in own.chain(children) {
            if anchors.len() == max_impact_seeds {
                break 'seeds;
            }
            if seen.insert(anchor.id) {
                anchors.push(anchor.clone());
            }
        }
    }
    anchors
}

/// Issue at most 2 `Callers`/`Refs` queries per anchor — `≤ 2 ×
/// max_impact_seeds` in total, since [`impact_anchors`] caps the anchor
/// list (A-0012-1a). An error records a degradation and stops the impact
/// fetch — never the projection (E2). Empty views are honest absence: no
/// entry, no degradation, no marker (A-0012-1c).
async fn fetch_impact(
    handle: &GraphViewHandle,
    fetch: &mut GraphFetch,
    anchors: &[GraphNode],
    max_impact_nodes: usize,
) -> ImpactFetch {
    let mut out = ImpactFetch::default();
    if max_impact_nodes == 0 {
        return out;
    }
    'seeds: for anchor in anchors {
        for (relation, query) in [
            (
                ImpactRelation::Caller,
                GraphQuery::Callers { fn_node: anchor.id },
            ),
            (
                ImpactRelation::Reference,
                GraphQuery::Refs { node: anchor.id },
            ),
        ] {
            fetch.queried += 1;
            match query_once_retry_busy(handle, query).await {
                Ok(view) => {
                    if !view.nodes.is_empty() && view.truncated {
                        out.truncated = true;
                    }
                    for node in view.nodes {
                        if node.id == anchor.id {
                            continue; // a self-row is noise, not impact
                        }
                        out.entries.push(ImpactEntry {
                            seed_path: anchor.path.clone(),
                            relation,
                            node,
                        });
                    }
                }
                Err(e) => {
                    fetch.degraded_queries += 1;
                    push_degradation(
                        &mut fetch.degradations,
                        map_graph_error(&e),
                        &format!("impact query failed: {e}"),
                    );
                    break 'seeds;
                }
            }
        }
    }
    // D5 total order, then dedup and the cap (A-0012-1b).
    out.entries.sort_by(|a, b| {
        let key = |e: &ImpactEntry| {
            (
                e.seed_path.clone(),
                e.relation,
                e.node.kind,
                e.node.path.clone(),
                e.node.id,
            )
        };
        key(a).cmp(&key(b))
    });
    out.entries.dedup_by(|a, b| {
        a.seed_path == b.seed_path && a.relation == b.relation && a.node.id == b.node.id
    });
    out.omitted = out.entries.len().saturating_sub(max_impact_nodes);
    out.entries.truncate(max_impact_nodes);
    out
}

/// The `Diagnostics` fallback query, issued **only** when the caller
/// supplied no diagnostics (§4.3c) — the recorded log is a fallback rather
/// than a duplicate. Returns `(diagnostics, degradations, queried,
/// degraded)`.
pub(super) async fn fetch_diagnostics_fallback(
    handle: &GraphViewHandle,
) -> (Vec<DiagnosticEvent>, Vec<Degradation>, u64, u64) {
    match query_once_retry_busy(
        handle,
        GraphQuery::Diagnostics {
            crate_id: None,
            since: None,
        },
    )
    .await
    {
        Ok(view) => (view.diagnostics, Vec::new(), 1, 0),
        Err(e) => {
            let mut degradations = Vec::new();
            push_degradation(&mut degradations, map_graph_error(&e), &e.to_string());
            (Vec::new(), degradations, 1, 1)
        }
    }
}

fn push_degradation(out: &mut Vec<Degradation>, reason: DegradationReason, detail: &str) {
    out.push(Degradation {
        domain: DomainId::WorkingSet,
        reason,
        detail: bound_bytes(&sanitize_line(detail), 200),
    });
}

// ---------------------------------------------------------------------
// Diagnostics (§4.3c)
// ---------------------------------------------------------------------

fn level_rank(level: DiagnosticLevel) -> u8 {
    match level {
        DiagnosticLevel::Error => 0,
        DiagnosticLevel::Warning => 1,
        DiagnosticLevel::Note => 2,
        DiagnosticLevel::Help => 3,
    }
}

fn level_label(level: DiagnosticLevel) -> &'static str {
    match level {
        DiagnosticLevel::Error => "error",
        DiagnosticLevel::Warning => "warning",
        DiagnosticLevel::Note => "note",
        DiagnosticLevel::Help => "help",
    }
}

/// The D9 primary span path (`spans[0]`), relativised.
pub(super) fn primary_span_path(d: &DiagnosticEvent, workspace_root: &Path) -> Option<String> {
    d.spans
        .first()
        .and_then(|s| relativize(workspace_root, &s.path))
}

/// Rule D11 ordering: `(level DESC, code ASC, primary path ASC,
/// DiagnosticId ASC)`; diagnostics without spans sort after those with one.
pub(super) fn order_diagnostics(diags: &mut [DiagnosticEvent], workspace_root: &Path) {
    diags.sort_by(|a, b| {
        let key = |d: &DiagnosticEvent| {
            (
                level_rank(d.level),
                d.code.is_none(),
                d.code.clone().unwrap_or_default(),
                d.spans.is_empty(),
                primary_span_path(d, workspace_root).unwrap_or_default(),
                d.id,
            )
        };
        key(a).cmp(&key(b))
    });
}

/// Render one diagnostic as `level[code] path:line:col — message` with
/// `children` flattened to at most three lines each and `raw_json` never
/// rendered (D17, SEC10).
#[must_use]
pub(super) fn render_diagnostic(d: &DiagnosticEvent, workspace_root: &Path) -> String {
    let mut out = String::new();
    out.push_str(&diagnostic_line(d, workspace_root, false));
    for child in &d.children {
        // At most three rendered lines per child (D17); a child renders one.
        for line in diagnostic_line(child, workspace_root, true)
            .split('\n')
            .take(3)
        {
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

fn diagnostic_line(d: &DiagnosticEvent, workspace_root: &Path, child: bool) -> String {
    let level = level_label(d.level);
    let code = match &d.code {
        Some(code) => format!("[{}]", sanitize_line(code)),
        None => String::new(),
    };
    let location = d
        .spans
        .first()
        .and_then(|s| {
            relativize(workspace_root, &s.path)
                .map(|p| format!(" {}:{}:{}", p, s.start_line, s.start_col))
        })
        .unwrap_or_default();
    let message = sanitize_line(&d.message);
    if child {
        format!("  {level}{code}:{location} — {message}")
    } else {
        format!("{level}{code}{location} — {message}")
    }
}
