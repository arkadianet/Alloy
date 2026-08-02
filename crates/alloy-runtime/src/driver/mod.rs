//! [`GenerationDriver`] — bounded in-run repair generations (RFC-0017 §5.5).
//!
//! Implements [`RunExecutor`], so it is reached only from inside
//! `RunController::start` (AM-0003-2). It is neither a scheduler nor a
//! planner: each generation is dispatched through
//! [`crate::RuntimeHandle::run_dag_within`] (preserving `try_admit_run`
//! single-flight), topology writes go through [`PlanService::replan`], and
//! the run's lifecycle events and row writes stay single-sourced in §6.3
//! (rules RX1/RX2 — see the AC 48 CI grep).
//!
//! Admission is GN1–GN7: a seedable compile `FailureIr` within the
//! `max_repair_generations` bound, the run's budget, and one **absolute**
//! run deadline (GN7 / AM-0010-2). Day-1 the seed usually comes from a
//! `VerifyCompile` Fail; a narrow GN2 exception also admits an exhausted
//! `Edit`/`Analyze` Model failure when the current repair lineage still
//! carries that seed (AM-0017-1). When wired, GN13 restores the newest
//! edit checkpoint and re-verifies before replan so gen N+1 does not
//! repair a morph. Everything else — including exhaustion — is an
//! *outcome*, not an error (GN11).
//!
//! Declines are reported, not just counted: every §9.2 decline record
//! carries the repair-budget accounting (`bumps_used` /
//! `max_repair_generations`), and the AM-0017-1 near-miss — an
//! Edit/Analyze `Model` failure declined only because no in-run lineage
//! seed exists yet, which is every first generation — additionally records
//! `lineage: "absent"` and annotates the terminal `FailureIr` so the run
//! does not exit shaped like a repair that fought and lost (live E2
//! finding: 25/144 attempts burned the whole edit budget this way and
//! exited silently). Declines additionally carry `failure_notes` — the
//! terminal `FailureIr`'s bounded notes — because the decision row is the
//! only durable channel the E2 evidence export keeps: on dev-gn13, 7/48
//! attempts died exactly here with the edit validator's violation text
//! living solely in a CAS `failure_ref` the export drops, leaving the
//! whole never-edited bucket unattributable to a cause.
//!
//! Author: arkadianet

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::adapters::{NodeExecContext, NodeExecRef, VerdictOutcome, Verifier};
use crate::capabilities::{WorkerPermissions, WorkerToolClass};
use crate::dag::NodeKind;
use crate::edit::{transactions_of_run, EditContext, EditEngine};
use crate::error::{RunError, RuntimeError};
use crate::events::{NewSessionEvent, SessionEvent, SessionEventType};
use crate::obs::{
    redact_secrets, truncate_utf8_bytes, BudgetCheck, CostMeterFactory, DecisionKind, DecisionLog,
    DecisionRecord,
};
use crate::planner::seed::seed_projection_is_empty;
use crate::planner::{PlanContext, PlanError, PlanProducedPayload, PlanService, PlanSource};
use crate::scheduler::{DagOutcome, DagState};
use crate::session::{ReplanReason, RunController, RunExecCtx, RunExecutor, RunGoalRecord};
use crate::storage::{DagStore, EventStore, SessionRows, StoreError};
use crate::types::budget::BudgetPolicy;
use crate::types::diagnostic::{ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{DagId, Digest, EventSeq, SessionId};

/// The driver's own policy struct (RFC-0017 §3.8). One field today; it
/// exists so the bound has a home that is neither `SchedConfig` nor a bare
/// `u32` argument.
#[derive(Debug, Clone, Copy)]
pub struct GenerationPolicy {
    /// Maximum automatic generation bumps per run (total generations ≤ 1 +
    /// this value). `0` disables auto-replan: the driver executes exactly
    /// one generation (RX4).
    pub max_repair_generations: u32,
}

/// The three `PlanContext` fingerprints, captured once by the composition
/// root (the same values RFC-0015 §7.1 computes for generation 1). The
/// driver cannot recompute them in-process — the toolchain probe lives in
/// the host — so they ride on the deps.
#[derive(Debug, Clone)]
pub struct PlanFingerprints {
    /// Profile + budget policy hash.
    pub policy_hash: Digest,
    /// Toolchain versions digest.
    pub tool_versions: Digest,
    /// Compiler fingerprint digest.
    pub compiler_fingerprint: Digest,
}

/// Dependencies for [`GenerationDriver`] (RFC-0017 §3.8).
pub struct GenerationDriverDeps {
    /// Dispatch seam. Deliberately the **handle**, not `Arc<dyn Scheduler>`:
    /// `RuntimeHandle::run_dag_within` keeps the `try_admit_run`
    /// single-flight admission and the `SchedError → RuntimeError` mapping
    /// §6.3 step 10 is written against.
    pub handle: crate::RuntimeHandle,
    /// Topology writes (GN8) — the driver *requests*, the planner writes.
    pub plans: Arc<dyn PlanService>,
    /// AM-0003-3 control seams (`begin_repair_generation` /
    /// `complete_repair_generation` / `control_state`) — never `start`
    /// (re-entrancy) and never `request_replan` (the external path, GN9).
    pub runs: Arc<dyn RunController>,
    /// Read-only: GN2's failed-node kind lookup.
    pub dags: Arc<dyn DagStore>,
    /// Read-only: the run row's goal envelope for the replan context.
    pub sessions: Arc<dyn SessionRows>,
    /// Read-only: GN10 provenance recovery from the last `PlanProduced`
    /// event (AM-0009-3's durable fields, AC 26b).
    pub events: Arc<dyn EventStore>,
    /// `Replan` decision records (§9.2); best-effort per GN12.
    pub decisions: Arc<dyn DecisionLog>,
    /// GN6 budget admission: the **run's** meter, resolved per run through
    /// the same factory the scheduler and router bridge share, read via
    /// `check_budget(&budget_policy)` — the verdict enum is the seam.
    pub cost_meters: Arc<dyn CostMeterFactory>,
    /// Run-level ceilings for the GN6 check.
    pub budget_policy: BudgetPolicy,
    /// GN6's second half: the process/run cancellation token.
    pub cancellation: CancellationToken,
    /// Composition-root fingerprints for the rebuilt replan `PlanContext`.
    pub fingerprints: PlanFingerprints,
    /// The generation bound (from `RuntimeConfig.max_repair_generations`).
    pub policy: GenerationPolicy,
    /// GN13: restore pre-edit workspace before replan. Absent in scripted
    /// harnesses — rollback is then a no-op.
    pub edit_engine: Option<Arc<dyn EditEngine>>,
    /// GN13: mint Patch grants for [`EditEngine::rollback`].
    pub worker_permissions: Option<Arc<dyn WorkerPermissions>>,
    /// GN13: re-verify after a successful restore so the seed matches disk.
    pub verify_compile: Option<Arc<dyn Verifier>>,
}

/// Internal driver failure; folded into [`RuntimeError::Internal`] at the
/// [`RunExecutor`] boundary so RFC-0003 §6.3 needs no new error arm (GN11:
/// this is infrastructure only — it never encodes "repair failed").
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DriveError {
    /// Plan service failure during a replan.
    #[error("plan: {0}")]
    Plan(#[from] PlanError),
    /// Control-plane failure from an AM-0003-3 method.
    #[error("run control: {0}")]
    Run(#[from] RunError),
    /// Store failure during admission lookups.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}

/// §9.4 driver counters. Failures to bump never fail a generation.
#[derive(Debug, Default)]
struct AtomicDriverMetrics {
    replans_admitted: AtomicU64,
    replans_rejected: AtomicU64,
    generations_run: AtomicU64,
}

/// Snapshot of the §9.4 driver counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverMetrics {
    /// `driver.replans_admitted`.
    pub replans_admitted: u64,
    /// `driver.replans_rejected`.
    pub replans_rejected: u64,
    /// `driver.generations_run`.
    pub generations_run: u64,
}

/// Bounded repair-generation loop (RFC-0017 §5.5). Holds no lock of its
/// own: exclusivity comes from the run's execution lease in
/// `RunController::start` and the scheduler's per-DAG ownership lock.
pub struct GenerationDriver {
    deps: GenerationDriverDeps,
    metrics: AtomicDriverMetrics,
}

/// A declined admission. `reason` is the first failed GN rule (§9.2's
/// closed vocabulary). `missing_lineage` marks the AM-0017-1 near-miss —
/// an Edit/Analyze `Model` failure that would have been admissible had this
/// dispatch already admitted a verify Fail; true of every first generation,
/// where no in-run lineage can exist yet. Live E2 measurement showed this
/// decline ending runs indistinguishably from a repair that fought and
/// lost, so the near-miss is reported, not merely declined.
struct ReplanReject {
    reason: &'static str,
    missing_lineage: bool,
}

impl ReplanReject {
    /// An ordinary GN1–GN7 decline.
    const fn rule(reason: &'static str) -> Self {
        Self {
            reason,
            missing_lineage: false,
        }
    }

    /// The AM-0017-1 near-miss (reason stays `"kind"` per AC 23b).
    const fn missing_lineage() -> Self {
        Self {
            reason: "kind",
            missing_lineage: true,
        }
    }
}

/// Fields for a best-effort `Replan` decision record (§9.2).
struct ReplanDecisionMeta<'a> {
    admitted: bool,
    reason: Option<&'a str>,
    /// AM-0017-1 near-miss marker (declines only): records `lineage:
    /// "absent"` so the refused-first-generation case is countable without
    /// joining on generation numbers.
    missing_lineage: bool,
    /// `(bumps_used, max_repair_generations)` on declines — every decline
    /// is terminal for the dispatch, so the record states how much of the
    /// repair budget the run died holding.
    bumps: Option<(u32, u32)>,
    provenance: Option<&'a str>,
    seed: Option<&'a FailureIr>,
    seed_source: Option<&'a str>,
}

/// GN10 provenance for the rebuilt replan context.
#[derive(Debug, Clone)]
pub(crate) struct RecoveredProvenance {
    template_override: Option<crate::dag::TemplateId>,
    prior_source: Option<PlanSource>,
    prior_proposal_artifact: Option<crate::types::ids::ArtifactId>,
    /// `false` when no `PlanProduced` event was found (GN10's degraded
    /// path: `template_override` alone, never a silent re-selection).
    preserved: bool,
}

impl GenerationDriver {
    /// Construct over the assembly's dependencies.
    #[must_use]
    pub fn new(deps: GenerationDriverDeps) -> Self {
        Self {
            deps,
            metrics: AtomicDriverMetrics::default(),
        }
    }

    /// Snapshot the §9.4 driver counters.
    #[must_use]
    pub fn metrics(&self) -> DriverMetrics {
        DriverMetrics {
            replans_admitted: self.metrics.replans_admitted.load(Ordering::Relaxed),
            replans_rejected: self.metrics.replans_rejected.load(Ordering::Relaxed),
            generations_run: self.metrics.generations_run.load(Ordering::Relaxed),
        }
    }

    /// GN1–GN7 in order; the first failed rule names the rejection reason
    /// (§9.2's `reason` vocabulary). On admit, returns the `FailureIr` that
    /// MUST seed the next generation (outcome verify Fail, or the in-run
    /// lineage seed for AM-0017-1). A first-generation Edit/Analyze `Model`
    /// failure declines deliberately (AC 23b): the lineage seed is in-run
    /// only, and generation 1 precedes any admitted verify Fail — its
    /// compile evidence travelled the graph channel (issue #53), never a
    /// driver-held IR. That decline is flagged `missing_lineage` so the
    /// caller can report it honestly.
    async fn admission_reason(
        &self,
        ctx: &RunExecCtx,
        outcome: &DagOutcome,
        bumps: u32,
        lineage_seed: Option<&FailureIr>,
    ) -> Result<Result<FailureIr, ReplanReject>, DriveError> {
        // GN1 — a Failed outcome must carry its failure and failed node.
        // Without them there is nothing to seed, so the reject is reported
        // as `no_diagnostics`.
        let (Some(failure), Some(failed_node)) = (&outcome.failure, outcome.failed_node) else {
            return Ok(Err(ReplanReject::rule("no_diagnostics")));
        };

        // GN2 — VerifyCompile Fail seeds from the outcome. AM-0017-1: an
        // exhausted Edit/Analyze Model failure may reuse the lineage seed
        // from the VerifyCompile Fail that admitted the current repair
        // generation. No session-history scrape — only the in-run stash.
        let kind = self
            .deps
            .dags
            .get(ctx.dag_id)
            .await?
            .and_then(|dag| dag.nodes.get(&failed_node).map(|n| n.kind));
        let seed = match kind {
            Some(NodeKind::VerifyCompile) => failure.clone(),
            Some(NodeKind::Edit | NodeKind::Analyze) => {
                if failure.error_class != ErrorClass::Model {
                    return Ok(Err(ReplanReject::rule("kind")));
                }
                match lineage_seed {
                    Some(seed) => seed.clone(),
                    None => return Ok(Err(ReplanReject::missing_lineage())),
                }
            }
            _ => return Ok(Err(ReplanReject::rule("kind"))),
        };

        // GN3 — seed must be a genuine verify Fail verdict.
        if seed.error_class != ErrorClass::Compile {
            return Ok(Err(ReplanReject::rule("class")));
        }

        // GN4 — no diagnostics after the SD9 projection, no seed, no bump.
        if seed_projection_is_empty(&seed) {
            return Ok(Err(ReplanReject::rule("no_diagnostics")));
        }

        // GN5 — the bound.
        if bumps >= self.deps.policy.max_repair_generations {
            return Ok(Err(ReplanReject::rule("exhausted")));
        }

        // GN6 — run not cancelled…
        let state = self.deps.runs.control_state(ctx.run_id).await?;
        if state == crate::session::RunControlState::Cancelling
            || state.is_terminal()
            || self.deps.cancellation.is_cancelled()
        {
            return Ok(Err(ReplanReject::rule("cancelled")));
        }
        // …and budget not exhausted.
        let meter = self.deps.cost_meters.meter_for(ctx.run_id);
        if meter.check_budget(&self.deps.budget_policy) != BudgetCheck::Ok {
            return Ok(Err(ReplanReject::rule("budget")));
        }

        // GN7 — the absolute deadline must have budget left.
        if ctx
            .deadline
            .saturating_duration_since(Instant::now())
            .is_zero()
        {
            return Ok(Err(ReplanReject::rule("deadline")));
        }

        Ok(Ok(seed))
    }

    /// Best-effort `Replan` decision record (GN12 / LP11): failures are
    /// logged at `warn` and dropped, never converted into a [`DriveError`].
    async fn record_replan_decision(
        &self,
        ctx: &RunExecCtx,
        outcome: &DagOutcome,
        decision: ReplanDecisionMeta<'_>,
    ) {
        let metadata = replan_decision_metadata(ctx, outcome, &decision);
        let record = DecisionRecord {
            session: ctx.session_id,
            run: Some(ctx.run_id),
            node: None,
            kind: DecisionKind::Replan,
            metadata,
            content_hash: None,
            prompt_body: None,
        };
        if let Err(e) = self.deps.decisions.record(record).await {
            warn!(
                run_id = %ctx.run_id,
                dag_id = %ctx.dag_id,
                error = %e,
                "Replan decision record dropped (best-effort, GN12)"
            );
        }
    }

    /// GN13: after admit, restore the **newest** journaled edit checkpoint
    /// only (not the whole run) and re-verify so the replan seed matches the
    /// pre-edit workspace. No-op when edit/verify wiring is absent.
    async fn restore_workspace_and_reseed(
        &self,
        ctx: &RunExecCtx,
        seed: FailureIr,
    ) -> Result<FailureIr, DriveError> {
        let (Some(engine), Some(perms), Some(verify)) = (
            self.deps.edit_engine.as_ref(),
            self.deps.worker_permissions.as_ref(),
            self.deps.verify_compile.as_ref(),
        ) else {
            return Ok(seed);
        };

        let session = self
            .deps
            .sessions
            .get_session(ctx.session_id)
            .await?
            .ok_or_else(|| DriveError::Internal("session missing during GN13 restore".into()))?;
        let events = list_all_session_events(&self.deps.events, ctx.session_id).await?;
        let Some(newest) = transactions_of_run(&events, ctx.run_id).last().copied() else {
            return Ok(seed);
        };
        let meta = NodeExecRef {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            dag_id: ctx.dag_id,
            node_id: seed.node,
            workspace_root: session.workspace_root.clone(),
            attempt: 1,
        };
        let token = match perms.token_for(&meta, WorkerToolClass::Patch).await {
            Ok(token) => token,
            Err(error) => {
                warn!(
                    run_id = %ctx.run_id,
                    dag_id = %ctx.dag_id,
                    node_id = %seed.node,
                    error = %error,
                    "GN13: patch token mint failed; keeping admitted seed"
                );
                return Ok(seed);
            }
        };
        let edit_ctx = EditContext {
            session_id: Some(ctx.session_id),
            run_id: Some(ctx.run_id),
            perms: token,
        };
        if let Err(err) = engine.rollback(newest, &edit_ctx).await {
            warn!(
                run_id = %ctx.run_id,
                tx = %newest,
                error = %err,
                "GN13: newest-edit rollback declined; reseeding against current workspace"
            );
            return Ok(seed);
        }
        info!(
            run_id = %ctx.run_id,
            tx = %newest,
            "GN13: restored newest edit checkpoint; re-verifying for seed"
        );

        let verdict = match verify
            .verify(&NodeExecContext {
                meta: meta.clone(),
                cancellation: self.deps.cancellation.clone(),
            })
            .await
        {
            Ok(verdict) => verdict,
            Err(error) => {
                warn!(
                    run_id = %ctx.run_id,
                    dag_id = %ctx.dag_id,
                    node_id = %seed.node,
                    error = %error,
                    "GN13: re-verification failed; keeping admitted seed"
                );
                return Ok(seed);
            }
        };
        match verdict.outcome {
            VerdictOutcome::Fail if !verdict.diagnostics.is_empty() => {
                let reseeds = FailureIr {
                    node: seed.node,
                    error_class: ErrorClass::Compile,
                    retry: RetryDisposition::NonRetryable,
                    diagnostics: verdict.diagnostics,
                    notes: "GN13 re-verify after edit rollback".into(),
                };
                if seed_projection_is_empty(&reseeds) {
                    warn!(
                        run_id = %ctx.run_id,
                        "GN13: re-verify diagnostics projected empty; keeping admitted seed"
                    );
                    return Ok(seed);
                }
                Ok(reseeds)
            }
            other => {
                warn!(
                    run_id = %ctx.run_id,
                    ?other,
                    diag_count = verdict.diagnostics.len(),
                    "GN13: re-verify did not yield Compile Fail with diags; keeping admitted seed"
                );
                Ok(seed)
            }
        }
    }

    /// Record the seed GN13 just discarded as an `error` event with class
    /// `rollback` on the run — the durable channel the context engine's
    /// conversation domain reads. Best-effort (GN12 posture): a sink
    /// failure is logged and never fails the generation.
    async fn note_discarded_seed_rollback(&self, ctx: &RunExecCtx, discarded: &FailureIr) {
        let payload = json!({
            "class": "rollback",
            "message": rollback_note_message(discarded),
        });
        if let Err(error) = self
            .deps
            .events
            .append_session(NewSessionEvent {
                session_id: ctx.session_id,
                run_id: Some(ctx.run_id),
                type_: SessionEventType::Error,
                payload,
            })
            .await
        {
            warn!(
                run_id = %ctx.run_id,
                dag_id = %ctx.dag_id,
                error = %error,
                "GN13 rollback note dropped (best-effort)"
            );
        }
    }

    /// Rebuild the replan [`PlanContext`] (GN10): goal from the run row's
    /// durable goal envelope, fingerprints from the composition root,
    /// provenance from the last `PlanProduced` event for this DAG.
    async fn replan_context(
        &self,
        ctx: &RunExecCtx,
        provenance: &RecoveredProvenance,
    ) -> Result<PlanContext, DriveError> {
        let row = self
            .deps
            .sessions
            .get_run(ctx.run_id)
            .await?
            .ok_or_else(|| DriveError::Internal(format!("run row missing: {}", ctx.run_id)))?;
        let record: RunGoalRecord = serde_json::from_value(row.goal_json).map_err(|e| {
            DriveError::Internal(format!("corrupt goal_json for run {}: {e}", ctx.run_id))
        })?;
        Ok(PlanContext {
            session_id: ctx.session_id,
            run_id: ctx.run_id,
            dag_id: ctx.dag_id,
            goal: record.goal,
            template_override: provenance.template_override,
            policy_hash: self.deps.fingerprints.policy_hash.clone(),
            tool_versions: self.deps.fingerprints.tool_versions.clone(),
            compiler_fingerprint: self.deps.fingerprints.compiler_fingerprint.clone(),
            prior_source: provenance.prior_source,
            prior_proposal_artifact: provenance.prior_proposal_artifact,
        })
    }
}

/// Byte bound on the `failure_notes` a §9.2 decline record carries. The
/// worker-side FM15 bound is 2 KiB; a decision row is not a log sink, and
/// every observed edit-validator violation fits well inside this.
const MAX_DECLINE_NOTE_BYTES: usize = 512;

/// Byte bound on a GN13 rollback note's message. The context engine cuts
/// the rendered `prior_failure` section at its own 2 KiB cap; staying under
/// it here means a compliant note is never truncated mid-diagnostic.
const MAX_ROLLBACK_NOTE_BYTES: usize = 1_536;

/// How many of the discarded seed's diagnostics a rollback note quotes.
const MAX_ROLLBACK_NOTE_DIAGS: usize = 3;

/// The rollback note for a seed GN13 discarded: what the applied edit
/// broke, that the workspace was restored, and what the next generation
/// must therefore do. Pure and bounded ([`MAX_ROLLBACK_NOTE_BYTES`],
/// secret-redacted) so the message surface is testable without the driver
/// assembly. Read by a weak model next to the ORIGINAL file and the
/// ORIGINAL diagnostics, so it must say explicitly that the shown state is
/// the pre-edit one.
fn rollback_note_message(discarded: &FailureIr) -> String {
    let mut quoted = String::new();
    for d in discarded.diagnostics.iter().take(MAX_ROLLBACK_NOTE_DIAGS) {
        if !quoted.is_empty() {
            quoted.push_str("; ");
        }
        if let Some(code) = &d.code {
            quoted.push_str(&format!("error[{code}] "));
        }
        if let Some(span) = d.spans.first() {
            quoted.push_str(&format!("{}:{} — ", span.path, span.start_line));
        }
        quoted.push_str(&truncate_utf8_bytes(&d.message, 256));
    }
    let more = discarded
        .diagnostics
        .len()
        .saturating_sub(MAX_ROLLBACK_NOTE_DIAGS);
    if more > 0 {
        quoted.push_str(&format!("; and {more} more"));
    }
    if quoted.is_empty() {
        quoted.push_str("(no diagnostics captured)");
    }
    let message = format!(
        "an earlier repair edit was applied, but verification then failed with: {quoted}. \
         That edit was rolled back — the files and diagnostics shown are the ORIGINAL \
         pre-edit state, and no patch from history or artifacts is in the file. Fix the \
         shown diagnostics with a complete change that does not reintroduce the failure \
         quoted above."
    );
    truncate_utf8_bytes(&redact_secrets(&message), MAX_ROLLBACK_NOTE_BYTES)
}

/// §9.2 `Replan` decision metadata, pure so the record surface is testable
/// without the full driver assembly.
fn replan_decision_metadata(
    ctx: &RunExecCtx,
    outcome: &DagOutcome,
    decision: &ReplanDecisionMeta<'_>,
) -> serde_json::Value {
    let seed_or_outcome = decision.seed.or(outcome.failure.as_ref());
    let mut metadata = json!({
        "run_id": ctx.run_id,
        "dag_id": ctx.dag_id,
        "from_generation": outcome.generation,
        "failed_node": outcome.failed_node,
        "error_class": outcome.failure.as_ref().map(|f| f.error_class),
        "diagnostic_count": seed_or_outcome.map_or(0, |f| f.diagnostics.len()),
        "admitted": decision.admitted,
    });
    if let Some(map) = metadata.as_object_mut() {
        if decision.admitted {
            map.insert("to_generation".into(), json!(outcome.generation + 1));
        }
        if let Some(reason) = decision.reason {
            map.insert("reason".into(), json!(reason));
        }
        if decision.missing_lineage {
            // AM-0017-1 near-miss: right kind, right class, no in-run
            // lineage seed (every generation 1). Additive to §9.2 —
            // `reason` keeps AC 23b's "kind".
            map.insert("lineage".into(), json!("absent"));
        }
        if let Some((used, max)) = decision.bumps {
            map.insert("bumps_used".into(), json!(used));
            map.insert("max_repair_generations".into(), json!(max));
        }
        // Declines are terminal for the dispatch, and the E2 evidence export
        // carries decision rows but not the CAS `failure_ref` bodies —
        // without this, the measured never-edited bucket (7/48 on dev-gn13)
        // ends with the edit validator's violation text nowhere on disk.
        // Worker notes are already FM15-redacted; this only re-bounds them.
        if !decision.admitted {
            if let Some(notes) = outcome
                .failure
                .as_ref()
                .map(|f| f.notes.as_str())
                .filter(|n| !n.is_empty())
            {
                map.insert(
                    "failure_notes".into(),
                    json!(truncate_utf8_bytes(notes, MAX_DECLINE_NOTE_BYTES)),
                );
            }
        }
        if let Some(provenance) = decision.provenance {
            map.insert("provenance".into(), json!(provenance));
        }
        if let Some(seed_source) = decision.seed_source {
            map.insert("seed_source".into(), json!(seed_source));
        }
    }
    metadata
}

async fn list_all_session_events(
    events: &Arc<dyn EventStore>,
    session: SessionId,
) -> Result<Vec<SessionEvent>, StoreError> {
    let mut out = Vec::new();
    let mut after: Option<EventSeq> = None;
    loop {
        let page = events.list_session_events(session, after, 256).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|e| e.seq);
        out.extend(page);
    }
    Ok(out)
}

/// GN10 / AC 26b — recover plan provenance from the last durable
/// `PlanProduced` event for `dag`. Absent `source` decodes as the template
/// path (AM-0009-3's back-compat rule); a missing event yields the degraded
/// provenance (`template_override = None`, recorded as `"degraded"` — never
/// a silent re-selection).
pub(crate) async fn recover_provenance(
    events: &Arc<dyn EventStore>,
    session: SessionId,
    dag: DagId,
) -> Result<RecoveredProvenance, StoreError> {
    let mut after = None;
    let mut last: Option<PlanProducedPayload> = None;
    loop {
        let page = events
            .list_session_events(session, after, crate::session::MAX_EVENTS_PAGE)
            .await?;
        let Some(tail) = page.last() else { break };
        after = Some(tail.seq);
        let short_page = page.len() < crate::session::MAX_EVENTS_PAGE;
        for ev in &page {
            if ev.type_ != SessionEventType::PlanProduced {
                continue;
            }
            match serde_json::from_value::<PlanProducedPayload>(ev.payload.clone()) {
                Ok(payload) if payload.dag_id == dag => last = Some(payload),
                Ok(_) => {}
                Err(e) => {
                    warn!(%session, %dag, error = %e, "undecodable PlanProduced skipped");
                }
            }
        }
        if short_page {
            break;
        }
    }
    Ok(match last {
        Some(payload) => RecoveredProvenance {
            template_override: Some(payload.template_id),
            prior_source: Some(payload.source.unwrap_or(PlanSource::Template)),
            prior_proposal_artifact: payload.proposal_artifact,
            preserved: true,
        },
        None => RecoveredProvenance {
            template_override: None,
            prior_source: None,
            prior_proposal_artifact: None,
            preserved: false,
        },
    })
}

fn fold(e: DriveError) -> RuntimeError {
    RuntimeError::Internal(format!("generation driver: {e}"))
}

#[async_trait]
impl RunExecutor for GenerationDriver {
    /// RFC-0017 §5.5 — executes generations until a non-admissible outcome,
    /// the bound, or the absolute deadline, returning the **final**
    /// generation's [`DagOutcome`] (exhaustion is an outcome, not an
    /// error). `Err` is infrastructure only.
    async fn execute(&self, ctx: RunExecCtx) -> Result<DagOutcome, RuntimeError> {
        let mut bumps: u32 = 0;
        let mut last: Option<DagOutcome> = None;
        // AM-0017-1: Compile FailureIr from the VerifyCompile Fail that
        // admitted the current repair lineage. Never loaded from session
        // history — only from admitted verify outcomes in this execute().
        let mut lineage_seed: Option<FailureIr> = None;
        loop {
            let remaining = ctx.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if let Some(outcome) = last {
                    // GN7: never dispatch with a zero budget once a
                    // generation has run.
                    return Ok(outcome);
                }
                // First dispatch with a zero budget is only reachable when
                // run_timeout is itself zero; fall through so the
                // scheduler's own run clock yields the honest timeout
                // outcome rather than the driver inventing one.
            }

            // §9.1 `driver.generation` — identifiers and counters only.
            let gen_span = tracing::info_span!(
                "driver.generation",
                run_id = %ctx.run_id,
                dag_id = %ctx.dag_id,
                bumps,
                remaining_ms = remaining.as_millis() as u64,
                generation = tracing::field::Empty,
                admitted = tracing::field::Empty,
                reject_reason = tracing::field::Empty,
            );
            let outcome = {
                use tracing::Instrument as _;
                self.deps
                    .handle
                    .run_dag_within(ctx.dag_id, remaining)
                    .instrument(gen_span.clone())
                    .await?
            };
            gen_span.record("generation", outcome.generation);
            self.metrics.generations_run.fetch_add(1, Ordering::Relaxed);

            // GN9 — everything except Failed passes through unconverted.
            if outcome.state != DagState::Failed {
                return Ok(outcome);
            }

            let seed = match self
                .admission_reason(&ctx, &outcome, bumps, lineage_seed.as_ref())
                .await
            {
                Ok(Err(reject)) => {
                    gen_span.record("admitted", false);
                    gen_span.record("reject_reason", reject.reason);
                    self.metrics
                        .replans_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    self.record_replan_decision(
                        &ctx,
                        &outcome,
                        ReplanDecisionMeta {
                            admitted: false,
                            reason: Some(reject.reason),
                            missing_lineage: reject.missing_lineage,
                            bumps: Some((bumps, self.deps.policy.max_repair_generations)),
                            provenance: None,
                            seed: None,
                            seed_source: None,
                        },
                    )
                    .await;
                    if reject.missing_lineage {
                        // The measured silent exit (E2 bucket A): the whole
                        // edit budget is already burned, no repair was ever
                        // admitted, and the raw outcome reads like an
                        // ordinary model failure. Loud log, and an honest
                        // terminal IR — GN11's intact-IR rule binds only
                        // the exhausted/deadline declines, which never
                        // reach this branch.
                        warn!(
                            run_id = %ctx.run_id,
                            dag_id = %ctx.dag_id,
                            generation = outcome.generation,
                            "repair declined without lineage (AM-0017-1 near-miss): \
                             Edit/Analyze Model failure before any admitted verify Fail; \
                             no repair generation was admitted in this dispatch"
                        );
                        let mut outcome = outcome;
                        if let Some(failure) = outcome.failure.as_mut() {
                            if !failure.notes.is_empty() {
                                failure.notes.push_str("; ");
                            }
                            failure.notes.push_str(
                                "auto-repair declined (kind): Edit/Analyze Model failure \
                                 with no in-run lineage seed (AM-0017-1); no repair \
                                 generation was admitted in this dispatch",
                            );
                        }
                        return Ok(outcome);
                    }
                    info!(
                        run_id = %ctx.run_id,
                        dag_id = %ctx.dag_id,
                        generation = outcome.generation,
                        reason = reject.reason,
                        "repair generation declined"
                    );
                    return Ok(outcome);
                }
                Ok(Ok(seed)) => {
                    gen_span.record("admitted", true);
                    seed
                }
                Err(e) => return Err(fold(e)),
            };

            // GN13 — undo morphing edits and re-derive the seed against the
            // restored tree before the planner writes generation N+1.
            let admitted_seed = seed.clone();
            let seed = match self.restore_workspace_and_reseed(&ctx, seed).await {
                Ok(seed) => seed,
                Err(e) => return Err(fold(e)),
            };
            // A replaced seed means GN13 rolled the newest edit back and
            // re-verified: the admitted (post-edit) failure — the one fact
            // generation N+1 needs and generation N's file state no longer
            // shows — is about to be discarded. Record it as a run-scoped
            // rollback note; the context engine promotes the newest note
            // into the next generation's `conversation:prior_failure`
            // section. Without this, dev-loop showed generation N+1 to be
            // a byte-for-byte replay of generation N (16/16 two-plus-edit
            // runs saw zero post-edit diagnostics; 0% passed).
            if seed != admitted_seed {
                self.note_discarded_seed_rollback(&ctx, &admitted_seed)
                    .await;
            }

            // Admitted. Recover provenance first (read-only) so the
            // decision record can carry it (§9.2).
            let provenance =
                match recover_provenance(&self.deps.events, ctx.session_id, ctx.dag_id).await {
                    Ok(p) => p,
                    Err(e) => return Err(fold(DriveError::Store(e))),
                };
            let provenance_str = if provenance.preserved {
                "preserved"
            } else {
                "degraded"
            };
            let seed_source = if outcome
                .failure
                .as_ref()
                .is_some_and(|f| f.node == seed.node && f.error_class == ErrorClass::Compile)
            {
                "outcome"
            } else {
                "lineage"
            };
            self.metrics
                .replans_admitted
                .fetch_add(1, Ordering::Relaxed);
            self.record_replan_decision(
                &ctx,
                &outcome,
                ReplanDecisionMeta {
                    admitted: true,
                    reason: None,
                    missing_lineage: false,
                    bumps: None,
                    provenance: Some(provenance_str),
                    seed: Some(&seed),
                    seed_source: Some(seed_source),
                },
            )
            .await;

            let reason = ReplanReason::FailureIr(seed.clone());

            // GN8 ordering: begin → replan (topology write, seeded per
            // §5.4) → complete → next dispatch.
            self.deps
                .runs
                .begin_repair_generation(ctx.run_id, &reason)
                .await
                .map_err(|e| fold(DriveError::Run(e)))?;
            let plan_ctx = self.replan_context(&ctx, &provenance).await.map_err(fold)?;
            let plan = self
                .deps
                .plans
                .replan(reason, plan_ctx)
                .await
                .map_err(|e| fold(DriveError::Plan(e)))?;
            self.deps
                .runs
                .complete_repair_generation(ctx.run_id, plan.dag.generation)
                .await
                .map_err(|e| fold(DriveError::Run(e)))?;

            info!(
                run_id = %ctx.run_id,
                dag_id = %ctx.dag_id,
                generation = plan.dag.generation,
                bumps = bumps + 1,
                provenance = provenance_str,
                seed_source,
                "repair generation replanned"
            );
            // Lineage tracks the Compile seed that opened (or reopened)
            // this repair generation — never an Edit/Analyze Model IR.
            lineage_seed = Some(seed);
            bumps += 1;
            last = Some(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::TemplateId;
    use crate::events::{EventSink, NewSessionEvent};
    use crate::storage::{AlloyStorage, StorageOpenOptions};
    use crate::types::ids::{ArtifactId, NodeId, RunId};

    fn exec_ctx() -> RunExecCtx {
        RunExecCtx {
            run_id: RunId::new(),
            session_id: SessionId::new(),
            dag_id: DagId::new(),
            deadline: Instant::now(),
        }
    }

    fn failed_outcome(dag_id: DagId, notes: &str) -> DagOutcome {
        let node = NodeId::new();
        DagOutcome {
            dag_id,
            generation: 1,
            state: DagState::Failed,
            failed_node: Some(node),
            failure: Some(FailureIr {
                node,
                error_class: ErrorClass::Model,
                retry: RetryDisposition::Retryable,
                diagnostics: vec![],
                notes: notes.into(),
            }),
        }
    }

    fn decline(reason: &'static str, missing_lineage: bool) -> ReplanDecisionMeta<'static> {
        ReplanDecisionMeta {
            admitted: false,
            reason: Some(reason),
            missing_lineage,
            bumps: Some((0, 2)),
            provenance: None,
            seed: None,
            seed_source: None,
        }
    }

    /// E2 residual (dev-gn13, 7/48 attempts): a run that never applied an
    /// edit dies on the AM-0017-1 decline with the edit worker's validator
    /// violation living only in the CAS-backed `failure_ref`, which the
    /// evidence export does not carry — the measured bucket was
    /// unattributable to a cause. The §9.2 decline record is the durable
    /// channel, so it must carry the terminal failure's notes.
    #[test]
    fn declined_replan_record_carries_terminal_failure_notes() {
        let ctx = exec_ctx();
        let notes =
            "invalid model response after repair turn: expect lists 2 line(s) but start..=end \
             covers 4; expect must repeat every current line in the range";
        let outcome = failed_outcome(ctx.dag_id, notes);
        let metadata = replan_decision_metadata(&ctx, &outcome, &decline("kind", true));
        assert_eq!(
            metadata["failure_notes"].as_str(),
            Some(notes),
            "decline records must carry the terminal failure notes"
        );
        // The pre-existing decline fields survive the refactor.
        assert_eq!(metadata["reason"], json!("kind"));
        assert_eq!(metadata["lineage"], json!("absent"));
        assert_eq!(metadata["bumps_used"], json!(0));
        assert_eq!(metadata["max_repair_generations"], json!(2));
        assert_eq!(metadata["admitted"], json!(false));
    }

    /// The notes ride only the decline path (terminal for the dispatch) and
    /// stay bounded: an admitted record describes a seed the planner will
    /// consume, and a §9.2 row is not a log sink.
    #[test]
    fn replan_record_notes_are_decline_only_and_bounded() {
        let ctx = exec_ctx();
        // Bounded on a UTF-8 boundary: 2-byte 'é' repeated past the cap.
        let long = "é".repeat(MAX_DECLINE_NOTE_BYTES);
        let outcome = failed_outcome(ctx.dag_id, &long);
        let metadata = replan_decision_metadata(&ctx, &outcome, &decline("kind", false));
        let stored = metadata["failure_notes"].as_str().expect("notes recorded");
        assert!(
            stored.len() <= MAX_DECLINE_NOTE_BYTES,
            "notes must be bounded"
        );
        assert!(
            long.starts_with(stored),
            "bounding must truncate, not rewrite"
        );

        // Admitted record: no failure_notes.
        let admitted = ReplanDecisionMeta {
            admitted: true,
            reason: None,
            missing_lineage: false,
            bumps: None,
            provenance: Some("preserved"),
            seed: outcome.failure.as_ref(),
            seed_source: Some("outcome"),
        };
        let metadata = replan_decision_metadata(&ctx, &outcome, &admitted);
        assert!(
            metadata.get("failure_notes").is_none(),
            "admitted records must not carry failure notes"
        );

        // Empty notes: field omitted rather than recorded as "".
        let outcome = failed_outcome(ctx.dag_id, "");
        let metadata = replan_decision_metadata(&ctx, &outcome, &decline("no_diagnostics", false));
        assert!(metadata.get("failure_notes").is_none());
    }

    async fn store() -> (tempfile::TempDir, AlloyStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        (dir, storage)
    }

    /// AC 26b: the last `PlanProduced` event's AM-0009-3 fields are
    /// sufficient to reconstruct GN10's provenance after a restart.
    #[tokio::test]
    async fn ac26b_provenance_recovered_from_plan_produced_event() {
        let (_dir, storage) = store().await;
        let session = SessionId::new();
        let dag = DagId::new();
        let proposal = ArtifactId::new();
        let payload = crate::planner::PlanProducedPayload {
            dag_id: dag,
            generation: 1,
            template_id: TemplateId::RepairLocalDiagnostic,
            snapshot_artifact: ArtifactId::new(),
            node_ids: vec![],
            replan: false,
            reason: None,
            source: Some(PlanSource::LlmProposed),
            proposal_artifact: Some(proposal),
            seeded_root: None,
        };
        let events = storage.events();
        events
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: None,
                type_: SessionEventType::PlanProduced,
                payload: serde_json::to_value(&payload).unwrap(),
            })
            .await
            .unwrap();

        let events: Arc<dyn EventStore> = storage.events() as _;
        let recovered = recover_provenance(&events, session, dag).await.unwrap();
        assert!(recovered.preserved);
        assert_eq!(
            recovered.template_override,
            Some(TemplateId::RepairLocalDiagnostic)
        );
        assert_eq!(recovered.prior_source, Some(PlanSource::LlmProposed));
        assert_eq!(recovered.prior_proposal_artifact, Some(proposal));

        // A pre-RFC payload without `source` decodes as the template path.
        let other_dag = DagId::new();
        let legacy = serde_json::json!({
            "dag_id": other_dag,
            "generation": 1,
            "template_id": "repair_local_diagnostic",
            "snapshot_artifact": ArtifactId::new(),
            "node_ids": [],
            "replan": false,
            "reason": null,
        });
        storage
            .events()
            .append_session(NewSessionEvent {
                session_id: session,
                run_id: None,
                type_: SessionEventType::PlanProduced,
                payload: legacy,
            })
            .await
            .unwrap();
        let recovered = recover_provenance(&events, session, other_dag)
            .await
            .unwrap();
        assert!(recovered.preserved);
        assert_eq!(recovered.prior_source, Some(PlanSource::Template));
        assert_eq!(recovered.prior_proposal_artifact, None);
        storage.close().await.unwrap();
    }

    /// GN10's degraded path: no `PlanProduced` event → no template
    /// override, `preserved == false` (recorded as `"degraded"`, never a
    /// silent re-selection).
    #[tokio::test]
    async fn ac26_missing_plan_produced_is_degraded() {
        let (_dir, storage) = store().await;
        let events: Arc<dyn EventStore> = storage.events() as _;
        let recovered = recover_provenance(&events, SessionId::new(), DagId::new())
            .await
            .unwrap();
        assert!(!recovered.preserved);
        assert_eq!(recovered.template_override, None);
        assert_eq!(recovered.prior_source, None);
        assert_eq!(recovered.prior_proposal_artifact, None);
        storage.close().await.unwrap();
    }
}
