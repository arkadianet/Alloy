#!/usr/bin/env bash
# Stub-binary tests for run.sh / score.py. No live endpoint, no real `alloy`:
# `ALLOY` is pointed at a shell stub whose exit code is chosen per fixture.
#
# Usage: ./eval/live-repair/tests/run_sh_test.sh
#
# Author: arkadianet
set -u

here="$(cd "$(dirname "$0")" && pwd)"
runner="$here/../run.sh"
scorer="$here/../score.py"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failures=0

ok() { echo "ok   - $1"; }
bad() { echo "FAIL - $1"; failures=$((failures + 1)); }
check() { if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (expected $1, got $2)"; fi; }

# A fixture corpus whose per-fixture stub exit code is encoded in its name.
fixtures="$tmp/fixtures"
for name in pass_fixture fail_fixture; do
  mkdir -p "$fixtures/$name/src"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n' \
    "$name" >"$fixtures/$name/Cargo.toml"
  printf 'fn main() {}\n' >"$fixtures/$name/src/main.rs"
done

stub="$tmp/stub-alloy"
cat >"$stub" <<'STUB'
#!/usr/bin/env bash
set -u
ws=""
while [ $# -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "$ws" ] || exit 0
name="$(grep -m1 '^name = ' "$ws/Cargo.toml" | cut -d'"' -f2)"
case "$name" in
  pass_fixture) echo "retrying with fresh diagnostics" >&2; exit 0;;
  fail_fixture) exit 1;;
  broken_fixture) exit 127;;
esac
exit 90
STUB
chmod +x "$stub"

run_bench() {
  FIXTURES="$fixtures" ALLOY="$1" REPS="$2" MODEL=stub-model TEMP=0.6 \
    TIMEOUT=60 bash "$runner" "$3" >"$3.stdout" 2>"$3.stderr"
  echo $?
}

# 1. A missing `alloy` binary is a broken sweep, not a corpus of failures.
code="$(run_bench "$tmp/does-not-exist" 1 "$tmp/missing.jsonl")"
if [ "$code" != 0 ]; then ok "missing ALLOY exits non-zero"; else bad "missing ALLOY exits non-zero"; fi
rows=0
[ -f "$tmp/missing.jsonl" ] && rows="$(wc -l <"$tmp/missing.jsonl")"
check 0 "$rows" "missing ALLOY scores no rows"

# 2. A non-numeric REPS must fail loudly rather than silently run zero reps.
code="$(run_bench "$stub" abc "$tmp/reps.jsonl")"
if [ "$code" != 0 ]; then ok "non-numeric REPS exits non-zero"; else bad "non-numeric REPS exits non-zero"; fi

# 3. A healthy sweep with genuine fixture failures still exits 0.
code="$(run_bench "$stub" 2 "$tmp/ok.jsonl")"
check 0 "$code" "healthy sweep exits 0 despite failing fixtures"
check 4 "$(wc -l <"$tmp/ok.jsonl")" "healthy sweep records 2 fixtures x 2 reps"

# 4. A could-not-execute row (127) means the sweep itself is broken.
mkdir -p "$fixtures/broken_fixture/src"
printf '[package]\nname = "broken_fixture"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n' \
  >"$fixtures/broken_fixture/Cargo.toml"
printf 'fn main() {}\n' >"$fixtures/broken_fixture/src/main.rs"
code="$(run_bench "$stub" 1 "$tmp/broken.jsonl")"
if [ "$code" != 0 ]; then ok "exec-failure rows exit non-zero"; else bad "exec-failure rows exit non-zero"; fi
rm -rf "$fixtures/broken_fixture"

# 5. score.py prints a Wilson interval for every fixture, not only OVERALL.
report="$(python3 "$scorer" "$tmp/ok.jsonl")"
for fixture in pass_fixture fail_fixture; do
  line="$(printf '%s\n' "$report" | grep -- "$fixture" || true)"
  case "$line" in
    *"95% CI"*) ok "score.py prints a per-fixture CI for $fixture";;
    *) bad "score.py prints a per-fixture CI for $fixture (got: $line)";;
  esac
done

if [ "$failures" -eq 0 ]; then
  echo "all run.sh stub tests passed"
else
  echo "$failures failing check(s)" >&2
fi
exit $((failures > 0))
