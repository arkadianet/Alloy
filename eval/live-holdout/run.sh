#!/usr/bin/env bash
# Live holdout executor — one model driver + live OpenAI-compatible endpoint
# on the RFC-0016 holdout workspaces. NOT an offline gate. See ./README.md.
#
# DRIVER=alloy runs the real agent; DRIVER=naive runs the one-shot, tool-free
# baseline. Both arms then share one independent cargo check, hidden-test,
# reference, and strict-oracle path.
#
# Usage:
#   MODEL=… BASEURL=http://127.0.0.1:8089/v1/ PROFILE=default REPS=1 \
#     ./eval/live-holdout/run.sh /tmp/live-holdout.jsonl
#   DRIVER=naive MODEL=… BASEURL=… REPS=1 \
#     ./eval/live-holdout/run.sh /tmp/live-naive.jsonl
#
# Author: arkadianet
set -u

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:?usage: run.sh <out.jsonl>}"
artifacts="${out%.jsonl}.artifacts"

FIXTURES="${FIXTURES:-$repo/crates/alloy-eval/fixtures/holdout}"
DRIVER="${DRIVER:-alloy}"
MODEL="${MODEL:-Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf}"
TEMP="${TEMP:-0.6}"
REPS="${REPS:-1}"
PROFILE="${PROFILE:-}"
BASEURL="${BASEURL:-http://127.0.0.1:8089/v1/}"
TIMEOUT="${TIMEOUT:-600}"
GOAL="${GOAL:-fix the compile error in this crate}"
SCORE="${SCORE:-1}"
# Optional file of "<fixture_id> <repetition>" lines. When set, exactly those
# attempts run, in that order, so the matrix can interleave arms by block
# instead of running each arm start-to-finish.
SCHEDULE="${SCHEDULE:-}"
APPEND="${APPEND:-0}"
EVAL_HOLDOUT="${EVAL_HOLDOUT:-}"

die() { echo "live-holdout/run.sh: $1" >&2; exit 2; }

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
  for target in "${CARGO_TARGET_DIR:-$repo/target}" "$HOME/.cache/cargo-target"; do
    if [ -x "$target/debug/$name" ]; then
      printf '%s' "$target/debug/$name"
      return
    fi
  done
  die "missing $name (cargo build -p …); looked under configured target directories"
}

EVAL_HOLDOUT="$(resolve_bin alloy-eval-live-holdout "$EVAL_HOLDOUT")"

# Each arm resolves only the driver it runs, and only the naive arm may omit
# a profile — the report schema treats driver+profile as arm identity.
case "$DRIVER" in
  naive)
    driver_bin="$(resolve_bin alloy-eval-live-naive "${NAIVE:-}")"
    PROFILE="${PROFILE:-none}"
    [ "$PROFILE" = "none" ] ||
      die "DRIVER=naive runs no profile; unset PROFILE or set it to none, got '$PROFILE'"
    profile_json=null
    ;;
  alloy)
    driver_bin="$(resolve_bin alloy "${ALLOY:-}")"
    SCORER="$(resolve_bin alloy-eval-live-repair "${SCORER:-}")"
    PROFILE="${PROFILE:-default}"
    case "$PROFILE" in
      default|autonomous) ;;
      *) die "PROFILE must be default or autonomous, got '$PROFILE'";;
    esac
    profile_json="\"$PROFILE\""
    "$driver_bin" --version >/dev/null 2>&1
    probe=$?
    case "$probe" in
      126|127) die "alloy at $driver_bin could not be executed (exit $probe)";;
    esac
    ;;
  *) die "DRIVER must be naive or alloy, got '$DRIVER'";;
esac

# Build provenance: two reports are the same arm only if they came from the
# same source and the same binaries. A multi-arm wrapper computes one bundle
# digest over every arm's binaries and passes it in; the fallback below only
# covers the binaries this single sweep runs.
SOURCE_REVISION="${SOURCE_REVISION:-$(git -C "$repo" rev-parse HEAD 2>/dev/null || true)}"
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] ||
  die "SOURCE_REVISION must be a 40-hex commit sha, got '$SOURCE_REVISION'"
