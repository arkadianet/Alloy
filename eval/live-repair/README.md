# Live-repair benchmark

Operator tooling that measures the **real `alloy` binary** against a **live**
OpenAI-compatible endpoint on ten single-error Rust crates.

> **This is not a gate.** Live-repair results are operator telemetry. They MUST
> NOT gate a milestone, MUST NOT be quoted as an RFC-0016 holdout score, and
> MUST NOT be used as a prompt-tuning signal that is then reported as a holdout
> number. The RFC-0016 offline holdout gates live entirely elsewhere — see
> "Separation from the offline gates" below.

## Layout

```text
eval/live-repair/
  README.md
  run.sh                       # the only component that spawns or networks
  fixtures/<fixture_id>/
    live-manifest.toml         # id, goal, expected outcome, error-class tags
    LICENSE                    # R17 provenance, same rules as the offline corpus
    workspace/                 # Cargo project snapshot copied per run
      Cargo.toml
      Cargo.lock
      src/main.rs              # exactly one compile error
```

### `live-manifest.toml`

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `live_manifest_version` | `u32` | yes | `1` |
| `id` | string | yes | Matches the directory name; `[a-z0-9_.-]`, ≤128 bytes |
| `goal` | string | yes | Prompt handed to `alloy run`; ≤512 bytes, no tabs/newlines |
| `expected_outcome` | enum | yes | Only `compile_clean` today |
| `tags` | array | yes | Error class, e.g. `["e0384", "mutability"]`; 1–16 entries, `[a-z0-9_.-]` |
| `license.class` | `permitted`/`forbidden` | yes | Same R17 rules as the offline corpus |
| `license.spdx` | string | yes | One of the five permitted SPDX values |
| `license.source_note` | string | yes | Provenance |
| `workspace.path` | string | yes | Relative directory containing the Cargo project |
| `workspace.package` | string | yes | Package name |

The wire DTOs use `serde(deny_unknown_fields)`, so an offline-only key such as
`set = "holdout"` is a hard load error rather than a silent accept.

## Running

```bash
cargo build -p alloy-cli --bin alloy
cargo build -p alloy-eval --bin alloy-eval-live-repair

MODEL=qwen2.5-coder:32b TEMP=0.6 REPS=10 \
  BASEURL=http://127.0.0.1:11434/v1/ \
  ./eval/live-repair/run.sh /tmp/live-repair.jsonl
```

`run.sh` gives every repetition a fresh temporary workspace: it copies the
fixture's `workspace/`, copies the repo `profiles/`, writes the rendered
`router.toml`, makes a git commit, then runs

```text
alloy --workspace <tmp> run "<goal from the manifest>" --yes
```

Exit code `0` is only a process pass. The runner independently executes
`cargo check --offline` against the final workspace and records
`compile_clean`/`cargo_check_exit`; the scored pass requires both the process
exit and a clean post-check. Wall time and the retry-line count scraped from
the run log are recorded alongside it. Nothing is written inside the repository.

Before the first repetition, `run.sh` preflights itself: the scorer and the
`alloy` binary must exist and be executable (and must not answer a probe with
`126`/`127`), and `REPS`, `TIMEOUT`, `TEMP`, `MODEL` and `BASEURL` must parse.
Its exit codes distinguish the two kinds of bad news:

| Exit | Meaning |
| --- | --- |
| `0` | The sweep ran and was scored. Fixtures may have failed — that is a result. |
| `2` | The sweep is broken before it started (bad config, unusable binary) or the observations are invalid. |
| `3` | The sweep ran but at least one repetition could not execute `alloy`; do not publish it. |

### Environment

| Var | Default | Meaning |
| --- | --- | --- |
| `FIXTURES` | `eval/live-repair/fixtures` | Corpus root |
| `MODEL` | `qwen2.5-coder:32b` | Wire model id |
| `TEMP` | `0.6` | Sampling temperature |
| `REPS` | `10` | Repetitions per fixture |
| `BASEURL` | `http://127.0.0.1:11434/v1/` | Endpoint base URL |
| `ALLOY` | `target/debug/alloy` | Path to the real `alloy` binary |
| `SCORER` | `target/debug/alloy-eval-live-repair` | Path to the planner/scorer |
| `TIMEOUT` | `600` | Per-run timeout, seconds |
| `RETRY_PATTERN` | `retrying with fresh diagnostics` | Log line counted as a retry |
| `SCORE` | `1` | Set `0` to skip scoring |

## Outputs

`run.sh` appends one JSON object per repetition to the results file:

```json
{"fixture_id":"missing_mut","repetition":1,"exit_code":0,"compile_clean":true,"cargo_check_exit":0,"retries":1,"wall_ms":18422,"model":"qwen2.5-coder:32b","temperature":0.6,"base_url":"http://127.0.0.1:11434/v1/"}
```

