# Alloy

Alloy is a modular AI engineering runtime for software development.

**Author:** arkadianet  
**Architecture:** [`docs/architecture/alloy-architecture-v2.md`](docs/architecture/alloy-architecture-v2.md) (frozen)  
**Implementation RFCs:** [`docs/rfcs/`](docs/rfcs/)  
**MSRV:** Rust 1.85 (workspace `rust-version`)

## Workspace

```text
crates/
  alloy-cli/       # `alloy` binary
  alloy-runtime/   # host + shared IR
  alloy-tools/     # stub (MCP/sandbox later)
  alloy-index/     # stub (ProjectGraph later)
  alloy-eval/      # stub (eval harness later)
```

## Build

```bash
cargo build --workspace
cargo test --workspace
./target/debug/alloy --help
```

Configuration is documented in `example.env`. Alloy never creates or overwrites `.env`.

## Definition of Done

RFC implementations merge only when the [Definition of Done](docs/rfcs/README.md#definition-of-done-merge-gate) is fully met.
