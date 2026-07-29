//! `alloy events` (RFC-0015 §7.2) — durable-log reads, read-depth assembly
//! only (CR11).
//!
//! Author: arkadianet

use std::sync::Arc;
use std::time::Duration;

use alloy_runtime::{
    clamp_events_page_limit, list_decision_events, EventSeq, SessionEvent, SessionEventType,
    MAX_EVENTS_PAGE,
};

use crate::args::EventsArgs;
use crate::assembly;
use crate::errx::{CliError, Exit};
use crate::outfmt;
use crate::resolve::Ctx;

pub async fn exec(ctx: Ctx, args: EventsArgs) -> Result<Exit, CliError> {
    let mut base = assembly::assemble_read(ctx.cfg.clone()).await?;
    base.rt.start().await?;

    let session = match args.session {
        Some(id) => id,
        None => assembly::read_last_session(&base.cfg.data_dir).ok_or_else(|| {
            CliError::new(
                Exit::Usage,
                "no session recorded for this workspace yet; pass --session <id> (a prior `alloy run` records the most recent one)",
            )
        })?,
    };

    // SQ6 — larger limits are clamped and reported, not rejected.
    let limit = clamp_events_page_limit(args.limit);
    if limit != args.limit && !ctx.quiet {
        eprintln!(
            "--limit {} clamped to {limit} (max {MAX_EVENTS_PAGE})",
            args.limit
        );
    }

    let sessions = base.plane.sessions();
    let mut after = args.after.map(EventSeq);
    let mut poll_delay = Duration::from_millis(250);
    let mut emitted = 0usize;

    'outer: loop {
        let (page, next_cursor): (Vec<SessionEvent>, Option<EventSeq>) = if args.decisions_only {
            // SQ4 — empty events with Some(next_after) means "keep paging".
            let store = base.storage.events();
            let page = list_decision_events(store.as_ref(), session, after, limit).await?;
            (page.events, page.next_after)
        } else {
            let events = sessions.events(session, after, limit).await?;
            let cursor = events.last().map(|e| e.seq);
            (events, cursor)
        };

        let mut saw_terminal = false;
        for ev in &page {
            if let Some(run) = args.run {
                if ev.run_id != Some(run) {
                    continue;
                }
            }
            if ev.type_ == SessionEventType::RunCompleted
                && (args.run.is_none() || ev.run_id == args.run)
            {
                saw_terminal = true;
            }
            if ctx.json {
                println!("{}", outfmt::event_json(ev));
            } else {
                println!("{}", outfmt::event_line(ev));
            }
            emitted += 1;
        }

        if let Some(next) = next_cursor {
            after = Some(next);
            if args.decisions_only && page.is_empty() {
                continue; // keep paging through the scan window (SQ4).
            }
        }

        if !args.follow {
            if !args.decisions_only || next_cursor.is_none() || emitted >= limit {
                break 'outer;
            }
            continue;
        }
        if saw_terminal {
            break 'outer; // SQ7 — stop on RunCompleted for the followed run.
        }
        if page.is_empty() {
            tokio::time::sleep(poll_delay).await;
            poll_delay = (poll_delay * 2).min(Duration::from_secs(2));
        } else {
            poll_delay = Duration::from_millis(250);
        }
    }

    // The resume cursor, on stderr (OUT1: stdout is results only).
    if let Some(after) = after {
        if !ctx.quiet {
            eprintln!("resume with --after {}", after.0);
        }
    }

    let storage = Arc::clone(&base.storage);
    assembly::shutdown_all(base.rt, &storage, None, Duration::from_secs(5)).await?;
    Ok(Exit::Ok)
}
