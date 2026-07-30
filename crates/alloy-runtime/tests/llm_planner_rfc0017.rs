//! RFC-0017 `LlmPlanService` / `CapabilityPlanProposer` behaviour
//! (ACs 7–12, 14/14b/15/16, 26 LLM half, 37, 45 planner half).
//!
//! Scripted proposers and executors only — no live models (RFC-0016
//! posture). Persistence runs against real SQLite storage so the single
//! validated write path is exercised end to end.
//!
//! Author: arkadianet

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use alloy_runtime::CostMeterFactory;
use alloy_runtime::{
    compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, AlloyStorage,
    ArtifactStore, CapabilityExecContext, CapabilityExecError, CapabilityExecutor,
    CapabilityOutcome, CapabilityPlanProposer, DagId, DagState, DagStore, DecisionKind,
    DecisionRecord, DiagnosticEvent, DiagnosticId, DiagnosticLevel, Digest, ErrorClass, EventSink,
    FailureIr, Goal, InMemoryEventSink, LlmPlanService, ModelCallRecord, NodeInputPayload,
    NodeKind, PlanContext, PlanError, PlanProducedPayload, PlanProposer, PlanService, PlanSource,
    PlannerConfig, PlannerMode, ProcessCostMeterFactory, ProfileId, ProposeError,
    ProposedDagManifest, ProposedNodeSpec, ProposerDeps, ReplanReason, RunId, SessionEventType,
    SessionId, StorageOpenOptions, TemplatePlanService, ToolCallRecord, PROPOSAL_SCHEMA_VERSION,
};

// ---------- doubles ----------

/// Scripted proposer: consumes one queued result per call in declaration order.
struct ScriptedProposer {
    queue: Mutex<VecDeque<Result<ProposedDagManifest, ProposeError>>>,
    calls: AtomicUsize,
}

impl ScriptedProposer {
    fn new(results: Vec<Result<ProposedDagManifest, ProposeError>>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::from(results)),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl PlanProposer for ScriptedProposer {
    async fn propose(&self, _ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(ProposeError::Unavailable("script exhausted".into())))
    }
}

/// A proposer that never returns until dropped (AC 16).
struct HangingProposer;

#[async_trait]
impl PlanProposer for HangingProposer {
    async fn propose(&self, _ctx: &PlanContext) -> Result<ProposedDagManifest, ProposeError> {
        std::future::pending().await
    }
}

/// Recording (optionally failing) decision log.
#[derive(Default)]
struct RecordingDecisions {
    records: Mutex<Vec<DecisionRecord>>,
    fail: bool,
}

#[async_trait]
impl alloy_runtime::DecisionLog for RecordingDecisions {
    async fn record(
        &self,
        rec: DecisionRecord,
    ) -> Result<alloy_runtime::EventSeq, alloy_runtime::ObsError> {
        if self.fail {
            return Err(alloy_runtime::ObsError::Invalid("injected".into()));
        }
        self.records.lock().unwrap().push(rec);
        Ok(alloy_runtime::EventSeq(0))
    }
    async fn record_model_call(
        &self,
        _rec: ModelCallRecord,
    ) -> Result<alloy_runtime::EventSeq, alloy_runtime::ObsError> {
        Ok(alloy_runtime::EventSeq(0))
    }
    async fn record_tool_call(
        &self,
        _rec: ToolCallRecord,
    ) -> Result<alloy_runtime::EventSeq, alloy_runtime::ObsError> {
        Ok(alloy_runtime::EventSeq(0))
    }
}

// ---------- harness ----------

fn manifest() -> ProposedDagManifest {
    let node = |name: &str, kind: NodeKind, reason: Option<&str>| ProposedNodeSpec {
        name: name.into(),
        kind,
        approval_reason: reason.map(String::from),
    };
    ProposedDagManifest {
        schema_version: PROPOSAL_SCHEMA_VERSION,
        nodes: vec![
            node("analyze", NodeKind::Analyze, None),
            node("edit", NodeKind::Edit, None),
            node("verify", NodeKind::VerifyCompile, None),
            node(
                "gate",
                NodeKind::GateHuman,
                Some("Approve before completion"),
            ),
        ],
        rationale: "test chain".into(),
    }
}

