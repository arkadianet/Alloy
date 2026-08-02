//! Composition-root worker dependencies and the per-call worker context
//! (RFC-0013 §3.5–§3.9).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::adapters::NodeExecRef;
use crate::context::ContextEngine;
use crate::dag::NodeInputEnvelope;
use crate::graph::GraphViewHandle;
use crate::obs::{DecisionLog, SharedCostMeter};
use crate::router::{
    ModelProvider, ModelRouter, RouterConfig, RouterError, TomlModelRouter, TomlModelRouterParts,
};
use crate::storage::{ArtifactStore, SessionRows};
use crate::types::budget::{BudgetPolicy, ModelTier, TokenBudget};
use crate::types::diagnostic::FailureIr;
use crate::types::ids::{CapabilityId, DagId, NodeId, RunId, SessionId};
use crate::ToolCaller;

use super::perms::WorkerPermissions;

/// Everything a worker needs that is *not* per-attempt. Cloneable (all `Arc`).
///
/// Constructor-injected by the composition root (RFC-0015, §3.8) — never
/// carried on the merged `CapabilityExecContext` (whose only post-RFC-0010
/// addition is the per-attempt `prior_failure` retry memory).
#[derive(Clone)]
pub struct WorkerDeps {
    /// Run-scoped router provider (§3.7).
    pub routers: Arc<dyn RunRouterProvider>,
    /// Prompt assembly (RFC-0012).
    pub context: Arc<dyn ContextEngine>,
    /// The only tool seam (RFC-0006 / RFC-0010 M5).
    pub tools: Arc<dyn ToolCaller>,
    /// Host-owned permission minting (§11).
    pub perms: Arc<dyn WorkerPermissions>,
    /// Read-only graph (RFC-0011 SEC1). `GraphViewHandle::null()` when
    /// `--no-graph`.
    pub graph: GraphViewHandle,
    /// Prompt / patch / review artifacts (RFC-0002).
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Decision records (RFC-0004). Model-call records stay router-owned
    /// (BG2 / AM-0007-1).
    pub decisions: Arc<dyn DecisionLog>,
    /// Session rows: workspace root and profile lookups.
    pub sessions: Arc<dyn SessionRows>,
    /// Worker-side knobs (§3.9).
    pub config: WorkerConfig,
}

impl std::fmt::Debug for WorkerDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerDeps")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Worker-side knobs (§3.9). Profile-sourcing is RFC-0015's concern; this
/// RFC defines the struct and its defaults only (Q7).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerConfig {
    /// Max model turns per attempt, including parse/ops/dry-run repair
    /// turns (PS6 / EW6). Default 3.
    pub max_model_turns: u8,
    /// Max tool calls per attempt (TL4).
    pub max_tool_calls: u8,
    /// Max bytes of a single tool result fed back into a prompt (PR6).
    pub max_tool_result_bytes: usize,
    /// Whether the patch builtin runs `dry_run` first (EW6).
    pub validate_before_apply: bool,
    /// Whether the Review capability is registered (V2 §9.3 "optional").
    pub enable_review: bool,
}

impl Default for WorkerConfig {
    /// §3.9 defaults: 3 turns, 8 tool calls, 16 KiB tool-result feedback,
    /// dry-run first, review registered.
    ///
    /// `max_model_turns = 3` lets Edit spend an author turn plus both an
    /// ops-repair and a dry-run-repair turn inside one scheduler attempt
    /// (EW6 / PS6), instead of terminalizing after the first repair.
    fn default() -> Self {
        Self {
            max_model_turns: 3,
            max_tool_calls: 8,
            max_tool_result_bytes: 16 * 1024,
            validate_before_apply: true,
            enable_review: true,
        }
    }
}

/// One attempt's worker-facing context (§3.6, V2 §9.2 shape with AM-V2-3/5).
///
/// Built by [`super::RegistryCapabilityExecutor`] from the merged
/// `CapabilityExecContext` plus [`WorkerDeps`]; borrows the envelope and
/// workspace root from the scheduler's context for the attempt's lifetime.
pub struct CapabilityContext<'a> {
    // --- identity (from `NodeExecRef`) ---
    /// Owning session.
    pub session: SessionId,
    /// Owning run.
    pub run: RunId,
    /// Owning DAG.
    pub dag: DagId,
    /// Dispatched node.
    pub node: NodeId,
    /// 1-based attempt index.
    pub attempt: u32,
    /// Workspace root (jail).
    pub workspace_root: &'a Path,

    // --- dispatch parameters ---
    /// Dispatched capability id.
    pub capability: CapabilityId,
    /// Dispatched node kind.
    pub kind: crate::dag::NodeKind,
    /// Post-escalation tier. Overrides `preferred_tier` (MR2).
    pub effective_tier: ModelTier,
    /// Per-node token budget.
    pub budget: TokenBudget,
    /// Node deadline already clamped by the remaining run budget.
    pub deadline: Duration,
    /// Cancellation token (BG6).
    pub cancel: CancellationToken,

    // --- input ---
    /// Decoded input envelope.
    pub input: &'a NodeInputEnvelope,
    /// Terminal failure of this node's previous scheduler attempt, when one
    /// was captured (retry memory). Absent on first attempts and on resumed
    /// attempts whose prior outcome was not captured. Workers forward it to
    /// `AssembleInputs.prior_failure`; the context engine bounds the
    /// rendering (`PRIOR_FAILURE_MAX_BYTES`).
    pub prior_failure: Option<&'a FailureIr>,

    // --- seams ---
    /// Router bound to `run` and to `cost_meter` (MR1, BG1).
    pub router: Arc<dyn ModelRouter>,
    /// Prompt assembly (PR1).
    pub context: Arc<dyn ContextEngine>,
    /// Tool bus (TL1).
    pub tools: Arc<dyn ToolCaller>,
    /// Permission minting (PM1).
    pub perms: Arc<dyn WorkerPermissions>,
    /// Read-only graph handle.
    pub graph: GraphViewHandle,
    /// Artifact CAS.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Decision log — `worker_attempt` records only (OB1/OB3).
    pub decisions: Arc<dyn DecisionLog>,
    /// Run-scoped meter. Read-only to workers (BG2).
    pub cost_meter: SharedCostMeter,

    /// Attempt start instant, set by the executor at construction; the basis
    /// for [`CapabilityContext::remaining`] (BG5).
    pub(crate) started: Instant,
}

