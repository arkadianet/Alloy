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
PROFILE="${PROFILE:-default}"
BASEURL="${BASEURL:-http://127.0.0.1:8089/v1/}"
TIMEOUT="${TIMEOUT:-600}"
GOAL="${GOAL:-fix the compile error in this crate}"
SCORE="${SCORE:-1}"
ORACLE="${ORACLE:-$repo/eval/live-holdout/oracle.py}"
SCORE_SCRIPT="${SCORE_SCRIPT:-$repo/eval/live-holdout/score.py}"

die() { echo "live-holdout/run.sh: $1" >&2; exit 2; }

if ! python3 - <<'PY'
import sys

if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11 or newer is required")
PY
then
  die "Python 3.11 or newer is required"
fi

resolve_bin() {
  local name="$1"
  local override="${2:-}"
  # An explicit override must win — including when it is missing/unusable —
  # so preflight can refuse a broken sweep instead of silently falling back.
  if [ -n "$override" ]; then
    [ -x "$override" ] ||
      die "$name binary not found or not executable at $override"
    printf '%s' "$override"
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
[ "$TIMEOUT" -ge 1 ] || die "TIMEOUT must be at least 1 second, got '$TIMEOUT'"
case "$MODEL$BASEURL" in
  *[\"\\]*) die "MODEL and BASEURL must not contain quotes or backslashes";;
esac
case "$TEMP" in
  ''|*[!0-9.]*) die "TEMP must be a number, got '$TEMP'";;
esac
case "$PROFILE" in
  default|autonomous) ;;
  *) die "PROFILE must be default or autonomous, got '$PROFILE'";;
esac
[ -d "$FIXTURES" ] || die "fixtures root missing: $FIXTURES"
[ -f "$ORACLE" ] || die "oracle script missing: $ORACLE"
[ -f "$SCORE_SCRIPT" ] || die "score script missing: $SCORE_SCRIPT"

router="$("$SCORER" render-router --model "$MODEL" --temperature "$TEMP" --base-url "$BASEURL")" \
  || die "render-router failed"

mapfile -t ids < <(
  find "$FIXTURES" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | LC_ALL=C sort
)
[ "${#ids[@]}" -gt 0 ] || die "no holdout fixture directories under $FIXTURES"

fixture_target_path() {
  python3 - "$1" <<'PY'
import sys
import tomllib
from pathlib import PurePosixPath

with open(sys.argv[1], "rb") as handle:
    path = tomllib.load(handle)["naive_target_path"]
parsed = PurePosixPath(path)
if not path or parsed.is_absolute() or ".." in parsed.parts:
    raise SystemExit("naive_target_path must stay inside the fixture workspace")
print(path)
PY
}

for id in "${ids[@]}"; do
  fixture_dir="$FIXTURES/$id"
  manifest="$fixture_dir/manifest.toml"
  [ -f "$manifest" ] || die "fixture $id missing manifest.toml"
  target_path="$(fixture_target_path "$manifest")" ||
    die "fixture $id manifest has no naive_target_path"
  [ -f "$fixture_dir/workspace/$target_path.post" ] ||
    die "fixture $id missing strict oracle workspace/$target_path.post"
done

# Exclusive lock on the observations file so two sweeps cannot interleave rows.
mkdir -p "$(dirname -- "$out")"
exec 9>"$out.lock" || die "could not open lock $out.lock"
flock -n 9 || die "another live-holdout sweep holds $out.lock"
: > "$out" || die "could not initialize observations file: $out"
total=0
process_passed=0
oracle_passed=0
unexecutable=0

