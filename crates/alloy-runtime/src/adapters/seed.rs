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
use crate::types::diagnostic::DiagnosticLevel;

/// What one seeding pass found and recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReport {
    /// Diagnostics ingested into the graph.
    pub recorded: usize,
    /// How many of those were error-level — the driver's retry signal.
    pub errors: usize,
}

/// Run `verifier` once and ingest every parsed diagnostic into `graph`.
///
/// Graph write failures degrade to a warning (ingest is best-effort —
/// IN1/IX7 spirit); a verifier error propagates so the caller can decide
/// (the CLI warns and continues — a missing toolchain must never fail a
/// run before it starts).
pub async fn seed_graph_diagnostics(
    verifier: &dyn Verifier,
    graph: &dyn ProjectGraph,
    ctx: &NodeExecContext,
) -> Result<SeedReport, AdapterError> {
    let verdict = verifier.verify(ctx).await?;
    // This pass is a full check of the current workspace: its output
    // supersedes everything previously recorded. Clearing is best-effort —
    // stale extras degrade prompt quality, not correctness.
    if let Err(e) = graph.clear_diagnostics().await {
        tracing::warn!(error = %e, "clearing superseded diagnostics failed; continuing");
    }
    let mut report = SeedReport {
        recorded: 0,
        errors: 0,
    };
    for diagnostic in verdict.diagnostics {
        let is_error = diagnostic.level == DiagnosticLevel::Error;
        match graph.record_diagnostic(diagnostic).await {
            Ok(()) => {
                report.recorded += 1;
                if is_error {
                    report.errors += 1;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "diagnostic seed ingest failed; continuing");
            }
        }
    }
    Ok(report)
}