fn plan_ctx(session: SessionId, run: RunId, dag: DagId) -> PlanContext {
    let toolchain = alloy_runtime::ToolchainRecord {
        channel: "1.97.1".into(),
        rustc_version: "rustc 1.97.1 (test)".into(),
        cargo_version: "cargo 1.97.1 (test)".into(),
    };
    PlanContext {
        session_id: session,
        run_id: run,
        dag_id: dag,
        goal: Goal {
            text: "fix E0308".into(),
            constraints: vec![],
            attachments: vec![],
        },
        template_override: None,
        policy_hash: policy_hash_digest(
            &ProfileId::new("default").unwrap(),
            &alloy_runtime::BudgetPolicy::default(),
        ),
        tool_versions: tool_versions_digest(&toolchain),
        compiler_fingerprint: compiler_fingerprint_digest(&toolchain, "x86_64-unknown-linux-gnu"),
        prior_source: None,
        prior_proposal_artifact: None,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    storage: AlloyStorage,
    events: Arc<InMemoryEventSink>,
    decisions: Arc<RecordingDecisions>,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap();
        Self {
            _dir: dir,
            storage,
            events: Arc::new(InMemoryEventSink::new()),
            decisions: Arc::new(RecordingDecisions::default()),
        }
    }

    fn service(&self, proposer: Arc<dyn PlanProposer>, cfg: PlannerConfig) -> LlmPlanService {
        let template = TemplatePlanService::new(
            self.storage.dags() as Arc<dyn DagStore>,
            self.storage.artifacts() as Arc<dyn ArtifactStore>,
            Arc::clone(&self.events) as Arc<dyn EventSink>,
        );
        LlmPlanService::new(
            template,
            proposer,
            self.storage.artifacts() as Arc<dyn ArtifactStore>,
            Arc::clone(&self.decisions) as Arc<dyn alloy_runtime::DecisionLog>,
            cfg,
            true,
        )
    }

    fn llm_cfg() -> PlannerConfig {
        PlannerConfig {
            mode: PlannerMode::Llm,
            ..PlannerConfig::new()
        }
    }

    fn proposal_decisions(&self) -> Vec<DecisionRecord> {
        self.decisions
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == DecisionKind::PlanProposal)
            .cloned()
            .collect()
    }

    fn last_plan_produced(&self, session: SessionId) -> PlanProducedPayload {
        let evs = self.events.session_events(session);
        let ev = evs
            .iter()
            .rev()
            .find(|e| e.type_ == SessionEventType::PlanProduced)
            .expect("PlanProduced present");
        serde_json::from_value(ev.payload.clone()).unwrap()
    }
}

// ---------- service tests ----------

/// AC 9 + AC 7 + AC 10: accepted proposal → `source = llm_proposed`,
/// resolvable `plan_proposal` artifact (labelled, written pre-compilation),
/// exactly one accepted `PlanProposal` decision with `prompt_body = None`.
#[tokio::test]
async fn ac9_accepted_proposal_is_llm_proposed_with_resolving_artifact() {
    let h = Harness::new().await;
    let svc = h.service(
        ScriptedProposer::new(vec![Ok(manifest())]),
        Harness::llm_cfg(),
    );
    let (session, run, dag) = (SessionId::new(), RunId::new(), DagId::new());
    let result = svc.plan(plan_ctx(session, run, dag)).await.unwrap();

    assert_eq!(result.source, PlanSource::LlmProposed);
    let artifact = result.proposal_artifact.expect("proposal artifact");
    let blob = h.storage.artifacts().get(artifact).await.unwrap();
    assert_eq!(
        blob.meta
            .labels
            .get("alloy.envelope")
            .and_then(|v| v.as_str()),
        Some("plan_proposal")
    );
    let stored: ProposedDagManifest = serde_json::from_slice(&blob.bytes).unwrap();
    assert_eq!(stored, manifest());

    // The persisted DAG carries the compiled 4-node chain.
    let dag_row = h.storage.dags().get(dag).await.unwrap().unwrap();
    assert_eq!(dag_row.nodes.len(), 4);
    assert_eq!(dag_row.generation, 1);

    // AM-0009-3 on the wire.
    let payload = h.last_plan_produced(session);
    assert_eq!(payload.source, Some(PlanSource::LlmProposed));
    assert_eq!(payload.proposal_artifact, Some(artifact));

    // AC 10 — exactly one PlanProposal decision, §9.2 payload, no prompt.
    let decisions = h.proposal_decisions();
    assert_eq!(decisions.len(), 1);
    let rec = &decisions[0];
    assert!(rec.prompt_body.is_none());
    assert_eq!(rec.metadata["accepted"], true);
    assert_eq!(rec.metadata["node_count"], 4);
    assert_eq!(rec.run, Some(run));

    assert_eq!(svc.metrics().proposals_accepted, 1);
    h.storage.close().await.unwrap();
}

