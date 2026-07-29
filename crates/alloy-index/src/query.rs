//! Query engine (RFC-0011 §7) and diagnostic/fix ingest (§6.7).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use alloy_runtime::graph::{FixEvent, GraphEdge, GraphError, GraphNode, GraphQuery, GraphView};
use alloy_runtime::types::diagnostic::DiagnosticEvent;
use alloy_runtime::types::ids::{
    ArtifactId, CrateId, DiagnosticId, Digest, GraphNodeId, Timestamp, TransactionId,
};
use rusqlite::Connection;

use crate::db::from_rusqlite;
use crate::ingest::{parse_edge_kind, parse_kind};
use crate::layout::IngestLimits;
use crate::store::{now_rfc3339, read_version};

/// Q7's radius clamp.
const MAX_SUBGRAPH_RADIUS: u8 = 3;

/// Encode a serde value that serializes to a JSON string (timestamps,
/// unit-enum levels) into its bare string form, failing loudly on any other
/// shape instead of trimming quotes off arbitrary JSON.
fn json_string<T: serde::Serialize>(value: &T, what: &str) -> Result<String, GraphError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(other) => Err(GraphError::Internal(format!(
            "encode {what}: expected a JSON string, got {other}"
        ))),
        Err(e) => Err(GraphError::Internal(format!("encode {what}: {e}"))),
    }
}

/// Run one read query (Q1–Q11). Never writes (Q10).
pub(crate) fn run(
    conn: &Connection,
    q: &GraphQuery,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    // Q11: version read in the same transaction as the rows.
    let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
    let version = read_version(&tx)?;
    // RS4/A-0014-4: fidelity is computed from graph_meta.model_version by
    // the one seam function, for every query kind including the Stubs.
    let fidelity =
        crate::migrate::fidelity_for_model_version(crate::migrate::read_model_version(&tx)?);
    let mut view = match q {
        GraphQuery::Symbol { path } => symbol(&tx, path, version, limits)?,
        GraphQuery::Diagnostics { crate_id, since } => {
            diagnostics(&tx, crate_id.as_ref(), since.as_ref(), version, limits)?
        }
        GraphQuery::Subgraph { seeds, radius } => subgraph(&tx, seeds, *radius, version, limits)?,
        // Q6 as amended by A-0011-5: the recorded fixes are read back.
        GraphQuery::SimilarFixes {
            diagnostic_code,
            limit,
        } => similar_fixes(&tx, diagnostic_code, *limit, version, limits)?,
        // Q4/Q5 Stubs: empty views, truncated, never an error.
        GraphQuery::Refs { .. } | GraphQuery::Impls { .. } | GraphQuery::Callers { .. } => {
            let mut view = GraphView::empty(version);
            view.truncated = true;
            view
        }
    };
    view.fidelity = fidelity;
    // Read-only: the transaction rolls back on drop having written nothing.
    drop(tx);
    Ok(view)
}

fn node_from_row(row: &rusqlite::Row<'_>) -> Result<GraphNode, GraphError> {
    let id: String = row.get(0).map_err(from_rusqlite)?;
    let kind: String = row.get(1).map_err(from_rusqlite)?;
    let path: String = row.get(2).map_err(from_rusqlite)?;
    let crate_id: Option<String> = row.get(3).map_err(from_rusqlite)?;
    let file: Option<String> = row.get(4).map_err(from_rusqlite)?;
    let digest: Option<String> = row.get(5).map_err(from_rusqlite)?;
    Ok(GraphNode {
        id: GraphNodeId::parse(&id)
            .map_err(|e| GraphError::Corrupt(format!("node id {id:?}: {e}")))?,
        kind: parse_kind(&kind)?,
        path,
        crate_id: crate_id
            .map(|c| CrateId::new(c).map_err(|e| GraphError::Corrupt(format!("crate_id: {e}"))))
            .transpose()?,
        file,
        digest: digest
            .map(|d| {
                Digest::try_from_hex(d)
                    .map_err(|e| GraphError::Corrupt(format!("node digest: {e}")))
            })
            .transpose()?,
    })
}

const NODE_COLS: &str = "id, kind, path, crate_id, file, digest";