impl CapabilityContext<'_> {
    /// `NodeExecRef` for permission minting and tool attribution.
    #[must_use]
    pub fn exec_ref(&self) -> NodeExecRef {
        NodeExecRef {
            session_id: self.session,
            run_id: self.run,
            dag_id: self.dag,
            node_id: self.node,
            workspace_root: self.workspace_root.to_path_buf(),
            attempt: self.attempt,
        }
    }

    /// Remaining wall-clock before the node deadline (BG5). Cooperative:
    /// RFC-0010 also enforces the deadline externally.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_sub(self.started.elapsed())
    }

    /// `true` once cancellation is observed (BG6).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl std::fmt::Debug for CapabilityContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityContext")
            .field("session", &self.session)
            .field("run", &self.run)
            .field("node", &self.node)
            .field("attempt", &self.attempt)
            .field("capability", &self.capability)
            .field("kind", &self.kind)
            .field("effective_tier", &self.effective_tier)
            .finish_non_exhaustive()
    }
}

/// Run-scoped `ModelRouter` provider, mirroring `CostMeterFactory` (§3.7).
///
/// The production `TomlModelRouter` is **run-bound** (`bound_run` +
/// `cost_meter` in `TomlModelRouterParts`), so a process-wide singleton
/// cannot serve two runs without corrupting attribution. This seam memoizes
/// one router per `RunId`.
pub trait RunRouterProvider: Send + Sync {
    /// Return the router for `run`, constructing it against `meter` on first
    /// use.
    ///
    /// MUST return the same instance for repeated calls with the same
    /// `RunId` in a process, and MUST bind the router to `meter` so RFC-0007
    /// meters into the same `SharedCostMeter` the scheduler handed the
    /// worker (BG1). A later call with a *different* meter for the same run
    /// is a wiring fault and MUST be an error, never a silent rebind.
    fn router_for(
        &self,
        run: RunId,
        meter: &SharedCostMeter,
    ) -> Result<Arc<dyn ModelRouter>, RouterError>;

    /// Drop the memoized router for a finished run (host-scheduled, like
    /// `ProcessCostMeterFactory::release` — RFC-0015 owns the call site).
    fn release(&self, run: RunId);
}

/// One memoized run router plus the meter it was bound to (BG1).
type RunRouterEntry = (Arc<dyn ModelRouter>, SharedCostMeter);

/// Process-local [`RunRouterProvider`] over a validated `RouterConfig` and
/// one provider (Q9).
pub struct ProcessRunRouterProvider {
    config: RouterConfig,
    provider: Arc<dyn ModelProvider>,
    budget_policy: BudgetPolicy,
    decisions: Option<Arc<dyn DecisionLog>>,
    routers: Mutex<HashMap<RunId, RunRouterEntry>>,
}

impl std::fmt::Debug for ProcessRunRouterProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRunRouterProvider")
            .finish_non_exhaustive()
    }
}

impl ProcessRunRouterProvider {
    /// Build over a validated config, a provider, the run budget policy, and
    /// the decision log RFC-0007 records model calls into (AM-0007-1).
    #[must_use]
    pub fn new(
        config: RouterConfig,
        provider: Arc<dyn ModelProvider>,
        budget_policy: BudgetPolicy,
        decisions: Option<Arc<dyn DecisionLog>>,
    ) -> Self {
        Self {
            config,
            provider,
            budget_policy,
            decisions,
            routers: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, RunRouterEntry>> {
        self.routers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RunRouterProvider for ProcessRunRouterProvider {
    fn router_for(
        &self,
        run: RunId,
        meter: &SharedCostMeter,
    ) -> Result<Arc<dyn ModelRouter>, RouterError> {
        let mut routers = self.lock();
        if let Some((router, bound_meter)) = routers.get(&run) {
            // BG1: a memoized router is only valid for the meter it was
            // built against; a mismatch means two meters exist for one run.
            if !bound_meter.shares_state_with(meter) {
                return Err(RouterError::Internal(format!(
                    "router/meter mismatch for run {run}"
                )));
            }
            return Ok(Arc::clone(router));
        }
        let router = TomlModelRouter::from_parts(TomlModelRouterParts::new(
            self.config.clone(),
            Arc::clone(&self.provider),
            self.budget_policy.clone(),
            self.decisions.clone(),
            Some(meter.clone()),
            Some(run),
        ))?;
        let router: Arc<dyn ModelRouter> = Arc::new(router);
        routers.insert(run, (Arc::clone(&router), meter.clone()));
        Ok(router)
    }

    fn release(&self, run: RunId) {
        self.lock().remove(&run);
    }
}
