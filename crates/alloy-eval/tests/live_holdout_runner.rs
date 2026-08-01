//! Offline integration coverage for the live-holdout shell contract.
//!
//! Stubs replace the `alloy` and naive drivers; no model endpoint is
//! contacted. The real runner, independent cargo post-check, Rust oracle, and
//! scorer still execute for both driver arms.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use alloy_eval::{
    live_holdout_target_path_text, LiveHoldoutDriver, LiveHoldoutReport,
    LIVE_HOLDOUT_REPORT_VERSION,
};

const EVALUATOR: &str = env!("CARGO_BIN_EXE_alloy-eval-live-holdout");
const SCORER: &str = env!("CARGO_BIN_EXE_alloy-eval-live-repair");
const NAIVE_SOURCE_REVISION: &str = "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1";
const NAIVE_BUNDLE_SHA256: &str =
    "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn write_fixture(root: &Path, id: &str) {
    let fixture = root.join(id);
    fs::create_dir_all(fixture.join("workspace/src")).unwrap();
    fs::create_dir_all(fixture.join("oracle-tests")).unwrap();
    fs::write(
        fixture.join("manifest.toml"),
        format!("naive_target_path = \"src/lib.rs\"\nid = \"{id}\"\n"),
    )
    .unwrap();
    fs::write(
        fixture.join("workspace/Cargo.toml"),
        format!(
            "[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
        ),
    )
    .unwrap();
    fs::write(
        fixture.join("workspace/src/lib.rs"),
        "pub fn broken() -> i32 { missing }\n",
    )
    .unwrap();
    fs::write(
        fixture.join("workspace/src/lib.rs.post"),
        "pub fn repaired() -> i32 { 42 }\n",
    )
    .unwrap();
    fs::write(
        fixture.join("oracle-tests/semantic.rs"),
        format!(
            "#[test]\nfn intended_result_is_preserved() {{\n    assert_eq!({id}::repaired(), {});\n}}\n",
            if id == "test_fail_fixture" { 43 } else { 42 }
        ),
    )
    .unwrap();
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn write_stub_alloy(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env bash
set -u
argv="$*"
ws=""
command=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2 ;;
    events) command="events"; shift ;;
    *) shift ;;
  esac
done
[ -n "$ws" ] || { echo "missing workspace" >&2; exit 90; }
if [ "$command" = "events" ]; then
  printf '%s\n' "$argv" >>"${EVENTS_ARGV_LOG:-/dev/null}"
  if [ "${FAIL_EVENTS_EXPORT:-0}" = "1" ]; then
    echo "stub event export failed" >&2
    exit 94
  fi
  printf '%s\n' '{"type":"model_call","payload":{"input_tokens":100,"output_tokens":20}}'
  printf '%s\n' '{"type":"model_call","payload":{"output_tokens":5}}'
  printf '%s\n' '{"type":"run_completed","payload":{"dag_state":"succeeded"}}'
  exit 0
fi
[ ! -e "$ws/src/lib.rs.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
[ ! -e "$ws/tests" ] || { echo "hidden tests leaked into workspace" >&2; exit 93; }
package="$(awk -F'"' '/^name = / { print $2; exit }' "$ws/Cargo.toml")"
case "$package" in
  pass_fixture|test_fail_fixture)
    printf '%s\n' 'pub fn repaired() -> i32 { 42 }' >"$ws/src/lib.rs"
    ;;
  mismatch_fixture)
    printf '%s\n' 'pub fn repaired() -> i32 { 41 }' >"$ws/src/lib.rs"
    ;;
  compile_fail_fixture)
    printf '%s\n' 'pub fn still_broken() -> i32 { missing }' >"$ws/src/lib.rs"
    ;;
  *)
    echo "unknown fixture $package" >&2
    exit 92
    ;;
esac
"#,
    )
    .unwrap();
    make_executable(path);
}

/// Stub for `alloy-eval-live-naive`: proves the runner hands the driver a
/// clean workspace and pre-run diagnostics, then records one model call.
fn write_stub_naive(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env bash
set -u
ws=""
target=""
diagnostics=""
result=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --diagnostics) diagnostics="$2"; shift 2 ;;
    --result) result="$2"; shift 2 ;;
    --goal|--model|--temperature|--base-url) shift 2 ;;
    *) echo "unexpected argument $1" >&2; exit 90 ;;
  esac
done
[ -n "$ws" ] && [ -n "$target" ] && [ -n "$diagnostics" ] && [ -n "$result" ] ||
  { echo "missing required option" >&2; exit 90; }