/// Q2: exact Rust-path match, else exact file-path match, else empty.
fn symbol(
    conn: &Connection,
    path: &str,
    version: alloy_runtime::GraphVersion,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    let mut nodes = query_nodes(
        conn,
        &format!("SELECT {NODE_COLS} FROM graph_nodes WHERE path = ?1"),
        [path],
    )?;
    if nodes.is_empty() && (path.contains('/') || path.ends_with(".rs")) {
        nodes = query_nodes(
            conn,
            &format!(
                "SELECT {NODE_COLS} FROM graph_nodes
                  WHERE id = (SELECT module_id FROM graph_files WHERE path = ?1)"
            ),
            [path],
        )?;
    }
    finish_view(nodes, Vec::new(), version, limits)
}

fn query_nodes<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<GraphNode>, GraphError> {
    let mut stmt = conn.prepare(sql).map_err(from_rusqlite)?;
    let mut rows = stmt.query(params).map_err(from_rusqlite)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(from_rusqlite)? {
        out.push(node_from_row(row)?);
    }
    Ok(out)
}

/// Q3: filtered, deterministically ordered diagnostics.
fn diagnostics(
    conn: &Connection,
    crate_id: Option<&CrateId>,
    since: Option<&Timestamp>,
    version: alloy_runtime::GraphVersion,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    let since_str = since.map(|t| json_string(t, "since")).transpose()?;
    let mut sql = String::from("SELECT event_json FROM graph_diagnostics WHERE 1 = 1");
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(c) = crate_id {
        sql.push_str(" AND package = ?1");
        params.push(Box::new(c.as_str().to_string()));
    }
    if let Some(s) = since_str {
        sql.push_str(if params.is_empty() {
            " AND recorded_at >= ?1"
        } else {
            " AND recorded_at >= ?2"
        });
        params.push(Box::new(s));
    }
    sql.push_str(" ORDER BY recorded_at, diagnostic_id");
    // Q9: fetch one over the cap to detect truncation.
    let cap = limits.max_query_nodes as usize;
    sql.push_str(&format!(" LIMIT {}", cap + 1));

    let mut stmt = conn.prepare(&sql).map_err(from_rusqlite)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(param_refs))
        .map_err(from_rusqlite)?;
    let mut out: Vec<DiagnosticEvent> = Vec::new();
    while let Some(row) = rows.next().map_err(from_rusqlite)? {
        let json: String = row.get(0).map_err(from_rusqlite)?;
        out.push(
            serde_json::from_str(&json)
                .map_err(|e| GraphError::Corrupt(format!("diagnostic event_json: {e}")))?,
        );
    }
    let truncated = out.len() > cap;
    out.truncate(cap);
    let mut view = GraphView::empty(version);
    view.diagnostics = out;
    view.truncated = truncated;
    Ok(view)
}

/// Q6 (amendment A-0011-5): fixes recorded for `code`, most recent first,
/// capped by the query's `limit` and by the store's own query cap. Rows are
/// returned verbatim — the reader decides what, if anything, to show a
/// model.
fn similar_fixes(
    conn: &Connection,
    code: &str,
    limit: usize,
    version: alloy_runtime::GraphVersion,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    // Q9: the caller's limit never exceeds the store's own cap, and one row
    // over the cap is fetched to detect truncation.
    let cap = limit.min(limits.max_query_nodes as usize);
    let mut stmt = conn
        .prepare(
            "SELECT diagnostic_id, diagnostic_code, crate_id, transaction_id, patch_artifact,
                    verified, recorded_at
               FROM graph_fixes
              WHERE diagnostic_code = ?1
              ORDER BY recorded_at DESC, rowid DESC
              LIMIT ?2",
        )
        .map_err(from_rusqlite)?;
    let over = i64::try_from(cap.saturating_add(1)).unwrap_or(i64::MAX);
    let mut rows = stmt
        .query(rusqlite::params![code, over])
        .map_err(from_rusqlite)?;
    let mut out: Vec<FixEvent> = Vec::new();
    while let Some(row) = rows.next().map_err(from_rusqlite)? {
        out.push(fix_from_row(row)?);
    }
    let truncated = out.len() > cap;
    out.truncate(cap);
    let mut view = GraphView::empty(version);
    view.fixes = out;
    view.truncated = truncated;
    Ok(view)
}

