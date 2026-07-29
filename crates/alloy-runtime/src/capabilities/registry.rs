//! Trivial-resolve capability registry (RFC-0013 §4, V2 §9.2). Fails closed.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dag::{expected_capability, NodeKind};
use crate::types::ids::CapabilityId;
use crate::types::tools::ToolSelector;

use super::deps::WorkerDeps;
use super::traits::{Capability, CapabilityDescriptor};
use super::workers::{EditWorker, PlanningWorker, RepairWorker, ReviewWorker};
use super::{CAPABILITY_CATALOG, MAX_LLM_CAPABILITIES};

/// Resolution hints. Empty in MVP; the seam for future scoring (RG5,
/// **Stub** per V2 §9.2 "trivial resolve").
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ResolveHints;

/// Registry failure taxonomy (§3.3).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegError {
    /// Id was never registered; resolution fails closed (RG5).
    #[error("unknown capability: {0}")]
    Unknown(CapabilityId),
    /// Id already registered; registration is not idempotent (RG4).
    #[error("duplicate capability: {0}")]
    Duplicate(CapabilityId),
    /// Id is outside the closed catalog (RG2).
    #[error("capability not in catalog: {0}")]
    NotInCatalog(CapabilityId),
    /// A fifth registration was attempted (RG1).
    #[error("capability limit exceeded: {0} > {max}", max = MAX_LLM_CAPABILITIES)]
    TooMany(usize),
    /// `accepts_kind` disagrees with the RFC-0009 validation map (RG3).
    #[error("capability {id} does not accept node kind {kind:?}")]
    KindMismatch {
        /// Offending id.
        id: CapabilityId,
        /// Kind the catalog maps to this id.
        kind: NodeKind,
    },
    /// A declared selector is outside the worker-callable builtins (RG6).
    #[error("capability {id} declares unregistered tool selector")]
    UnknownToolSelector {
        /// Offending id.
        id: CapabilityId,
    },
}

/// Tool names workers may declare (RG6 ∩ SEC1/TL7): of the four registered
/// builtins, the two verification tools are runtime-adapter-only, so the
/// worker-declarable set is exactly these.
const WORKER_TOOL_NAMES: [&str; 2] = ["fs_read", "apply_patch"];

/// Trivial-resolve registry (V2 §9.2). Immutable once handed to the
/// executor (RG8): no interior mutability, no hot reload.
#[derive(Default)]
pub struct CapabilityRegistry {
    impls: BTreeMap<CapabilityId, Arc<dyn Capability>>,
    /// Composition-root deps the executor builds per-call contexts from.
    /// `None` for hand-assembled test registries that never execute.
    deps: Option<WorkerDeps>,
}

impl std::fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityRegistry")
            .field("ids", &self.ids())
            .finish_non_exhaustive()
    }
}

impl CapabilityRegistry {
    /// Empty registry without worker deps (registration-rule tests; an
    /// executor over this fails closed at dispatch).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach composition-root deps (used by [`CapabilityRegistry::mvp`] and
    /// by tests that drive the executor with recording doubles).
    #[must_use]
    pub fn with_deps(mut self, deps: WorkerDeps) -> Self {
        self.deps = Some(deps);
        self
    }

    pub(crate) fn deps(&self) -> Option<&WorkerDeps> {
        self.deps.as_ref()
    }

    /// Register one implementation. Fails closed on catalog violations
    /// (RG1/RG2/RG3/RG4/RG6).
    pub fn register(&mut self, cap: Arc<dyn Capability>) -> Result<(), RegError> {
        let id = cap.id();
        // RG1 first: a fifth registration is TooMany regardless of its id.
        if self.impls.len() >= MAX_LLM_CAPABILITIES {
            return Err(RegError::TooMany(self.impls.len() + 1));
        }
        if !CAPABILITY_CATALOG.contains(&id.as_str()) {
            return Err(RegError::NotInCatalog(id));
        }
        if self.impls.contains_key(&id) {
            return Err(RegError::Duplicate(id));
        }
        // RG3: agree with the RFC-0009 kind ↔ capability validation map.
        for kind in [
            NodeKind::Plan,
            NodeKind::Analyze,
            NodeKind::Edit,
            NodeKind::Review,
        ] {
            let expected = expected_capability(kind) == Some(id.as_str());
            if expected != cap.accepts_kind(kind) {
                return Err(RegError::KindMismatch { id, kind });
            }
        }
        // RG6/SEC5: only exact-name selectors over the worker-callable
        // builtins; tags, `graph_query`-style names, and shell-ish names all
        // fail closed here.
        for selector in cap.required_tools() {
            let ok = matches!(
                &selector,
                ToolSelector::Name { name } if WORKER_TOOL_NAMES.contains(&name.as_str())
            );
            if !ok {
                return Err(RegError::UnknownToolSelector { id });
            }
        }
        self.impls.insert(id, cap);
        Ok(())
    }

