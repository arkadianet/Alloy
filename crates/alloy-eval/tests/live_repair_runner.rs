//! End-to-end plumbing for the live-repair operator benchmark.
//!
//! The real `alloy` binary is replaced by a stub shell script: these tests
//! never contact a model endpoint, never run `cargo check`, and never touch
//! the offline RFC-0016 fixture corpus.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use alloy_eval::{LiveRepairReport, LIVE_REPAIR_MANIFEST_FILE, LIVE_REPAIR_REPORT_VERSION};

const SCORER: &str = env!("CARGO_BIN_EXE_alloy-eval-live-repair");

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_root()
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn run_script() -> PathBuf {
    workspace_root().join("eval/live-repair/run.sh")
}

fn manifest_src(id: &str) -> String {
    format!(
        r#"live_manifest_version = 1
id = "{id}"
goal = "fix the compile error in src/main.rs"
expected_outcome = "compile_clean"
tags = ["e0000", "stub"]

[license]
class = "permitted"
spdx = "Alloy-Original"
source_note = "Alloy-original live-repair test fixture by arkadianet."

[workspace]
path = "workspace"
package = "{id}"
"#
    )
}

fn write_fixture(root: &Path, id: &str) {
    let dir = root.join(id);
    fs::create_dir_all(dir.join("workspace/src")).unwrap();
    fs::write(dir.join(LIVE_REPAIR_MANIFEST_FILE), manifest_src(id)).unwrap();
    fs::write(dir.join("LICENSE"), "license text\n").unwrap();
    fs::write(
        dir.join("workspace/Cargo.toml"),
        format!(
            "[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
        ),
    )
    .unwrap();
    fs::write(dir.join("workspace/src/main.rs"), "fn main() {}\n").unwrap();
}

/// A stand-in for the real `alloy` binary: it passes `pass_fixture`, fails
/// `fail_fixture`, and emits the retry log line once for `pass_fixture`.
fn write_stub_alloy(path: &Path, retry_line: &str) {
    fs::write(
        path,
        format!(
            r#"#!/usr/bin/env bash
set -u
ws=""
goal=""
saw_yes=0
while [ $# -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2;;
    --yes) saw_yes=1; shift;;
    run) goal="$2"; shift 2;;
    *) shift;;
  esac
done
echo "stub alloy workspace=$ws goal=$goal yes=$saw_yes"
[ -f "$ws/router.toml" ] || {{ echo "missing router.toml" >&2; exit 90; }}
[ -d "$ws/profiles" ] || {{ echo "missing profiles" >&2; exit 91; }}
[ -d "$ws/.git" ] || {{ echo "missing git workspace" >&2; exit 92; }}
[ "$saw_yes" = 1 ] || {{ echo "missing --yes" >&2; exit 93; }}
[ "$goal" = "fix the compile error in src/main.rs" ] || {{ echo "bad goal" >&2; exit 94; }}
pkg="$(grep -m1 '^name = ' "$ws/Cargo.toml" | cut -d'"' -f2)"
case "$pkg" in
  pass_fixture) echo "{retry_line}" >&2; exit 0;;
  fail_fixture) exit 1;;
  timeout_fixture) exit 124;;
esac
exit 95
"#
        ),
    )
    .unwrap();
    make_executable(path);
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

struct Bench {
    _dir: tempfile::TempDir,
    fixtures: PathBuf,
    alloy: PathBuf,
    out: PathBuf,
}

fn bench(ids: &[&str]) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = dir.path().join("live-fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    for id in ids {
        write_fixture(&fixtures, id);
    }
    let alloy = dir.path().join("stub-alloy");
    write_stub_alloy(&alloy, "retrying with fresh diagnostics");
    let out = dir.path().join("results.jsonl");
    Bench {
        _dir: dir,
        fixtures,
        alloy,
        out,
    }
}

fn run_bench(bench: &Bench, reps: &str) -> std::process::Output {
    Command::new("bash")
        .arg(run_script())
        .arg(&bench.out)
        .env("FIXTURES", &bench.fixtures)
        .env("ALLOY", &bench.alloy)
        .env("SCORER", SCORER)
        .env("REPS", reps)
        .env("MODEL", "stub-model")
        .env("TEMP", "0.6")
        .env("BASEURL", "http://127.0.0.1:11434/v1/")
        .env("TIMEOUT", "60")
        .output()
        .expect("run.sh")
}

