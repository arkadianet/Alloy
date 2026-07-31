# Live holdout (independent model outputs)

Operator tooling that runs the **real `alloy` binary** against the RFC-0016
**holdout** fixture workspaces with a **live** OpenAI-compatible endpoint.

The evaluator and scorer are implemented in Rust in `alloy-eval`; the shell
wrapper only orchestrates `alloy`, `cargo check`, and the evaluator binary.

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
cargo build -p alloy-eval --bin alloy-eval-live-holdout

# llama.cpp on :8089 (see router.toml.local-example), or Ollama on :11434
MODEL='Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf' \
  BASEURL='http://127.0.0.1:8089/v1/' \
  TEMP=0.6 PROFILE=default REPS=1 \
  ./eval/live-holdout/run.sh /tmp/live-holdout.jsonl
```

Each repetition gets a fresh temp workspace copied from
`crates/alloy-eval/fixtures/holdout/*/workspace`, a rendered `router.toml`,
a git commit, then:

```text
alloy --workspace <tmp> --profile <profile> run "fix the compile error in this crate" --yes
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

JSONL rows and `<results>.report.json` are written outside the repo. The report
validates dense repetition coverage and endpoint identity, and includes
per-fixture and overall Wilson 95% intervals for process, compile, reference,
and strict-oracle rates. Exit `0` from `alloy` alone is no longer sufficient for
the strict oracle, and each row also records `failure_class`, `compile_clean`,
`reference_match`, cargo's post-check exit code, repair-generation count, and
the selected `profile`. Profile is part of endpoint identity, so context/profile
arms cannot be silently mixed in one report.
Exit codes:

| Exit | Meaning |
| --- | --- |
| `0` | Sweep ran; fixture failures are a result |
| `2` | Startup failure (bad config / missing binary), or post-sweep report validation failure (inconsistent observations, repetition gaps) from `score` |
| `3` | At least one repetition could not execute `alloy` |

The strict-oracle score is still operator telemetry only — do not cite it as
the RFC-0016 offline holdout score. `REPS=3` is useful for fast direction
checks; use a larger repetition count when estimating reliability and report
both process and oracle rates. A malformed or incomplete observation file is a
harness error, not a zero-result model score.

## Matrix runs

Use `matrix.sh` to compare model, temperature, or profile/context arms. Each
arm writes a separate JSONL observation file and validated report. The matrix
refuses to compare arms with different fixture sets, corpora, or repetition
counts, and never pools incompatible denominators.

The arms file is tab-separated:

```text
arm_id	model	temperature	profile	base_url	reps
baseline	Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf	0.6	default	http://127.0.0.1:8089/v1/	30
context	Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf	0.6	autonomous	http://127.0.0.1:8089/v1/	30
```

Run it with:

```bash
./eval/live-holdout/matrix.sh \
  /path/to/arms.tsv \
  /tmp/live-holdout-matrix
```

The first arm is the descriptive baseline. `matrix.report.json` retains each
arm's Wilson 95% interval, failure classes, and per-fixture deltas. A positive
strict-oracle delta is evidence of improvement on this measured corpus; zero
or negative deltas are retained as the documented "why not" result, not hidden
by an aggregate score.

## Separation from live-repair

| | `eval/live-holdout` | `eval/live-repair` |
| --- | --- | --- |
| Corpus | RFC-0016 holdout workspaces | Ten single-error operator fixtures |
| Thesis role | Live-BYOM evidence toward MVP / Beta | Reliability telemetry only |
| Scoring | Validated JSON report with Wilson CIs | Wilson CIs via `alloy-eval-live-repair score` |

Author: arkadianet