for id in "${ids[@]}"; do
  workspace="$FIXTURES/$id/workspace"
  [ -d "$workspace" ] || die "fixture $id missing workspace/ at $workspace"
  target_path="$(fixture_target_path "$FIXTURES/$id/manifest.toml")"
  for rep in $(seq 1 "$REPS"); do
    ws="$(mktemp -d)" || die "could not create workspace for $id#$rep"
    cp -a "$workspace"/. "$ws/" ||
      die "fixture copy failed for $id#$rep"
    # The golden reference is an oracle input, never model-visible workspace
    # content.
    rm -f "$ws/$target_path.post"
    cp -a "$repo/profiles" "$ws/profiles" ||
      die "profile copy failed for $id#$rep"
    printf '%s' "$router" >"$ws/router.toml" ||
      die "router write failed for $id#$rep"
    git -C "$ws" init -q || die "git init failed for $id#$rep"
    git -C "$ws" add -A || die "git add failed for $id#$rep"
    git -C "$ws" -c user.name=live-holdout \
      -c user.email=live-holdout@localhost commit -qm fixture ||
      die "git commit failed for $id#$rep"
    start_ms=$(date +%s%3N)
    if ALLOY_API_KEY="${ALLOY_API_KEY:-local}" timeout "$TIMEOUT" \
      "$ALLOY" --workspace "$ws" --profile "$PROFILE" run "$GOAL" --yes \
      >"$ws/run.log" 2>&1; then
      code=0
    else
      code=$?
    fi
    wall_ms=$(($(date +%s%3N) - start_ms))
    oracle_json="$(
      python3 "$ORACLE" \
        --fixture-dir "$FIXTURES/$id" \
        --workspace "$ws" \
        --run-log "$ws/run.log" \
        --exit-code "$code" \
        --cargo-timeout "$TIMEOUT"
    )" || die "oracle failed for $id#$rep"
    read -r process_pass compile_clean reference_match oracle_pass failure_class cargo_check_exit generations <<<"$(
      python3 - "$oracle_json" <<'PY'
import json
import sys

row = json.loads(sys.argv[1])
print(
    row["process_pass"],
    row["compile_clean"],
    row["reference_match"],
    row["oracle_pass"],
    row["failure_class"],
    row["cargo_check_exit"] if row["cargo_check_exit"] is not None else "null",
    row["repair_generations"],
)
PY
    )"
    total=$((total + 1))
    [ "$process_pass" = "True" ] && process_passed=$((process_passed + 1))
    [ "$oracle_pass" = "True" ] && oracle_passed=$((oracle_passed + 1))
    case "$code" in
      126|127) unexecutable=$((unexecutable + 1));;
    esac
    printf '{"fixture_id":"%s","repetition":%d,"exit_code":%d,"process_pass":%s,"compile_clean":%s,"reference_match":%s,"oracle_pass":%s,"failure_class":"%s","cargo_check_exit":%s,"repair_generations":%d,"wall_ms":%d,"model":"%s","temperature":%s,"profile":"%s","base_url":"%s","corpus":"rfc0016-holdout-live"}\n' \
      "$id" "$rep" "$code" "${process_pass,,}" "${compile_clean,,}" \
      "${reference_match,,}" "${oracle_pass,,}" "$failure_class" "$cargo_check_exit" \
      "$generations" "$wall_ms" "$MODEL" "$TEMP" "$PROFILE" "$BASEURL" >>"$out"
    echo "[$oracle_passed/$total oracle; $process_passed process] $id#$rep \
oracle=$oracle_pass class=$failure_class generations=$generations ${wall_ms}ms"
    # Keep non-oracle logs under /tmp for diagnosis; wipe strict passes.
    if [ "$oracle_pass" = "True" ]; then
      rm -rf "$ws"
    else
      echo "  log: $ws/run.log" >&2
    fi
  done
done

echo "DONE oracle=$oracle_passed/$total process=$process_passed/$total -> $out \
(live-BYOM holdout; not an offline gate)"

if [ "$total" -eq 0 ]; then
  die "no repetitions ran — the sweep is broken, not the fixtures"
fi

status=0
if [ "$SCORE" = "1" ]; then
  python3 "$SCORE_SCRIPT" \
    --fixtures "$FIXTURES" \
    --observations "$out" \
    --model "$MODEL" \
    --temperature "$TEMP" \
    --profile "$PROFILE" \
    --base-url "$BASEURL" \
    --reps "$REPS" \
    --out "${out%.jsonl}.report.json"
  status=$?
fi

if [ "$unexecutable" -gt 0 ]; then
  echo "live-holdout/run.sh: $unexecutable/$total repetition(s) could not execute $ALLOY;" \
    "harness failures — do not publish" >&2
  exit 3
fi
exit "$status"
