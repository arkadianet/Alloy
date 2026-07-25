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

## Quarantine limits

Quarantine forces offline mode for known cargo subcommands and blocks the
network-facing cargo family. Other cargo subcommands are allowed with
`CARGO_NET_OFFLINE` only (no `--offline` insert). Non-cargo tools are unchanged
aside from the shared env scrub and FS/network isolation.

---

## Platform notes

- **Landlock** requires ABI ≥ 2 with `CompatLevel::HardRequirement`, plus
  unprivileged user + mount + network namespaces for `network=deny`.
- **Seatbelt** uses `/usr/bin/sandbox-exec`, which Apple documents as deprecated;
  treat as best-effort on macOS until a replacement profile path exists.
- **Container** depends on docker/podman availability and a pinned image
  (`rust:1.97.1-bookworm` by default).

---

## Alloy-on-Alloy dogfood ban

Per ADR F-07 / RFC-0016: **do not** dogfood Alloy-on-Alloy until the sandbox
path is green **and** Milestone-1 holdout eval gates pass. Sandbox-before-dogfood
is non-negotiable.

---

*— arkadianet*
