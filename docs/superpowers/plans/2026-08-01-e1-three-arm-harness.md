# E1 Three-Arm Live Holdout Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a reproducible live holdout harness that compares one raw model call with Alloy default and Alloy autonomous using identical endpoint settings and independent semantic scoring.

**Architecture:** Keep shell responsible for process orchestration and Rust responsible for model protocol, validation, scoring, and comparison. Add a one-shot, tool-free Rust naive driver; route all three arms through the existing workspace isolation and oracle path; and require an explicit, hashed binary bundle so a matrix cannot mix binaries rebuilt from different commits.

**Tech Stack:** Rust, Tokio, `alloy-runtime` OpenAI-compatible provider, Bash orchestration, JSONL reports, Cargo integration tests.

## Global Constraints

- Rust remains the only implementation language; shell is limited to operator orchestration.
- Never read, create, or overwrite `.env`; use process environment and `example.env` documentation only.
- Hidden `.post` references and `oracle-tests/*.rs` never enter a model-visible workspace before model execution completes.
- Naive means exactly one model completion, no Alloy tools, no repository index, no replanning, and no retry.
- Model, quantization, temperature, base URL, fixtures, and repetitions are identical across compared arms.
- Every matrix uses binaries built once from one clean Git commit in a dedicated Cargo target directory.
- Process, compile, semantic-test, reference-match, and strict-oracle outcomes remain independent.
- Tests are written and observed failing before production changes.
- Commit steps run only with explicit operator authorization; otherwise stop after validation with changes uncommitted.

---

## File Map

**Create:**
- `crates/alloy-eval/src/live_naive.rs` — pure prompt, schema, response, and safe replacement logic.
- `crates/alloy-eval/src/bin/alloy-eval-live-naive.rs` — one-shot OpenAI-compatible network entry point.
- `crates/alloy-eval/tests/live_naive_runner.rs` — stub-endpoint integration coverage for the naive binary.
- `crates/alloy-eval/tests/live_holdout_matrix.rs` — shell-level bundle and three-arm matrix contract tests.
- `eval/live-holdout/prepare.sh` — clean-commit binary bundle builder and manifest writer.
- `eval/live-holdout/e1.sh` — exact naive/default/autonomous E1 preflight and matrix wrapper.
- `eval/live-holdout/E1-CHECKLIST.md` — exact pilot/target operator procedure and result template.

**Modify:**
- `crates/alloy-eval/Cargo.toml` — `live-naive` feature, binary gate, and integration-test gate.
- `crates/alloy-eval/src/lib.rs` — export live-naive types and helpers.
- `crates/alloy-eval/src/live_holdout.rs` — schema v4 arm identity, provenance, telemetry, and comparison validation.
- `crates/alloy-eval/src/bin/alloy-eval-live-holdout.rs` — parse/render schema v4 fields and telemetry.
- `crates/alloy-eval/tests/live_holdout_runner.rs` — cover naive and Alloy dispatch through the common oracle.
- `eval/live-holdout/run.sh` — dispatch one arm while retaining one common post-check/oracle path.
- `eval/live-holdout/matrix.sh` — consume an explicit bundle and reject incompatible or stale runs.
- `eval/live-holdout/arms.example.tsv` — three equal E1 arms.
- `eval/live-holdout/README.md` — bundle, matrix, pilot, and target instructions.
- `.github/workflows/ci.yml` — compile and test the feature-gated naive binary.

---

### Task 1: Add Explicit Arm Identity and Provenance to Report Schema v4

