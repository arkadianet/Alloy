//! `RegistryCapabilityExecutor` — the sole production implementation of
//! RFC-0010's merged `CapabilityExecutor` seam (RFC-0013 §3.4, §4.3).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::adapters::{
    CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityOutcome,
};

use super::deps::CapabilityContext;
use super::registry::{CapabilityRegistry, ResolveHints};

/// Sole production `CapabilityExecutor` (RFC-0010 §3.8).
///
/// Holds the registry only; per-worker dependencies live on the registry's
/// [`super::WorkerDeps`] (constructor injection), which is what keeps the
/// merged `CapabilityExecContext` unchanged (AC 11).
pub struct RegistryCapabilityExecutor {
    registry: Arc<CapabilityRegistry>,
}

impl std::fmt::Debug for RegistryCapabilityExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCapabilityExecutor")
            .field("registry", &self.registry)
            .finish()
    }
}

impl RegistryCapabilityExecutor {
    /// Wrap an immutable registry (RG8).
    #[must_use]
    pub fn new(registry: Arc<CapabilityRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl CapabilityExecutor for RegistryCapabilityExecutor {
    /// Normative order X1–X9 (§4.3). The executor never retries, never
    /// rewrites `failure.node` (RFC-0010 CE2 owns that), and never inspects
    /// or transforms a `Succeeded` payload (AC 13).
    async fn execute(
        &self,
        ctx: &CapabilityExecContext,
    ) -> Result<CapabilityOutcome, CapabilityExecError> {
        // X1: RFC-0010 CE3 — the dispatch attempt is carried twice; they
        // must agree.
        if ctx.attempt != ctx.meta.attempt {
            return Err(CapabilityExecError::Internal(format!(
                "attempt mismatch: ctx {} != meta {}",
                ctx.attempt, ctx.meta.attempt
            )));
        }
        // X2: envelope schema.
        if !ctx.input.is_supported_schema() {
            return Err(CapabilityExecError::Internal(
                "unsupported envelope schema".into(),
            ));
        }
        // X3: fail-closed resolve (RG5, §4.2).
        let cap = self
            .registry
            .resolve(&ctx.capability, &ResolveHints)
            .map_err(|e| CapabilityExecError::Internal(e.to_string()))?;
        // X4: kind agreement.
        if !cap.accepts_kind(ctx.kind) {
            return Err(CapabilityExecError::Internal(format!(
                "capability/kind mismatch: {} does not accept {:?}",
                ctx.capability, ctx.kind
            )));
        }
        // X5: one cancellation check before any work.
        if ctx.cancellation.is_cancelled() {
            return Err(CapabilityExecError::Cancelled);
        }
        // X6: run-scoped router bound to this run's meter (MR1/BG1).
        let Some(deps) = self.registry.deps() else {
            return Err(CapabilityExecError::Internal(
                "registry constructed without worker deps".into(),
            ));
        };
        let router = deps
            .routers
            .router_for(ctx.meta.run_id, &ctx.cost_meter)
            .map_err(|e| CapabilityExecError::Internal(format!("router_for: {e}")))?;
        // X7: build the per-call worker context (infallible).
        let worker_ctx = CapabilityContext {
            session: ctx.meta.session_id,
            run: ctx.meta.run_id,
            dag: ctx.meta.dag_id,
            node: ctx.meta.node_id,
            attempt: ctx.attempt,
            workspace_root: &ctx.meta.workspace_root,
            capability: ctx.capability.clone(),
            kind: ctx.kind,
            effective_tier: ctx.effective_tier,
            budget: ctx.budget.clone(),
            deadline: ctx.timeout,
            cancel: ctx.cancellation.clone(),
            input: &ctx.input,
            prior_failure: ctx.prior_failure.as_ref(),
            router,
            context: Arc::clone(&deps.context),
            tools: Arc::clone(&deps.tools),
            perms: Arc::clone(&deps.perms),
            graph: deps.graph.clone(),
            artifacts: Arc::clone(&deps.artifacts),
            decisions: Arc::clone(&deps.decisions),
            cost_meter: ctx.cost_meter.clone(),
            started: Instant::now(),
        };
        // X8: race the worker against cancellation. No timer here — RFC-0010
        // owns the node deadline and already wraps dispatch in
        // `tokio::time::timeout`.
        tokio::select! {
            () = ctx.cancellation.cancelled() => Err(CapabilityExecError::Cancelled),
            // X9: the worker's outcome is returned verbatim.
            outcome = cap.execute(&worker_ctx) => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use crate::adapters::NodeExecRef;
    use crate::dag::{NodeInputEnvelope, NodeInputPayload, NodeKind, ENVELOPE_SCHEMA_VERSION};
    use crate::obs::SharedCostMeter;
    use crate::types::budget::{Goal, ModelTier, TokenBudget};
    use crate::types::ids::{CapabilityId, DagId, NodeId, RunId, SessionId};
    use crate::types::tools::ToolSelector;

    use super::super::deps::CapabilityContext;
    use super::super::traits::{
        Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass,
    };
    use super::*;

    /// A worker that must never run.
    struct PanicCap;

    #[async_trait]
    impl Capability for PanicCap {
        fn id(&self) -> CapabilityId {
            CapabilityId::new("repair").unwrap()
        }
        fn version(&self) -> CapabilityVersion {
            CapabilityVersion::new(1, 0, 0)
        }
        fn describe(&self) -> CapabilityDescriptor {
            CapabilityDescriptor {
                id: self.id(),
                version: self.version(),
                summary: "test".into(),
                uses_model: false,
                side_effects: SideEffectClass::Pure,
                kinds: vec![NodeKind::Analyze],
            }
        }
        fn required_tools(&self) -> Vec<ToolSelector> {
            vec![]
        }
        fn preferred_tier(&self) -> ModelTier {
            ModelTier::Standard
        }
        fn accepts_kind(&self, kind: NodeKind) -> bool {
            kind == NodeKind::Analyze
        }
        async fn execute(
            &self,
            _ctx: &CapabilityContext<'_>,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            panic!("worker must not be called");
        }
    }

    fn exec_ctx(kind: NodeKind) -> CapabilityExecContext {
        let dag_id = DagId::new();
        let node_id = NodeId::new();
        CapabilityExecContext {
            meta: NodeExecRef {
                session_id: SessionId::new(),
                run_id: RunId::new(),
                dag_id,
                node_id,
                workspace_root: std::path::PathBuf::from("/tmp/ws"),
                attempt: 1,
            },
            cancellation: CancellationToken::new(),
            capability: CapabilityId::new("repair").unwrap(),
            kind,
            effective_tier: ModelTier::Standard,
            budget: TokenBudget {
                max_input: 1000,
                max_output: 1000,
            },
            timeout: Duration::from_secs(30),
            input: NodeInputEnvelope {
                schema_version: ENVELOPE_SCHEMA_VERSION,
                dag_id,
                node_id,
                kind,
                generation: 1,
                payload: NodeInputPayload::Goal(Goal {
                    text: "fix".into(),
                    constraints: vec![],
                    attachments: vec![],
                }),
            },
            attempt: 1,
            cost_meter: SharedCostMeter::new(),
            prior_failure: None,
        }
    }

    fn executor_with_panic_cap() -> RegistryCapabilityExecutor {
        let mut registry = CapabilityRegistry::new();
        registry.register(Arc::new(PanicCap)).unwrap();
        RegistryCapabilityExecutor::new(Arc::new(registry))
    }

    #[tokio::test]
    async fn executor_maps_unknown_capability_to_internal() {
        // §4.2: loud, non-retried stop.
        let executor = RegistryCapabilityExecutor::new(Arc::new(CapabilityRegistry::new()));
        let err = executor
            .execute(&exec_ctx(NodeKind::Analyze))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CapabilityExecError::Internal(m) if m.contains("unknown capability")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn executor_rejects_attempt_mismatch_and_bad_envelope_schema() {
        // X1 / X2.
        let executor = executor_with_panic_cap();
        let mut ctx = exec_ctx(NodeKind::Analyze);
        ctx.attempt = 2; // meta says 1.
        let err = executor.execute(&ctx).await.unwrap_err();
        assert!(matches!(&err, CapabilityExecError::Internal(m) if m.contains("attempt")));

        let mut ctx = exec_ctx(NodeKind::Analyze);
        ctx.input.schema_version = 99;
        let err = executor.execute(&ctx).await.unwrap_err();
        assert!(matches!(&err, CapabilityExecError::Internal(m) if m.contains("schema")));
    }

    #[tokio::test]
    async fn executor_rejects_capability_kind_mismatch() {
        // X4.
        let executor = executor_with_panic_cap();
        let ctx = exec_ctx(NodeKind::Edit); // PanicCap only accepts Analyze.
        let err = executor.execute(&ctx).await.unwrap_err();
        assert!(matches!(&err, CapabilityExecError::Internal(m) if m.contains("mismatch")));
    }

    #[tokio::test]
    async fn executor_returns_cancelled_without_calling_worker() {
        // X5 / BG6: PanicCap proves the worker future is never polled.
        let executor = executor_with_panic_cap();
        let ctx = exec_ctx(NodeKind::Analyze);
        ctx.cancellation.cancel();
        let err = executor.execute(&ctx).await.unwrap_err();
        assert!(matches!(err, CapabilityExecError::Cancelled));
    }

    #[tokio::test]
    async fn executor_without_deps_is_internal_before_worker_dispatch() {
        // X6: a registry with no composition-root deps fails closed.
        let executor = executor_with_panic_cap();
        let ctx = exec_ctx(NodeKind::Analyze);
        let err = executor.execute(&ctx).await.unwrap_err();
        assert!(matches!(&err, CapabilityExecError::Internal(m) if m.contains("worker deps")));
    }
}
