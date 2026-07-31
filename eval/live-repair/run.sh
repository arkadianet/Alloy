#!/usr/bin/env bash
# Live-repair benchmark executor — a THIN wrapper around `alloy-eval-live-repair`.
#
# This script is operator tooling for measuring the REAL `alloy` binary against
# a LIVE OpenAI-compatible endpoint. It is NOT an RFC-0016 holdout gate and its
# results MUST NOT gate a milestone. See ./README.md.
#
# Everything except process execution lives in `alloy-eval`'s `live_repair`
# module (fixture manifests, router rendering, scoring, reporting); RFC-0016
# §10.2 keeps `crates/alloy-eval/src` free of process spawning and network I/O,
# so this script is the only component that spawns anything or talks to the
# endpoint.
#
# Usage:
#   MODEL=qwen2.5-coder:32b TEMP=0.6 REPS=10 ./eval/live-repair/run.sh out.jsonl
#
# Env:
#   FIXTURES      corpus root                   (default eval/live-repair/fixtures)
#   MODEL         wire model id                 (default qwen2.5-coder:32b)
#   TEMP          sampling temperature          (default 0.6)
#   REPS          repetitions per fixture       (default 10)
#   BASEURL       endpoint base url             (default http://127.0.0.1:11434/v1/)
#   ALLOY         path to the alloy binary      (default target/debug/alloy)
#   SCORER        path to alloy-eval-live-repair(default target/debug/...)
#   TIMEOUT       per-run timeout seconds       (default 600)
#   RETRY_PATTERN log line counted as a retry
#   SCORE         set to 0 to skip scoring      (default 1)
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:?usage: run.sh <out.jsonl>}"
FIXTURES="${FIXTURES:-$repo/eval/live-repair/fixtures}"
MODEL="${MODEL:-qwen2.5-coder:32b}"
TEMP="${TEMP:-0.6}"
REPS="${REPS:-10}"
BASEURL="${BASEURL:-http://127.0.0.1:11434/v1/}"
TIMEOUT="${TIMEOUT:-600}"
RETRY_PATTERN="${RETRY_PATTERN:-retrying with fresh diagnostics}"
SCORE="${SCORE:-1}"

# Preflight. A broken sweep must fail before it writes a single row: rows that
# only record "the binary could not be executed" would otherwise be scored, and
# a 0% pass rate produced by a missing binary is a lie about the model.
die() { echo "run.sh: $1" >&2; exit 2; }

resolve_bin() {
  local name="$1"
  if [ -n "${2:-}" ] && [ -x "$2" ]; then
    printf '%s' "$2"
    return
  fi
  local target
  target="$(cargo metadata --no-deps --format-version 1 --manifest-path "$repo/Cargo.toml" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')" \
    || die "could not resolve cargo target directory"
  if [ -x "$target/debug/$name" ]; then
    printf '%s' "$target/debug/$name"
    return
  fi
  if [ -x "$repo/target/debug/$name" ]; then
    printf '%s' "$repo/target/debug/$name"
    return
  fi
  die "missing $name (cargo build -p …); looked under $target/debug and $repo/target/debug"
}

ALLOY="$(resolve_bin alloy "${ALLOY:-}")"
SCORER="$(resolve_bin alloy-eval-live-repair "${SCORER:-}")"

# Executability probe: any exit code is fine except the shell's
# "found but could not execute" (126) and "not found" (127).
"$ALLOY" --version >/dev/null 2>&1
probe=$?
case "$probe" in
  126|127) die "alloy binary at $ALLOY could not be executed (exit $probe)";;
esac
case "$REPS" in
  ''|*[!0-9]*) die "REPS must be a positive integer, got '$REPS'";;
esac
[ "$REPS" -ge 1 ] || die "REPS must be at least 1, got '$REPS'"
case "$TIMEOUT" in
  ''|*[!0-9]*) die "TIMEOUT must be a positive integer of seconds, got '$TIMEOUT'";;
esac
# Endpoint identity is written verbatim into every JSON row, so it must not
# need escaping.
case "$MODEL$BASEURL" in
  *[\"\\]*) die "MODEL and BASEURL must not contain quotes or backslashes";;
esac
case "$TEMP" in
  ''|*[!0-9.]*) die "TEMP must be a number, got '$TEMP'";;
esac

# The manifests are the single source of fixture identity, goal text, and the
# workspace snapshot to copy; the router document is rendered by the same
# binary so the shell never templates TOML by hand.
plan="$("$SCORER" plan --fixtures "$FIXTURES")" || exit 2
router="$("$SCORER" render-router --model "$MODEL" --temperature "$TEMP" --base-url "$BASEURL")" || exit 2

: > "$out"
total=0
passed=0
unexecutable=0
while IFS=$'\t' read -r id workspace goal; do
  [ -n "$id" ] || continue
  for rep in $(seq 1 "$REPS"); do
    ws="$(mktemp -d)"
    cp -r "$workspace/." "$ws/"
    cp -r "$repo/profiles" "$ws/profiles"
    printf '%s' "$router" >"$ws/router.toml"
    git -C "$ws" init -q
    git -C "$ws" add -A
    git -C "$ws" -c user.name=bench -c user.email=bench@localhost commit -qm fixture
    start_ms=$(date +%s%3N)
    ALLOY_API_KEY="${ALLOY_API_KEY:-local}" timeout "$TIMEOUT" \
      "$ALLOY" --workspace "$ws" run "$goal" --yes >"$ws/run.log" 2>&1
    code=$?
    wall_ms=$(($(date +%s%3N) - start_ms))
    retries=$(grep -c -- "$RETRY_PATTERN" "$ws/run.log" || true)
    total=$((total + 1))
    [ "$code" -eq 0 ] && passed=$((passed + 1))
    case "$code" in
      126|127) unexecutable=$((unexecutable + 1));;
    esac
    # Every row carries the endpoint it was produced against, so rows from two
    # models or two temperatures can never be pooled into one pass rate.
    printf '{"fixture_id":"%s","repetition":%d,"exit_code":%d,"retries":%d,"wall_ms":%d,"model":"%s","temperature":%s,"base_url":"%s"}\n' \
      "$id" "$rep" "$code" "$retries" "$wall_ms" "$MODEL" "$TEMP" "$BASEURL" >>"$out"
    echo "[$passed/$total] $id#$rep exit=$code retries=$retries ${wall_ms}ms"
    rm -rf "$ws"
  done
done <<<"$plan"

echo "DONE $passed/$total -> $out"

if [ "$total" -eq 0 ]; then
  echo "run.sh: no repetitions ran — the sweep is broken, not the fixtures" >&2
  exit 2
fi

status=0
if [ "$SCORE" = "1" ]; then
  "$SCORER" score \
    --fixtures "$FIXTURES" \
    --observations "$out" \
    --model "$MODEL" \
    --temperature "$TEMP" \
    --base-url "$BASEURL" \
    --reps "$REPS" \
    --out "${out%.jsonl}.report.json"
  status=$?
fi

# A repetition that could not execute `alloy` is a harness failure, not a model
# failure. Fixtures failing is a result and still exits 0.
if [ "$unexecutable" -gt 0 ]; then
  echo "run.sh: $unexecutable/$total repetition(s) could not execute $ALLOY;" \
    "these are harness failures, not model failures — do not publish this run" >&2
  exit 3
fi
exit "$status"
