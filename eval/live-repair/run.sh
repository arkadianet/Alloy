#!/usr/bin/env bash
# Live-repair benchmark: N fixtures x REPS repetitions against a live
# OpenAI-compatible endpoint. Each run is a fresh workspace + fresh .alloy.
#
# Usage:
#   MODEL=qwen2.5-coder:32b TEMP=0.6 REPS=10 ./eval/live-repair/run.sh out.jsonl
#
# Env:
#   MODEL   wire model id served by the endpoint   (default qwen2.5-coder:32b)
#   TEMP    endpoint sampling temperature          (default 0.6)
#   REPS    repetitions per fixture                (default 10)
#   BASEURL endpoint base url                      (default http://127.0.0.1:11434/v1/)
#   ALLOY   path to the alloy binary               (default: cargo target debug)
#   TIMEOUT per-run timeout seconds                (default 600)
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
fixtures="$repo/eval/live-repair/fixtures"
out="${1:?usage: run.sh <out.jsonl>}"
MODEL="${MODEL:-qwen2.5-coder:32b}"
TEMP="${TEMP:-0.6}"
REPS="${REPS:-10}"
BASEURL="${BASEURL:-http://127.0.0.1:11434/v1/}"
ALLOY="${ALLOY:-$HOME/.cache/cargo-target/debug/alloy}"
TIMEOUT="${TIMEOUT:-600}"

router() {
  cat <<EOF
[policy]
default_tier = "standard"
connect_timeout_ms = 10000
request_timeout_ms = 600000
shutdown_grace_ms = 5000
max_in_flight = 1

[[providers]]
id = "local"
kind = "openai_compatible"
base_url = "$BASEURL"
api_key_env = "ALLOY_API_KEY"

[[providers.endpoints]]
id = "bench"
display_name = "Bench"
model = "$MODEL"
tiers = ["standard", "economy"]
supports_tools = true
supports_structured_output = true
max_context = 32768
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0
temperature = $TEMP

[capability_tiers]
repair = "standard"
edit = "standard"
review = "economy"
planning = "standard"
EOF
}

: > "$out"
total=0 passed=0
for dir in "$fixtures"/*/; do
  name="$(basename "$dir")"
  for rep in $(seq 1 "$REPS"); do
    ws="$(mktemp -d)"
    cp -r "$dir." "$ws/"
    cp -r "$repo/profiles" "$ws/profiles"
    router > "$ws/router.toml"
    printf 'ALLOY_API_KEY=\n' > "$ws/example.env"
    git -C "$ws" init -q
    git -C "$ws" add .
    git -C "$ws" -c user.name=bench -c user.email=bench@localhost commit -qm fixture
    start=$(date +%s)
    ALLOY_API_KEY=local timeout "$TIMEOUT" \
      "$ALLOY" --workspace "$ws" run "fix the compile error in src/main.rs" --yes \
      > "$ws/run.log" 2>&1
    code=$?
    dur=$(( $(date +%s) - start ))
    retries=$(grep -c "retrying with fresh diagnostics" "$ws/run.log" || true)
    total=$((total + 1))
    [ "$code" -eq 0 ] && passed=$((passed + 1))
    printf '{"fixture":"%s","rep":%s,"exit":%s,"retries":%s,"secs":%s,"model":"%s","temp":%s}\n' \
      "$name" "$rep" "$code" "$retries" "$dur" "$MODEL" "$TEMP" >> "$out"
    echo "[$passed/$total] $name#$rep exit=$code retries=$retries ${dur}s"
    rm -rf "$ws"
  done
done
echo "DONE $passed/$total -> $out"
