# RFC-0012: Context Engine

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001, RFC-0011 |
| Effort | 4–6 person-days |

## Purpose

Assemble bounded `PromptPack`s with citations and domain labels. MVP: **three live domains**—Conversation, WorkingSet, Artifacts—with fixed weights. No embedding index (V2 §8, ADR F-12).

## Scope

### In scope

- `ContextEngine` trait: `assemble` / `compact` / `evict` / `mark_stale`
- Live domains: Conversation, WorkingSet (files + graph projection + diagnostics), Artifacts
- Reserved domain IDs return empty (Architecture, Scratchpad, LongTerm, Planning, …)
- Token budgets and Appendix B weights
- Stale-detection hooks via digests
- Citations in PromptPack for observability hashes

### Out of scope

- Embedding fuzzy recall / eight live domains → deferred
- Model completion → [RFC-0007](./RFC-0007-model-router-provider.md)
- Worker business logic → [RFC-0013](./RFC-0013-capability-registry-workers.md)
- External Memory auto-retrieve → deferred

## Dependencies

- **RFC-0001** — budgets, session/node IDs
- **RFC-0011** — `GraphView` projections into WorkingSet (may be empty early)

## Public API

From V2 §8.1:

```rust
#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn assemble(&self, req: AssembleRequest) -> Result<PromptPack, ContextError>;
    async fn compact(&self, domain: DomainId, strategy: CompactStrategy) -> Result<(), ContextError>;
    async fn evict(&self, policy: EvictPolicy) -> Result<EvictReport, ContextError>;
    async fn mark_stale(&self, summary_id: SummaryId, reason: StaleReason) -> Result<(), ContextError>;
}

pub enum DomainId {
    Conversation,
    WorkingSet,
    Artifacts,
    Architecture,
    Scratchpad,
    LongTerm,
    Planning,
    ProjectLegacyAlias,
}

pub struct AssembleRequest {
    pub session: SessionId,
    pub node: NodeId,
    pub capability: CapabilityId,
    pub token_budget: usize,
    pub must_include: Vec<ContextHandle>,
}
```

## Internal architecture

Module `alloy-runtime::context`. Pulls conversation events, working set files, artifact metadata, graph subgraph projection. Fixed weights from profile.

## Data structures

`PromptPack` { messages/sections, domain labels, citations, content hashes }. Domain weight table.

## State machine

N/A — functional assembly. Stale marks are metadata transitions without a multi-state machine requirement.

## Failure modes

| Failure | Handling |
| --- | --- |
| Stale summaries (R1) | Prefer graph projections; mark_stale; digests |
| Token explosion (R5) | Hard budget; drop lowest-weight first; lazy tools elsewhere |
| Empty graph | WorkingSet still includes files + diagnostics |

## Testing strategy

- Unit: three domains populate; reserved domains empty
- Unit: budget truncation deterministic
- Integration: assemble after diagnostic ingest includes diagnostics citation
- Hash stability for identical inputs

## Acceptance criteria

- [ ] Exactly three live domains in MVP behavior
- [ ] `assemble` returns PromptPack with citations
- [ ] No embedding index
- [ ] Reserved domains empty / unused
- [ ] Profile weights honored

## Estimated implementation effort

**4–6 person-days**.

## Future extensions

- Enable Architecture/Scratchpad/LongTerm when metrics show need; keep PromptPack shape stable
- Aggressive economy summarization measured in Eval