/// AC 8: every `ProposeError` variant except `Cancelled`, and a clamp
/// rejection, fall back to the template path with a named-reason decision
/// (FB2/FB3/FB5). Fallback plans have `source = template`.
#[tokio::test]
async fn ac8_every_fallback_trigger_yields_a_template_plan() {
    let h = Harness::new().await;
    let triggers: Vec<(Result<ProposedDagManifest, ProposeError>, &str)> = vec![
        (Err(ProposeError::Unavailable("down".into())), "unavailable"),
        (
            Err(ProposeError::Model("5xx".into())),
            "planning call failed",
        ),
        (Err(ProposeError::Malformed("bad".into())), "malformed"),
        (Err(ProposeError::Budget), "budget"),
        (Err(ProposeError::Timeout), "timed out"),
        // A proposal that violates PC8 (no verify after the edit).
        (
            Ok(ProposedDagManifest {
                schema_version: PROPOSAL_SCHEMA_VERSION,
                nodes: vec![
                    ProposedNodeSpec {
                        name: "analyze".into(),
                        kind: NodeKind::Analyze,
                        approval_reason: None,
                    },
                    ProposedNodeSpec {
                        name: "verify".into(),
                        kind: NodeKind::VerifyCompile,
                        approval_reason: None,
                    },
                    ProposedNodeSpec {
                        name: "edit".into(),
                        kind: NodeKind::Edit,
                        approval_reason: None,
                    },
                    ProposedNodeSpec {
                        name: "gate".into(),
                        kind: NodeKind::GateHuman,
                        approval_reason: Some("approve".into()),
                    },
                ],
                rationale: "adversarial".into(),
            }),
            "not followed by a verify",
        ),
    ];
    for (idx, (trigger, needle)) in triggers.into_iter().enumerate() {
        let rejected_is_proposal = trigger.is_ok();
        let svc = h.service(ScriptedProposer::new(vec![trigger]), Harness::llm_cfg());
        let (session, run, dag) = (SessionId::new(), RunId::new(), DagId::new());
        let result = svc.plan(plan_ctx(session, run, dag)).await.unwrap();
        // FB5 — an ordinary template plan.
        assert_eq!(result.source, PlanSource::Template, "case {idx}");
        assert!(result.proposal_artifact.is_none(), "case {idx}");
        assert_eq!(
            result.template_id,
            alloy_runtime::TemplateId::RepairLocalDiagnostic
        );
        let decisions = h.proposal_decisions();
        let rec = decisions.last().unwrap();
        assert_eq!(rec.metadata["accepted"], false, "case {idx}");
        let reason = rec.metadata["rejected_reason"].as_str().unwrap();
        assert!(
            reason.to_lowercase().contains(needle),
            "case {idx}: {reason:?} lacks {needle:?}"
        );
        // AC 7 — a rejected *proposal* remains auditable.
        if rejected_is_proposal {
            let artifact = rec.metadata["proposal_artifact"].as_str().unwrap();
            let id = alloy_runtime::ArtifactId::parse(artifact).unwrap();
            assert!(h.storage.artifacts().get(id).await.is_ok());
        }
        assert_eq!(svc.metrics().proposals_rejected, 1, "case {idx}");
    }
    h.storage.close().await.unwrap();
}

