//! RFC-0015 §4 — command grammar (clap shapes only; no I/O here, CL9).
//!
//! Author: arkadianet

use std::path::PathBuf;

use alloy_runtime::{GateId, RunId, SessionId};
use clap::{Args, Parser, Subcommand, ValueEnum};

fn parse_session_id(s: &str) -> Result<SessionId, String> {
    SessionId::parse(s).map_err(|e| format!("malformed session id {s:?}: {e}"))
}

fn parse_run_id(s: &str) -> Result<RunId, String> {
    RunId::parse(s).map_err(|e| format!("malformed run id {s:?}: {e}"))
}

fn parse_gate_id(s: &str) -> Result<GateId, String> {
    GateId::parse(s).map_err(|e| format!("malformed gate id {s:?}: {e}"))
}

/// `alloy` — Alloy AI Engineering Runtime.
#[derive(Debug, Parser)]
#[command(name = "alloy", version, about = "Alloy AI Engineering Runtime", long_about = None)]
pub struct Cli {
    /// Global flags shared by every subcommand (CL2–CL4, CL7).
    #[command(flatten)]
    pub globals: Globals,
    /// Subcommand.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Flags every subcommand accepts (CL2, CL3, CL4, CL7).
#[derive(Debug, Args)]
pub struct Globals {
    /// Workspace root used to resolve config files and `.alloy` (CL2).
    #[arg(long, global = true, default_value = ".")]
    pub workspace: PathBuf,

    /// Catalog profile id: default | autonomous | readonly (CL4).
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// Emit machine-readable JSON on stdout (CL3).
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress progress rendering on stderr (CL7).
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Raise the tracing filter (CL7).
    #[arg(long, global = true)]
    pub verbose: bool,
}

/// Subcommands (§4.2).
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Take a goal to a compile-verified patch (or an honest failure).
    Run(RunArgs),
    /// List session events from durable storage.
    Events(EventsArgs),
    /// Resolve a human gate from any process.
    Approve(ApproveArgs),
    /// Cancel a run.
    Cancel(CancelArgs),
    /// Resume a session and re-dispatch its non-terminal run.
    Resume(ResumeArgs),
    /// Rebuild the project graph for this workspace.
    Index(IndexArgs),
    /// Start the runtime host and wait for shutdown signal (RFC-0001,
    /// preserved unchanged — CL1).
    Host,
}

/// `alloy run <GOAL>`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Natural-language goal text.
    pub goal: String,

    /// Reuse an existing session instead of creating one (SQ5).
    #[arg(long, value_parser = parse_session_id)]
    pub session: Option<SessionId>,

    /// Constraint::MaxUsd; may only lower the profile ceiling (PF11).
    #[arg(long)]
    pub max_usd: Option<f64>,

    /// Constraint::RequireCargoCheck (implied by the default profile).
    #[arg(long)]
    pub require_cargo_check: bool,

    /// Pre-approve gates non-interactively (refused by readonly, GA4/PF9).
    #[arg(long, conflicts_with = "no_input")]
    pub yes: bool,

    /// Never prompt; a gate becomes EX_GATE_REQUIRED (GA5).
    #[arg(long)]
    pub no_input: bool,

    /// Plan and print the DAG without dispatching it (CL12).
    #[arg(long)]
    pub dry_run: bool,

    /// Template override; dry-run plan inspection only (CL6/CL12).
    #[arg(long, requires = "dry_run")]
    pub template: Option<String>,

    /// Skip the graph bootstrap at session create (IX3).
    #[arg(long)]
    pub no_index: bool,
}

/// `alloy events`.
#[derive(Debug, Args)]
pub struct EventsArgs {
    /// Session id (default: the most recent session in this workspace).
    #[arg(long, value_parser = parse_session_id)]
    pub session: Option<SessionId>,

    /// Filter to one run (display filter, applied after retrieval).
    #[arg(long, value_parser = parse_run_id)]
    pub run: Option<RunId>,

    /// Exclusive cursor: return events with seq > AFTER.
    #[arg(long)]
    pub after: Option<u64>,

    /// Page size (clamped by the merged events-page limit, SQ6).
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Only Decision | ModelCall | ToolCall events (obs query helper).
    #[arg(long)]
    pub decisions_only: bool,

    /// Poll until the followed run is terminal or Ctrl-C (SQ7).
    #[arg(long)]
    pub follow: bool,
}

/// Gate decision answers (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DecisionArg {
    /// `Approval::Allow`.
    Allow,
    /// `Approval::Deny`.
    Deny,
    /// `Approval::AllowOnce`.
    AllowOnce,
}

/// `alloy approve`.
#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// Run id.
    #[arg(long, value_parser = parse_run_id)]
    pub run: RunId,

    /// Gate id.
    #[arg(long, value_parser = parse_gate_id)]
    pub gate: GateId,

    /// allow | deny | allow-once.
    #[arg(long, value_enum)]
    pub decision: DecisionArg,
}

/// `alloy cancel`.
#[derive(Debug, Args)]
pub struct CancelArgs {
    /// Run id.
    #[arg(long, value_parser = parse_run_id)]
    pub run: RunId,
}

/// `alloy resume`.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Session id.
    #[arg(long, value_parser = parse_session_id)]
    pub session: SessionId,

    /// Run id (default: the session's single non-terminal run).
    #[arg(long, value_parser = parse_run_id)]
    pub run: Option<RunId>,
}

/// `alloy index`.
#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Force a full rebuild (default: rebuild if stale).
    #[arg(long)]
    pub rebuild: bool,

    /// Print the graph metrics snapshot and exit without writing (IX8).
    #[arg(long, conflicts_with = "rebuild")]
    pub stats: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_shapes_are_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn malformed_ids_are_usage_errors_at_parse_time() {
        // CL5 — clap value_parser rejects before any handler runs.
        let err = Cli::try_parse_from(["alloy", "approve", "--run", "notauuid", "--gate", "x"])
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn yes_and_no_input_conflict_at_parse_time() {
        let err = Cli::try_parse_from(["alloy", "run", "goal", "--yes", "--no-input"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn template_requires_dry_run() {
        // CL6/CL12 — `--template` exists only behind `--dry-run`.
        let err = Cli::try_parse_from(["alloy", "run", "goal", "--template", "t"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(Cli::try_parse_from([
            "alloy",
            "run",
            "goal",
            "--dry-run",
            "--template",
            "repair_local_diagnostic"
        ])
        .is_ok());
    }
}