/// Decode one `graph_fixes` row back into its [`FixEvent`].
fn fix_from_row(row: &rusqlite::Row<'_>) -> Result<FixEvent, GraphError> {
    let diagnostic: Option<String> = row.get(0).map_err(from_rusqlite)?;
    let diagnostic_code: Option<String> = row.get(1).map_err(from_rusqlite)?;
    let crate_id: Option<String> = row.get(2).map_err(from_rusqlite)?;
    let transaction: Option<String> = row.get(3).map_err(from_rusqlite)?;
    let patch_artifact: Option<String> = row.get(4).map_err(from_rusqlite)?;
    let verified: i64 = row.get(5).map_err(from_rusqlite)?;
    let recorded_at: String = row.get(6).map_err(from_rusqlite)?;
    Ok(FixEvent {
        diagnostic: diagnostic
            .map(|d| {
                DiagnosticId::parse(&d).map_err(|e| GraphError::Corrupt(format!("fix {d:?}: {e}")))
            })
            .transpose()?,
        diagnostic_code,
        crate_id: crate_id
            .map(|c| CrateId::new(c).map_err(|e| GraphError::Corrupt(format!("fix crate_id: {e}"))))
            .transpose()?,
        transaction: transaction
            .map(|t| {
                TransactionId::parse(&t)
                    .map_err(|e| GraphError::Corrupt(format!("fix transaction {t:?}: {e}")))
            })
            .transpose()?,
        patch_artifact: patch_artifact
            .map(|a| {
                ArtifactId::parse(&a)
                    .map_err(|e| GraphError::Corrupt(format!("fix artifact {a:?}: {e}")))
            })
            .transpose()?,
        verified: verified != 0,
        recorded_at: serde_json::from_value(serde_json::Value::String(recorded_at))
            .map_err(|e| GraphError::Corrupt(format!("fix recorded_at: {e}")))?,
    })
}

