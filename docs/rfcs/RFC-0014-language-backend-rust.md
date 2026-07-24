# RFC-0014: LanguageBackend (Rust Module)

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | arkadianet |
| Architecture | Alloy Architecture V2 (frozen) |
| Depends on | RFC-0001, RFC-0011 |
| Effort | 3–5 person-days |

## Purpose

Keep language-specific logic behind `LanguageBackend`. MVP: Rust-only **internal module** (no dynamic loading, no PY/TS crates, no cdylib). Control-plane traits stay language-agnostic (V2 §16, ADR F-15).

## Scope

### In scope

- `LanguageBackend` trait
- Rust impl: `detect`, `index` (into ProjectGraph), `diagnostics` (cargo check JSON → `DiagnosticEvent`), `test` selector wrapper, `lower_edit` stub fail-closed for unsupported ops
- Wire as internal module in `alloy-runtime` and/or `alloy-index` (V2 §16.1)
- `LanguageManifest` for Rust capabilities

### Out of scope

- Python/TS backends, cdylib, trait freeze ceremony → deferred (≥6 months Rust dogfood)
- Full SemanticEditOp lowering → EditEngine / M3
- Scheduler/MCP changes — none required

## Dependencies

- **RFC-0001** — `LanguageId`, diagnostics IR
- **RFC-0011** — graph index target

## Public API

From V2 §16.1:

```rust
#[async_trait]
pub trait LanguageBackend: Send + Sync {
    fn id(&self) -> LanguageId;
    fn manifest(&self) -> LanguageManifest;
    async fn detect(&self, root: &Path) -> Result<bool, LangError>;
    async fn index(&self, root: &Path, graph: &dyn ProjectGraph) -> Result<(), LangError>;
    async fn diagnostics(&self, root: &Path, scope: Scope) -> Result<Vec<DiagnosticEvent>, LangError>;
    async fn test(&self, root: &Path, sel: TestSelector) -> Result<TestReport, LangError>;
    async fn lower_edit(&self, op: &SemanticEditOp) -> Result<Vec<TextEdit>, LangError>;
    fn capabilities_extended(&self) -> Vec<CapabilityId>;
}
```

## Internal architecture

`RustBackend` uses cargo metadata/syn (shared with index) and sandboxed cargo for diagnostics when invoked from adapters. No plugin loading.

## Data structures

`LanguageManifest` { id, file_extensions, toolchain hints }. `TestReport`, `TextEdit` spans.

## State machine

N/A — synchronous language services invoked by graph rebuild / Verify adapters.

## Failure modes

| Failure | Handling |
| --- | --- |
| Not a Rust workspace | `detect` false; session create may error if only rust configured |
| Unsupported `lower_edit` | Fail closed |
| Premature second language | Out of scope—do not add crates |

## Testing strategy

- Unit: detect Cargo.toml workspace
- Integration: diagnostics parse fixture JSON → DiagnosticEvent
- lower_edit UnsupportedOp
- index populates thin graph nodes

## Acceptance criteria

- [ ] Trait matches V2; Rust-only impl
- [ ] No PY/TS/cdylib packages
- [ ] index/diagnostics integrate with ProjectGraph
- [ ] lower_edit fail-closed for unsupported ops
- [ ] Scheduler/MCP unchanged

## Estimated implementation effort

**3–5 person-days**.

## Future extensions

- Second language after ≥6 months Rust dogfood; freeze trait when second impl forces it
- RA-assisted lower_edit for RenameType (M3)
