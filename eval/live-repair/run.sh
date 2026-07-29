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
ALLOY="${ALLOY:-$repo/target/debug/alloy}"
SCORER="${SCORER:-$repo/target/debug/alloy-eval-live-repair}"
TIMEOUT="${TIMEOUT:-600}"
RETRY_PATTERN="${RETRY_PATTERN:-retrying with fresh diagnostics}"
SCORE="${SCORE:-1}"

if [ ! -x "$SCORER" ]; then
  echo "missing scorer at $SCORER (cargo build -p alloy-eval --bin alloy-eval-live-repair)" >&2
  exit 2
fi

# The manifests are the single source of fixture identity, goal text, and the
# workspace snapshot to copy; the router document is rendered by the same
# binary so the shell never templates TOML by hand.
plan="$("$SCORER" plan --fixtures "$FIXTURES")" || exit 2
router="$("$SCORER" render-router --model "$MODEL" --temperature "$TEMP" --base-url "$BASEURL")" || exit 2

: > "$out"
total=0
passed=0
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
    printf '{"fixture_id":"%s","repetition":%d,"exit_code":%d,"retries":%d,"wall_ms":%d}\n' \
      "$id" "$rep" "$code" "$retries" "$wall_ms" >>"$out"
    echo "[$passed/$total] $id#$rep exit=$code retries=$retries ${wall_ms}ms"
    rm -rf "$ws"
  done
done <<<"$plan"

echo "DONE $passed/$total -> $out"

if [ "$SCORE" = "1" ]; then
  "$SCORER" score \
    --fixtures "$FIXTURES" \
    --observations "$out" \
    --model "$MODEL" \
    --temperature "$TEMP" \
    --base-url "$BASEURL" \
    --out "${out%.jsonl}.report.json"
fi