Every row carries the endpoint identity it was produced against, and the scorer
refuses to pool rows whose `model` / `temperature` / `base_url` disagree with
the report's endpoint: two sweeps concatenated by mistake are an error, not an
average.

and then writes `<results>.report.json`, a `LiveRepairReport`:

```text
fixture missing_mut pass=9/10 timeout=0 harness_error=0 retries=3 wilson95=[0.595758,0.982431] tags=e0384,mutability
...
alloy-eval-live-repair run_id=<uuid v4>
offline=false
holdout_gate=not_applicable
endpoint model=qwen2.5-coder:32b temperature=0.600000
overall pass=87 fail=13 timeout=0 harness_error=0
denominator attempts=100 (timeouts included, harness errors excluded)
pass_rate=0.870000
wilson95=[0.789338,0.923136]
retries_total=21 passes_via_retry=14
cost=uncalibrated
cost_disclaimer=internal-only
```

Score an existing results file (or several appended together) without re-running:

```bash
target/debug/alloy-eval-live-repair score \
  --fixtures eval/live-repair/fixtures \
  --observations /tmp/live-repair.jsonl \
  --model qwen2.5-coder:32b --temperature 0.6 \
  --base-url http://127.0.0.1:11434/v1/ \
  --reps 10 \
  --out /tmp/live-repair.report.json
```

### Scoring semantics

* Exit `0` with `compile_clean=true` → `Pass`; exit `0` with a failed
  post-check → `Fail`; exit `124` (killed by `timeout(1)`) → `Timeout`; exit
  `126` / `127` (the binary could not be executed) → `HarnessError`; any other
  non-zero code → `Fail`.
* **A timeout is a failure.** It stays in the pass-rate denominator and is
  reported in its own `timeout=` column. Excluding it would let a run of
  1 pass + 9 timeouts render as a 100% pass rate, which is the opposite of what
  happened: the code was not fixed.
* Only `HarnessError` attempts are excluded from the denominator, because no
  measurement happened at all — and such a run is not published: the scorer
  exits `3` and `run.sh` exits `3` after reporting what it saw.
* Observations are validated before scoring: an unknown fixture id, a duplicate
  `(fixture, repetition)` pair, a gap in the `1..=REPS` sequence, a fixture with
  no rows when `--reps` is declared, or a row from a different endpoint is a
  hard error rather than a quietly smaller sample.
* Every fixture line carries its own Wilson 95% interval, not just `OVERALL`.
* Wilson 95% intervals are ported verbatim from `score.py::wilson`, including
  its `n == 0` degenerate case, and the port is pinned by unit tests against
  the Python reference values.
* An empty population is reported as `unmeasured:empty_sample`, never as a
  measured zero.

`score.py` has been removed; `alloy-eval-live-repair score` replaces it.

## Separation from the offline gates

RFC-0016 makes `alloy-eval` offline by construction. Nothing here changes that.

| Concern | Offline holdout gates | Live-repair benchmark |
| --- | --- | --- |
| Corpus root | `crates/alloy-eval/fixtures/{train,holdout}/` | `eval/live-repair/fixtures/` |
| Manifest file | `manifest.toml` | `live-manifest.toml` |
| Entry point | `EvalHarness` / `evaluate_gate` | `alloy-eval-live-repair` + `run.sh` |
| Report type | `EvalReport` (`alloy-eval run_id=…`, `offline=true`) | `LiveRepairReport` (`alloy-eval-live-repair run_id=…`, `offline=false`, `holdout_gate=not_applicable`) |
| Model | Scripted replay, no network | Live endpoint |
| Gating | Milestone exit gates | None, ever |

Mechanically enforced:

* `LiveRepairCorpus::load` **refuses** any root containing a `train` or
  `holdout` path component, and refuses a fixture that also carries an offline
  `manifest.toml`.
* The offline loader only ever opens `manifest.toml`, so it cannot read a live
  fixture; pointing `EvalHarness` at this corpus yields `FixtureNotFound`.
* `LiveRepairReport` is a distinct type. It cannot be passed to
  `evaluate_gate`, it hard-codes `offline = false`, and its summary always
  renders `holdout_gate=not_applicable`.
* The library and the `alloy-eval-live-repair` binary are pure: no
  `std::process::Command`, no network, no new crate features and no new
  dependencies — RFC-0016 §10.1/§10.2 hold verbatim, and the existing
  `tests/offline_ci.rs` guards still pass unmodified. `run.sh` is the only
  component that executes anything.
* `.github/workflows/eval-holdout-hygiene.yml` is unchanged; this corpus is
  outside its `crates/alloy-eval/fixtures/holdout/*` glob and none of these
  paths are prompt/template tuning surfaces.

Author: arkadianet