/// Q7: BFS over `Defines` **and** `Imports` edges in both directions,
/// radius clamped. Imports participate since the RFC-0014 deep pass — the
/// Appendix B projection reaches an imported node across one hop.
fn subgraph(
    conn: &Connection,
    seeds: &[GraphNodeId],
    radius: u8,
    version: alloy_runtime::GraphVersion,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    let radius = radius.min(MAX_SUBGRAPH_RADIUS);

    // Load the (small) adjacency once, ordered for deterministic BFS (Q8).
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut edge_rows: Vec<(String, String, String)> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT from_id, to_id, kind FROM graph_edges ORDER BY from_id, to_id, kind")
            .map_err(from_rusqlite)?;
        let mut rows = stmt.query([]).map_err(from_rusqlite)?;
        while let Some(row) = rows.next().map_err(from_rusqlite)? {
            let from: String = row.get(0).map_err(from_rusqlite)?;
            let to: String = row.get(1).map_err(from_rusqlite)?;
            let kind: String = row.get(2).map_err(from_rusqlite)?;
            adjacency.entry(from.clone()).or_default().push(to.clone());
            adjacency.entry(to.clone()).or_default().push(from.clone());
            edge_rows.push((from, to, kind));
        }
    }

    // Unknown seeds are ignored (Q7). The visited set is capped at
    // `max_query_nodes + 1`: one over the cap is enough to detect Q9
    // truncation, and it bounds both memory and the SQL `IN` placeholder
    // count well under SQLite's variable limit. BFS order over sorted
    // adjacency is deterministic, so the capped set is too (Q8).
    let visit_cap = limits.max_query_nodes as usize + 1;
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut frontier: VecDeque<(String, u8)> = VecDeque::new();
    for seed in seeds {
        if visited.len() >= visit_cap {
            break;
        }
        let id = seed.to_string();
        let known: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )
            .map_err(from_rusqlite)?;
        if known > 0 && visited.insert(id.clone()) {
            frontier.push_back((id, 0));
        }
    }
    'bfs: while let Some((id, depth)) = frontier.pop_front() {
        if depth >= radius {
            continue;
        }
        if let Some(neighbours) = adjacency.get(&id) {
            for n in neighbours {
                if visited.len() >= visit_cap {
                    break 'bfs;
                }
                if visited.insert(n.clone()) {
                    frontier.push_back((n.clone(), depth + 1));
                }
            }
        }
    }

    if visited.is_empty() {
        return Ok(GraphView::empty(version));
    }
    let placeholders = std::iter::repeat_n("?", visited.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {NODE_COLS} FROM graph_nodes WHERE id IN ({placeholders})");
    let ids: Vec<String> = visited.iter().cloned().collect();
    let nodes = query_nodes(conn, &sql, rusqlite::params_from_iter(ids.iter()))?;

    // Edges with both endpoints in the view (post-truncation, in finish_view).
    let edges = edge_rows
        .into_iter()
        .filter(|(f, t, _)| visited.contains(f) && visited.contains(t))
        .map(|(f, t, k)| -> Result<GraphEdge, GraphError> {
            Ok(GraphEdge {
                from: GraphNodeId::parse(&f)
                    .map_err(|e| GraphError::Corrupt(format!("edge from {f:?}: {e}")))?,
                to: GraphNodeId::parse(&t)
                    .map_err(|e| GraphError::Corrupt(format!("edge to {t:?}: {e}")))?,
                kind: parse_edge_kind(&k)?,
                confidence: 1.0,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    finish_view(nodes, edges, version, limits)
}

/// Q8 ordering + Q9 truncation, shared by every node-returning query.
fn finish_view(
    mut nodes: Vec<GraphNode>,
    mut edges: Vec<GraphEdge>,
    version: alloy_runtime::GraphVersion,
    limits: &IngestLimits,
) -> Result<GraphView, GraphError> {
    nodes.sort_by(|a, b| (a.kind, &a.path, a.id).cmp(&(b.kind, &b.path, b.id)));
    let cap = limits.max_query_nodes as usize;
    let truncated = nodes.len() > cap;
    nodes.truncate(cap);
    let kept: BTreeSet<GraphNodeId> = nodes.iter().map(|n| n.id).collect();
    edges.retain(|e| kept.contains(&e.from) && kept.contains(&e.to));
    edges.sort_by_key(|e| (e.from, e.to, e.kind));
    let mut view = GraphView::empty(version);
    view.nodes = nodes;
    view.edges = edges;
    view.truncated = truncated;
    Ok(view)
}

/// §6.7 / IN13: idempotent diagnostic upsert keyed by `DiagnosticEvent.id`.
pub(crate) fn record_diagnostic(
    conn: &mut Connection,
    d: &DiagnosticEvent,
    workspace_root: Option<&Path>,
) -> Result<(), GraphError> {
    let level = json_string(&d.level, "level")?;
    let primary_path = d
        .spans
        .first()
        .and_then(|s| relativise(&s.path, workspace_root));
    let json = serde_json::to_string(d)
        .map_err(|e| GraphError::Internal(format!("encode diagnostic: {e}")))?;
    let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
    tx.execute(
        "INSERT INTO graph_diagnostics
           (diagnostic_id, code, level, package, fingerprint, primary_path, message,
            event_json, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(diagnostic_id) DO UPDATE SET
           code = excluded.code, level = excluded.level, package = excluded.package,
           fingerprint = excluded.fingerprint, primary_path = excluded.primary_path,
           message = excluded.message, event_json = excluded.event_json",
        rusqlite::params![
            d.id.to_string(),
            d.code,
            level,
            d.package,
            d.fingerprint.as_hex(),
            primary_path,
            d.message,
            json,
            now_rfc3339()?,
        ],
    )
    .map_err(from_rusqlite)?;
    tx.commit().map_err(from_rusqlite)?;
    Ok(())
}

/// G12: workspace-relativise a span path; an absolute path outside the
/// workspace stores `None` rather than a host path (SEC6).
fn relativise(path: &str, workspace_root: Option<&Path>) -> Option<String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Some(path.replace('\\', "/"));
    }
    let root = workspace_root?;
    let rel = p.strip_prefix(root).ok()?;
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    Some(out)
}

/// §6.7 / IN14: append-only fix record.
pub(crate) fn record_fix(conn: &mut Connection, f: &FixEvent) -> Result<(), GraphError> {
    let recorded_at = json_string(&f.recorded_at, "recorded_at")?;
    let tx = conn.unchecked_transaction().map_err(from_rusqlite)?;
    tx.execute(
        "INSERT INTO graph_fixes
           (fix_id, diagnostic_id, diagnostic_code, crate_id, transaction_id, patch_artifact,
            verified, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            uuid::Uuid::new_v4().to_string(),
            f.diagnostic.map(|d| d.to_string()),
            f.diagnostic_code,
            f.crate_id.as_ref().map(|c| c.as_str().to_string()),
            f.transaction.map(|t| t.to_string()),
            f.patch_artifact.map(|a| a.to_string()),
            i64::from(f.verified),
            recorded_at,
        ],
    )
    .map_err(from_rusqlite)?;
    tx.commit().map_err(from_rusqlite)?;
    Ok(())
}
