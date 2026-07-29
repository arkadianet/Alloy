//! Fix recording on the host verify path (RFC-0011 IN1/IN14, amendment
//! A-0011-5).
//!
//! RFC-0011 IN1 names exactly two permitted ingest callers: `alloy-cli` and
//! **the runtime host's verify path**. [`FixRecordingVerifier`] is that
//! path: a [`Verifier`] decorator, composed at the composition root, that
//! remembers the diagnostic codes a failing verification reported and — when
//! a later verification of the same run passes *after a new edit* — records
//! one [`FixEvent`] per code.
//!
//! Layering (SEC3/SEC4): the decorator lives in `adapters`, holds the
//! `Arc<dyn ProjectGraph>` that workers are never given, and is unreachable
//! from `Capability::execute`. The scheduler dispatches it as an ordinary
//! `Verifier` and never learns that a graph exists.
//!
//! Recording is bookkeeping, never adjudication: every graph error is
//! logged and dropped, and the inner verdict is returned unchanged.
//!
//! # Per-run state machine
//!
//! The decorator keeps at most one *pending set* per `RunId` — the codes
//! the run's **most recent** failing verification named, plus the sequence
//! of the last `EditApplied` that was already in the log at that moment.
//!
//! | verdict | effect on the run's pending set |
//! |---|---|
//! | `Fail` with codes | **replaced** by those codes (edit-at-failure refreshed) |
//! | `Fail` with no code | **cleared** — the build is broken for reasons this path cannot name, so nothing may later be claimed |
//! | `Inconclusive` | **untouched** — nothing was decided about the patch |
//! | `Pass` | **taken**; one `FixEvent` per code is recorded iff the latest applied edit is strictly newer than edit-at-failure |
//!
//! A failure never *accumulates* onto an earlier one: attributing a passing
//! verification to codes some other, older failure reported would credit a
//! patch for a fix nobody observed.
//!
//! The map is bounded by [`MAX_PENDING_RUNS`]. A run that fails and is then
//! cancelled or abandoned never returns to clear its entry, so on overflow
//! the oldest remembered failure is evicted (with a warning); the run being
//! remembered is never the one evicted.
//!
//! Author: arkadianet

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{NodeExecContext, Verdict, Verifier};
use crate::error::AdapterError;
use crate::events::SessionEventType;
use crate::graph::{FixEvent, ProjectGraph};
use crate::session::MAX_EVENTS_PAGE;
use crate::storage::EventStore;
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{
    ArtifactId, CrateId, DiagnosticId, EventSeq, RunId, SessionId, Timestamp, TransactionId,
};

/// Codes remembered from one failing verification. Bounds the pending map
/// so a pathological build cannot grow it without limit.
const MAX_PENDING_CODES: usize = 32;

/// Runs remembered at once. A run whose verify fails and which is then
/// cancelled or abandoned never returns to clear its own entry, so the map
/// is capped and the *oldest* remembered failure is evicted to make room.
/// Well above any plausible number of runs concurrently mid-repair, so an
/// eviction means something is wrong and is warned about.
pub const MAX_PENDING_RUNS: usize = 256;

/// The facts a recorded fix borrows from the edit that preceded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    /// Sequence of the `EditApplied` event, so a *new* edit can be told
    /// from the one that was already there when the failure was seen.
    pub seq: EventSeq,
    /// Transaction that applied it, when the backend created one.
    pub transaction: Option<TransactionId>,
    /// CAS id of the canonical patch, when one was stored.
    pub patch_artifact: Option<ArtifactId>,
}

/// Narrow read seam: "what was the last edit applied in this run?".
///
/// A trait rather than a bare `EventStore` so the verify path depends on
/// the one question it asks, and so tests can answer it without a store.
#[async_trait]
pub trait AppliedEditSource: Send + Sync {
    /// The most recent applied edit for `run`, or `None` when the run has
    /// not edited anything (or the log could not be read — absence is not
    /// an error here).
    async fn latest_applied_edit(&self, session: SessionId, run: RunId) -> Option<AppliedEdit>;
}

/// [`AppliedEditSource`] over the durable session event log.
pub struct EventLogEdits {
    events: Arc<dyn EventStore>,
}

impl EventLogEdits {
    /// Construct over the host's event store.
    #[must_use]
    pub fn new(events: Arc<dyn EventStore>) -> Self {
        Self { events }
    }
}

#[async_trait]
impl AppliedEditSource for EventLogEdits {
    async fn latest_applied_edit(&self, session: SessionId, run: RunId) -> Option<AppliedEdit> {
        let mut cursor: Option<EventSeq> = None;
        let mut latest: Option<AppliedEdit> = None;
        loop {
            let page = match self
                .events
                .list_session_events(session, cursor, MAX_EVENTS_PAGE)
                .await
            {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(error = %e, "edit log read failed; no fix will be recorded");
                    return latest;
                }
            };
            if page.is_empty() {
                return latest;
            }
            for ev in page {
                cursor = Some(ev.seq);
                if ev.run_id != Some(run) || ev.type_ != SessionEventType::EditApplied {
                    continue;
                }
                // RFC-0008 §9.3 payload; an unreadable one is skipped, not
                // guessed at.
                let Ok(payload) =
                    serde_json::from_value::<crate::edit::EditAppliedPayload>(ev.payload.clone())
                else {
                    continue;
                };
                latest = Some(AppliedEdit {
                    seq: ev.seq,
                    transaction: Some(payload.transaction_id),
                    patch_artifact: Some(payload.patch_artifact_id),
                });
            }
        }
    }
}

/// One diagnostic a failing verification blamed, kept until the run either
/// passes (⇒ recorded) or ends.
#[derive(Debug, Clone)]
struct PendingCode {
    diagnostic: DiagnosticId,
    code: String,
    crate_id: Option<CrateId>,
}

