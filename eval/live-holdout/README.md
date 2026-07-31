# Live holdout (independent model outputs)

Operator tooling that runs the **real `alloy` binary** against the RFC-0016
**holdout** fixture workspaces with a **live** OpenAI-compatible endpoint.

> **This is not the offline holdout gate.** Offline CI (`ScriptedProvider` /
> committed `recordings/*`) stays the milestone falsification target. Results
> here are live-BYOM evidence for MVP honesty and Beta measurement — they MUST
> NOT be quoted as an RFC-0016 offline holdout score, and MUST NOT replace
> `cargo test -p alloy-eval` offline gates.

## Why this exists

Stack-driver integration smoke (`--features stack-driver`) still feeds
control-plane patches from committed recordings. That proves scheduler /
Landlock / EditEngine wiring. It does **not** prove a real model can clear
the holdout diagnostics. This sweep does.

## Running

```bash
cargo build -p alloy-cli --bin alloy
cargo build -p alloy-eval --bin alloy-eval-live-repair

# llama.cpp on :8089 (see router.toml.local-example), or Ollama on :11434
MODEL='Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf' \
  BASEURL='http://127.0.0.1:8089/v1/' \
  TEMP=0.6 REPS=1 \
  ./eval/live-holdout/run.sh /tmp/live-holdout.jsonl
```

Each repetition gets a fresh temp workspace copied from
`crates/alloy-eval/fixtures/holdout/*/workspace`, a rendered `router.toml`,
a git commit, then:

```text
alloy --workspace <tmp> run "fix the compile error in this crate" --yes
```

The runner records two separate results:

* `process_pass` — `alloy` exited `0`.
* `oracle_pass` — the process exited `0`, an independent offline `cargo check
  --offline` is clean, and the final target file exactly matches the fixture's
  committed `<target>.post` reference. The reference is removed from the
  temporary workspace before the model runs, so it is not a hidden prompt.

The reference match is intentionally strict for this small RFC-0016 corpus. It
is what exposes a compiling but semantically wrong morph such as changing E0502
into E0614. A reference mismatch is diagnostic evidence, not proof that all
valid Rust repairs are impossible.

JSONL rows and a short stderr summary are written outside the repo. Exit `0`
from `alloy` alone is no longer sufficient for the strict oracle, and each row
also records `failure_class`, `compile_clean`, `reference_match`, cargo's
post-check exit code, and repair-generation count. Exit codes:

| Exit | Meaning |
| --- | --- |
| `0` | Sweep ran; fixture failures are a result |
| `2` | Broken before start (bad config / missing binary) |
| `3` | At least one repetition could not execute `alloy` |

The strict-oracle score is still operator telemetry only — do not cite it as
the RFC-0016 offline holdout score. `REPS=3` is useful for fast direction
checks; use a larger repetition count when estimating reliability and report
both process and oracle rates.

## Separation from live-repair

| | `eval/live-holdout` | `eval/live-repair` |
| --- | --- | --- |
| Corpus | RFC-0016 holdout workspaces | Ten single-error operator fixtures |
| Thesis role | Live-BYOM evidence toward MVP / Beta | Reliability telemetry only |
| Scoring | Pass/fail + wall time JSONL | Wilson CIs via `alloy-eval-live-repair score` |

Author: arkadianet
