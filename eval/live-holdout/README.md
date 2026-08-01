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

`DRIVER` selects which harness runs against the endpoint (default `alloy`):

```bash
cargo build -p alloy-eval --features live-naive --bin alloy-eval-live-naive
DRIVER=naive MODEL='…' BASEURL='http://127.0.0.1:8089/v1/' TEMP=0.6 REPS=1 \
  ./eval/live-holdout/run.sh /tmp/live-naive.jsonl
```

`DRIVER=naive` is a **one-shot, tool-free baseline**: exactly one completion
per fixture, no tools, no repository index, no replanning, no retry, and no
profile (its `profile` field is always `null`). It exists to give `alloy`'s
orchestration something to be measured against on the same corpus and
endpoint. It is not a second product mode and its low ceiling on a
multi-error or multi-file repair is expected, not a driver bug. `DRIVER=alloy`
runs the real agent under `--profile default` or `--profile autonomous`.
Both drivers share one independent cargo check, hidden-test, reference, and
strict-oracle path, so their reports are comparable observation-for-
observation.

Each repetition gets a fresh temp workspace copied from
`crates/alloy-eval/fixtures/holdout/*/workspace`, a rendered `router.toml`,
a git commit, then:

```text
alloy --workspace <tmp> --profile <profile> run "fix the compile error in this crate" --yes
```

The runner records independent layers:

* `process_pass` — `alloy` exited `0`.
* `compile_clean` — an independent offline `cargo check --offline` passed.
* `tests_pass` — hidden fixture-owned semantic tests, copied in only after the
  model run, passed under `cargo test --offline`.
* `oracle_pass` — the process exited `0`, independent offline `cargo check` and
  hidden semantic tests passed, and the final target file exactly matches the
  fixture's committed `<target>.post` reference. The reference is removed from
  the temporary workspace before the model runs, so it is not a hidden prompt.

Semantic tests are required independent evidence and do not replace or relax
the strict reference match. A compile-clean mismatch that fails tests is a
detected likely morph; a mismatch that passes tests is a plausible alternate
repair that still remains a strict-reference failure.

The reference match is intentionally strict for this small RFC-0016 corpus. It
is what exposes a compiling but semantically wrong morph such as changing E0502
into E0614. A reference mismatch is diagnostic evidence, not proof that all
valid Rust repairs are impossible.

JSONL rows and `<results>.report.json` are written outside the repo. Report
schema v4 adds independent semantic-test rates, durable evidence pointers,
`driver` identity, and build provenance. The report validates dense
repetition coverage and endpoint identity, and includes per-fixture and
overall Wilson 95% intervals for process, compile, tests, reference,
strict-oracle, compile-clean mismatch, compile-clean test failure, and
test-passing reference-mismatch rates. Exit `0` from `alloy` alone is no
longer sufficient for the strict oracle, and each row also records
`failure_class`, cargo's post-check/test exit codes, repair-generation count,
model-call/token telemetry, and the selected `profile`.

Endpoint identity — the fields that must match for two reports to be one
arm — is `model`, `temperature`, `base_url`, `driver`, `profile`, and
`harness`. `driver` (`naive` or `alloy`) and `profile` (`none` for naive,
`default`/`autonomous` for alloy) are both part of that identity, so a naive
run and an agent run, or two agent runs on different profiles, are never
silently pooled into one report. `harness` is `{source_revision,
binary_bundle_sha256}` — build provenance, described next — so a rebuild
into a shared Cargo target directory can never be mistaken for the binaries
that actually produced a report.

A malformed or incomplete observation — a naive result that is not exactly
one model call, an event export at the page limit, unparsable telemetry
JSON — is a harness error (`run.sh` exits non-zero before scoring), not a
zero-result model score. Do not record an aborted sweep as evidence.

Every attempt writes `<results-without-.jsonl>.artifacts/<fixture>/rep-N/` with
the model run log, final target, patch, independent cargo logs, event export,
and metadata. `evidence_relpath` in each observation points below that root.
Temporary workspaces are removed after the evidence bundle is complete.
These bundles contain model output and local paths; treat them as confidential
operator artifacts and review them before sharing. Move the JSONL and its
`.artifacts/` sibling together to preserve evidence pointers.
Exit codes:

| Exit | Meaning |
| --- | --- |
| `0` | Sweep ran; fixture failures are a result |
| `2` | Startup failure (bad config / missing binary), or post-sweep report validation failure (inconsistent observations, repetition gaps) from `score` |
| `3` | At least one repetition could not execute `alloy` |