#[test]
fn runner_executes_every_fixture_and_records_structured_observations() {
    let bench = bench(&["pass_fixture", "fail_fixture"]);
    let output = run_bench(&bench, "2");
    assert!(
        output.status.success(),
        "run.sh failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&bench.out).unwrap();
    let observations = alloy_eval::parse_observations_jsonl(&jsonl).unwrap();
    assert_eq!(observations.len(), 4, "2 fixtures x 2 repetitions");

    let passes: Vec<_> = observations
        .iter()
        .filter(|o| o.fixture_id.as_str() == "pass_fixture")
        .collect();
    assert_eq!(passes.len(), 2);
    for observation in &passes {
        assert_eq!(observation.exit_code, 0);
        assert_eq!(observation.retries, 1, "retry line must be counted");
    }
    let mut reps: Vec<u32> = passes.iter().map(|o| o.repetition).collect();
    reps.sort_unstable();
    assert_eq!(reps, vec![1, 2]);

    let failures: Vec<_> = observations
        .iter()
        .filter(|o| o.fixture_id.as_str() == "fail_fixture")
        .collect();
    assert_eq!(failures.len(), 2);
    for observation in &failures {
        assert_eq!(observation.exit_code, 1);
        assert_eq!(observation.retries, 0);
    }
}

#[test]
fn runner_writes_a_structured_report_using_the_eval_report_vocabulary() {
    let bench = bench(&["pass_fixture", "fail_fixture", "timeout_fixture"]);
    let output = run_bench(&bench, "1");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("alloy-eval-live-repair run_id="),
        "{stdout}"
    );
    assert!(stdout.contains("offline=false"), "{stdout}");
    assert!(stdout.contains("holdout_gate=not_applicable"), "{stdout}");
    assert!(stdout.contains("overall pass=1 fail=1 error=1"), "{stdout}");
    assert!(stdout.contains("pass_rate=0.500000"), "{stdout}");
    assert!(
        stdout.contains("retries_total=1 passes_via_retry=1"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("alloy-eval run_id="),
        "must not imitate the offline CI summary: {stdout}"
    );

    let report_path = bench.out.with_extension("").with_extension("");
    let report_path = report_path.parent().unwrap().join("results.report.json");
    let report: LiveRepairReport =
        serde_json::from_str(&fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report.schema_version, LIVE_REPAIR_REPORT_VERSION);
    assert!(!report.offline);
    assert_eq!(report.fixtures.len(), 3);
    assert_eq!(report.overall.attempts, 2);
    assert_eq!(report.overall.errors, 1);
    assert_eq!(report.observations.len(), 3);
    assert!(report
        .fixtures
        .iter()
        .all(|fixture| fixture.tags == vec!["e0000".to_owned(), "stub".to_owned()]));
}

#[test]
fn runner_leaves_no_state_in_the_repository() {
    let bench = bench(&["pass_fixture"]);
    let before = fs::read_to_string(workspace_root().join("router.toml.example")).ok();
    let output = run_bench(&bench, "1");
    assert!(output.status.success());
    assert!(!workspace_root().join("router.toml").exists());
    assert_eq!(
        fs::read_to_string(workspace_root().join("router.toml.example")).ok(),
        before
    );
}

#[test]
fn scorer_rejects_observations_for_an_unknown_fixture() {
    let bench = bench(&["pass_fixture"]);
    fs::write(
        &bench.out,
        "{\"fixture_id\":\"ghost\",\"repetition\":1,\"exit_code\":0,\"retries\":0,\"wall_ms\":1}\n",
    )
    .unwrap();
    let output = Command::new(SCORER)
        .args(["score", "--fixtures"])
        .arg(&bench.fixtures)
        .arg("--observations")
        .arg(&bench.out)
        .args([
            "--model",
            "stub-model",
            "--temperature",
            "0.6",
            "--base-url",
            "http://127.0.0.1:11434/v1/",
        ])
        .output()
        .expect("scorer");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown live-repair fixture"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn scorer_refuses_the_offline_holdout_corpus() {
    let holdout = crate_root().join("fixtures/holdout");
    let output = Command::new(SCORER)
        .args(["plan", "--fixtures"])
        .arg(&holdout)
        .output()
        .expect("scorer");
    assert!(
        !output.status.success(),
        "the live benchmark must never be pointed at the holdout corpus"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("train/holdout"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plan_output_is_tab_delimited_and_shell_safe() {
    let bench = bench(&["pass_fixture", "fail_fixture"]);
    let output = Command::new(SCORER)
        .args(["plan", "--fixtures"])
        .arg(&bench.fixtures)
        .output()
        .expect("scorer");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    // Deterministic fixture-id ordering.
    assert!(lines[0].starts_with("fail_fixture\t"));
    assert!(lines[1].starts_with("pass_fixture\t"));
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "id, workspace, goal");
        assert!(Path::new(fields[1]).join("Cargo.toml").is_file());
        assert_eq!(fields[2], "fix the compile error in src/main.rs");
    }
}

#[test]
fn rendered_router_matches_the_library_renderer() {
    let output = Command::new(SCORER)
        .args([
            "render-router",
            "--model",
            "stub-model",
            "--temperature",
            "0.6",
            "--base-url",
            "http://127.0.0.1:11434/v1/",
        ])
        .output()
        .expect("scorer");
    assert!(output.status.success());
    let rendered = String::from_utf8(output.stdout).unwrap();
    let expected = alloy_eval::render_router_toml(&alloy_eval::LiveRepairEndpoint {
        model: "stub-model".to_owned(),
        temperature: 0.6,
        base_url: "http://127.0.0.1:11434/v1/".to_owned(),
    })
    .unwrap();
    assert_eq!(rendered, expected);
}
