#!/usr/bin/env bash
# Run independent arms on one verified binary bundle and compare their reports.
#
# Usage:
#   ./eval/live-holdout/matrix.sh <arms.tsv> <out-dir> <bundle-dir>
#
# Format: arm_id<TAB>driver<TAB>model<TAB>temperature<TAB>profile<TAB>base_url<TAB>reps
#
# This stays the generic comparator: any mix of drivers, models, temperatures,
# and profiles may be compared. E1's stricter three-equal-arm contract lives in
# ./e1.sh. There is no binary fallback — every arm runs the bundle prepare.sh
# built, or nothing runs.
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
arms="${1:?usage: matrix.sh <arms.tsv> <out-dir> <bundle-dir>}"
out_dir="${2:?usage: matrix.sh <arms.tsv> <out-dir> <bundle-dir>}"
bundle="${3:?usage: matrix.sh <arms.tsv> <out-dir> <bundle-dir>}"

die() { echo "matrix.sh: $1" >&2; exit 2; }

mapfile -t binaries < <(
  printf '%s\n' alloy alloy-eval-live-holdout alloy-eval-live-naive \
    alloy-eval-live-repair | LC_ALL=C sort
)

content_sha() { sha256sum <"$1" | cut -d ' ' -f1; }

# Split on tabs without collapsing empty columns, so a missing value is a
# parse error instead of a silent column shift.
split_row() {
  local rest="$1"
  fields=()
  while [ "$rest" != "${rest#*$'\t'}" ]; do
    fields+=("${rest%%$'\t'*}")
    rest="${rest#*$'\t'}"
  done
  fields+=("$rest")
}

# --- Bundle identity: verified before the arms file is even read. -----------

manifest="$bundle/manifest.tsv"
[ -d "$bundle" ] || die "bundle directory missing: $bundle (run prepare.sh)"
[ -f "$manifest" ] || die "bundle manifest missing: $manifest (run prepare.sh)"

debug="$bundle/target/debug"
source_revision=""
worktree=""
declare -A manifest_sha=()
while IFS= read -r line || [ -n "$line" ]; do
  [ -n "$line" ] || continue
  split_row "$line"
  case "${fields[0]}" in
    source_revision)
      [ "${#fields[@]}" -eq 2 ] || die "malformed manifest source_revision record"
      source_revision="${fields[1]}"
      ;;
    worktree)
      [ "${#fields[@]}" -eq 2 ] || die "malformed manifest worktree record"
      worktree="${fields[1]}"
      ;;
    binary)
      [ "${#fields[@]}" -eq 3 ] || die "malformed manifest binary record"
      manifest_sha["${fields[1]}"]="${fields[2]}"
      ;;
    *) die "unknown manifest record '${fields[0]}' in $manifest" ;;
  esac
done <"$manifest"

[[ "$source_revision" =~ ^[0-9a-f]{40}$ ]] ||
  die "manifest source_revision must be a 40-hex commit sha, got '$source_revision'"
[ "$worktree" = "clean" ] ||
  die "bundle must be built from a clean worktree, manifest says '$worktree'"
[ "${#manifest_sha[@]}" -eq "${#binaries[@]}" ] ||
  die "manifest must list exactly ${#binaries[@]} binaries, got ${#manifest_sha[@]}"
for name in "${binaries[@]}"; do
  [ -n "${manifest_sha[$name]+x}" ] || die "manifest does not list $name"
  [ -x "$debug/$name" ] || die "bundle binary missing or not executable: $debug/$name"
  [ "$(content_sha "$debug/$name")" = "${manifest_sha[$name]}" ] ||
    die "bundle binary $name sha256 does not match the manifest; rebuild with prepare.sh"
done
bundle_sha256="$(content_sha "$manifest")"

# The bundle pins the binaries, but this checkout still supplies run.sh, the
# profiles, and the fixture corpus — the treatment and the oracle. Those must
# be the bundle's commit, or the arms are not the harness the manifest names.
# Unrelated working-tree edits elsewhere in the repository are not the harness
# and do not block a run.
treatment_paths=(eval/live-holdout profiles crates/alloy-eval/fixtures/holdout)
head_revision="$(git -C "$repo" rev-parse HEAD 2>/dev/null || true)"
[ -n "$head_revision" ] || die "cannot read the checkout revision at $repo"
[ "$head_revision" = "$source_revision" ] ||
  die "checkout is at revision $head_revision but the bundle was built from \
$source_revision; check out that commit or rebuild the bundle"
drift="$(git -C "$repo" status --porcelain --untracked-files=all \
  -- "${treatment_paths[@]}" 2>/dev/null)" ||
  die "cannot inspect ${treatment_paths[*]} for uncommitted changes"
[ -z "$drift" ] ||
  die "uncommitted changes to the harness (${treatment_paths[*]}) make this \
checkout differ from $source_revision:
$drift"

# --- Output and arms: validated before any model work. ----------------------

if [ -e "$out_dir" ]; then
  [ -d "$out_dir" ] || die "output path is not a directory: $out_dir"
  [ -z "$(ls -A "$out_dir")" ] ||
    die "output directory must be empty, got existing entries in $out_dir"