/// AC 8b (FB2b): `Cancelled` propagates as a `PlanError` — no plan, no DAG
/// row, no template fallback.
#[tokio::test]
async fn ac8b_cancelled_propagates_without_a_plan_or_row() {
    let h = Harness::new().await;
    let svc = h.service(
        ScriptedProposer::new(vec![Err(ProposeError::Cancelled)]),
        Harness::llm_cfg(),
    );
    let (session, run, dag) = (SessionId::new(), RunId::new(), DagId::new());
    let err = svc.plan(plan_ctx(session, run, dag)).await.unwrap_err();
    assert!(matches!(err, PlanError::Internal(m) if m.contains("cancelled")));
    assert!(h.storage.dags().get(dag).await.unwrap().is_none());
    assert!(h.events.session_events(session).is_empty());
    h.storage.close().await.unwrap();
}

/// AC 11 (LP6): `load_template` never consults the proposer.
#[tokio::test]
async fn ac11_load_template_never_invokes_proposer() {
    let h = Harness::new().await;
    let proposer = ScriptedProposer::new(vec![]);
    let svc = h.service(
        Arc::clone(&proposer) as Arc<dyn PlanProposer>,
        Harness::llm_cfg(),
    );
    let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    svc.load_template(alloy_runtime::TemplateId::RepairLocalDiagnostic, ctx)
        .await
        .unwrap();
    assert_eq!(proposer.calls.load(Ordering::SeqCst), 0);
    h.storage.close().await.unwrap();
}

/// AC 16 (LP3): the planning call is bounded by `planning_timeout_ms`; the
/// elapse is a `Timeout` fallback trigger, not a run failure.
#[tokio::test]
async fn ac16_planning_timeout_falls_back() {
    let h = Harness::new().await;
    let mut cfg = Harness::llm_cfg();
    cfg.planning_timeout_ms = 50;
    let svc = h.service(Arc::new(HangingProposer), cfg);
    let result = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), DagId::new()))
        .await
        .unwrap();
    assert_eq!(result.source, PlanSource::Template);
    let decisions = h.proposal_decisions();
    assert!(decisions.last().unwrap().metadata["rejected_reason"]
        .as_str()
        .unwrap()
        .contains("timed out"));
    h.storage.close().await.unwrap();
}

/// AC 37 (FB6/BG4): budget denial falls back once — no lower-tier retry,
/// exactly one propose call.
#[tokio::test]
async fn ac37_budget_denial_falls_back_without_retry() {
    let h = Harness::new().await;
    let proposer = ScriptedProposer::new(vec![Err(ProposeError::Budget)]);
    let svc = h.service(
        Arc::clone(&proposer) as Arc<dyn PlanProposer>,
        Harness::llm_cfg(),
    );
    let result = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), DagId::new()))
        .await
        .unwrap();
    assert_eq!(result.source, PlanSource::Template);
    assert_eq!(proposer.calls.load(Ordering::SeqCst), 1);
    h.storage.close().await.unwrap();
}

/// AC 45 (planner half / LP11): a failing decision log never fails a plan;
/// the counters still move.
#[tokio::test]
async fn ac45_failing_decision_log_never_fails_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let events = Arc::new(InMemoryEventSink::new());
    let template = TemplatePlanService::new(
        storage.dags() as Arc<dyn DagStore>,
        storage.artifacts() as Arc<dyn ArtifactStore>,
        Arc::clone(&events) as Arc<dyn EventSink>,
    );
    let failing = Arc::new(RecordingDecisions {
        records: Mutex::new(vec![]),
        fail: true,
    });
    let svc = LlmPlanService::new(
        template,
        ScriptedProposer::new(vec![Err(ProposeError::Model("x".into())), Ok(manifest())]),
        storage.artifacts() as Arc<dyn ArtifactStore>,
        failing as Arc<dyn alloy_runtime::DecisionLog>,
        Harness::llm_cfg(),
        true,
    );
    // Rejected path first (queue is FIFO), then accepted.
    let a = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), DagId::new()))
        .await
        .unwrap();
    assert_eq!(a.source, PlanSource::Template);
    let b = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), DagId::new()))
        .await
        .unwrap();
    assert_eq!(b.source, PlanSource::LlmProposed);
    assert_eq!(svc.metrics().proposals_accepted, 1);
    assert_eq!(svc.metrics().proposals_rejected, 1);
    storage.close().await.unwrap();
}