/// Per-run memory: what failed, and which edit was already in the log when
/// it did.
#[derive(Debug, Default)]
struct Pending {
    codes: Vec<PendingCode>,
    edit_at_failure: Option<EventSeq>,
    /// Monotonic insertion ticket, so the *oldest* remembered failure can
    /// be evicted. `RunId` is a random id, so the map's own key order says
    /// nothing about age.
    recorded_seq: u64,
}

/// [`Verifier`] decorator that closes the repair loop's write half.
pub struct FixRecordingVerifier {
    inner: Arc<dyn Verifier>,
    graph: Arc<dyn ProjectGraph>,
    edits: Arc<dyn AppliedEditSource>,
    pending: Mutex<PendingRuns>,
}

/// The pending map plus the ticket counter that orders it by age.
#[derive(Debug, Default)]
struct PendingRuns {
    by_run: BTreeMap<RunId, Pending>,
    next_seq: u64,
}

impl PendingRuns {
    /// Remember `run`'s failure, replacing whatever it said before.
    ///
    /// Eviction can only ever drop a *different*, older run: `run` is
    /// inserted after the eviction, and replacing an existing key does not
    /// grow the map, so no eviction happens on that path at all.
    fn remember(&mut self, run: RunId, codes: Vec<PendingCode>, edit_at_failure: Option<EventSeq>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if !self.by_run.contains_key(&run) && self.by_run.len() >= MAX_PENDING_RUNS {
            if let Some(oldest) = self
                .by_run
                .iter()
                .min_by_key(|(_, p)| p.recorded_seq)
                .map(|(r, _)| *r)
            {
                self.by_run.remove(&oldest);
                tracing::warn!(
                    evicted_run = %oldest,
                    cap = MAX_PENDING_RUNS,
                    "pending fix codes evicted; abandoned runs are leaking entries"
                );
            }
        }
        self.by_run.insert(
            run,
            Pending {
                codes,
                edit_at_failure,
                recorded_seq: seq,
            },
        );
    }
}

impl FixRecordingVerifier {
    /// Wrap `inner`, recording verified fixes into `graph`.
    #[must_use]
    pub fn new(
        inner: Arc<dyn Verifier>,
        graph: Arc<dyn ProjectGraph>,
        edits: Arc<dyn AppliedEditSource>,
    ) -> Self {
        Self {
            inner,
            graph,
            edits,
            pending: Mutex::new(PendingRuns::default()),
        }
    }

    /// Distinct codes from a failing verdict, in report order.
    fn pending_codes(diagnostics: &[DiagnosticEvent]) -> Vec<PendingCode> {
        let mut out: Vec<PendingCode> = Vec::new();
        for d in diagnostics {
            let Some(code) = d.code.clone() else { continue };
            if out.iter().any(|p| p.code == code) {
                continue;
            }
            out.push(PendingCode {
                diagnostic: d.id,
                code,
                crate_id: d
                    .package
                    .as_ref()
                    .and_then(|p| CrateId::new(p.as_str()).ok()),
            });
            if out.len() == MAX_PENDING_CODES {
                break;
            }
        }
        out
    }
}

#[async_trait]
impl Verifier for FixRecordingVerifier {
    async fn verify(&self, ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
        let verdict = self.inner.verify(ctx).await?;
        let run = ctx.meta.run_id;

        if !verdict.passed() {
            // Only a *failing* verdict names something to fix; an
            // inconclusive one proves nothing either way, so it is left
            // alone rather than clearing what a real failure recorded.
            if matches!(verdict.outcome, super::VerdictOutcome::Fail) {
                // A failure *replaces* the run's pending set rather than
                // adding to it: it is the current statement of what is
                // broken. When it names no code at all (a `Fail` whose
                // diagnostics carry none), the honest replacement is the
                // empty set — keeping the previous failure's codes would
                // let a later pass claim them for a patch that may have
                // fixed something else entirely.
                let codes = Self::pending_codes(&verdict.diagnostics);
                let seq = self
                    .edits
                    .latest_applied_edit(ctx.meta.session_id, run)
                    .await
                    .map(|e| e.seq);
                if let Ok(mut map) = self.pending.lock() {
                    if codes.is_empty() {
                        map.by_run.remove(&run);
                    } else {
                        map.remember(run, codes, seq);
                    }
                }
            }
            return Ok(verdict);
        }

        let Some(pending) = self
            .pending
            .lock()
            .ok()
            .and_then(|mut m| m.by_run.remove(&run))
        else {
            return Ok(verdict); // Nothing failed in this run: nothing was fixed.
        };
        let Some(edit) = self
            .edits
            .latest_applied_edit(ctx.meta.session_id, run)
            .await
        else {
            return Ok(verdict); // Passed without ever editing: not a fix.
        };
        if pending.edit_at_failure.is_some_and(|at| edit.seq <= at) {
            // No edit newer than the one already applied when the failure
            // was observed. Nothing new was tried, so nothing is claimed.
            // (`<=` rather than `==`: a log that went backwards is still
            // not evidence of a fix.)
            return Ok(verdict);
        }

        let recorded_at = Timestamp::now();
        for p in pending.codes {
            let event = FixEvent {
                diagnostic: Some(p.diagnostic),
                diagnostic_code: Some(p.code.clone()),
                crate_id: p.crate_id.clone(),
                transaction: edit.transaction,
                patch_artifact: edit.patch_artifact,
                verified: true,
                recorded_at: recorded_at.clone(),
            };
            if let Err(e) = self.graph.record_fix(event).await {
                // OB5/E1 shape: ingest is bookkeeping, never adjudication.
                tracing::warn!(error = %e, code = %p.code, "fix record dropped");
            }
        }
        Ok(verdict)
    }
}