fi

fixtures="${FIXTURES:-$repo/crates/alloy-eval/fixtures/holdout}"
[ -d "$fixtures" ] || die "fixtures root missing: $fixtures"
[ -f "$arms" ] || die "arms file missing: $arms"

arm_ids=()
arm_drivers=()
arm_models=()
arm_temps=()
arm_profiles=()
arm_urls=()
arm_reps=()
declare -A seen_arms=()
line_no=0
while IFS= read -r line || [ -n "$line" ]; do
  line_no=$((line_no + 1))
  case "$line" in '' | \#*) continue ;; esac
  split_row "$line"
  [ "${#fields[@]}" -eq 7 ] ||
    die "line $line_no: expected 7 tab-separated columns \
(arm_id driver model temperature profile base_url reps), got ${#fields[@]}"
  arm_id="${fields[0]}"
  driver="${fields[1]}"
  model="${fields[2]}"
  temperature="${fields[3]}"
  profile="${fields[4]}"
  base_url="${fields[5]}"
  reps="${fields[6]}"

  if [ "$arm_id" = "arm_id" ]; then
    [ "$driver" = "driver" ] || die "line $line_no: invalid header"
    continue
  fi
  case "$arm_id" in
    *[!A-Za-z0-9_.-]*) die "line $line_no: invalid arm id $arm_id" ;;
  esac
  [ -z "${seen_arms[$arm_id]+x}" ] || die "duplicate arm id $arm_id"
  seen_arms[$arm_id]=1
  case "$driver" in
    naive)
      [ "$profile" = "none" ] ||
        die "arm $arm_id: driver naive runs no profile, expected none, got '$profile'"
      ;;
    alloy)
      case "$profile" in
        default | autonomous) ;;
        *) die "arm $arm_id: profile must be default or autonomous, got '$profile'" ;;
      esac
      ;;
    *) die "arm $arm_id: driver must be naive or alloy, got '$driver'" ;;
  esac
  [[ "$temperature" =~ ^[0-9]+([.][0-9]+)?$ ]] ||
    die "arm $arm_id: temperature must be a number, got '$temperature'"
  case "$model$base_url" in
    *[\"\\]*) die "arm $arm_id: model and base_url must not contain quotes or backslashes" ;;
  esac
  [ -n "$model" ] || die "arm $arm_id: model is required"
  [ -n "$base_url" ] || die "arm $arm_id: base_url is required"
  case "$reps" in
    '' | *[!0-9]*) die "arm $arm_id: reps must be a positive integer, got '$reps'" ;;
  esac
  [ "$reps" -ge 1 ] || die "arm $arm_id: reps must be at least 1"

  arm_ids+=("$arm_id")
  arm_drivers+=("$driver")
  arm_models+=("$model")
  arm_temps+=("$temperature")
  arm_profiles+=("$profile")
  arm_urls+=("$base_url")
  arm_reps+=("$reps")
done <"$arms"

[ "${#arm_ids[@]}" -ge 2 ] || die "at least two arms are required, got ${#arm_ids[@]}"
[ -n "${ALLOY_API_KEY:-}" ] ||
  die "ALLOY_API_KEY must be set to a non-empty process environment variable before any repetition"

mkdir -p "$out_dir" || die "cannot create $out_dir"

# --- Run every arm on the same bundle and the same provenance. --------------

echo "BUNDLE $bundle source_revision=$source_revision binary_bundle_sha256=$bundle_sha256"

arm_args=()
status=0
for index in "${!arm_ids[@]}"; do
  arm_id="${arm_ids[$index]}"
  observations="$out_dir/$arm_id.jsonl"
  report="$out_dir/$arm_id.report.json"
  echo "RUN $arm_id: driver=${arm_drivers[$index]} model=${arm_models[$index]}" \
    "profile=${arm_profiles[$index]} temp=${arm_temps[$index]} reps=${arm_reps[$index]}"
  if ! DRIVER="${arm_drivers[$index]}" MODEL="${arm_models[$index]}" \
    TEMP="${arm_temps[$index]}" PROFILE="${arm_profiles[$index]}" \
    BASEURL="${arm_urls[$index]}" REPS="${arm_reps[$index]}" \
    FIXTURES="$fixtures" \
    ALLOY="$debug/alloy" NAIVE="$debug/alloy-eval-live-naive" \
    SCORER="$debug/alloy-eval-live-repair" \
    EVAL_HOLDOUT="$debug/alloy-eval-live-holdout" \
    SOURCE_REVISION="$source_revision" BUNDLE_SHA256="$bundle_sha256" \
    "$repo/eval/live-holdout/run.sh" "$observations" </dev/null; then
    status=1
  fi
  [ -f "$report" ] || {
    echo "matrix.sh: missing report for $arm_id: $report" >&2
    status=1
    continue
  }
  arm_args+=(--arm "$arm_id=$report")
done

[ "$status" -eq 0 ] || exit "$status"

"$debug/alloy-eval-live-holdout" compare "${arm_args[@]}" \
  --out "$out_dir/matrix.report.json"
