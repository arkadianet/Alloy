# Dogfooding: first live-model runs

Everything below the model is already proven end-to-end: the CLI e2e suite
drives the spawned `alloy` binary through a real OpenAI-compatible HTTP
server (scripted responses), so router → provider → capability worker →
patch → verify → gate all work. What has never been tested is a **real
model's outputs** flowing through that pipe. That is this milestone.

Author: arkadianet

Prerequisites: Rust 1.97, Git, and a working Linux Landlock sandbox. The
default profile uses a container for `cargo test`, so Docker or Podman is also
needed when a run reaches that tool. Alloy fails closed when its configured
sandbox backend is unavailable.

## 1. Stand up a local model (Ollama shown; any OpenAI-compatible server works)

```sh
ollama pull qwen2.5-coder:14b     # or any tool-capable coding model you like
ollama serve                       # listens on 127.0.0.1:11434
```

`http://` is only accepted for loopback (RFC-0007); public endpoints must be
`https://`.

## 2. Point the router at it

From the Alloy checkout, copy `router.toml.local-example` to the active
`router.toml` and edit the model or URL if needed. The key lines:

```toml
[[providers]]
id = "local"
kind = "openai_compatible"
base_url = "http://127.0.0.1:11434/v1/"
api_key_env = "ALLOY_API_KEY"        # Ollama ignores the value; set anything

[[providers.endpoints]]
model = "qwen2.5-coder:14b"          # must match the served model id
```

Alloy never invents keys or loads `.env`: export `ALLOY_API_KEY=local` (any
non-empty value), or use shell/direnv tooling to export it.

`repair_local_diagnostic` escalates its analyze/edit nodes to the `premium`
tier on their one retry (RFC-0010 §5.11.4 ES1), and that tier now reaches
endpoint selection — so serve `premium` with something if you want the retry
to run on a better model. The commented `local-coder-big` endpoint in
`router.toml.local-example` (`ollama pull qwen2.5-coder:32b`) is the intended
landing spot; adding `"premium"` to `local-coder`'s `tiers` is the low-effort
alternative. Serving nothing is also fine: the retry then routes at the
configured tier and the route decision records `escalation_unserved = true`.

## 3. Run against a broken fixture, not a precious tree

```sh
# Run from the Alloy checkout after configuring router.toml.
cargo build -p alloy-cli --bin alloy
export ALLOY_API_KEY=local
export ALLOY_PROFILE="$PWD/profiles/default.toml"
export ALLOY_ROUTER="$PWD/router.toml"

dogfood="$(mktemp -d)"
cp -a eval/live-repair/fixtures/missing_mut/workspace/. "$dogfood/"
git -C "$dogfood" init
git -C "$dogfood" add -A
git -C "$dogfood" -c user.name=dogfood -c user.email=dogfood@localhost \
  -c commit.gpgsign=false commit -m fixture

"$PWD/target/debug/alloy" --workspace "$dogfood" \
  run "fix the compile error in src/main.rs"
```

`ALLOY_PROFILE` and `ALLOY_ROUTER` are absolute because Alloy resolves relative
overrides against `--workspace`. Alternatively, copy `profiles/` and
`router.toml` into the disposable crate.

The default profile keeps the guard rails on: Landlock/container sandbox,
`require_cargo_check`, human gate before edits land, $5 / 2M-token budget
ceilings per run. `--dry-run` shows the plan without dispatching.

On success, Alloy reports that `cargo check` passed and separately states that
the intended fix was not independently verified. Inspect the exact run using
the command printed at completion:

```sh
"$PWD/target/debug/alloy" --workspace "$dogfood" \
  events --session <session-id> --run <run-id>
```

For non-interactive use, add `--yes` to pre-approve the human gate or
`--no-input` to stop with an out-of-band approval command.

## 3b. Review a diff (Alloy on Alloy's own PRs)