/// AC 26 (LLM half / GN10): a proposal-sourced run replans by re-compiling
/// the **same stored manifest** at the new generation with a seeded root —
/// same shape, same `proposal_artifact`, new inputs.
#[tokio::test]
async fn ac26_llm_replan_recompiles_stored_manifest_with_seed() {
    let h = Harness::new().await;
    let svc = h.service(
        ScriptedProposer::new(vec![Ok(manifest())]),
        Harness::llm_cfg(),
    );
    let (session, run, dag) = (SessionId::new(), RunId::new(), DagId::new());
    let first = svc.plan(plan_ctx(session, run, dag)).await.unwrap();
    let artifact = first.proposal_artifact.unwrap();

    // Terminalize generation 1 as Failed so replace_for_replan admits.
    let mut failed = first.dag.clone();
    failed.state = DagState::Failed;
    h.storage.dags().put(&failed).await.unwrap();

    let verify_node = *failed
        .nodes
        .iter()
        .find(|(_, n)| n.kind == NodeKind::VerifyCompile)
        .map(|(id, _)| id)
        .unwrap();
    let failure = FailureIr {
        node: verify_node,
        error_class: ErrorClass::Compile,
        retry: Default::default(),
        diagnostics: vec![DiagnosticEvent {
            id: DiagnosticId::new(),
            code: Some("E0308".into()),
            level: DiagnosticLevel::Error,
            message: "mismatched types".into(),
            spans: vec![],
            children: vec![],
            package: None,
            fingerprint: Digest::sha256(b"d"),
            raw_json: None,
        }],
        notes: "cargo check failed".into(),
    };

    let mut ctx = plan_ctx(session, run, dag);
    ctx.prior_source = Some(PlanSource::LlmProposed);
    ctx.prior_proposal_artifact = Some(artifact);
    let second = svc
        .replan(ReplanReason::FailureIr(failure), ctx)
        .await
        .unwrap();

    assert_eq!(second.dag.generation, 2);
    assert_eq!(second.source, PlanSource::LlmProposed);
    assert_eq!(
        second.proposal_artifact,
        Some(artifact),
        "same stored manifest"
    );
    assert_eq!(second.dag.nodes.len(), 4, "same shape");
    let payload = h.last_plan_produced(session);
    assert!(payload.replan);
    assert_eq!(payload.seeded_root, Some(true), "SD1–SD10 applied");
    assert_eq!(payload.source, Some(PlanSource::LlmProposed));
    h.storage.close().await.unwrap();
}

/// AC 12 (AM-0013-2): the pre-0017 `PlanningProposalPayload` wire shape —
/// no `proposal` field — still decodes.
#[test]
fn ac12_old_planning_payload_wire_shape_decodes() {
    let old = serde_json::json!({
        "schema_version": 1,
        "capability": "planning",
        "template_id": "repair_local_diagnostic",
        "rationale": "sole MVP template",
        "replan_requested": false,
        "truncated": false,
        "confidence": 1.0,
        "citations": [],
        "artifacts": [],
        "metrics": {
            "model_tier_used": "economy",
            "provider_id": "unrouted",
            "input_tokens": null,
            "output_tokens": null,
            "tool_calls": 0,
            "cache_hits": 0,
            "duration_ms": 1,
            "confidence": null,
            "error_class": null
        }
    });
    let decoded: alloy_runtime::PlanningProposalPayload = serde_json::from_value(old).unwrap();
    assert!(decoded.proposal.is_none());
}

// ---------- proposer tests ----------

/// Scripted executor capturing the context it was invoked with.
struct CapturingExecutor {
    captured: Mutex<Option<CapabilityExecContext>>,
    outcome: Mutex<Option<Result<CapabilityOutcome, CapabilityExecError>>>,
    wait_for_cancel: bool,
}

impl CapturingExecutor {
    fn ok_with(payload: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            captured: Mutex::new(None),
            outcome: Mutex::new(Some(Ok(CapabilityOutcome::Succeeded { payload }))),
            wait_for_cancel: false,
        })
    }

    fn scripted(outcome: Result<CapabilityOutcome, CapabilityExecError>) -> Arc<Self> {
        Arc::new(Self {
            captured: Mutex::new(None),
            outcome: Mutex::new(Some(outcome)),
            wait_for_cancel: false,
        })
    }
}

