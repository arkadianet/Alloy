# Sandbox residual risk (RFC-0005 / Architecture V2 §14.2 / ADR F-07)

| Field | Value |
| --- | --- |
| **Author** | arkadianet |
| **Status** | Normative residual-risk record for MVP |

This document records security limitations that remain **after** the Sandbox Broker
is correctly applied. It does not weaken the fail-closed broker contract.

---

## build.rs and procedural macros still execute

`cargo check` / `cargo test` invoked through the broker still compile and run:

- package `build.rs` scripts
- procedural macros

Both execute **inside** the selected sandbox (Landlock/Seatbelt/container), not on
the bare host. They remain a Critical residual (V2 threat model): malicious or
confused-deputy build scripts can still perform work permitted by the jail
(read/write under the workspace, use allowed RO toolchain paths, etc.).

**Mitigations in MVP:**

- `network = deny` (no egress for exfil by default)
- `quarantine_deps = true` (`CARGO_NET_OFFLINE`, block `fetch`/`update`/`install`/…)
- workspace jail + deny-globs for `.env` / keys / SSH / AWS material
- never mount whole `CARGO_HOME` / `RUSTUP_HOME`; credentials bind-over `/dev/null`

**Not mitigated in MVP:** fully disabling build scripts / proc-macros for every
check (optional future wrappers such as stricter `CARGO_BUILD_RUSTC_WRAPPER`
policies).

---

## Deny-glob bind-overs are a spawn-time snapshot

Credential and deny-glob bind-overs (`/dev/null` or empty RO dirs) are computed
**once per `exec`**, from paths that exist under the jail when the broker walks
it. Paths created *after* spawn are **not** retroactively bound over.

The jail itself is Landlock-writable, so deny-glob coverage for **in-jail**
secrets depends on the walk finding them. The walk therefore **fails closed**
when its entry budget is exhausted (refuses the exec) rather than returning a
partial bind list. `target/` and `.git/` are pruned as build/VCS noise;
`node_modules/` is **not** pruned (it commonly holds `.env` files).

**Operator guidance:** keep secrets out of the jail when possible; treat
deny-globs as defense-in-depth on top of fail-closed walks, not a live FS monitor.

---

## Quarantine limits

Quarantine forces offline mode for known cargo subcommands and blocks the
network-facing cargo family (`fetch` / `update` / `install` / …). Other cargo
subcommands are allowed with `CARGO_NET_OFFLINE` only (no `--offline` insert).

**Known holes:** cargo flags such as `--config` / value-taking flags can make
`cargo_subcommand` mis-classify the subcommand, so the **QuarantineBlocked**
denial may not fire (network deny + `CARGO_NET_OFFLINE` still apply). Basename
grants match rustup shims (`cargo` → `rustup`) via invocation authority, but
arbitrary `rustup run` wrappers are out of scope for MVP quarantine rewriting.

---

## Platform notes

- **Landlock** requires ABI ≥ 2 with `CompatLevel::HardRequirement`, plus
  unprivileged user + mount + network namespaces for `network=deny`. Identity-map
  failure at **probe** time marks Landlock `Unavailable` (fail closed on
  `NativeSandboxBroker::new`). Nested user namespaces (some CI / cloud agent
  hosts) may refuse `uid_map` writes → Unavailable even when Landlock ABI exists.
  After binds apply, the broker drops `CAP_SYS_ADMIN` from the bounding/effective
  sets and locks `SECBIT_NOROOT` so the child cannot `umount` deny-glob binds
  (verified by `child_cannot_umount_dotenv_bind`).
- **Seatbelt** uses `/usr/bin/sandbox-exec` (Apple-deprecated). MVP uses a bash
  trampoline for the ready-byte handshake and `exec -a` argv0 preservation;
  arguments are never re-joined into an unquoted `bash -c` string. The SBPL and
  trampoline are written under a broker-owned 0700 directory **outside** the jail
  (never under `.alloy-sbx`), so jail-writable `build.rs` cannot rewrite policy.
- **Container** depends on docker/podman availability and a pinned image
  (`rust:1.97.1-bookworm` by default). Runtime cleanup uses a cidfile drop-guard
  on every exit path. The cidfile and env-file live under the jail (RFC-mandated
  paths) and are therefore visible to the container as RW — a compromised child
  could rewrite the id the broker later signals.

---

## Native `cargo check` layout

Per-exec `CARGO_TARGET_DIR` points at the scratch carve-out. Operator
`CARGO_HOME` is mounted RO for allowlisted subtrees (`registry`, `git`, `bin`,
`config.toml`/`config`, `.package-cache`) plus rustup toolchains. Full online
registry fetches remain blocked by quarantine + netns. The Landlock
`landlock_cargo_check_fixture` proves offline `cargo check` against a path
dependency; registry-dependent checks may still need the Container backend when
operator cache layout differs.

---

## Scratch and bind concurrency

Per-exec trees live under `<jail>/.alloy-sbx/<uuid>/`. Broker-owned bind sources
live under the host temp dir (`alloy-sbx-binds/`), **outside** the jail, so
sandboxed children cannot rewrite deny bind sources. Concurrent execs use unique
UUIDs; operators should not share a single scratch uuid across brokers.

---

## Alloy-on-Alloy dogfood ban

Per ADR F-07 / RFC-0016: **do not** dogfood Alloy-on-Alloy until the sandbox
path is green **and** Milestone-1 holdout eval gates pass. Sandbox-before-dogfood
is non-negotiable.

---

*— arkadianet*