content_sha() { sha256sum <"$1" | cut -d ' ' -f1; }
bundle_binaries=("$driver_bin" "$EVAL_HOLDOUT")
if [ "$DRIVER" = "alloy" ]; then
  bundle_binaries+=("$SCORER")
fi
BUNDLE_SHA256="${BUNDLE_SHA256:-$(
  for binary in "${bundle_binaries[@]}"; do
    content_sha "$binary"
  done | tr -d '\n' | sha256sum | cut -d ' ' -f1
)}"
[[ "$BUNDLE_SHA256" =~ ^[0-9a-f]{64}$ ]] ||
  die "BUNDLE_SHA256 must be a 64-hex sha256, got '$BUNDLE_SHA256'"

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
[[ "$TEMP" =~ ^[0-9]+([.][0-9]+)?$ ]] || die "TEMP must be a number, got '$TEMP'"
[ -d "$FIXTURES" ] || die "fixtures root missing: $FIXTURES"
[ -n "${ALLOY_API_KEY:-}" ] ||
  die "ALLOY_API_KEY must be set to a non-empty process environment variable before any repetition"

# Only the agent arm reads router.toml; the naive driver takes the endpoint
# on its command line.
router=""
if [ "$DRIVER" = "alloy" ]; then
  router="$("$SCORER" render-router --model "$MODEL" --temperature "$TEMP" --base-url "$BASEURL")" \
    || die "render-router failed"
fi

mapfile -t ids < <(
  find "$FIXTURES" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | LC_ALL=C sort
)
[ "${#ids[@]}" -gt 0 ] || die "no holdout fixture directories under $FIXTURES"

fixture_target_path() {
  "$EVAL_HOLDOUT" target-path --manifest "$1"
}

for id in "${ids[@]}"; do
  fixture_dir="$FIXTURES/$id"
  manifest="$fixture_dir/manifest.toml"
  [ -f "$manifest" ] || die "fixture $id missing manifest.toml"
  target_path="$(fixture_target_path "$manifest")" ||
    die "fixture $id manifest has no naive_target_path"
  [ -f "$fixture_dir/workspace/$target_path.post" ] ||
    die "fixture $id missing strict oracle workspace/$target_path.post"
  [ -d "$fixture_dir/oracle-tests" ] ||
    die "fixture $id missing hidden semantic oracle directory oracle-tests/"
  compgen -G "$fixture_dir/oracle-tests/*.rs" >/dev/null ||
    die "fixture $id has no hidden semantic oracle Rust tests"
done

# Exclusive lock on the observations file so two sweeps cannot interleave rows.
mkdir -p "$(dirname -- "$out")"
exec 9>"$out.lock" || die "could not open lock $out.lock"
flock -n 9 || die "another live-holdout sweep holds $out.lock"
# APPEND lets an interleaved matrix accumulate one arm's attempts across many
# invocations; a fresh sweep still starts from an empty file.
if [ "$APPEND" = "1" ]; then
  [ -e "$out" ] || : > "$out" || die "could not initialize observations file: $out"
else
  : > "$out" || die "could not initialize observations file: $out"
fi
mkdir -p "$artifacts" || die "could not create evidence root: $artifacts"
total=0
process_passed=0
oracle_passed=0
# v5 primary outcome: compile + hidden tests + safety, without byte canonicality.
semantic_passed=0
unexecutable=0

# One attempt per line: "<fixture_id> <repetition>". Without a SCHEDULE this
# is every fixture across every repetition, which is the standalone sweep.
attempts=()
if [ -n "$SCHEDULE" ]; then
  [ -f "$SCHEDULE" ] || die "SCHEDULE file not found: $SCHEDULE"
  while read -r sched_id sched_rep; do
    case "$sched_id" in '' | '#'*) continue ;; esac
    case " ${ids[*]} " in
      *" $sched_id "*) ;;
      *) die "SCHEDULE names unknown fixture '$sched_id'" ;;
    esac
    case "$sched_rep" in
      '' | *[!0-9]*) die "SCHEDULE repetition must be a positive integer, got '$sched_rep'" ;;
    esac
    attempts+=("$sched_id $sched_rep")
  done <"$SCHEDULE"
  [ "${#attempts[@]}" -gt 0 ] || die "SCHEDULE $SCHEDULE contained no attempts"
