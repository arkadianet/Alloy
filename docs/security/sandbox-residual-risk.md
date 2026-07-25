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
partial bind list. `target/` is pruned as build noise; `.git/` is **not** pruned (hooks and
credentials can live there and PathPolicy would deny them). `node_modules/`
is **not** pruned (it commonly holds `.env` files).

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
  After binds apply, the broker also drops `CAP_SYS_ADMIN` and locks
  `SECBIT_NOROOT`. The load-bearing umount defense is the **Landlock domain**
  itself (Landlock's mount hooks deny `umount`/`mount` for sandboxed tasks even
  if a nested userns restores capabilities); the cap drop is defense-in-depth.
  Verified by `child_cannot_umount_dotenv_bind`. Handled FS access rights use
  ABI v2 as a hard floor and best-effort ABI v5 so `truncate(2)` is mediated on
  kernels ≥ 6.2 (verified by `landlock_denies_outside_jail_truncate`).
  **Metadata residual:** Landlock does not mediate `chmod` / `utimes` at any
  ABI — a sandboxed payload that somehow obtains a path to an operator-writable
  file outside the jail cannot open/write/truncate it under the v5 ruleset, but
  could still change mode/mtime if it knew the path. Keep secrets out of
  guessable absolute paths; document as inherent to the Landlock backend.
  **Orphan note:** supervision kills the process group and bounds stdio drains
  by `exec_timeout`. A payload that calls `setsid` (or otherwise leaves the
  session) while holding an inherited pipe can still escape the group signal;
  MVP does not yet add `CLONE_NEWPID` (deferred: needs a private `/proc` mount).
  Container backends already get PID-namespace teardown via the runtime
  `--init`. Operators should treat long-running daemonization inside
  `build.rs` as a residual availability risk until NEWPID lands.
- **Seatbelt** uses `/usr/bin/sandbox-exec` (Apple-deprecated). MVP uses a bash
  trampoline for the ready-byte handshake and `exec -a` argv0 preservation;
  arguments are never re-joined into an unquoted `bash -c` string. The SBPL and
  trampoline are written under a broker-owned 0700 tempdir **outside** the jail
  (never under `.alloy-sbx`), so jail-writable `build.rs` cannot rewrite policy.
  **Host caveat:** on current macOS 26 GitHub runners, `sandbox-exec` SIGABRTs
  when applying deny-default profiles (even a minimal probe of `/usr/bin/true`).
  The probe therefore reports Seatbelt `Unavailable` and
  `NativeSandboxBroker::new` fails closed when `check=seatbelt`. Operators on
  affected hosts should set `check = "container"` (or run Linux Landlock) until
  a non-deprecated Seatbelt/AppSandbox path replaces `sandbox-exec`.
  **CI note:** the macOS job compiles and runs unit/integration tests but does
  not set `ALLOY_REQUIRE_SEATBELT=1`, so Seatbelt enforcement is not proven in
  CI on those hosts (allowed by RFC §11; Container is the practical macOS check).
- **Container** depends on docker/podman availability and a pinned image
  (`rust:1.97.1-bookworm` by default). Probe requires a reachable daemon
  (`info` success); CLI-present-but-daemon-down is `Unavailable`. Runtime
  cleanup kills by broker-chosen `--name` (not the jail-writable cidfile).
  Exec refuses to map a child exit code when the cidfile is empty (no positive
  confirmation a container ran). The cidfile and env-file still live under the
  jail (RFC-mandated paths) and are visible to the container as RW.

---

## Native `cargo check` layout

Sandboxed cargo uses the jail's persistent `target/` directory (no forced
per-exec `CARGO_TARGET_DIR`). Operator `CARGO_HOME` RO grants follow the closed
RFC-0005 §5.5 list (`registry`, `git`, `bin`, `toolchains`, `settings.toml`) —
**not** `config.toml`, which may hold registry tokens the credential bind-over
does not cover. Full online registry fetches remain blocked by quarantine +
netns. The Landlock `landlock_cargo_check_fixture` proves offline `cargo check`
against a path dependency; registry-dependent checks with custom `config.toml`
may need the Container backend or an operator-owned config that does not embed
tokens.

---

## Scratch and bind concurrency

Per-exec trees live under `<jail>/.alloy-sbx/<uuid>/`. Broker-owned bind and
Seatbelt policy sources live in unique `tempfile` directories under the host temp
dir (prefix `alloy-sbx-binds-` / `alloy-sbx-seatbelt-`), **outside** the jail,
mode 0700, removed when the exec returns. Concurrent execs never share a fixed
parent name. (`tempfile` is a production dependency for these RAII dirs; RFC §10
still lists it under `dev` — amend on the next RFC editorial pass.)

---

## Alloy-on-Alloy dogfood ban

Per ADR F-07 / RFC-0016: **do not** dogfood Alloy-on-Alloy until the sandbox
path is green **and** Milestone-1 holdout eval gates pass. Sandbox-before-dogfood
is non-negotiable.

---

*— arkadianet*