**Files:**
- Modify: `crates/alloy-eval/src/live_holdout.rs:16-131`
- Modify: `crates/alloy-eval/src/bin/alloy-eval-live-holdout.rs:19-216`
- Test: `crates/alloy-eval/src/live_holdout.rs:600-803`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveHoldoutDriver {
    Naive,
    Alloy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessIdentity {
    pub source_revision: String,
    pub binary_bundle_sha256: String,
}
```

- Extends `StrictObservation` with `driver`, `harness`, `model_calls`, `tokens_in`, and `tokens_out`.
- Extends `Endpoint` with `driver`, `profile: Option<String>`, and `harness`.
- Extends `FixtureSummary` with model-call and token totals.
- Bumps `REPORT_SCHEMA_VERSION` from `3` to `4`.

- [ ] **Step 1: Write failing schema and comparison tests**

Add tests proving:

```rust
#[test]
fn score_rejects_driver_or_harness_mixing() {
    let fixtures = fixtures_with(&["a"]);
    let mut row = observation("a", 1);
    row.harness.source_revision = "other".to_owned();
    let error = score(fixtures.path(), vec![row], endpoint(), 1).unwrap_err();
    assert!(error.contains("harness identity mismatch"), "{error}");
}

#[test]
fn compare_requires_same_harness_identity() {
    let baseline = report_for_driver(LiveHoldoutDriver::Naive, None);
    let mut candidate = report_for_driver(
        LiveHoldoutDriver::Alloy,
        Some("default".to_owned()),
    );
    candidate.endpoint.harness.source_revision = "other".to_owned();
    let error = compare(vec![
        ("naive".to_owned(), baseline),
        ("alloy-default".to_owned(), candidate),
    ])
    .unwrap_err();
    assert!(error.contains("harness identity mismatch"), "{error}");
}
```

Also assert that a naive endpoint rejects a profile and an Alloy endpoint requires `default` or `autonomous`.

- [ ] **Step 2: Run the focused tests and verify red**

Run:

```bash
cargo test -p alloy-eval live_holdout::tests:: --lib --locked
```

Expected: compilation fails because the schema types and fields do not exist.

- [ ] **Step 3: Implement schema v4 and fail-closed validation**

Use one identity predicate in `score`:

```rust
fn endpoint_matches(row: &StrictObservation, endpoint: &Endpoint) -> bool {
    row.model == endpoint.model
        && row.temperature == endpoint.temperature
        && row.base_url == endpoint.base_url
        && row.driver == endpoint.driver
        && row.profile == endpoint.profile
        && row.harness == endpoint.harness
}
```

In `compare`, require equal corpus, fixture IDs, repetitions, and harness identity. Preserve the existing generic matrix capability to compare model, temperature, driver, and profile treatments; the dedicated E1 wrapper in Task 4 enforces E1's stricter equal-endpoint contract.

- [ ] **Step 4: Update CLI parsing and rendering**

Require these score arguments:

```text
--driver <naive|alloy>
--profile <none|default|autonomous>
--source-revision <40-hex-sha>
--binary-bundle-sha256 <64-hex-sha>
```

Render driver/profile and model-call totals in both per-arm and matrix summaries.

- [ ] **Step 5: Run schema tests**

Run:

```bash
cargo test -p alloy-eval live_holdout::tests:: --lib --locked
```

Expected: all live-holdout unit tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/alloy-eval/src/live_holdout.rs \
  crates/alloy-eval/src/bin/alloy-eval-live-holdout.rs
git commit -m "feat(eval): bind live reports to arm and harness identity"
```

---

### Task 2: Implement the One-Shot Raw Model Driver

**Files:**
- Create: `crates/alloy-eval/src/live_naive.rs`
- Create: `crates/alloy-eval/src/bin/alloy-eval-live-naive.rs`
- Create: `crates/alloy-eval/tests/live_naive_runner.rs`
- Modify: `crates/alloy-eval/src/lib.rs`
- Modify: `crates/alloy-eval/Cargo.toml`

**Interfaces:**
- Consumes: a copied workspace, a validated relative target path, initial Cargo diagnostics, goal, and endpoint settings.
- Produces:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NaiveReplacement {
    pub replacement: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NaiveRunTelemetry {
    pub model_calls: u32,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub provider_request_id: Option<String>,
    pub finish_reason: Option<String>,
}
```

- Binary contract:

```text
alloy-eval-live-naive
  --workspace <dir>
  --target <relative-path>
  --diagnostics <path>
  --goal <text>
  --model <id>
  --temperature <f64>
  --base-url <url>
  --result <json-path>
```

- [ ] **Step 1: Add feature-gated binary/test declarations**

Add:

```toml
[features]
live-naive = ["alloy-runtime/http-provider"]

[[bin]]
name = "alloy-eval-live-naive"
path = "src/bin/alloy-eval-live-naive.rs"
required-features = ["live-naive"]

[[test]]
name = "live_naive_runner"
required-features = ["live-naive"]
```

- [ ] **Step 2: Write failing pure tests**

Test that the prompt contains only the goal, initial target source, and diagnostics; that the JSON Schema permits only `replacement`; and that empty or oversized replacements fail.

```rust
#[test]
fn prompt_and_schema_expose_no_oracle_inputs() {
    let request = build_naive_request(
        "fix the compile error",
        "src/lib.rs",
        "pub fn broken() { missing }",
        "error[E0425]: cannot find value `missing`",
        0.6,
    )
    .unwrap();
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(!encoded.contains(".post"));
    assert!(!encoded.contains("oracle-tests"));
    assert!(encoded.contains("src/lib.rs"));
}
```

- [ ] **Step 3: Run pure tests and verify red**

Run:

```bash
cargo test -p alloy-eval --features live-naive live_naive::tests:: --lib --locked
```

Expected: compilation fails because `live_naive` does not exist.

- [ ] **Step 4: Implement pure request and replacement handling**

Build a `CompletionRequest` with:

```rust
ResponseFormat::JsonSchema {
    name: "alloy_naive_replacement".to_owned(),
    schema: serde_json::json!({
        "type": "object",
        "properties": {
            "replacement": { "type": "string", "minLength": 1 }
        },
        "required": ["replacement"],
        "additionalProperties": false
    }),
}
```

Use one system message stating that the model has one attempt and no tools. Write the replacement through a sibling temporary file followed by `std::fs::rename`; reject absolute targets, parent traversal, symlink targets, empty replacement, and replacement larger than 1 MiB.

- [ ] **Step 5: Write the failing HTTP integration test**

Start a loopback stub server that records requests and returns:

```json
{
  "id": "naive-1",
  "choices": [{
    "message": {
      "content": "{\"replacement\":\"pub fn repaired() -> i32 { 42 }\\n\"}"
    },
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 100,
    "completion_tokens": 20
  }
}
```

Run the real binary against a temporary workspace and assert:
- exactly one request,
- no tools in the request,
- target replaced,
- telemetry reports one model call and `100/20` tokens,
- a sibling `.post` sentinel is absent from the request body.

- [ ] **Step 6: Implement the network binary**

Construct `OpenAiCompatibleProvider` with `ALLOY_API_KEY`, a single `ModelEndpoint`, and the pure request. Never print or serialize the API key. Persist bounded provider metadata to `--result`.

- [ ] **Step 7: Run naive-driver tests**

Run:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_naive_runner --locked -- --nocapture
cargo test -p alloy-eval --features live-naive \
  live_naive::tests:: --lib --locked
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/alloy-eval/Cargo.toml \
  crates/alloy-eval/src/lib.rs \
  crates/alloy-eval/src/live_naive.rs \
  crates/alloy-eval/src/bin/alloy-eval-live-naive.rs \
  crates/alloy-eval/tests/live_naive_runner.rs
git commit -m "feat(eval): add one-shot live naive driver"
```

---

### Task 3: Route Naive and Alloy Through One Oracle Pipeline

**Files:**
- Modify: `eval/live-holdout/run.sh:16-254`
- Modify: `crates/alloy-eval/tests/live_holdout_runner.rs`
- Modify: `crates/alloy-eval/src/bin/alloy-eval-live-holdout.rs`

**Interfaces:**
- Consumes `DRIVER=naive|alloy`.
- Requires `NAIVE` only for `DRIVER=naive` and `ALLOY` only for `DRIVER=alloy`.
- Produces the same JSONL observation and artifact layout for both drivers.

- [ ] **Step 1: Extend the shell integration fixture with a stub naive binary**

The stub must:
- assert that `<target>.post` and `tests/` are absent,
- read `initial-cargo.log`,
- write one replacement,
- write `naive-result.json` with one model call.

Run one naive and one Alloy fixture through `run.sh`, then assert both reports carry the correct `driver`, profile, harness identity, telemetry, and evidence paths.

- [ ] **Step 2: Run the runner integration test and verify red**

Run:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_holdout_runner --locked -- --nocapture
```

Expected: failure because `run.sh` has no driver dispatch or telemetry fields.

- [ ] **Step 3: Add common pre-run diagnostics**

Move per-attempt evidence-directory creation before driver execution. Before either driver executes:

```bash
(cd "$ws" && timeout "$TIMEOUT" \
  cargo check --offline --message-format=short) \
  >"$evidence/initial-cargo.log" 2>&1 || true
```

This file is model-visible only through the naive prompt. It contains no golden or semantic-test data.

- [ ] **Step 4: Add driver dispatch**

Use:

```bash
case "$DRIVER" in
  naive)
    "$NAIVE" \
      --workspace "$ws" \
      --target "$target_path" \
      --diagnostics "$evidence/initial-cargo.log" \
      --goal "$GOAL" \
      --model "$MODEL" \
      --temperature "$TEMP" \
      --base-url "$BASEURL" \
      --result "$evidence/naive-result.json"
    ;;
  alloy)
    ALLOY_API_KEY="${ALLOY_API_KEY:-local}" timeout "$TIMEOUT" \
      "$ALLOY" --workspace "$ws" --profile "$PROFILE" run "$GOAL" --yes
    ;;
esac
```

Do not duplicate post-check logic. Both branches continue into the existing independent `cargo check`, hidden-test copy, `cargo test`, reference match, and strict oracle.

- [ ] **Step 5: Add telemetry extraction**

Add a Rust CLI subcommand:

```text
alloy-eval-live-holdout telemetry
  --driver <naive|alloy>
  --input <naive-result.json|events.jsonl>
```

For Alloy, count `model_call` events and sum present token fields. For naive, validate `NaiveRunTelemetry`. Missing usage remains `null`; it never becomes zero.

- [ ] **Step 6: Make evidence layouts explicit**

Every arm retains:

```text
initial-cargo.log
run.log
final-target.rs
patch.diff
cargo-check.log
cargo-test.log
events.jsonl          # Alloy, empty documented file for naive
naive-result.json     # naive, absent for Alloy
metadata.json
```

- [ ] **Step 7: Run runner and scorer tests**

Run:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_holdout_runner --locked -- --nocapture
cargo test -p alloy-eval --features live-naive \
  live_holdout::tests:: --lib --locked
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add eval/live-holdout/run.sh \
  crates/alloy-eval/src/bin/alloy-eval-live-holdout.rs \
  crates/alloy-eval/tests/live_holdout_runner.rs
git commit -m "feat(eval): share strict oracle across live drivers"
```

---

### Task 4: Build and Enforce a Single-Commit Binary Bundle

**Files:**
- Create: `eval/live-holdout/prepare.sh`
- Create: `eval/live-holdout/e1.sh`
- Create: `crates/alloy-eval/tests/live_holdout_matrix.rs`
- Modify: `eval/live-holdout/matrix.sh`
- Modify: `crates/alloy-eval/Cargo.toml`

**Interfaces:**
- `prepare.sh <bundle-dir>` produces:

```text
<bundle-dir>/target/debug/alloy
<bundle-dir>/target/debug/alloy-eval-live-repair
<bundle-dir>/target/debug/alloy-eval-live-holdout
<bundle-dir>/target/debug/alloy-eval-live-naive
<bundle-dir>/manifest.tsv
```

- `manifest.tsv` contains source revision, dirty state (`clean` only), and SHA-256 for each binary.
- `binary_bundle_sha256` is the SHA-256 of the completed canonical `manifest.tsv` bytes; binary rows are sorted by binary name before the manifest is hashed.
- `matrix.sh <arms.tsv> <out-dir> <bundle-dir>` remains a generic multi-arm comparator but accepts no fallback binaries.
- `e1.sh <arms.tsv> <out-dir> <bundle-dir>` requires exactly naive/default/autonomous with equal model, temperature, base URL, fixtures, and repetitions before invoking `matrix.sh`.

- [ ] **Step 1: Write failing bundle/matrix tests**

Create fake executable files and a manifest in a temporary bundle. Assert that `matrix.sh` rejects:
- missing manifest,
- dirty source marker,
- changed binary hash,
- duplicate arm ID,
- non-empty output directory containing stale reports.

Assert that generic `matrix.sh` still accepts a valid two-arm model or temperature comparison. Separately assert that `e1.sh` rejects a missing role or mismatched model, temperature, base URL, or repetitions, and accepts exactly:

```text
naive           naive  <model> <temp> none        <base-url> <reps>
alloy-default   alloy  <model> <temp> default     <base-url> <reps>
alloy-autonomous alloy <model> <temp> autonomous  <base-url> <reps>
```

- [ ] **Step 2: Run matrix tests and verify red**

Run:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_holdout_matrix --locked -- --nocapture
```

Expected: failure because `prepare.sh`, `e1.sh`, bundle manifests, and the seven-column arms contract do not exist.

- [ ] **Step 3: Implement `prepare.sh`**

Require a clean Git worktree and a non-existing or empty bundle directory. Build once:

```bash
CARGO_TARGET_DIR="$bundle/target" cargo build --locked \
  -p alloy-cli --bin alloy \
  -p alloy-eval --features live-naive \
  --bin alloy-eval-live-repair \
  --bin alloy-eval-live-holdout \
  --bin alloy-eval-live-naive
```

Write `manifest.tsv` only after all binaries exist and hashes are computed. Use `Author: arkadianet` in the script header.

- [ ] **Step 4: Make `matrix.sh` bundle-only**

Remove `CARGO_TARGET_DIR` and `$HOME/.cache/cargo-target` fallback searches. Verify every binary hash before reading arms. Pass the same `SOURCE_REVISION` and `BINARY_BUNDLE_SHA256` to every `run.sh` invocation.

- [ ] **Step 5: Enforce the E1 arm contract in `e1.sh` before any run**

Parse all rows first, then reject the whole E1 run before launching an arm if treatment roles or shared endpoint values are inconsistent. Call generic `matrix.sh` only after E1 preflight succeeds. Create the output directory only after preflight succeeds.

- [ ] **Step 6: Run matrix tests**

Run:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_holdout_matrix --locked -- --nocapture
bash -n eval/live-holdout/prepare.sh \
  eval/live-holdout/e1.sh \
  eval/live-holdout/run.sh \
  eval/live-holdout/matrix.sh
```

Expected: all tests and syntax checks pass.

- [ ] **Step 7: Commit**

```bash
git add eval/live-holdout/prepare.sh \
  eval/live-holdout/e1.sh \
  eval/live-holdout/matrix.sh \
  crates/alloy-eval/Cargo.toml \
  crates/alloy-eval/tests/live_holdout_matrix.rs
git commit -m "fix(eval): bind live matrices to one binary bundle"
```

---

### Task 5: Publish the E1 Operator Contract

**Files:**
- Modify: `eval/live-holdout/arms.example.tsv`
- Modify: `eval/live-holdout/README.md`
- Create: `eval/live-holdout/E1-CHECKLIST.md`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Documents one bundle build, one three-arm matrix, pilot settings, target settings, evidence review, and result acceptance.

- [ ] **Step 1: Replace the arms example with the seven-column E1 contract**

```text
# arm_id	driver	model	temperature	profile	base_url	reps
naive	naive	Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf	0.6	none	http://127.0.0.1:8089/v1/	10
alloy-default	alloy	Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf	0.6	default	http://127.0.0.1:8089/v1/	10
alloy-autonomous	alloy	Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf	0.6	autonomous	http://127.0.0.1:8089/v1/	10
```

- [ ] **Step 2: Write the operator checklist**

The checklist must require:
1. clean commit,
2. endpoint health and exact model ID,
3. hidden-oracle corpus validation,
4. bundle preparation,
5. three-arm preflight,
6. pilot execution at three repetitions,
7. report and artifact validation,
8. target execution at ten repetitions,
9. extension only when uncertainty remains,
10. explicit uplift or “why not” conclusion.

Include the exact commands:

```bash
bundle=/tmp/alloy-e1-bundle
out=/tmp/alloy-e1-pilot
pilot_arms=/tmp/alloy-e1-pilot-arms.tsv
./eval/live-holdout/prepare.sh "$bundle"
./eval/live-holdout/e1.sh \
  "$pilot_arms" \
  "$out" \
  "$bundle"
```

The pilot model ID is an operator input because no smaller model is committed to this repository. The checklist must require the operator to create `$pilot_arms` from the documented seven-column example with that exact model ID and `reps=3`, and must refuse to silently substitute the target model.

- [ ] **Step 3: Update README contracts**

Document schema v4, driver identity, bundle identity, one-shot naive limitations, confidentiality of evidence, the generic matrix contract, and the stricter E1 wrapper contract requiring the named naive arm.

- [ ] **Step 4: Add feature-gated CI**

Add these commands to the existing Rust CI job:

```bash
cargo test -p alloy-eval --features live-naive \
  --test live_naive_runner --locked
cargo test -p alloy-eval --features live-naive \
  --test live_holdout_matrix --locked
```

No live endpoint or secret is used in CI.

- [ ] **Step 5: Run documentation and regression validation**

Run:

```bash
git diff --check
cargo fmt --all -- --check
cargo test -p alloy-eval --features live-naive --locked
cargo test -p alloy-cli --locked
cargo clippy -p alloy-eval -p alloy-cli \
  --all-targets --all-features --locked -- -D warnings
bash -n eval/live-holdout/prepare.sh \
  eval/live-holdout/e1.sh \
  eval/live-holdout/run.sh \
  eval/live-holdout/matrix.sh
```

Expected: all commands exit zero with no warnings.

- [ ] **Step 6: Commit**

```bash
git add eval/live-holdout/arms.example.tsv \
  eval/live-holdout/README.md \
  eval/live-holdout/E1-CHECKLIST.md \
  .github/workflows/ci.yml
git commit -m "docs(eval): define E1 three-arm measurement"
```

---

## Final Verification

- [ ] Confirm `git status --short` contains only intended files.
- [ ] Confirm every new behavior was observed failing before implementation and passing afterward.
- [ ] Confirm the naive HTTP integration recorded exactly one model call.
- [ ] Confirm runner tests prove `.post` and hidden tests were absent during both model drivers.
- [ ] Confirm matrix tests reject mixed endpoint, repetition, revision, and binary identities before starting an arm.
- [ ] Confirm full CLI/evaluator tests and clippy pass.
- [ ] Do not start a pilot until the operator supplies an installed smaller model ID.

## Execution Handoff

This plan delivers the E1 harness and operator contract. Pilot execution is the next operational step after the operator selects an installed smaller model; the target Q4 30B row is already specified.