else
  for id in "${ids[@]}"; do
    for rep in $(seq 1 "$REPS"); do
      attempts+=("$id $rep")
    done
  done
fi

for attempt in "${attempts[@]}"; do
  read -r id rep <<<"$attempt"
  workspace="$FIXTURES/$id/workspace"
  [ -d "$workspace" ] || die "fixture $id missing workspace/ at $workspace"
  target_path="$(fixture_target_path "$FIXTURES/$id/manifest.toml")" ||
    die "fixture $id manifest has no naive_target_path"
  {
    # Every cargo invocation below pins CARGO_TARGET_DIR inside this
    # throwaway workspace. Without it they inherit the machine-wide
    # ~/.cargo/config.toml target-dir, and because `cp -a` preserves mtimes,
    # cargo replays a cached result from a previous attempt on the same
    # fixture — reporting a clean build for source that does not compile.
    # That bias is arm-asymmetric: an arm that rewrites the whole file gets
    # fresh mtimes and escapes it, so the comparison itself is corrupted.
    ws="$(mktemp -d)" || die "could not create workspace for $id#$rep"
    cp -a "$workspace"/. "$ws/" ||
      die "fixture copy failed for $id#$rep"
    # The golden reference is an oracle input, never model-visible workspace
    # content.
    rm -f "$ws/$target_path.post"
    if [ "$DRIVER" = "alloy" ]; then
      cp -a "$repo/profiles" "$ws/profiles" ||
        die "profile copy failed for $id#$rep"
      printf '%s' "$router" >"$ws/router.toml" ||
        die "router write failed for $id#$rep"
    fi
    git -C "$ws" init -q || die "git init failed for $id#$rep"
    git -C "$ws" add -A || die "git add failed for $id#$rep"
    git -C "$ws" -c user.name=live-holdout \
      -c user.email=live-holdout@localhost commit -qm fixture ||
      die "git commit failed for $id#$rep"
    evidence_relpath="$id/rep-$rep"
    evidence="$artifacts/$evidence_relpath"
    # Start each attempt from an empty bundle so a previous arm's evidence
    # can never be read as this one's.
    rm -rf "$evidence"
    mkdir -p "$evidence" || die "could not create evidence directory for $id#$rep"
    # Pre-run diagnostics for every arm. The naive prompt is the only path
    # that shows this file to a model; it holds no golden or hidden-test data.
    (cd "$ws" && CARGO_TARGET_DIR="$ws/target" timeout "$TIMEOUT" \
      cargo check --offline --message-format=short) \
      >"$evidence/initial-cargo.log" 2>&1 || true
    start_ms=$(date +%s%3N)
    case "$DRIVER" in
      naive)
        if timeout "$TIMEOUT" "$driver_bin" \
          --workspace "$ws" \
          --target "$target_path" \
          --diagnostics "$evidence/initial-cargo.log" \
          --goal "$GOAL" \
          --model "$MODEL" \
          --temperature "$TEMP" \
          --base-url "$BASEURL" \
          --result "$evidence/naive-result.json" \
          >"$ws/run.log" 2>&1; then
          code=0
        else
          code=$?
        fi
        ;;
      alloy)
        if timeout "$TIMEOUT" "$driver_bin" --workspace "$ws" --profile "$PROFILE" \
          run "$GOAL" --yes \
          >"$ws/run.log" 2>&1; then
          code=0
        else
          code=$?
        fi
        ;;
    esac
    wall_ms=$(($(date +%s%3N) - start_ms))
    cp "$ws/run.log" "$evidence/run.log" ||
      die "could not retain run log for $id#$rep"
    if [ "$DRIVER" = "naive" ] && [ ! -f "$evidence/naive-result.json" ]; then
      die "naive driver did not persist naive-result.json for $id#$rep; telemetry is incomplete"
    fi
    if [ -f "$ws/$target_path" ]; then
      cp "$ws/$target_path" "$evidence/final-target.rs" ||
        die "could not retain final target for $id#$rep"
      git -C "$ws" --no-pager diff -- "$target_path" >"$evidence/patch.diff" ||
        die "could not retain patch for $id#$rep"
    fi
    compile_clean=false
    cargo_check_exit=null
    case "$code" in
      124|126|127) ;;
      *)
        if [ -f "$ws/$target_path" ]; then
          if (cd "$ws" && CARGO_TARGET_DIR="$ws/target" timeout "$TIMEOUT" \
            cargo check --offline --quiet) \
            >"$evidence/cargo-check.log" 2>&1; then
            compile_clean=true
            cargo_check_exit=0
          else
            cargo_check_exit=$?
          fi
        fi
        ;;
    esac
    tests_pass=false
    cargo_test_exit=null
    case "$code" in
      124|126|127) ;;
      *)
        # Scrub before injecting. `cp -a` MERGES, so any tests/ the driver
        # left behind would survive alongside the hidden oracle and be run by
        # a bare `cargo test` — letting a model's own tests decide the score
        # in either direction. No driver writes tests today, which is why this
        # has never fired; it is a hole, not a symptom.
        rm -rf "$ws/tests" ||
          die "could not clear staged tests for $id#$rep"
        mkdir -p "$ws/tests" ||
          die "could not stage semantic tests for $id#$rep"
        cp -a "$FIXTURES/$id/oracle-tests"/. "$ws/tests/" ||
          die "semantic test copy failed for $id#$rep"
        # Score the oracle target by name rather than everything cargo finds.
        # No fixture ships inline `#[cfg(test)]` (verified across the corpus),
        # so this is behaviour-preserving today and stays honest if one ever
        # does.
        if (cd "$ws" && CARGO_TARGET_DIR="$ws/target" timeout "$TIMEOUT" \
          cargo test --offline --quiet --test semantic) \
          >"$evidence/cargo-test.log" 2>&1; then
          tests_pass=true
          cargo_test_exit=0
        else
          cargo_test_exit=$?
        fi
        ;;
    esac
    oracle_out="$("$EVAL_HOLDOUT" oracle \
      --fixture-dir "$FIXTURES/$id" \
      --workspace "$ws" \
      --run-log "$ws/run.log" \
      --exit-code "$code" \
      --compile-clean "$compile_clean" \
      --cargo-check-exit "$cargo_check_exit" \
      --tests-pass "$tests_pass" \
      --cargo-test-exit "$cargo_test_exit")" ||
      die "oracle failed for $id#$rep"
    read -r process_pass compile_clean tests_pass safety_clean semantic_pass \
      reference_match oracle_pass failure_class \
      cargo_check_exit cargo_test_exit generations <<<"$oracle_out" ||
      die "oracle parse failed for $id#$rep"
    case "$DRIVER" in
      naive)
        # No event stream; the empty file keeps one evidence layout per arm.
        : >"$evidence/events.jsonl" ||
          die "could not create events placeholder for $id#$rep"
        telemetry_input="$evidence/naive-result.json"
        ;;
      alloy)
        # Ask for the runtime's whole page (1000); the extractor rejects an
        # export that fills it rather than counting a truncated run.
        if timeout 30 "$driver_bin" --workspace "$ws" --profile "$PROFILE" \
          events --json --limit 1000 >"$evidence/events.jsonl" 2>"$evidence/events.stderr"; then
          :
        else
          events_exit=$?
          die "Alloy event export failed for $id#$rep (exit $events_exit); see $evidence/events.stderr"
        fi
        telemetry_input="$evidence/events.jsonl"
        ;;
    esac
    telemetry_out="$("$EVAL_HOLDOUT" telemetry \
      --driver "$DRIVER" --input "$telemetry_input")" ||
      die "telemetry extraction failed for $id#$rep"
    read -r model_calls tokens_in tokens_out <<<"$telemetry_out" ||
      die "telemetry parse failed for $id#$rep"
    printf '{"fixture_id":"%s","repetition":%d,"driver":"%s","run_exit":%d,"process_pass":%s,"compile_clean":%s,"tests_pass":%s,"safety_clean":%s,"semantic_pass":%s,"reference_match":%s,"strict_oracle":%s,"failure_class":"%s","model_calls":%s,"tokens_in":%s,"tokens_out":%s}\n' \
      "$id" "$rep" "$DRIVER" "$code" "$process_pass" "$compile_clean" "$tests_pass" \
      "$safety_clean" "$semantic_pass" "$reference_match" "$oracle_pass" "$failure_class" \
      "$model_calls" "$tokens_in" "$tokens_out" >"$evidence/metadata.json"
    total=$((total + 1))
    [ "$process_pass" = "true" ] && process_passed=$((process_passed + 1))
    [ "$oracle_pass" = "true" ] && oracle_passed=$((oracle_passed + 1))
    [ "$semantic_pass" = "true" ] && semantic_passed=$((semantic_passed + 1))
    case "$code" in
      126|127) unexecutable=$((unexecutable + 1));;
    esac
    printf '{"fixture_id":"%s","repetition":%d,"exit_code":%d,"process_pass":%s,"compile_clean":%s,"tests_pass":%s,"safety_clean":%s,"semantic_pass":%s,"reference_match":%s,"oracle_pass":%s,"failure_class":"%s","cargo_check_exit":%s,"cargo_test_exit":%s,"repair_generations":%d,"wall_ms":%d,"evidence_relpath":"%s","model":"%s","temperature":%s,"driver":"%s","profile":%s,"base_url":"%s","harness":{"source_revision":"%s","binary_bundle_sha256":"%s"},"corpus":"rfc0016-holdout-live","model_calls":%s,"tokens_in":%s,"tokens_out":%s}\n' \
      "$id" "$rep" "$code" "${process_pass,,}" "${compile_clean,,}" \
      "${tests_pass,,}" "${safety_clean,,}" "${semantic_pass,,}" \
      "${reference_match,,}" "${oracle_pass,,}" "$failure_class" \
      "$cargo_check_exit" "$cargo_test_exit" "$generations" "$wall_ms" \
      "$evidence_relpath" "$MODEL" "$TEMP" "$DRIVER" "$profile_json" "$BASEURL" \
      "$SOURCE_REVISION" "$BUNDLE_SHA256" \
      "$model_calls" "$tokens_in" "$tokens_out" >>"$out"
    echo "[$semantic_passed/$total semantic; $oracle_passed oracle; $process_passed process] $id#$rep \
semantic=$semantic_pass oracle=$oracle_pass tests=$tests_pass class=$failure_class generations=$generations ${wall_ms}ms"
    echo "  evidence: $evidence" >&2
    rm -rf "$ws"
  }
done

echo "DONE driver=$DRIVER semantic=$semantic_passed/$total oracle=$oracle_passed/$total \
process=$process_passed/$total -> $out (live-BYOM holdout; not an offline gate)"
echo "EVIDENCE $artifacts"

if [ "$total" -eq 0 ]; then
  die "no repetitions ran — the sweep is broken, not the fixtures"
fi

status=0
if [ "$SCORE" = "1" ]; then
  "$EVAL_HOLDOUT" score \
    --fixtures "$FIXTURES" \
    --observations "$out" \
    --model "$MODEL" \
    --temperature "$TEMP" \
    --driver "$DRIVER" \
    --profile "$PROFILE" \
    --base-url "$BASEURL" \
    --source-revision "$SOURCE_REVISION" \
    --binary-bundle-sha256 "$BUNDLE_SHA256" \
    --reps "$REPS" \
    --out "${out%.jsonl}.report.json"
  status=$?
fi

if [ "$unexecutable" -gt 0 ]; then
  echo "live-holdout/run.sh: $unexecutable/$total repetition(s) could not execute $driver_bin;" \
    "harness failures — do not publish" >&2
  exit 3
fi
exit "$status"
