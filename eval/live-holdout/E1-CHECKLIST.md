# E1 checklist — naive vs Alloy default vs Alloy autonomous

Operator procedure for one E1 measurement: one raw-model baseline against both
Alloy profiles, on one binary bundle and one endpoint. Do not skip a step or
reorder it — several steps exist specifically to fail before any model call is
made. See `./README.md` for the underlying schema and scripts this checklist
drives.

## 1. Clean commit

```bash
git status --porcelain
```

Must print nothing. `prepare.sh` refuses a dirty worktree, so commit or stash
first — including `docs/roadmap/**` and `docs/superpowers/**` if they are
modified. This also fixes the `source_revision` every observation will carry.

## 2. Endpoint health and exact model ID

```bash
test -n "${ALLOY_API_KEY:-}" || {
  echo "ALLOY_API_KEY must be a non-empty process environment variable" >&2
  exit 2
}
curl --fail --show-error --silent \
  --header "Authorization: Bearer $ALLOY_API_KEY" \
  http://127.0.0.1:8089/v1/models
```

Set `ALLOY_API_KEY` in the calling shell before this check. A loopback endpoint
that ignores authentication can use `ALLOY_API_KEY=local`; no arm supplies a
fallback key, and neither script reads `.env`.

Copy the served model id **verbatim** into the arms file's `model` column.
Do not abbreviate a quantization suffix or assume the id matches a filename
on disk — `run.sh` and `matrix.sh` send exactly what the arms file says, and a
mismatched id is a silent wrong-model run, not a harness error.

## 3. Hidden-oracle corpus validation

```bash
cargo test -p alloy-eval --test live_holdout_runner --locked \
  committed_post_references_pass_hidden_oracles
```

This applies every fixture's own committed `.post` reference offline and
proves it still compiles and passes that fixture's hidden `oracle-tests/`.
It costs zero model calls. If it fails, the corpus itself is broken — fix the
fixture before spending any live budget on it, or every observation from this
matrix is worthless regardless of model behavior.

## 4. Bundle preparation

```bash
bundle=/tmp/alloy-e1-bundle
./eval/live-holdout/prepare.sh "$bundle"
```

- `$bundle` must live outside the repository (an in-repo bundle is refused
  before anything is built) and its parent directory must already exist.
- Requires the clean commit from step 1; `prepare.sh` re-checks `HEAD` and
  worktree cleanliness immediately after the build and refuses to publish a
  manifest if either moved during compilation.
- Prints `SOURCE_REVISION` and `BINARY_BUNDLE_SHA256`. Every arm run from this
  bundle will carry these two values as `harness` identity in its report —
  record them alongside the result.

## 5. Three-arm preflight

Preflight is not a separate command; it is the first phase of `e1.sh` (step
6). Before any arm runs, `e1.sh` reads every row of the arms file and refuses
the whole run unless it finds **exactly one** `naive/none`, **exactly one**
`alloy/default`, and **exactly one** `alloy/autonomous` role, all sharing one
`model`, `temperature`, `base_url`, and `reps`. The first data row must be
`naive/none`, because `matrix.sh` treats its first report as the comparison
baseline. `matrix.sh` then re-verifies the bundle manifest, that this
checkout's `HEAD` still matches
`SOURCE_REVISION`, and that `eval/live-holdout/`, `profiles/`, and
`crates/alloy-eval/fixtures/holdout/` carry no uncommitted changes, before
requiring a non-empty `ALLOY_API_KEY` and creating the output directory. A
failure at any of these checks leaves no output directory behind — nothing
has run yet.

## 6. Pilot execution at three repetitions

No smaller model is committed to this repository, so the pilot model id is an
**operator input**. Build `$pilot_arms` yourself from the seven-column
example in `arms.example.tsv`: keep all three role rows, set `model` to the
exact id from step 2 for an installed smaller model, and set `reps` to `3` on
all three rows. Do not point `$pilot_arms` at `arms.example.tsv` unedited —
that file specifies the Q4 30B **target** model, not a pilot model, and
`e1.sh` has no "pilot mode" that would substitute one for the other.

```bash
bundle=/tmp/alloy-e1-bundle
out=/tmp/alloy-e1-pilot
pilot_arms=/tmp/alloy-e1-pilot-arms.tsv
./eval/live-holdout/e1.sh \
  "$pilot_arms" \
  "$out" \
  "$bundle"
```

Use the bundle from step 4 while the worktree remains unchanged. If the
worktree changes, choose a new, empty bundle directory and run `prepare.sh`
against that path; never rerun it against an existing bundle. Each unchanged
worktree needs only one `prepare.sh` run per bundle.

## 7. Report and artifact validation

Before trusting a pilot result:

- Every `$out/<arm>.report.json` has `schema_version` 4 and the same
  `endpoint.harness.source_revision` / `binary_bundle_sha256` as the bundle
  printed in step 4 — a mismatch means an arm did not run the bundle you
  built.
- `$out/matrix.report.json` lists all three arms and a Wilson 95% interval
  per arm, not just a point estimate at `reps=3`.
- A malformed or incomplete telemetry record (truncated event export, a
  failed event export, a missing naive result, a naive result that is not
  exactly one model call, or unparsable JSON) makes `run.sh` exit as a
  harness error, not a zero-result model score. Treat that exit as "this
  pilot did not produce evidence," not as "the model scored zero."
- `$out.artifacts/` (per arm) holds model output, run logs, and local paths.
  Treat it as confidential and keep it alongside its `.jsonl`/`.report.json`
  siblings; do not publish it.

If any of the above fails, fix the cause and re-run the pilot into a fresh,
empty output directory — `matrix.sh` refuses to write into a non-empty one.

## 8. Target execution at ten repetitions

Only after step 7 passes, run the committed target contract as-is —
`arms.example.tsv` already specifies the Q4 30B model at `reps=10` on all
three arms:

```bash
bundle=/tmp/alloy-e1-bundle
out=/tmp/alloy-e1-target
./eval/live-holdout/e1.sh \
  eval/live-holdout/arms.example.tsv \
  "$out" \
  "$bundle"
```

Use a fresh, empty `$out` — never the pilot's output directory. Re-run step 7
in full against this run's reports before drawing any conclusion from it.

## 9. Extension only when uncertainty remains

If the target run's Wilson intervals for naive vs. either Alloy profile
overlap, or the strict-oracle deltas are too close to call, extend evidence
by re-running `e1.sh` with a larger `reps` on the **same** model, temperature,
base URL, and bundle, into a new empty output directory. Do not cherry-pick a
subset of repetitions or mix repetition counts across arms — `e1.sh` already
refuses arms with unequal `reps`.

## 10. Explicit uplift or "why not" conclusion

State, for each Alloy profile against the naive baseline, whether the
strict-oracle delta in `matrix.report.json` is positive, and by how much
(with its Wilson interval). A zero or negative delta is a valid, reportable
"why not" result on this corpus — it must be written down, not omitted or
buried under an aggregate score.

## What this checklist does not authorize

- No live endpoint or secret belongs in CI — this checklist is entirely a
  local operator procedure.
- Never read, create, or overwrite `.env`; router configuration comes from
  `router.toml` (copied from `router.toml.local-example`) and the
  `ALLOY_API_KEY` process environment variable.
- Do not start a pilot until you have an installed smaller model id from
  step 2. There is nothing to substitute it with.
