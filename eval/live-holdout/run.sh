#!/usr/bin/env bash
# Live holdout executor — real `alloy` + live OpenAI-compatible endpoint on
# the RFC-0016 holdout workspaces. NOT an offline gate. See ./README.md.
#
# Usage:
#   MODEL=… BASEURL=http://127.0.0.1:8089/v1/ REPS=1 \
#     ./eval/live-holdout/run.sh /tmp/live-holdout.jsonl
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:?usage: run.sh <out.jsonl>}"

FIXTURES="${FIXTURES:-$repo/crates/alloy-eval/fixtures/holdout}"
MODEL="${MODEL:-Qwen3-Coder-30B-A3B-Instruct-UD-Q6_K_XL.gguf}"
TEMP="${TEMP:-0.6}"
REPS="${REPS:-1}"
BASEURL="${BASEURL:-http://127.0.0.1:8089/v1/}"
TIMEOUT="${TIMEOUT:-600}"
GOAL="${GOAL:-fix the compile error in this crate}"
SCORE="${SCORE:-1}"

die() { echo "live-holdout/run.sh: $1" >&2; exit 2; }

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

"$ALLOY" --version >/dev/null 2>&1
probe=$?
case "$probe" in
  126|127) die "alloy at $ALLOY could not be executed (exit $probe)";;
esac

case "$REPS" in
  ''|*[!0-9]*) die "REPS must be a positive integer, got '$REPS'";;
esac
[ "$REPS" -ge 1 ] || die "REPS must be at least 1, got '$REPS'"
case "$TIMEOUT" in
  ''|*[!0-9]*) die "TIMEOUT must be a positive integer of seconds, got '$TIMEOUT'";;
esac
case "$MODEL$BASEURL" in
  *[\"\\]*) die "MODEL and BASEURL must not contain quotes or backslashes";;
esac
case "$TEMP" in
  ''|*[!0-9.]*) die "TEMP must be a number, got '$TEMP'";;
esac
[ -d "$FIXTURES" ] || die "fixtures root missing: $FIXTURES"

router="$("$SCORER" render-router --model "$MODEL" --temperature "$TEMP" --base-url "$BASEURL")" \
  || die "render-router failed"

mapfile -t ids < <(
  find "$FIXTURES" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | LC_ALL=C sort
)
[ "${#ids[@]}" -gt 0 ] || die "no holdout fixture directories under $FIXTURES"

: > "$out"
total=0
passed=0
unexecutable=0

for id in "${ids[@]}"; do
  workspace="$FIXTURES/$id/workspace"
  [ -d "$workspace" ] || die "fixture $id missing workspace/ at $workspace"
  for rep in $(seq 1 "$REPS"); do
    ws="$(mktemp -d)"
    cp -a "$workspace"/. "$ws/"
    cp -a "$repo/profiles" "$ws/profiles"
    printf '%s' "$router" >"$ws/router.toml"
    git -C "$ws" init -q
    git -C "$ws" add -A
    git -C "$ws" -c user.name=live-holdout -c user.email=live-holdout@localhost commit -qm fixture
    start_ms=$(date +%s%3N)
    set +e
    ALLOY_API_KEY="${ALLOY_API_KEY:-local}" timeout "$TIMEOUT" \
      "$ALLOY" --workspace "$ws" run "$GOAL" --yes >"$ws/run.log" 2>&1
    code=$?
    set -e
    wall_ms=$(($(date +%s%3N) - start_ms))
    total=$((total + 1))
    [ "$code" -eq 0 ] && passed=$((passed + 1))
    case "$code" in
      126|127) unexecutable=$((unexecutable + 1));;
    esac
    printf '{"fixture_id":"%s","repetition":%d,"exit_code":%d,"wall_ms":%d,"model":"%s","temperature":%s,"base_url":"%s","corpus":"rfc0016-holdout-live"}\n' \
      "$id" "$rep" "$code" "$wall_ms" "$MODEL" "$TEMP" "$BASEURL" >>"$out"
    echo "[$passed/$total] $id#$rep exit=$code ${wall_ms}ms"
    # Keep failing logs under /tmp for diagnosis; wipe passes.
    if [ "$code" -eq 0 ]; then
      rm -rf "$ws"
    else
      echo "  log: $ws/run.log" >&2
    fi
  done
done

echo "DONE $passed/$total -> $out (live-BYOM holdout; not an offline gate)"

if [ "$total" -eq 0 ]; then
  die "no repetitions ran — the sweep is broken, not the fixtures"
fi

if [ "$SCORE" = "1" ]; then
  python3 - "$out" "$passed" "$total" <<'PY'
import json, sys
path, passed, total = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rows = [json.loads(line) for line in open(path) if line.strip()]
by = {}
for r in rows:
    by.setdefault(r["fixture_id"], []).append(r["exit_code"] == 0)
print("fixture_id\tpasses\tattempts")
for fid in sorted(by):
    xs = by[fid]
    print(f"{fid}\t{sum(xs)}\t{len(xs)}")
rate = (passed / total) if total else 0.0
print(f"overall\t{passed}\t{total}\tpass_rate={rate:.3f}")
print("NOTE: operator telemetry only — do not cite as RFC-0016 offline holdout.")
PY
fi

if [ "$unexecutable" -gt 0 ]; then
  echo "live-holdout/run.sh: $unexecutable/$total repetition(s) could not execute $ALLOY;" \
    "harness failures — do not publish" >&2
  exit 3
fi
exit 0
