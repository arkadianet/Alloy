# AGENTS.md

Author: arkadianet

## Cursor Cloud specific instructions

Alloy is a single Rust Cargo workspace (binary `alloy`, crates under `crates/`). It is a
CLI with an embedded SQLite datastore — there are no long-running network services and no
ports to expose. Standard build/lint/test/doc commands live in `README.md` and
`.github/workflows/ci.yml`; use those rather than inventing new ones.

Non-obvious notes:

- Toolchain: the VM's default rustup toolchain may be older (e.g. 1.83), but
  `rust-toolchain.toml` pins `1.97.1` and rustup auto-selects it for any command run inside
  the repo. Do not `rustup default` a different version; run cargo from the repo root.
- Tests: CI runs `cargo test --workspace` with `ALLOY_API_KEY` **unset**. Match that —
  the router fails closed when a key is present but invalid, so leaving a stale key set can
  change test behavior.
- `.env`: Alloy never reads or writes `.env`. Environment variables must be exported into
  the process; `example.env` only documents keys. Never overwrite a user's `.env`.
- Running the app: `alloy host` is the only fully-working end-to-end path (pre-alpha). It
  needs an active `router.toml` (copy from `router.toml.example`); `router.toml` is
  gitignored/user-owned and safe to create locally. `host` idles until Ctrl-C/SIGTERM, then
  drains cleanly and writes its durable log under `<workspace>/.alloy`.
- Actual model runs (`alloy run`) require an OpenAI-compatible endpoint configured in
  `router.toml` plus `ALLOY_API_KEY`; not needed for build, test, or the `host` smoke test.
