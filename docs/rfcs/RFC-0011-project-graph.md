# RFC-0011: ProjectGraph (`alloy-index`)

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001, RFC-0002 |
| Effort | 6–10 person-days |

## Purpose

Thin persistent project intelligence graph: Workspace / Crate / Module / Item (+ Diagnostic + FixEvent ingest). Single writer service; workers get read-only `GraphView` in-process—**no** worker `graph_query` MCP (V2 §7, ADR F-02/F-04).

## Scope

### In scope

- `ProjectGraph` trait: rebuild / incremental invalidate / query / record_diagnostic / record_fix / snapshot
- MVP nodes via cargo metadata + syn; diagnostics from check JSON
- Edges: structural Defines/Imports as available
- File digest invalidation of module subgraphs
- Persistence under `.alloy/graph/` (or XDG); `GraphVersion` for sessions
- Stubs: `Callers` / `SimilarFixes` return empty; confidence field reserved

### Out of scope

- Typed call/lifetime layers, SimilarFixes auto-retrieve, Merkle multi-layer → deferred
- External Memory embeddings → deferred (ADR F-23)
- Context PromptPack assembly → [RFC-0012](./RFC-0012-context-engine.md)
- LanguageBackend orchestration details → [RFC-0014](./RFC-0014-language-backend-rust.md)
- Background `alloyd` indexer → deferred

## Dependencies

- **RFC-0001** — graph IDs, `DiagnosticEvent`, `FixEvent`
- **RFC-0002** — SQLite / storage roots patterns

## Public API

From V2 §7.2:

```rust
#[async_trait]
pub trait ProjectGraph: Send + Sync {
    async fn rebuild(&self, root: &Path) -> Result<GraphVersion, GraphError>;
    async fn apply_incremental(&self, changes: &[FileChange]) -> Result<GraphVersion, GraphError>;
    async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError>;
    async fn record_diagnostic(&self, d: DiagnosticEvent) -> Result<(), GraphError>;
    async fn record_fix(&self, f: FixEvent) -> Result<(), GraphError>;
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError>;
}

pub enum GraphQuery {
    Symbol { path: String },
    Refs { node: GraphNodeId },
    Impls { trait_node: GraphNodeId },
    Callers { fn_node: GraphNodeId },
    Diagnostics { crate_id: Option<CrateId>, since: Option<Timestamp> },
    SimilarFixes { diagnostic_code: String, limit: usize },
    Subgraph { seeds: Vec<GraphNodeId>, radius: u8 },
}

pub struct GraphViewHandle { /* read-only query handle for workers */ }
```

Crate: `alloy-index`.

## Internal architecture

Single writer service. RA passthrough for Refs/Impls optional when available; degraded syn/cargo mode required (R16).

## Data structures

SQLite tables for nodes/edges/diagnostics/fixes; edge confidence column reserved; snapshot IDs.

## State machine

N/A for query API. Versioning: `GraphVersion` monotonic on rebuild/incremental; corruption → rebuild from source + quarantine snapshot (V2 §5.6).

## Failure modes

| Failure | Handling |
| --- | --- |
| Graph corruption | Rebuild from source; quarantine snapshot |
| Macro-blind / incomplete parse | Degraded graph; never invent call edges |
| Worker mutation attempt | No mutation API on `GraphViewHandle` |

## Testing strategy

- Unit: rebuild small fixture workspace; Symbol/Diagnostics queries
- Unit: Callers/SimilarFixes empty
- Incremental: file change invalidates module subgraph
- Ingest: record_diagnostic / record_fix round-trip

## Acceptance criteria

- [ ] Trait matches V2; thin MVP nodes only
- [ ] In-process read-only worker handle; ingest-only writes
- [ ] No `graph_query` MCP for Alloy workers
- [ ] Stubs return empty for Callers/SimilarFixes
- [ ] Persists under `.alloy/graph/` (or XDG)

## Estimated implementation effort

**6–10 person-days** (syn + metadata dominate).

## Future extensions

- Confidence-scored call/lifetime edges; SimilarFixes after precision measured (eval fixtures first)
- RA-backed depth; never dual MCP+direct mutation
