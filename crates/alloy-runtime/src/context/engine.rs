//! The `ContextEngine` trait (V2 §8.1 verbatim) and [`NullContextEngine`]
//! (RFC-0012 §3.3, §3.8).

use async_trait::async_trait;

use crate::router::{ChatMessage, ChatRole, Citation, PromptPack};
use crate::types::ids::{Digest, SummaryId};

use super::error::ContextError;
use super::render::{sanitize_untrusted, system_frame, Section, SectionCitation};
use super::types::{
    AssembleInputs, AssembleRequest, CompactStrategy, DomainId, EvictPolicy, EvictReport,
    StaleReason,
};

/// Bounded prompt assembly over labelled context domains (V2 §8).
#[async_trait]
pub trait ContextEngine: Send + Sync {
    /// Assemble a budgeted, cited `PromptPack`. Deterministic (A1).
    async fn assemble(&self, req: AssembleRequest) -> Result<PromptPack, ContextError>;

    /// Assemble with host-held per-call inputs (RFC-0012 §3.5, consumed by
    /// RFC-0013 workers through the trait object seam).
    ///
    /// Additive default (RFC-0013): engines that do not consume
    /// [`AssembleInputs`] ignore it, preserving the shipped identity
    /// `assemble(req) == assemble_with(req, AssembleInputs::default())`.
    /// [`super::DefaultContextEngine`] overrides this with its inherent
    /// implementation.
    async fn assemble_with(
        &self,
        req: AssembleRequest,
        inputs: AssembleInputs,
    ) -> Result<PromptPack, ContextError> {
        let _ = inputs;
        self.assemble(req).await
    }

    /// Compact a domain. **Stub** in MVP: no-op on a live domain (A12).
    async fn compact(
        &self,
        domain: DomainId,
        strategy: CompactStrategy,
    ) -> Result<(), ContextError>;

    /// Evict memoized projections (§8.3).
    async fn evict(&self, policy: EvictPolicy) -> Result<EvictReport, ContextError>;

    /// Invalidate one memoized projection by id (§8.2).
    async fn mark_stale(
        &self,
        summary_id: SummaryId,
        reason: StaleReason,
    ) -> Result<(), ContextError>;
}

/// Engine that assembles only the system frame and an optional
/// caller-supplied goal. `AssembleRequest` carries no goal (V2 froze it) and
/// this engine holds no stores, so the goal must be injected at
/// construction. Mirrors the null graph's role: available before wiring, in
/// tests, and under `--no-context`.
#[derive(Debug, Default, Clone)]
pub struct NullContextEngine {
    goal: Option<String>,
}

impl NullContextEngine {
    /// Engine whose packs carry `goal` as their only user content.
    #[must_use]
    pub fn with_goal(goal: impl Into<String>) -> Self {
        Self {
            goal: Some(goal.into()),
        }
    }
}

#[async_trait]
impl ContextEngine for NullContextEngine {
    /// With a goal: system frame + the goal text, with citations for both.
    /// Without one (`Default`): `Err(ContextError::EmptyPrompt)` (A15) —
    /// there is no store to fetch a goal from. `token_budget == 0` is E5
    /// either way.
    async fn assemble(&self, req: AssembleRequest) -> Result<PromptPack, ContextError> {
        if req.token_budget == 0 {
            return Err(ContextError::BudgetTooSmall { needed: 1, have: 0 });
        }
        let Some(goal) = &self.goal else {
            return Err(ContextError::EmptyPrompt);
        };
        let system = system_frame(req.capability.as_str());
        let section = Section {
            domain_label: DomainId::Conversation.label(),
            kind: "goal",
            key: String::new(),
            body: sanitize_untrusted(goal),
            fidelity: None,
            citations: vec![SectionCitation {
                source: "alloy://conversation/goal".into(),
                bytes: None,
            }],
        };
        let mut citations = vec![Citation {
            source: "alloy://system/frame".into(),
            digest: Some(Digest::sha256(system.as_bytes())),
        }];
        citations.extend(section.resolved_citations());
        Ok(PromptPack {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: system,
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: section.render(),
                },
            ],
            citations,
            domains: None,
        })
    }

    async fn compact(&self, _d: DomainId, _s: CompactStrategy) -> Result<(), ContextError> {
        Ok(())
    }

    async fn evict(&self, _p: EvictPolicy) -> Result<EvictReport, ContextError> {
        Ok(EvictReport::default())
    }

    async fn mark_stale(&self, id: SummaryId, _r: StaleReason) -> Result<(), ContextError> {
        Err(ContextError::SummaryNotFound(id))
    }
}
