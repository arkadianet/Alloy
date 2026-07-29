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

Exit code `0` is a pass. Wall time and the retry-line count scraped from the
run log are recorded alongside it. Nothing is written inside the repository.

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
{"fixture_id":"missing_mut","repetition":1,"exit_code":0,"retries":1,"wall_ms":18422}
```

and then writes `<results>.report.json`, a `LiveRepairReport`:

```text
fixture missing_mut pass=9/10 error=0 retries=3 wilson95=[0.595758,0.982431] tags=e0384,mutability
...
alloy-eval-live-repair run_id=<uuid v4>
offline=false
holdout_gate=not_applicable
endpoint model=qwen2.5-coder:32b temperature=0.600000
overall pass=87 fail=13 error=0
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
  --out /tmp/live-repair.report.json
```

### Scoring semantics

* Exit `0` → `Pass`; exit `124` / `126` / `127` (timeout, or the binary could
  not be executed) → `Error`; any other non-zero code → `Fail`. This reuses the
  offline `Pass | Fail | Error` vocabulary.
* `Error` attempts are **excluded** from the pass-rate denominator, exactly as
  RFC-0016 excludes `Error` fixtures from `success_rate`. The retired
  `score.py` counted every attempt; that is the one deliberate divergence.
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