    /// Resolve by id. `hints` is accepted and ignored in MVP (RG5).
    pub fn resolve(
        &self,
        id: &CapabilityId,
        hints: &ResolveHints,
    ) -> Result<Arc<dyn Capability>, RegError> {
        let _ = hints;
        self.impls
            .get(id)
            .cloned()
            .ok_or_else(|| RegError::Unknown(id.clone()))
    }

    /// Registered ids, sorted. Used by tests and `alloy capabilities`.
    #[must_use]
    pub fn ids(&self) -> Vec<CapabilityId> {
        self.impls.keys().cloned().collect()
    }

    /// Descriptors, sorted by id.
    #[must_use]
    pub fn describe_all(&self) -> Vec<CapabilityDescriptor> {
        self.impls.values().map(|c| c.describe()).collect()
    }

    /// Day-1 production registry: all four MVP workers, in catalog order,
    /// skipping `review` when `WorkerConfig.enable_review == false` (RG7).
    /// Returns the first error rather than a partially registered registry.
    pub fn mvp(deps: WorkerDeps) -> Result<Self, RegError> {
        let config = deps.config.clone();
        let mut registry = Self::new().with_deps(deps);
        // CAPABILITY_CATALOG order: planning, repair, edit, review.
        registry.register(Arc::new(PlanningWorker::new(config.clone())))?;
        registry.register(Arc::new(RepairWorker::new(config.clone())))?;
        registry.register(Arc::new(EditWorker::new(config.clone())))?;
        if config.enable_review {
            registry.register(Arc::new(ReviewWorker::new(config)))?;
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::adapters::{CapabilityExecError, CapabilityOutcome};
    use crate::types::budget::ModelTier;

    use super::super::deps::CapabilityContext;
    use super::super::traits::{
        Capability, CapabilityDescriptor, CapabilityVersion, SideEffectClass,
    };
    use super::super::workers::{EditWorker, PlanningWorker, RepairWorker, ReviewWorker};
    use super::super::WorkerConfig;
    use super::*;

    /// Minimal catalog-id capability for registration-rule tests.
    struct TestCap {
        id: &'static str,
        kind: NodeKind,
        tools: Vec<ToolSelector>,
    }

    impl TestCap {
        fn cataloged(id: &'static str) -> Self {
            let kind = match id {
                "planning" => NodeKind::Plan,
                "repair" => NodeKind::Analyze,
                "edit" => NodeKind::Edit,
                _ => NodeKind::Review,
            };
            Self {
                id,
                kind,
                tools: vec![],
            }
        }
    }

    #[async_trait]
    impl Capability for TestCap {
        fn id(&self) -> CapabilityId {
            CapabilityId::new(self.id).unwrap()
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
                kinds: vec![self.kind],
            }
        }
        fn required_tools(&self) -> Vec<ToolSelector> {
            self.tools.clone()
        }
        fn preferred_tier(&self) -> ModelTier {
            ModelTier::Standard
        }
        fn accepts_kind(&self, kind: NodeKind) -> bool {
            kind == self.kind
        }
        async fn execute(
            &self,
            _ctx: &CapabilityContext<'_>,
        ) -> Result<CapabilityOutcome, CapabilityExecError> {
            Ok(CapabilityOutcome::Succeeded {
                payload: serde_json::Value::Null,
            })
        }
    }

    #[test]
    fn registry_resolves_registered_capability() {
        // T1 / RG5.
        let mut registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(TestCap::cataloged("repair")))
            .unwrap();
        let id = CapabilityId::new("repair").unwrap();
        let cap = registry.resolve(&id, &ResolveHints).unwrap();
        assert_eq!(cap.id(), id);
        assert_eq!(registry.ids(), vec![id]);
    }

