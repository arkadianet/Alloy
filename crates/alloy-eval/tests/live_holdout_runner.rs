//! Offline integration coverage for the live-holdout shell contract.
//!
//! A stub replaces `alloy`; no model endpoint is contacted. The real runner,
//! independent cargo post-check, Rust oracle, and scorer still execute.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use alloy_eval::{live_holdout_target_path_text, LiveHoldoutReport, LIVE_HOLDOUT_REPORT_VERSION};

const EVALUATOR: &str = env!("CARGO_BIN_EXE_alloy-eval-live-holdout");
const SCORER: &str = env!("CARGO_BIN_EXE_alloy-eval-live-repair");

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

fn write_stub_alloy(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env bash
set -u
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
  printf '%s\n' '{"type":"run_completed","payload":{"dag_state":"succeeded"}}'
  exit 0
fi
[ ! -e "$ws/src/lib.rs.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
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

    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
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
    let observations = directory.path().join("observations.jsonl");
    let output = Command::new("bash")
        .arg(repo_root().join("eval/live-holdout/run.sh"))
        .arg(&observations)
        .env("FIXTURES", &fixtures)
        .env("ALLOY", &alloy)
        .env("SCORER", SCORER)
        .env("EVAL_HOLDOUT", EVALUATOR)
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("PROFILE", "default")
        .env("BASEURL", "http://127.0.0.1:8089/v1/")
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

    let by_id = report
        .observations
        .iter()
        .map(|row| (row.fixture_id.as_str(), row))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_id["pass_fixture"].failure_class, "pass");
    assert_eq!(
        by_id["mismatch_fixture"].failure_class,
        "reference_mismatch_tests_failed"
    );
    assert_eq!(
        by_id["compile_fail_fixture"].failure_class,
        "process_claimed_success_but_compile_failed"
    );
    assert_eq!(
        by_id["test_fail_fixture"].failure_class,
        "strict_pass_tests_failed"
    );
    assert!(!by_id["test_fail_fixture"].oracle_pass);

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
    }
}
