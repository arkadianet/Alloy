//! Generation-1 diagnostic seeding (issue #53).
//!
//! The `repair_local_diagnostic` template runs analyze → edit → verify, so
//! before this seam the first edit happened *blind*: no cargo check had run
//! yet, and the model guessed the defect from the goal text alone. The
//! composition root now runs one verify pass up front and ingests the
//! parsed diagnostics into the project graph, where the repair worker's
//! existing `GraphQuery::Diagnostics` read picks them up (RW4).
//!
//! Author: arkadianet

use crate::adapters::{NodeExecContext, Verifier};
use crate::error::AdapterError;
use crate::graph::ProjectGraph;

/// Run `verifier` once and ingest every parsed diagnostic into `graph`.
///
/// Returns how many diagnostics were recorded. Graph write failures degrade
/// to a warning (ingest is best-effort — IN1/IX7 spirit); a verifier error
/// propagates so the caller can decide (the CLI warns and continues — a
/// missing toolchain must never fail a run before it starts).
pub async fn seed_graph_diagnostics(
    verifier: &dyn Verifier,
    graph: &dyn ProjectGraph,
    ctx: &NodeExecContext,
) -> Result<usize, AdapterError> {
    let verdict = verifier.verify(ctx).await?;
    let mut recorded = 0usize;
    for diagnostic in verdict.diagnostics {
        match graph.record_diagnostic(diagnostic).await {
            Ok(()) => recorded += 1,
            Err(e) => {
                tracing::warn!(error = %e, "diagnostic seed ingest failed; continuing");
            }
        }
    }
    Ok(recorded)
}
