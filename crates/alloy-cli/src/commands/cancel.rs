//! `alloy cancel` (RFC-0015 §7.4) — idempotent from the user's view (SQ12);
//! read-depth assembly only (CR11).
//!
//! Author: arkadianet

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::SessionRows;
use serde_json::json;

use crate::args::CancelArgs;
use crate::assembly;
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

pub async fn exec(ctx: Ctx, args: CancelArgs) -> Result<Exit, CliError> {
    let mut base = assembly::assemble_read(ctx.cfg.clone()).await?;
    base.rt.start().await?;

    // SQ12 — cancelling an already-terminal run is Ok from the plane.
    base.plane.cancel(args.run).await.map_err(|e| {
        CliError::new(
            crate::errx::exit_for_run_error(&e),
            format!("cancel run {}: {e}", args.run),
        )
    })?;

    let row = base.storage.sessions().get_run(args.run).await?;
    let (state, notice) = match &row {
        Some(row) => {
            // CR19 — the workspace-modified inference; absence proves nothing.
            let events =
                super::all_session_events(base.plane.sessions().as_ref(), row.session_id).await?;
            (
                row.state.clone(),
                super::workspace_maybe_modified(&events, args.run),
            )
        }
        None => ("unknown".to_owned(), None),
    };

    if let Some(notice) = &notice {
        eprintln!("{notice}");
    }
    if ctx.json {
        let doc = outfmt::envelope(
            "cancel",
            Exit::Ok,
            Some(&ctx.cfg),
            json!({
                "run": args.run.to_string(),
                "state": state,
                "workspace_notice": notice,
            }),
        );
        println!("{doc}");
    } else {
        println!("run {} is {state}", args.run);
    }

    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, None, Duration::from_secs(5)).await?;
    Ok(Exit::Ok)
}
