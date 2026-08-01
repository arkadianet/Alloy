#!/usr/bin/env bash
# E1: one raw model call vs Alloy default vs Alloy autonomous, on identical
# endpoint settings and one verified binary bundle.
#
# Usage:
#   ./eval/live-holdout/e1.sh <arms.tsv> <out-dir> <bundle-dir>
#
# Format: arm_id<TAB>driver<TAB>model<TAB>temperature<TAB>profile<TAB>base_url<TAB>reps
#
#   naive             naive  <model> <temp> none        <base-url> <reps>
#   alloy-default     alloy  <model> <temp> default     <base-url> <reps>
#   alloy-autonomous  alloy  <model> <temp> autonomous  <base-url> <reps>
#
# (Aligned above for reading; the file itself must be tab-separated.)
#
# Every row is read and checked before anything runs: E1 is only interpretable
# if the three treatment arms differ by treatment alone. Generic model,
# temperature, and profile comparisons belong in ./matrix.sh, which this
# wrapper delegates to once preflight passes.
#
# Author: arkadianet
set -u

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
arms="${1:?usage: e1.sh <arms.tsv> <out-dir> <bundle-dir>}"
out_dir="${2:?usage: e1.sh <arms.tsv> <out-dir> <bundle-dir>}"
bundle="${3:?usage: e1.sh <arms.tsv> <out-dir> <bundle-dir>}"

die() { echo "e1.sh: $1" >&2; exit 2; }

# Split on tabs without collapsing empty columns (see matrix.sh).
split_row() {
  local rest="$1"
  fields=()
  while [ "$rest" != "${rest#*$'\t'}" ]; do
    fields+=("${rest%%$'\t'*}")
    rest="${rest#*$'\t'}"
  done
  fields+=("$rest")
}

fixtures="${FIXTURES:-$repo/crates/alloy-eval/fixtures/holdout}"
[ -d "$fixtures" ] || die "fixtures root missing: $fixtures"
[ -f "$arms" ] || die "arms file missing: $arms"

roles=()
models=()
temperatures=()
base_urls=()
repetitions=()
line_no=0
while IFS= read -r line || [ -n "$line" ]; do
  line_no=$((line_no + 1))
  case "$line" in '' | \#*) continue ;; esac
  split_row "$line"
  [ "${#fields[@]}" -eq 7 ] ||
    die "line $line_no: expected 7 tab-separated columns \
(arm_id driver model temperature profile base_url reps), got ${#fields[@]}"
  [ "${fields[0]}" != "arm_id" ] || continue
  roles+=("${fields[1]}/${fields[4]}")
  models+=("${fields[2]}")
  temperatures+=("${fields[3]}")
  base_urls+=("${fields[5]}")
  repetitions+=("${fields[6]}")
done <"$arms"

[ "${#roles[@]}" -eq 3 ] ||
  die "E1 needs exactly three arms — naive/none, alloy/default, alloy/autonomous \
— got ${#roles[@]}"

# `compare` treats the first report as its baseline, so an E1 matrix is only
# interpretable when its raw-model control is the first data row.
[ "${roles[0]}" = "naive/none" ] ||
  die "E1 baseline must be the first data row: expected naive/none, got ${roles[0]}"

for required in naive/none alloy/default alloy/autonomous; do
  found=0
  for role in "${roles[@]}"; do
    if [ "$role" = "$required" ]; then
      found=$((found + 1))
    fi
  done
  [ "$found" -eq 1 ] ||
    die "E1 needs exactly one $required arm, got $found (roles: ${roles[*]})"
done

# Treatment is the only variable E1 may change.
require_shared() {
  local name="$1" first="$2" value
  shift 2
  for value in "$@"; do
    [ "$value" = "$first" ] ||
      die "E1 arms must share one $name, got '$first' and '$value'"
  done
}
require_shared model "${models[@]}"
require_shared temperature "${temperatures[@]}"
require_shared base_url "${base_urls[@]}"
require_shared repetitions "${repetitions[@]}"

echo "E1 preflight ok: naive/default/autonomous model=${models[0]}" \
  "temperature=${temperatures[0]} base_url=${base_urls[0]}" \
  "repetitions=${repetitions[0]} fixtures=$fixtures"

export FIXTURES="$fixtures"
exec "$here/matrix.sh" "$arms" "$out_dir" "$bundle"