#[async_trait]
impl CapabilityExecutor for CapturingExecutor {
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        *self.captured.lock().unwrap() = Some(ctx.clone());
        if self.wait_for_cancel {
            ctx.cancellation.cancelled().await;
            return Err(CapabilityExecError::Cancelled);
        }
        self.outcome
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Err(CapabilityExecError::Unavailable))
    }
}

fn worker_payload(proposal: Option<ProposedDagManifest>) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "capability": "planning",
        "template_id": "repair_local_diagnostic",
        "rationale": "r",
        "replan_requested": false,
        "truncated": false,
        "confidence": 0.8,
        "citations": [],
        "artifacts": [],
        "metrics": {
            "model_tier_used": "standard",
            "provider_id": "p",
            "input_tokens": 1,
            "output_tokens": 1,
            "tool_calls": 0,
            "cache_hits": 0,
            "duration_ms": 1,
            "confidence": null,
            "error_class": null
        },
        "proposal": proposal,
    })
}

fn proposer_with(
    executor: Arc<dyn CapabilityExecutor>,
    meters: &Arc<ProcessCostMeterFactory>,
    cancellation: CancellationToken,
) -> CapabilityPlanProposer {
    CapabilityPlanProposer::new(
        executor,
        ProposerDeps {
            workspace_root: std::path::PathBuf::from("/tmp/ws-proposer"),
            cancellation,
            cost_meters: Arc::clone(meters) as _,
            budget_policy: alloy_runtime::BudgetPolicy::default(),
        },
        PlannerConfig {
            mode: PlannerMode::Llm,
            ..PlannerConfig::new()
        },
    )
}

/// AC 14/14b (PP1/PP1b/PP2/PP3): the proposer builds a complete synthetic
/// Plan-node context — workspace root from deps, attempt agreement, tier
/// Standard, the planner budget/timeout, a Goal envelope — and passes the
/// run's meter through (`shares_state_with`).
#[tokio::test]
async fn ac14b_proposer_builds_complete_context_on_the_runs_meter() {
    let executor = CapturingExecutor::ok_with(worker_payload(Some(manifest())));
    let meters = Arc::new(ProcessCostMeterFactory::new());
    let proposer = proposer_with(
        Arc::clone(&executor) as _,
        &meters,
        CancellationToken::new(),
    );
    let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    let got = proposer.propose(&ctx).await.unwrap();
    assert_eq!(got, manifest());

    let captured = executor.captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        captured.meta.workspace_root,
        std::path::Path::new("/tmp/ws-proposer")
    );
    assert_eq!(captured.meta.attempt, 1);
    assert_eq!(captured.attempt, captured.meta.attempt, "CE3");
    assert_eq!(captured.kind, NodeKind::Plan);
    assert_eq!(captured.capability.as_str(), "planning");
    assert_eq!(captured.effective_tier, alloy_runtime::ModelTier::Standard);
    assert_eq!(captured.budget, PlannerConfig::new().planning_budget);
    assert_eq!(captured.timeout, Duration::from_millis(PlannerConfig::new().planning_timeout_ms));
    assert!(matches!(
        captured.input.payload,
        NodeInputPayload::Goal(ref g) if g.text == "fix E0308"
    ));
    // AC 14: the cost meter is the run's (PP4), not a fresh one.
    assert!(captured
        .cost_meter
        .shares_state_with(&meters.meter_for(ctx.run_id)));
}

