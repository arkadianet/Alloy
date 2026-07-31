#!/usr/bin/env bash
# Run independent model/profile arms and compare their Rust reports.
#
# Format: arm_id<TAB>model<TAB>temperature<TAB>profile<TAB>base_url<TAB>reps
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
arms="${1:?usage: matrix.sh <arms.tsv> <out-dir>}"
out_dir="${2:?usage: matrix.sh <arms.tsv> <out-dir>}"
eval_holdout="${EVAL_HOLDOUT:-}"
if [ -z "$eval_holdout" ]; then
  for target in "${CARGO_TARGET_DIR:-$repo/target}" "$HOME/.cache/cargo-target"; do
    if [ -x "$target/debug/alloy-eval-live-holdout" ]; then
      eval_holdout="$target/debug/alloy-eval-live-holdout"
      break
    fi
  done
fi

[ -f "$arms" ] || { echo "matrix.sh: arms file missing: $arms" >&2; exit 2; }
[ -x "$eval_holdout" ] || {
  echo "matrix.sh: evaluator missing or not executable: $eval_holdout" >&2
  exit 2
}
mkdir -p "$out_dir" || { echo "matrix.sh: cannot create $out_dir" >&2; exit 2; }

arm_args=()
declare -A seen_arms=()
status=0
while IFS=$'\t' read -r arm_id model temperature profile base_url reps; do
  case "$arm_id" in
    ''|\#*) continue ;;
    arm_id)
      [ "$model" = "model" ] || { echo "matrix.sh: invalid header" >&2; exit 2; }
      continue
      ;;
  esac
  case "$arm_id" in
    *[!A-Za-z0-9_.-]*) echo "matrix.sh: invalid arm id $arm_id" >&2; exit 2 ;;
  esac
  if [ -n "${seen_arms[$arm_id]+x}" ]; then
    echo "matrix.sh: duplicate arm id $arm_id" >&2
    exit 2
  fi
  seen_arms[$arm_id]=1
  case "$profile" in
    default|autonomous) ;;
    *) echo "matrix.sh: invalid profile $profile" >&2; exit 2 ;;
  esac
  case "$reps" in
    ''|*[!0-9]*) echo "matrix.sh: invalid reps for $arm_id" >&2; exit 2 ;;
  esac
  [ "$reps" -ge 1 ] || { echo "matrix.sh: reps must be positive" >&2; exit 2; }

  observations="$out_dir/$arm_id.jsonl"
  report="$out_dir/$arm_id.report.json"
  echo "RUN $arm_id: model=$model profile=$profile temp=$temperature reps=$reps"
  if ! MODEL="$model" TEMP="$temperature" PROFILE="$profile" BASEURL="$base_url" \
    REPS="$reps" EVAL_HOLDOUT="$eval_holdout" \
    "$repo/eval/live-holdout/run.sh" "$observations" </dev/null; then
    status=1
  fi
  [ -f "$report" ] || {
    echo "matrix.sh: missing report for $arm_id: $report" >&2
    status=1
    continue
  }
  arm_args+=(--arm "$arm_id=$report")
done <"$arms"

# Each complete arm contributes two array elements (--arm and id=report).
arm_count=$((${#arm_args[@]} / 2))
[ "$arm_count" -ge 2 ] || {
  echo "matrix.sh: at least two complete arms are required" >&2
  exit 2
}
[ "$status" -eq 0 ] || exit "$status"

"$eval_holdout" compare "${arm_args[@]}" --out "$out_dir/matrix.report.json"
