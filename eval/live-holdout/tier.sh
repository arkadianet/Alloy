#!/usr/bin/env bash
# Run the live-holdout comparison at one of three sizes.
#
# Usage:
#   ./eval/live-holdout/tier.sh <smoke|dev|gate> <out-dir> <bundle-dir>
#
# The tiers exist because gate-grade evidence is far too slow to iterate on.
# They differ only in corpus breadth and repetition count; the harness, arms,
# scoring, and abort behaviour are identical, so a result at one tier is a
# smaller sample of the same measurement, never a different one.
#
#   smoke  one fixture per repair family, 1 repetition  — "is the pipeline alive?"
#   dev    the whole corpus,              1 repetition  — "did my change move it?"
#   gate   the whole corpus,              3 repetitions — publishable evidence
#
# Only gate output should be cited. smoke and dev have too few observations per
# fixture to bound a per-fixture rate, so their clustered intervals are wide by
# construction and are reported for direction, not for a claim.
#
# Author: arkadianet
set -u

here="$(cd "$(dirname "$0")" && pwd -P)"
repo="$(cd "$here/../.." && pwd -P)"

tier="${1:?usage: tier.sh <smoke|dev|gate> <out-dir> <bundle-dir>}"
out_dir="${2:?usage: tier.sh <smoke|dev|gate> <out-dir> <bundle-dir>}"
bundle="${3:?usage: tier.sh <smoke|dev|gate> <out-dir> <bundle-dir>}"

die() { echo "live-holdout/tier.sh: $1" >&2; exit 2; }

MODEL="${MODEL:-Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf}"
TEMP="${TEMP:-0.6}"
BASEURL="${BASEURL:-http://127.0.0.1:8089/v1/}"

case "$tier" in
  smoke) fixtures="$here/fixtures/e2-smoke-v1"; reps=1 ;;
  dev)   fixtures="$here/fixtures/e2-semantic-v1"; reps=1 ;;
  gate)  fixtures="$here/fixtures/e2-semantic-v1"; reps=3 ;;
  *) die "tier must be smoke, dev, or gate, got '$tier'" ;;
esac
[ -d "$fixtures" ] || die "corpus missing for tier $tier: $fixtures"

arms="$out_dir/arms.tsv"
mkdir -p "$out_dir" || die "cannot create $out_dir"
{
  printf '# arm_id\tdriver\tmodel\ttemperature\tprofile\tbase_url\treps\n'
  printf 'naive\tnaive\t%s\t%s\tnone\t%s\t%s\n' "$MODEL" "$TEMP" "$BASEURL" "$reps"
  printf 'alloy-default\talloy\t%s\t%s\tdefault\t%s\t%s\n' "$MODEL" "$TEMP" "$BASEURL" "$reps"
  printf 'alloy-autonomous\talloy\t%s\t%s\tautonomous\t%s\t%s\n' "$MODEL" "$TEMP" "$BASEURL" "$reps"
} >"$arms" || die "cannot write $arms"

count="$(find "$fixtures" -mindepth 1 -maxdepth 1 -type d | wc -l)"
echo "TIER $tier: $count fixture(s) x 3 arms x $reps rep(s) = $((count * 3 * reps)) attempts"
[ "$tier" = "gate" ] ||
  echo "TIER $tier is for iteration only — do not cite it as evidence"

FIXTURES="$fixtures" exec "$here/matrix.sh" "$arms" "$out_dir" "$bundle"