/// AC 15 (PP5/PP6): outcome → `ProposeError` mapping, one arm each.
#[tokio::test]
async fn ac15_propose_error_mapping_per_arm() {
    let meters = Arc::new(ProcessCostMeterFactory::new());
    let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    let failed = |class: ErrorClass| CapabilityOutcome::Failed {
        failure: FailureIr {
            node: alloy_runtime::NodeId::new(),
            error_class: class,
            retry: Default::default(),
            diagnostics: vec![],
            notes: "n".into(),
        },
    };
    let cases: Vec<(Result<CapabilityOutcome, CapabilityExecError>, &str)> = vec![
        (Err(CapabilityExecError::Unavailable), "Unavailable"),
        (
            Err(CapabilityExecError::Internal("x".into())),
            "Unavailable",
        ),
        (Err(CapabilityExecError::Worker("x".into())), "Unavailable"),
        (Err(CapabilityExecError::Timeout), "Timeout"),
        (Err(CapabilityExecError::Cancelled), "Cancelled"),
        (Ok(failed(ErrorClass::Budget)), "Budget"),
        (Ok(failed(ErrorClass::Timeout)), "Timeout"),
        (Ok(failed(ErrorClass::Cancelled)), "Cancelled"),
        (Ok(failed(ErrorClass::Model)), "Model"),
        (Ok(failed(ErrorClass::Tool)), "Model"),
        (
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::json!({"not": "a payload"}),
            }),
            "Malformed",
        ),
        (
            Ok(CapabilityOutcome::Succeeded {
                payload: worker_payload(None),
            }),
            "Malformed",
        ),
    ];
    for (outcome, want) in cases {
        let proposer = proposer_with(
            CapturingExecutor::scripted(outcome) as _,
            &meters,
            CancellationToken::new(),
        );
        let err = proposer.propose(&ctx).await.unwrap_err();
        let got = match err {
            ProposeError::Unavailable(_) => "Unavailable",
            ProposeError::Model(_) => "Model",
            ProposeError::Malformed(_) => "Malformed",
            ProposeError::Budget => "Budget",
            ProposeError::Timeout => "Timeout",
            ProposeError::Cancelled => "Cancelled",
            _ => "other",
        };
        assert_eq!(got, want);
    }
}

/// PP5: a fired token classifies as `Cancelled` even when the observable
/// failure was a timeout — and firing the token aborts an in-flight call.
#[tokio::test]
async fn ac14b_firing_the_token_aborts_an_inflight_planning_call() {
    let meters = Arc::new(ProcessCostMeterFactory::new());
    let token = CancellationToken::new();
    // Timeout surfaced after the token fired → Cancelled (FB2b feed).
    token.cancel();
    let proposer = proposer_with(
        CapturingExecutor::scripted(Err(CapabilityExecError::Timeout)) as _,
        &meters,
        token.clone(),
    );
    let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    let err = proposer.propose(&ctx).await.unwrap_err();
    assert!(matches!(err, ProposeError::Cancelled));

    // In-flight abort: the executor waits on the token; cancel from outside.
    let waiting = Arc::new(CapturingExecutor {
        captured: Mutex::new(None),
        outcome: Mutex::new(None),
        wait_for_cancel: true,
    });
    let token = CancellationToken::new();
    let proposer = proposer_with(Arc::clone(&waiting) as _, &meters, token.clone());
    let ctx2 = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    let handle = tokio::spawn(async move { proposer.propose(&ctx2).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel();
    let err = handle.await.unwrap().unwrap_err();
    assert!(matches!(err, ProposeError::Cancelled));
}

/// FB6: an exhausted budget is denied before any executor call.
#[tokio::test]
async fn fb6_budget_precheck_denies_before_the_call() {
    let meters = Arc::new(ProcessCostMeterFactory::new());
    let ctx = plan_ctx(SessionId::new(), RunId::new(), DagId::new());
    // Exhaust the run's meter against a tiny policy.
    let meter = meters.meter_for(ctx.run_id);
    meter.add_model_usage(
        alloy_runtime::ModelTier::Standard,
        Some(10_000),
        Some(10_000),
        None,
    );
    let executor = CapturingExecutor::ok_with(worker_payload(Some(manifest())));
    let tight = alloy_runtime::BudgetPolicy {
        max_tokens_per_run: 1,
        ..alloy_runtime::BudgetPolicy::default()
    };
    let proposer = CapabilityPlanProposer::new(
        Arc::clone(&executor) as _,
        ProposerDeps {
            workspace_root: std::path::PathBuf::from("/tmp/ws-proposer"),
            cancellation: CancellationToken::new(),
            cost_meters: Arc::clone(&meters) as _,
            budget_policy: tight,
        },
        PlannerConfig::new(),
    );
    let err = proposer.propose(&ctx).await.unwrap_err();
    assert!(matches!(err, ProposeError::Budget));
    assert!(executor.captured.lock().unwrap().is_none(), "no model call");
}