[ ! -e "$ws/$target.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
[ ! -e "$ws/tests" ] || { echo "hidden tests leaked into workspace" >&2; exit 92; }
[ -s "$diagnostics" ] || { echo "no pre-run diagnostics at $diagnostics" >&2; exit 93; }
grep -q '\.post' "$diagnostics" && { echo "diagnostics reference the oracle" >&2; exit 94; }
[ -z "${NAIVE_SLEEP_SECONDS:-}" ] || sleep "$NAIVE_SLEEP_SECONDS"
printf '%s\n' 'pub fn repaired() -> i32 { 42 }' >"$ws/$target"
if [ "${OMIT_NAIVE_TELEMETRY:-0}" != "1" ]; then
  printf '%s\n' \
    '{"model_calls":1,"tokens_in":123,"tokens_out":45,"provider_request_id":"naive-1","finish_reason":"stop"}' \
    >"$result"
fi
"#,
    )
    .unwrap();
    make_executable(path);
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

#[test]
fn committed_post_references_pass_hidden_oracles() {
    let fixtures = repo_root().join("crates/alloy-eval/fixtures/holdout");
    let mut fixture_dirs = fs::read_dir(&fixtures)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    fixture_dirs.sort();

    for fixture in fixture_dirs {
        let manifest = fixture.join("manifest.toml");
        let target =
            PathBuf::from(live_holdout_target_path_text(&manifest).expect("fixture target path"));
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        copy_tree(&fixture.join("workspace"), &workspace);
        fs::copy(
            workspace.join(format!("{}.post", target.display())),
            workspace.join(&target),
        )
        .unwrap();
        fs::remove_file(workspace.join(format!("{}.post", target.display()))).unwrap();
        copy_tree(&fixture.join("oracle-tests"), &workspace.join("tests"));

        let output = Command::new("cargo")
            .args(["test", "--offline", "--quiet"])
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", directory.path().join("target"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} hidden oracle failed for its committed .post\n{}",
            fixture.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn runner_preserves_process_compile_reference_and_strict_results() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    for id in [
        "compile_fail_fixture",
        "mismatch_fixture",
        "pass_fixture",
        "test_fail_fixture",
    ] {
        write_fixture(&fixtures, id);
    }

    let alloy = directory.path().join("stub-alloy");
    write_stub_alloy(&alloy);
    let events_argv = directory.path().join("events-argv.log");
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("ALLOY", &alloy)
        .env("EVENTS_ARGV_LOG", &events_argv)
        .env("SCORER", SCORER)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("DRIVER", "alloy")
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("PROFILE", "default")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
        .env("ALLOY_API_KEY", "local-test-key")
        .env("REPS", "1")
        .env("TIMEOUT", "60")
        .env("TMPDIR", &tmp)
        .output()
        .expect("live-holdout runner");

    assert!(
        output.status.success(),
        "runner failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report_path = directory.path().join("observations.report.json");
    let report: LiveHoldoutReport =
        serde_json::from_str(&fs::read_to_string(report_path).unwrap()).unwrap();
    assert_eq!(report.schema_version, LIVE_HOLDOUT_REPORT_VERSION);
    assert_eq!(report.overall.process.passes, 4);
    assert_eq!(report.overall.compile_clean.passes, 3);
    assert_eq!(report.overall.tests_pass.passes, 1);
    assert_eq!(report.overall.reference_match.passes, 2);
    assert_eq!(report.overall.oracle.passes, 1);
    assert_eq!(report.overall.compile_clean_reference_mismatch.passes, 1);
    assert_eq!(report.overall.compile_clean_tests_failed.passes, 2);
    assert_eq!(report.overall.tests_pass_reference_mismatch.passes, 0);

    assert_eq!(report.endpoint.driver, LiveHoldoutDriver::Alloy);
    assert_eq!(report.endpoint.profile.as_deref(), Some("default"));
    assert!(
        is_lower_hex(&report.endpoint.harness.source_revision, 40),
        "{:?}",
        report.endpoint.harness.source_revision
    );
    assert!(
        is_lower_hex(&report.endpoint.harness.binary_bundle_sha256, 64),
        "{:?}",
        report.endpoint.harness.binary_bundle_sha256
    );
    // Two stub `model_call` events per attempt; the second reports output
    // tokens only, so present fields sum and absent ones stay unknown.
    assert_eq!(report.overall.model_calls_total, 8);
    assert_eq!(report.overall.tokens_in_total, 400);
    assert_eq!(report.overall.tokens_out_total, 100);

    // The export must request the runtime's whole page, not the 100-event
    // default, or long runs would be counted from a truncated export.
    let events_invocations = fs::read_to_string(&events_argv).unwrap();
    assert_eq!(
        events_invocations.lines().count(),
        4,
        "{events_invocations}"
    );
    for invocation in events_invocations.lines() {
        assert!(
            invocation.contains("--limit 1000"),
            "events export must request the full page: {invocation}"
        );
    }

    let by_id = report
        .observations
        .iter()
        .map(|row| (row.fixture_id.as_str(), row))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_id["pass_fixture"].failure_class, "pass");
    assert!(by_id["pass_fixture"].semantic_pass);
    // Compiled and claimed success, but the hidden tests disagreed. Under v5
    // that is a false green regardless of byte canonicality.
    assert_eq!(
        by_id["mismatch_fixture"].failure_class,
        "semantic_false_green"
    );
    assert!(!by_id["mismatch_fixture"].semantic_pass);
    assert_eq!(
        by_id["compile_fail_fixture"].failure_class,
        "process_claimed_success_but_compile_failed"
    );
    assert!(!by_id["compile_fail_fixture"].semantic_pass);
    assert_eq!(
        by_id["test_fail_fixture"].failure_class,
        "semantic_false_green"
    );
    assert!(!by_id["test_fail_fixture"].oracle_pass);
    assert!(!by_id["test_fail_fixture"].semantic_pass);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("compile_clean_reference_mismatch=1/4"),
        "{stdout}"
    );
    assert!(
        stdout.contains("compile_clean_tests_failed=2/4"),
        "{stdout}"
    );

    let evidence = directory.path().join("observations.artifacts");
    for id in [
        "compile_fail_fixture",
        "mismatch_fixture",
        "pass_fixture",
        "test_fail_fixture",
    ] {
        let attempt = evidence.join(id).join("rep-1");
        for name in [
            "initial-cargo.log",
            "run.log",
            "final-target.rs",
            "patch.diff",
            "cargo-check.log",
            "cargo-test.log",
            "events.jsonl",
            "metadata.json",
        ] {
            assert!(attempt.join(name).is_file(), "missing {id}/{name}");
        }
        assert!(
            !attempt.join("naive-result.json").exists(),
            "{id} must not carry naive evidence"
        );
    }
}

