//! Offline integration coverage for the live-holdout shell contract.
//!
//! A stub replaces `alloy`; no model endpoint is contacted. The real runner,
//! independent cargo post-check, Rust oracle, and scorer still execute.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use alloy_eval::{LiveHoldoutReport, LIVE_HOLDOUT_REPORT_VERSION};

const EVALUATOR: &str = env!("CARGO_BIN_EXE_alloy-eval-live-holdout");
const SCORER: &str = env!("CARGO_BIN_EXE_alloy-eval-live-repair");

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn write_fixture(root: &Path, id: &str) {
    let fixture = root.join(id);
    fs::create_dir_all(fixture.join("workspace/src")).unwrap();
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
}

fn write_stub_alloy(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env bash
set -u
ws=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$ws" ] || { echo "missing workspace" >&2; exit 90; }
[ ! -e "$ws/src/lib.rs.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
package="$(awk -F'"' '/^name = / { print $2; exit }' "$ws/Cargo.toml")"
case "$package" in
  pass_fixture)
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
fn runner_preserves_process_compile_reference_and_strict_results() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let tmp = directory.path().join("tmp");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    for id in ["compile_fail_fixture", "mismatch_fixture", "pass_fixture"] {
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
    assert_eq!(report.overall.process.passes, 3);
    assert_eq!(report.overall.compile_clean.passes, 2);
    assert_eq!(report.overall.reference_match.passes, 1);
    assert_eq!(report.overall.oracle.passes, 1);
    assert_eq!(report.overall.compile_clean_reference_mismatch.passes, 1);

    let by_id = report
        .observations
        .iter()
        .map(|row| (row.fixture_id.as_str(), row))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_id["pass_fixture"].failure_class, "pass");
    assert_eq!(
        by_id["mismatch_fixture"].failure_class,
        "reference_mismatch"
    );
    assert_eq!(
        by_id["compile_fail_fixture"].failure_class,
        "process_claimed_success_but_compile_failed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("compile_clean_reference_mismatch=1/3"),
        "{stdout}"
    );
}
