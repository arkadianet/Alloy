# Alloy

A modular **AI engineering runtime** for software development. Models are plugins;
structured execution, tools, and project state are the product.

**Author:** arkadianet  
**Architecture:** [`docs/architecture/alloy-architecture-v2.md`](docs/architecture/alloy-architecture-v2.md) (frozen)  
**Implementation RFCs:** [`docs/rfcs/`](docs/rfcs/) · [roadmap](docs/roadmap/IMPLEMENTATION-ROADMAP.md)  
**MSRV:** Rust 1.97 (`rust-toolchain.toml`)

## Status

Early. The runtime host (RFC-0001), durable SQLite event log / artifact store
(RFC-0002), and session / run control plane (RFC-0003) are in tree. Scheduler,
model router, MCP, and eval remain ahead on the RFC series.

## Workspace

```text
crates/
  alloy-cli/       # `alloy` binary
  alloy-runtime/   # host, session events, storage
  alloy-tools/     # stub (MCP / sandbox later)
  alloy-index/     # stub (ProjectGraph later)
  alloy-eval/      # stub (eval harness later)
```

≤5 crates by design (Architecture V2). No sixth crate for storage.

## Build

```bash
cargo build --workspace
cargo test --workspace
./target/debug/alloy --help
./target/debug/alloy host --workspace .
```

Copy [`example.env`](example.env) only if you want a local `.env` — Alloy never
creates or overwrites `.env`. Optional keys cover data dir, profile, router, and
SQLite storage.

## Docs

| Doc | Role |
| --- | --- |
| [Architecture V2](docs/architecture/alloy-architecture-v2.md) | Implementation contract |
| [RFC index](docs/rfcs/README.md) | Subsystem RFCs + Definition of Done |
| [Roadmap](docs/roadmap/IMPLEMENTATION-ROADMAP.md) | Milestone order |

## Definition of Done

RFC implementations merge only when the
[Definition of Done](docs/rfcs/README.md#definition-of-done-merge-gate) is fully met.
