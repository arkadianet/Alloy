//! `alloy resume` (RFC-0015 §7.5) — full assembly (resume can dispatch),
//! merged crash-recovery, single-run selection, re-dispatch. Never replans
//! (SQ14) and never re-registers gate waiters itself (SQ13).
//!
//! Author: arkadianet

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{RunControlState, SessionRows};

use super::gate::GateMode;
use crate::args::ResumeArgs;
use crate::assembly::{self, FullAssembly};
use crate::errx::{CliError, Exit};
use crate::resolve::Ctx;

pub async fn exec(ctx: Ctx, args: ResumeArgs) -> Result<Exit, CliError> {
    let base = assembly::assemble_read(ctx.cfg.clone()).await?;
    let mut full =
        assembly::assemble_full(base, &ctx.workspace_abs, ctx.readonly(), &ctx.profile).await?;

    let cancel = full.base.handle.cancellation();
    let signal_task = crate::arm_signal_task(move || cancel.cancel());

    let result = resume_after_assembly(&mut full, &ctx, &args).await;

    signal_task.abort();
    let FullAssembly { base, graph, .. } = full;
    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, Some(&graph), Duration::from_secs(10)).await?;
    result
}

async fn resume_after_assembly(
    full: &mut FullAssembly,
    ctx: &Ctx,
    args: &ResumeArgs,
) -> Result<Exit, CliError> {
    full.base.rt.start().await?;

    // Step 2 — merged crash recovery rewrites running/waiting_approval rows
    // back to accepted and finalizes cancelling rows.
    let sessions = full.base.plane.sessions();
    sessions.resume(args.session).await?;
    assembly::write_last_session(&full.base.cfg.data_dir, args.session);

    // Step 3 — run selection; ambiguity is EX_USAGE listing candidates.
    let run = match args.run {
        Some(run) => run,
        None => {
            let rows = full.base.storage.sessions().list_runs(args.session).await?;
            let candidates: Vec<_> = rows
                .iter()
                .filter(|r| RunControlState::parse(&r.state).is_some_and(|s| !s.is_terminal()))
                .map(|r| r.id)
                .collect();
            match candidates.as_slice() {
                [one] => *one,
                [] => {
                    return Err(CliError::new(
                        Exit::State,
                        format!(
                            "session {} has no non-terminal run to resume; see `alloy events --session {}`",
                            args.session, args.session
                        ),
                    ));
                }
                many => {
                    let list = many
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(CliError::new(
                        Exit::Usage,
                        format!(
                            "session {} has multiple non-terminal runs; pass --run <id>: {list}",
                            args.session
                        ),
                    ));
                }
            }
        }
    };

    if !ctx.quiet {
        eprintln!("resuming session {}  run {run}", args.session);
    }

    // Steps 4–5 — re-dispatch and monitor exactly like `run` (steps 8–11).
    super::run::execute_and_render(
        full,
        ctx,
        "resume",
        args.session,
        run,
        GateMode::Interactive,
    )
    .await
}
