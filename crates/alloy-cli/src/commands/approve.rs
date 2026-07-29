//! `alloy approve` (RFC-0015 §7.3) — out-of-band gate resolution from a
//! second process (SQ9); read-depth assembly only (CR11).
//!
//! Author: arkadianet

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{Approval, SessionEventType, SessionRows};
use serde_json::json;

use crate::args::{ApproveArgs, DecisionArg};
use crate::assembly;
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

pub async fn exec(ctx: Ctx, args: ApproveArgs) -> Result<Exit, CliError> {
    let mut base = assembly::assemble_read(ctx.cfg.clone()).await?;
    base.rt.start().await?;

    let decision = match args.decision {
        DecisionArg::Allow => Approval::Allow,
        DecisionArg::Deny => Approval::Deny,
        // SQ11 — passed through unchanged; scope semantics are the plane's.
        DecisionArg::AllowOnce => Approval::AllowOnce,
    };

    let result = base.plane.approve(args.run, args.gate, decision).await;

    let exit = match &result {
        Ok(()) => Exit::Ok,
        Err(e) => crate::errx::exit_for_run_error(e),
    };

    // Render the resulting ApprovalResolved event when the run's session is
    // reachable from the run row.
    let mut resolved = None;
    if exit == Exit::Ok {
        if let Ok(Some(row)) = base.storage.sessions().get_run(args.run).await {
            let events =
                super::all_session_events(base.plane.sessions().as_ref(), row.session_id).await?;
            resolved = events.into_iter().rev().find(|e| {
                e.type_ == SessionEventType::ApprovalResolved
                    && e.payload.get("gate_id").and_then(|v| v.as_str())
                        == Some(args.gate.to_string().as_str())
            });
        }
    }

    if ctx.json {
        let doc = outfmt::envelope(
            "approve",
            exit,
            Some(&ctx.cfg),
            json!({
                "run": args.run.to_string(),
                "gate": args.gate.to_string(),
                "resolved": resolved.as_ref().map(outfmt::event_json),
                "error": result.as_ref().err().map(ToString::to_string),
            }),
        );
        println!("{doc}");
    } else if let Some(ev) = &resolved {
        println!("{}", outfmt::event_line(ev));
    } else if exit == Exit::Ok {
        println!("approved gate {} on run {}", args.gate, args.run);
    }

    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, None, Duration::from_secs(5)).await?;
    match result {
        Ok(()) => Ok(Exit::Ok),
        // SQ10 — not retried; the mapped exit names the failing id.
        Err(e) => Err(CliError::new(
            exit,
            format!("approve run {} gate {}: {e}", args.run, args.gate),
        )),
    }
}