The strict-oracle score is still operator telemetry only — do not cite it as
the RFC-0016 offline holdout score. Three repetitions are useful for fast
direction checks; use a larger repetition count when estimating reliability
and report both process and oracle rates.

## Bundle identity

`matrix.sh` and `e1.sh` never resolve binaries by searching a Cargo target
directory — every arm of a matrix must run the exact same four binaries, or
"two arms" could silently be two codebases. `prepare.sh` builds that
guarantee once, from one commit:

```bash
./eval/live-holdout/prepare.sh /path/to/bundle
```

`prepare.sh` requires a clean worktree at a resolvable `HEAD`, and the bundle
path must be outside the repository (its parent directory must already
exist) — a bundle built inside the repo would dirty the very worktree the
script requires to be clean. It builds `alloy`, `alloy-eval-live-holdout`,
`alloy-eval-live-naive`, and `alloy-eval-live-repair` into
`<bundle>/target/debug/`, re-confirms `HEAD` did not move and the worktree
stayed clean during the build, then writes `<bundle>/manifest.tsv`: the
40-hex `source_revision`, a `worktree clean` marker, and one sha256 per
binary. That manifest's own sha256 is `binary_bundle_sha256`. A manifest is
written only for a complete bundle, so an interrupted build leaves nothing
that `matrix.sh` will accept.

## Matrix runs

Use `matrix.sh` to compare model, temperature, driver, or profile arms — any
mix is comparable; it is the generic comparator. `e1.sh` (next section) adds
the stricter naive-vs-default-vs-autonomous contract on top of it. Each arm
writes a separate JSONL observation file and validated report. The matrix
refuses to compare arms with different fixture sets, corpora, or repetition
counts, and never pools incompatible denominators.

Every arm now also runs from one verified binary bundle, so `matrix.sh` takes
a third argument:

```bash
./eval/live-holdout/matrix.sh \
  /path/to/arms.tsv \
  /tmp/live-holdout-matrix \
  /path/to/bundle
```

Before opening the arms file, `matrix.sh` verifies the bundle manifest
(revision, clean-worktree marker, and every binary's sha256), then checks
that this checkout's `HEAD` still matches `manifest.tsv`'s `source_revision`
and that `eval/live-holdout/`, `profiles/`, and
`crates/alloy-eval/fixtures/holdout/` have no uncommitted changes — the
bundle pins the binaries, but this checkout still supplies the orchestration,
profiles, and fixture oracles those binaries run against. It also refuses a
non-empty output directory so a stale report is never silently overwritten.
Any of these failing leaves no output directory behind.

The arms file is seven tab-separated columns:

```text
arm_id	driver	model	temperature	profile	base_url	reps
baseline	alloy	Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf	0.6	default	http://127.0.0.1:8089/v1/	30
autonomous-profile	alloy	Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf	0.6	autonomous	http://127.0.0.1:8089/v1/	30
```

The first arm is the descriptive baseline. `matrix.report.json` retains each
arm's Wilson 95% interval, failure classes, compile/test mismatch rates, and
per-fixture deltas. A positive strict-oracle delta is evidence of improvement
on this measured corpus; zero
or negative deltas are retained as the documented "why not" result, not hidden
by an aggregate score.

## E1: the three-arm operator contract

`e1.sh` wraps `matrix.sh` with the one contract this repository ships an
operator checklist for: exactly one `naive`/`none` arm, one `alloy`/`default`
arm, and one `alloy`/`autonomous` arm, sharing one `model`, `temperature`,
`base_url`, and `reps` — the only thing allowed to vary is the treatment
(driver + profile) itself. Arm ids are free-form; the role is derived from
`driver`+`profile`, not the id.

```bash
./eval/live-holdout/e1.sh /path/to/e1-arms.tsv /tmp/e1-out /path/to/bundle
```

`arms.example.tsv` is the committed target contract for this comparison
(Q4 30B model, `reps=10`). Follow `./E1-CHECKLIST.md` end to end before
running it: it covers endpoint health, hidden-oracle corpus validation, the
pilot run (an operator-supplied smaller model at `reps=3` — never the
target model silently substituted), report/artifact review, the target run,
and when to extend for more evidence.

## Separation from live-repair

| | `eval/live-holdout` | `eval/live-repair` |
| --- | --- | --- |
| Corpus | RFC-0016 holdout workspaces | Ten single-error operator fixtures |
| Thesis role | Live-BYOM evidence toward MVP / Beta | Reliability telemetry only |
| Scoring | Validated JSON report with Wilson CIs | Wilson CIs via `alloy-eval-live-repair score` |

Author: arkadianet
