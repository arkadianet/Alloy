# Dogfooding: first live-model runs

Everything below the model is already proven end-to-end: the CLI e2e suite
drives the spawned `alloy` binary through a real OpenAI-compatible HTTP
server (scripted responses), so router → provider → capability worker →
patch → verify → gate all work. What has never been tested is a **real
model's outputs** flowing through that pipe. That is this milestone.

Author: arkadianet

## 1. Stand up a local model (Ollama shown; any OpenAI-compatible server works)

```sh
ollama pull qwen2.5-coder:14b     # or any tool-capable coding model you like
ollama serve                       # listens on 127.0.0.1:11434
```

`http://` is only accepted for loopback (RFC-0007); public endpoints must be
`https://`.

## 2. Point the router at it

Copy `router.toml.local-example` to `router.toml` in the workspace you are
running against (or set `ALLOY_ROUTER`). The key lines:

```toml
[[providers]]
id = "local"
kind = "openai_compatible"
base_url = "http://127.0.0.1:11434/v1/"
api_key_env = "ALLOY_API_KEY"        # Ollama ignores the value; set anything

[[providers.endpoints]]
model = "qwen2.5-coder:14b"          # must match the served model id
```

Alloy never invents keys: export `ALLOY_API_KEY=local` (any non-empty value)
or put it in your env file.

## 3. Run against a broken fixture, not a precious tree

```sh
git init /tmp/dogfood && cd /tmp/dogfood
# drop in a small crate with a deliberate compile error, commit it
alloy run "fix the compile error in src/main.rs"
```

The default profile keeps the guard rails on: Landlock/container sandbox,
`require_cargo_check`, human gate before edits land, $5 / 2M-token budget
ceilings per run. `--dry-run` shows the plan without dispatching.

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