`alloy review` runs the `review` capability over a unified diff and prints
its findings. The CLI spawns nothing — not even `git` — so the diff is piped
in or named as a file:

```sh
git diff origin/main... | alloy review --diff -
alloy review --diff /tmp/pr.diff --json
```

Findings print as `severity file:line message`, then `summary:` and
`verdict:`. Exit `0` means `approve`; exit `16` (`EX_REVIEW_CHANGES`) means
the reviewer asked for changes — a successful run with an opinion, not a
failure. The planned template (`review_diff`) is a single read-only node: no
edit, no gate, no cargo. Note that the `readonly` profile's
`max_usd_per_run = 0` denies the model call, so review under the default
profile for now.

The diff does not travel in the goal text. It is stored as a `Patch`
artifact and attached to the goal; the `review` worker reads those bytes
back and fences them verbatim. Goal *text* is sanitised for prompt injection
on its way through the context engine (per-line `trim_end`, fence-marker
stripping), which would quietly reshape a whitespace-sensitive patch —
blank context lines, trailing-whitespace changes, `>>>>>>>` conflict
markers. Diffs over 128 KiB are cut, and the cut is stated in three places
that agree: an `[alloy: truncated — {kept} of {total} bytes shown]` marker
inside the fenced diff the model reads, a `(diff truncated: …)` line on
stdout, and `diff_truncated` / `diff_bytes` / `diff_total_bytes` in the
`--json` envelope. The model's own findings cap is a separate field,
`findings_truncated`.

## 4. What to record when it misbehaves

File an issue per failure with:
- the goal string and fixture (or a pointer),
- `alloy events --session <id> --json` output — decisions, model calls,
  tool calls, and any `audit_record_dropped` runtime events,
- whether the failure was model output (bad patch, malformed structured
  output) or harness behaviour (routing, retry, gate, verify classification).

The second category is ours; the first calibrates which local models are
good enough to recommend.

## 5. Graduation bar for `0.1.0-beta`

A live local model completes the same offline walkthrough the e2e suite
already passes — index → plan → edit → gate approval → verify →
`Succeeded`, workspace fixed on disk — plus at least a handful of varied
goals on real repos without harness-side failures. Model-side failures
don't block the tag; harness-side ones do.

## 6. Live measurement sweeps

Two operator corpora — neither is an offline milestone gate:

| Sweep | Path | Role |
| --- | --- | --- |
| Live holdout | [`eval/live-holdout/`](../eval/live-holdout/) | Real model on RFC-0016 holdout workspaces (independent outputs) |
| Live-repair | [`eval/live-repair/`](../eval/live-repair/) | Reliability telemetry across ten single-error crates |

```bash
cargo build -p alloy-cli --bin alloy
cargo build -p alloy-eval --bin alloy-eval-live-repair
export ALLOY_API_KEY=local

MODEL='Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf' \
  BASEURL='http://127.0.0.1:8089/v1/' REPS=1 \
  ./eval/live-holdout/run.sh /tmp/live-holdout.jsonl
```

## 7. Where this is going (honest)

Alloy is not aiming to become a chat-first general coding assistant. The
roadmap after the vertical slice is:

1. **Prove the control-plane thesis** — live holdout across borrow, move, and
   type diagnostics with independent model outputs; offline scripted success
   is not enough.
2. **Reliability on the repair loop** — retries, escalation, line-ops,
   schema-constrained decoding, repair generations — measured via
   `eval/live-repair`, not vibes.
3. **Close Beta measurement** — do graph/context weights help, or write the
   why-not with numbers (RFC-0012).
4. **Eval-gate the LLM planner** — RFC-0017 §12.4 before any default flip.
5. **Widen only when justified** — harder diagnostics, multi-file, cargo
   metadata, SemanticEditOp / RA — Future extensions, not a redesign.
6. **Productize last** — alloyd, ACP, IDE surfaces — Production track after
   the thesis holds for someone outside the core team.