#[test]
fn naive_driver_shares_the_strict_oracle_pipeline() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    write_fixture(&fixtures, "pass_fixture");

    let naive = directory.path().join("stub-naive");
    write_stub_naive(&naive);
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("DRIVER", "naive")
        .env_remove("PROFILE")
        .env("NAIVE", &naive)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
        .env("ALLOY_API_KEY", "local-test-key")
        .env("SOURCE_REVISION", NAIVE_SOURCE_REVISION)
        .env("BUNDLE_SHA256", NAIVE_BUNDLE_SHA256)
        .env("REPS", "1")
        .env("TIMEOUT", "60")
        .env("TMPDIR", &tmp)
        .output()
        .expect("live-holdout runner");

    assert!(
        output.status.success(),
        "naive runner failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: LiveHoldoutReport = serde_json::from_str(
        &fs::read_to_string(directory.path().join("observations.report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report.schema_version, LIVE_HOLDOUT_REPORT_VERSION);
    assert_eq!(report.endpoint.driver, LiveHoldoutDriver::Naive);
    assert_eq!(report.endpoint.profile, None);
    assert_eq!(
        report.endpoint.harness.source_revision,
        NAIVE_SOURCE_REVISION
    );
    assert_eq!(
        report.endpoint.harness.binary_bundle_sha256,
        NAIVE_BUNDLE_SHA256
    );
    assert_eq!(report.overall.oracle.passes, 1);
    assert_eq!(report.overall.tests_pass.passes, 1);
    assert_eq!(report.overall.model_calls_total, 1);
    assert_eq!(report.overall.tokens_in_total, 123);
    assert_eq!(report.overall.tokens_out_total, 45);

    let row = &report.observations[0];
    assert_eq!(row.driver, LiveHoldoutDriver::Naive);
    assert_eq!(row.profile, None);
    assert_eq!(row.model_calls, 1);
    assert_eq!(row.evidence_relpath, "pass_fixture/rep-1");
    assert_eq!(row.failure_class, "pass");

    let attempt = directory
        .path()
        .join("observations.artifacts")
        .join(&row.evidence_relpath);
    for name in [
        "initial-cargo.log",
        "run.log",
        "final-target.rs",
        "patch.diff",
        "cargo-check.log",
        "cargo-test.log",
        "events.jsonl",
        "naive-result.json",
        "metadata.json",
    ] {
        assert!(
            attempt.join(name).is_file(),
            "missing naive evidence {name}"
        );
    }
    // The naive arm has no event stream; the file exists so both arms keep
    // one evidence layout.
    assert_eq!(
        fs::read_to_string(attempt.join("events.jsonl")).unwrap(),
        ""
    );
}

#[test]
fn runner_aborts_when_alloy_event_export_fails() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    write_fixture(&fixtures, "pass_fixture");

    let alloy = directory.path().join("stub-alloy");
    write_stub_alloy(&alloy);
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("ALLOY", &alloy)
        .env("SCORER", SCORER)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("DRIVER", "alloy")
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("PROFILE", "default")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
        .env("ALLOY_API_KEY", "local-test-key")
        .env("FAIL_EVENTS_EXPORT", "1")
        .env("REPS", "1")
        .env("TIMEOUT", "60")
        .env("TMPDIR", &tmp)
        .output()
        .expect("live-holdout runner");

    assert_eq!(
        output.status.code(),
        Some(2),
        "runner must fail closed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("event export failed"), "{stderr}");
    let attempt = directory
        .path()
        .join("observations.artifacts/pass_fixture/rep-1");
    assert!(fs::read_to_string(attempt.join("events.stderr"))
        .unwrap()
        .contains("stub event export failed"));
    assert!(
        !directory.path().join("observations.report.json").exists(),
        "failed telemetry must not produce a report"
    );
}

#[test]
fn fallback_bundle_identity_includes_the_alloy_scorer() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    write_fixture(&fixtures, "pass_fixture");

    let alloy = directory.path().join("stub-alloy");
    write_stub_alloy(&alloy);
    let scorer_a = directory.path().join("scorer-a");
    let scorer_b = directory.path().join("scorer-b");
    fs::copy(SCORER, &scorer_a).unwrap();
    fs::copy(SCORER, &scorer_b).unwrap();
    let mut changed = fs::read(&scorer_b).unwrap();
    changed.extend_from_slice(b"\n");
    fs::write(&scorer_b, changed).unwrap();
    make_executable(&scorer_a);
    make_executable(&scorer_b);

    let run = |name: &str, scorer: &Path| {
        let observations = directory.path().join(format!("{name}.jsonl"));
        let output = Command::new("bash")
            .arg(repo_root().join("eval/live-holdout/run.sh"))
            .arg(&observations)
            .env("FIXTURES", &fixtures)
            .env("ALLOY", &alloy)
            .env("SCORER", scorer)
            .env("EVAL_HOLDOUT", EVALUATOR)
            .env("DRIVER", "alloy")
            .env("MODEL", "stub-model")
            .env("TEMP", "0.6")
            .env("PROFILE", "default")
            .env("BASEURL", "http://127.0.0.1:8089/v1/")
            .env("ALLOY_API_KEY", "local-test-key")
            .env("REPS", "1")
            .env("TIMEOUT", "60")
            .env("TMPDIR", &tmp)
            .output()
            .expect("live-holdout runner");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str::<LiveHoldoutReport>(
            &fs::read_to_string(directory.path().join(format!("{name}.report.json"))).unwrap(),
        )
        .unwrap()
    };

    let first = run("first", &scorer_a);
    let second = run("second", &scorer_b);

    assert_ne!(
        first.endpoint.harness.binary_bundle_sha256,
        second.endpoint.harness.binary_bundle_sha256
    );
}

#[test]
fn runner_aborts_when_naive_telemetry_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    write_fixture(&fixtures, "pass_fixture");

    let naive = directory.path().join("stub-naive");
    write_stub_naive(&naive);
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("DRIVER", "naive")
        .env_remove("PROFILE")
        .env("NAIVE", &naive)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
        .env("ALLOY_API_KEY", "local-test-key")
        .env("SOURCE_REVISION", NAIVE_SOURCE_REVISION)
        .env("BUNDLE_SHA256", NAIVE_BUNDLE_SHA256)
        .env("OMIT_NAIVE_TELEMETRY", "1")
        .env("REPS", "1")
        .env("TIMEOUT", "60")
        .env("TMPDIR", &tmp)
        .output()
        .expect("live-holdout runner");

    assert_eq!(
        output.status.code(),
        Some(2),
        "runner must fail closed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("naive-result.json"), "{stderr}");
    assert!(
        !directory.path().join("observations.report.json").exists(),
        "missing telemetry must not produce a report"
    );
}

#[test]
fn naive_driver_is_bounded_by_the_attempt_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    write_fixture(&fixtures, "pass_fixture");

    let naive = directory.path().join("stub-naive");
    write_stub_naive(&naive);
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("DRIVER", "naive")
        .env_remove("PROFILE")
        .env("NAIVE", &naive)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
        .env("ALLOY_API_KEY", "local-test-key")
        .env("SOURCE_REVISION", NAIVE_SOURCE_REVISION)
        .env("BUNDLE_SHA256", NAIVE_BUNDLE_SHA256)
        .env("NAIVE_SLEEP_SECONDS", "3")
        .env("REPS", "1")
        .env("TIMEOUT", "1")
        .env("TMPDIR", &tmp)
        .output()
        .expect("live-holdout runner");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("telemetry is incomplete"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