    #[test]
    fn registry_unknown_id_fails_closed() {
        // T1 / RG5 / §4.2: no default worker is substituted.
        let registry = CapabilityRegistry::new();
        let id = CapabilityId::new("repair").unwrap();
        match registry.resolve(&id, &ResolveHints) {
            Err(err) => assert_eq!(err, RegError::Unknown(id)),
            Ok(_) => panic!("unregistered id must not resolve"),
        }
    }

    #[test]
    fn registry_rejects_fifth_capability_and_non_catalog_id() {
        // T2 / RG1 / RG2 / SEC6.
        let mut registry = CapabilityRegistry::new();
        for id in CAPABILITY_CATALOG {
            registry.register(Arc::new(TestCap::cataloged(id))).unwrap();
        }
        assert_eq!(
            registry
                .register(Arc::new(TestCap::cataloged("repair")))
                .unwrap_err(),
            RegError::TooMany(5)
        );

        let mut registry = CapabilityRegistry::new();
        let err = registry
            .register(Arc::new(TestCap {
                id: "benchmarking",
                kind: NodeKind::Review,
                tools: vec![],
            }))
            .unwrap_err();
        assert_eq!(
            err,
            RegError::NotInCatalog(CapabilityId::new("benchmarking").unwrap())
        );
    }

    #[test]
    fn registry_rejects_duplicate_registration() {
        // RG4: not idempotent.
        let mut registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(TestCap::cataloged("edit")))
            .unwrap();
        assert_eq!(
            registry
                .register(Arc::new(TestCap::cataloged("edit")))
                .unwrap_err(),
            RegError::Duplicate(CapabilityId::new("edit").unwrap())
        );
    }

    #[test]
    fn registry_rejects_kind_mismatch() {
        // RG3.
        let mut registry = CapabilityRegistry::new();
        let err = registry
            .register(Arc::new(TestCap {
                id: "repair",
                kind: NodeKind::Edit, // disagrees with the validation map.
                tools: vec![],
            }))
            .unwrap_err();
        assert!(matches!(err, RegError::KindMismatch { .. }));
    }

    #[test]
    fn registry_rejects_unknown_tool_selector() {
        // RG6/SEC5: tags and non-builtin names fail closed.
        let mut registry = CapabilityRegistry::new();
        let err = registry
            .register(Arc::new(TestCap {
                id: "repair",
                kind: NodeKind::Analyze,
                tools: vec![ToolSelector::tag("sel.compiler")],
            }))
            .unwrap_err();
        assert!(matches!(err, RegError::UnknownToolSelector { .. }));

        let mut registry = CapabilityRegistry::new();
        let err = registry
            .register(Arc::new(TestCap {
                id: "repair",
                kind: NodeKind::Analyze,
                tools: vec![ToolSelector::name(
                    crate::types::tools::ToolName::new("curl").unwrap(),
                )],
            }))
            .unwrap_err();
        assert!(matches!(err, RegError::UnknownToolSelector { .. }));
    }

    #[test]
    fn catalog_kind_map_matches_dag_validate_expected_capability() {
        // T5 / RG3: the two tables agree, and the real workers agree with
        // both.
        let table = [
            (NodeKind::Plan, "planning"),
            (NodeKind::Analyze, "repair"),
            (NodeKind::Edit, "edit"),
            (NodeKind::Review, "review"),
        ];
        let config = WorkerConfig::default();
        let workers: Vec<Arc<dyn Capability>> = vec![
            Arc::new(PlanningWorker::new(config.clone())),
            Arc::new(RepairWorker::new(config.clone())),
            Arc::new(EditWorker::new(config.clone())),
            Arc::new(ReviewWorker::new(config)),
        ];
        for (kind, id) in table {
            assert_eq!(expected_capability(kind), Some(id));
            for worker in &workers {
                assert_eq!(
                    worker.accepts_kind(kind),
                    worker.id().as_str() == id,
                    "worker {} kind {kind:?}",
                    worker.id()
                );
            }
        }
    }
}
