//! Offline contract tests for the live-holdout binary bundle, the generic
//! matrix comparator, and the E1 three-arm wrapper.
//!
//! No model endpoint is contacted. The bundle carries stub `alloy` and naive
//! drivers beside the real evaluator and scorer binaries, so the whole shell
//! contract — bundle verification, arm parsing, and comparison — runs for real.
//!
//! Each test builds its own committed checkout of the harness (scripts,
//! profiles, one fixture) in a temporary directory. The suite therefore owns
//! the repository state the drift checks read, instead of inheriting whatever
//! the developer happens to have uncommitted.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use alloy_eval::{LiveHoldoutMatrixComparison, LiveHoldoutReport};

const EVALUATOR: &str = env!("CARGO_BIN_EXE_alloy-eval-live-holdout");
const SCORER: &str = env!("CARGO_BIN_EXE_alloy-eval-live-repair");
const NAIVE: &str = env!("CARGO_BIN_EXE_alloy-eval-live-naive");
const MODEL: &str = "stub-model";
const BASE_URL: &str = "http://127.0.0.1:8089/v1/";
/// The corpus path `matrix.sh` treats as a treatment input.
const FIXTURES_RELPATH: &str = "crates/alloy-eval/fixtures/holdout";
/// Manifest order: `LC_ALL=C` sorted, exactly what `prepare.sh` hashes.
const BUNDLE_BINARIES: [&str; 4] = [
    "alloy",
    "alloy-eval-live-holdout",
    "alloy-eval-live-naive",
    "alloy-eval-live-repair",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn script(name: &str) -> PathBuf {
    repo_root().join("eval/live-holdout").join(name)
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
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

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        repo.display()
    );
}

fn git_commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=live-holdout",
            "-c",
            "user.email=live-holdout@localhost",
            "commit",
            "-qm",
            message,
        ],
    );
}

fn git_status(repo: &Path) -> String {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn head_revision(repo: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum {}", path.display());
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("digest")
        .to_owned()
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
        format!("#[test]\nfn intended_result_is_preserved() {{\n    assert_eq!({id}::repaired(), 42);\n}}\n"),
    )
    .unwrap();
}

/// Stub `alloy`: applies the reference repair and exports two model calls.
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
  printf '%s\n' '{"type":"model_call","payload":{"input_tokens":100,"output_tokens":20}}'
  printf '%s\n' '{"type":"run_completed","payload":{"dag_state":"succeeded"}}'
  exit 0
fi
[ ! -e "$ws/src/lib.rs.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
[ ! -e "$ws/tests" ] || { echo "hidden tests leaked into workspace" >&2; exit 93; }
printf '%s\n' 'pub fn repaired() -> i32 { 42 }' >"$ws/src/lib.rs"
"#,
    )
    .unwrap();
    make_executable(path);
}

/// Stub `alloy-eval-live-naive`: one recorded model call, one replacement.
fn write_stub_naive(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env bash
set -u
ws=""
target=""
result=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workspace) ws="$2"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --result) result="$2"; shift 2 ;;
    --goal|--model|--temperature|--base-url|--diagnostics) shift 2 ;;
    *) echo "unexpected argument $1" >&2; exit 90 ;;
  esac
done
[ -n "$ws" ] && [ -n "$target" ] && [ -n "$result" ] ||
  { echo "missing required option" >&2; exit 90; }
[ ! -e "$ws/$target.post" ] || { echo "oracle leaked into workspace" >&2; exit 91; }
printf '%s\n' 'pub fn repaired() -> i32 { 42 }' >"$ws/$target"
printf '%s\n' \
  '{"model_calls":1,"tokens_in":123,"tokens_out":45,"provider_request_id":"naive-1","finish_reason":"stop"}' \
  >"$result"
"#,
    )
    .unwrap();
    make_executable(path);
}

fn manifest_text(bundle: &Path, revision: &str, worktree: &str) -> String {
    let debug = bundle.join("target/debug");
    let mut text = format!("source_revision\t{revision}\nworktree\t{worktree}\n");
    for name in BUNDLE_BINARIES {
        text.push_str(&format!("binary\t{name}\t{}\n", sha256(&debug.join(name))));
    }
    text
}

/// Builds the bundle layout `prepare.sh` produces, with stub drivers.
fn write_bundle(root: &Path, revision: &str) -> PathBuf {
    let bundle = root.join("bundle");
    let debug = bundle.join("target/debug");
    fs::create_dir_all(&debug).unwrap();
    write_stub_alloy(&debug.join("alloy"));
    write_stub_naive(&debug.join("alloy-eval-live-naive"));
    for (source, name) in [
        (EVALUATOR, "alloy-eval-live-holdout"),
        (SCORER, "alloy-eval-live-repair"),
    ] {
        fs::copy(source, debug.join(name)).unwrap();
        make_executable(&debug.join(name));
    }
    fs::write(
        bundle.join("manifest.tsv"),
        manifest_text(&bundle, revision, "clean"),
    )
    .unwrap();
    bundle
}

fn bundle_sha256(bundle: &Path) -> String {
    sha256(&bundle.join("manifest.tsv"))
}

fn arms_file(root: &Path, name: &str, rows: &[&str]) -> PathBuf {
    let path = root.join(name);
    let mut text = "# arm_id\tdriver\tmodel\ttemperature\tprofile\tbase_url\treps\n".to_owned();
    for row in rows {
        text.push_str(row);
        text.push('\n');
    }
    fs::write(&path, text).unwrap();
    path
}

fn arm_row(
    arm_id: &str,
    driver: &str,
    model: &str,
    temp: &str,
    profile: &str,
    reps: &str,
) -> String {
    format!("{arm_id}\t{driver}\t{model}\t{temp}\t{profile}\t{BASE_URL}\t{reps}")
}

/// A committed checkout of everything `matrix.sh` treats as the harness: the
/// orchestration scripts, the profiles, and the holdout corpus. Unrelated
/// documents are left deliberately uncommitted — a real operator's roadmap
/// edits must never block a run.
fn write_checkout(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    let scripts = repo.join("eval/live-holdout");
    fs::create_dir_all(&scripts).unwrap();
    for name in ["run.sh", "matrix.sh", "e1.sh"] {
        fs::copy(script(name), scripts.join(name)).unwrap();
        make_executable(&scripts.join(name));
    }
    copy_tree(&repo_root().join("profiles"), &repo.join("profiles"));
    write_fixture(&repo.join(FIXTURES_RELPATH), "pass_fixture");
    fs::create_dir_all(repo.join("docs/roadmap")).unwrap();
    fs::write(repo.join("docs/roadmap/README.md"), "roadmap\n").unwrap();
    git(&repo, &["init", "-q"]);
    git_commit(&repo, "harness");
    fs::write(repo.join("docs/roadmap/README.md"), "roadmap, edited\n").unwrap();
    fs::write(repo.join("docs/roadmap/NOTES.md"), "untracked\n").unwrap();
    repo
}

struct Harness {
    directory: tempfile::TempDir,
    repo: PathBuf,
    revision: String,
    bundle: PathBuf,
    fixtures: PathBuf,
    tmp: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let repo = write_checkout(&root);
        let revision = head_revision(&repo);
        // The bundle lives outside the checkout, as prepare.sh requires.
        let bundle = write_bundle(&root, &revision);
        let fixtures = repo.join(FIXTURES_RELPATH);
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        Self {
            directory,
            repo,
            revision,
            bundle,
            fixtures,
            tmp,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn run(&self, name: &str, arms: &Path, out: &Path) -> Output {
        self.run_with_api_key(name, arms, out, Some("local-test-key"))
    }

    fn run_with_api_key(
        &self,
        name: &str,
        arms: &Path,
        out: &Path,
        api_key: Option<&str>,
    ) -> Output {
        let mut command = Command::new("bash");
        command
            .arg(self.repo.join("eval/live-holdout").join(name))
            .arg(arms)
            .arg(out)
            .arg(&self.bundle)
            .env("FIXTURES", &self.fixtures)
            .env("TMPDIR", &self.tmp)
            .env("TIMEOUT", "120");
        match api_key {
            Some(value) => {
                command.env("ALLOY_API_KEY", value);
            }
            None => {
                command.env_remove("ALLOY_API_KEY");
            }
        }
        command
            .output()
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    }
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read_report(path: &Path) -> LiveHoldoutReport {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn matrix_refuses_a_bundle_without_a_manifest() {
    let harness = Harness::new();
    fs::remove_file(harness.bundle.join("manifest.tsv")).unwrap();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("manifest"),
        "{}",
        describe(&output)
    );
    assert!(!out.exists(), "no arm may run without a bundle manifest");
}

#[test]
fn matrix_requires_a_nonempty_api_key_before_creating_output() {
    for (label, api_key) in [("missing", None), ("empty", Some(""))] {
        let harness = Harness::new();
        let arms = arms_file(
            harness.root(),
            "arms.tsv",
            &[
                &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
                &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
            ],
        );
        let out = harness.root().join(format!("out-{label}"));

        let output = harness.run_with_api_key("matrix.sh", &arms, &out, api_key);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{label} key must abort before model work: {}",
            describe(&output)
        );
        assert!(
            stderr_of(&output).contains("ALLOY_API_KEY"),
            "{}",
            describe(&output)
        );
        assert!(!out.exists(), "{label} key must precede output creation");
    }
}

#[test]
fn matrix_refuses_a_bundle_built_from_a_dirty_worktree() {
    let harness = Harness::new();
    fs::write(
        harness.bundle.join("manifest.tsv"),
        manifest_text(&harness.bundle, &harness.revision, "dirty"),
    )
    .unwrap();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("clean"),
        "{}",
        describe(&output)
    );
    assert!(!out.exists());
}

#[test]
fn matrix_refuses_a_binary_that_no_longer_matches_the_manifest() {
    let harness = Harness::new();
    let replaced = harness.bundle.join("target/debug/alloy");
    let mut tampered = fs::read(&replaced).unwrap();
    tampered.extend_from_slice(b"\n# rebuilt from another commit\n");
    fs::write(&replaced, tampered).unwrap();
    make_executable(&replaced);
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("alloy"), "{}", describe(&output));
    assert!(
        stderr.contains("hash") || stderr.contains("sha256"),
        "{}",
        describe(&output)
    );
    assert!(!out.exists());
}

#[test]
fn matrix_refuses_duplicate_arm_ids_before_running_anything() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("a", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("duplicate arm id"),
        "{}",
        describe(&output)
    );
    assert!(!out.exists(), "arm validation must precede any model work");
}

#[test]
fn matrix_refuses_the_legacy_six_column_arms_file() {
    let harness = Harness::new();
    let path = harness.root().join("legacy.tsv");
    fs::write(
        &path,
        format!(
            "# arm_id\tmodel\ttemperature\tprofile\tbase_url\treps\n\
             a\t{MODEL}\t0.6\tdefault\t{BASE_URL}\t1\n\
             b\t{MODEL}\t0.6\tautonomous\t{BASE_URL}\t1\n"
        ),
    )
    .unwrap();
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &path, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("7"),
        "the seven-column contract must be explicit: {}",
        describe(&output)
    );
    assert!(!out.exists());
}

#[test]
fn matrix_refuses_a_non_empty_output_directory() {
    let harness = Harness::new();
    let out = harness.root().join("out");
    fs::create_dir_all(&out).unwrap();
    let stale = out.join("matrix.report.json");
    fs::write(&stale, "{\"stale\":true}\n").unwrap();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("empty"),
        "{}",
        describe(&output)
    );
    assert_eq!(
        fs::read_to_string(&stale).unwrap(),
        "{\"stale\":true}\n",
        "stale evidence must be left untouched for the operator"
    );
    assert_eq!(
        fs::read_dir(&out).unwrap().count(),
        1,
        "no arm may have run"
    );
}

#[test]
fn matrix_compares_two_generic_model_arms_from_one_bundle() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("baseline", "alloy", "stub-model-a", "0.6", "default", "1"),
            &arm_row("hotter", "alloy", "stub-model-b", "0.9", "default", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert!(output.status.success(), "{}", describe(&output));
    let matrix: LiveHoldoutMatrixComparison =
        serde_json::from_str(&fs::read_to_string(out.join("matrix.report.json")).unwrap()).unwrap();
    assert_eq!(matrix.baseline, "baseline");
    assert_eq!(matrix.arms.len(), 2);
    assert_eq!(matrix.comparisons.len(), 1);

    let expected = bundle_sha256(&harness.bundle);
    for arm in ["baseline", "hotter"] {
        let report = read_report(&out.join(format!("{arm}.report.json")));
        assert_eq!(report.endpoint.harness.source_revision, harness.revision);
        assert_eq!(report.endpoint.harness.binary_bundle_sha256, expected);
        assert_eq!(report.overall.oracle.passes, 1, "{arm} strict oracle");
        assert_eq!(report.overall.oracle.attempts, 1, "{arm} attempts");
    }
    assert_eq!(matrix.arms["baseline"].endpoint.model, "stub-model-a");
    assert_eq!(matrix.arms["hotter"].endpoint.model, "stub-model-b");
}

#[test]
fn matrix_refuses_a_checkout_that_is_not_the_bundle_commit() {
    let harness = Harness::new();
    // The harness moved on after the bundle was built.
    fs::write(
        harness.repo.join("eval/live-holdout/notes.md"),
        "changed after the bundle\n",
    )
    .unwrap();
    git_commit(&harness.repo, "later commit");
    let moved = head_revision(&harness.repo);
    assert_ne!(moved, harness.revision);
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("revision"), "{}", describe(&output));
    assert!(stderr.contains(&harness.revision), "{}", describe(&output));
    assert!(!out.exists(), "revision drift must precede any model work");
}

#[test]
fn matrix_refuses_uncommitted_changes_to_the_harness() {
    for path in [
        "eval/live-holdout/run.sh",
        "profiles/extra.toml",
        "crates/alloy-eval/fixtures/holdout/pass_fixture/workspace/src/lib.rs.post",
    ] {
        let harness = Harness::new();
        fs::write(harness.repo.join(path), "# changed after the bundle\n").unwrap();
        // A staged change is drift too, not a clean tree.
        if path.starts_with("profiles/") {
            git(&harness.repo, &["add", path]);
        }
        let arms = arms_file(
            harness.root(),
            "arms.tsv",
            &[
                &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
                &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
            ],
        );
        let out = harness.root().join("out");

        let output = harness.run("matrix.sh", &arms, &out);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{path} drift must abort: {}",
            describe(&output)
        );
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("uncommitted"),
            "{path}: {}",
            describe(&output)
        );
        assert!(stderr.contains(path), "{path}: {}", describe(&output));
        assert!(!out.exists(), "{path}: drift must precede any model work");
    }
}

#[test]
fn matrix_ignores_uncommitted_changes_outside_the_harness() {
    let harness = Harness::new();
    // The checkout already carries a modified and an untracked roadmap file;
    // add a staged one so all three states are covered.
    fs::write(harness.repo.join("docs/roadmap/STAGED.md"), "staged\n").unwrap();
    git(&harness.repo, &["add", "docs/roadmap/STAGED.md"]);
    assert_eq!(git_status(&harness.repo).lines().count(), 3);
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("a", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("b", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("matrix.sh", &arms, &out);

    assert!(
        output.status.success(),
        "documents are not the harness: {}",
        describe(&output)
    );
    assert!(out.join("matrix.report.json").is_file());
}

#[test]
fn e1_requires_all_three_treatment_roles() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            &arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("e1.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("autonomous"),
        "{}",
        describe(&output)
    );
    assert!(
        !out.exists(),
        "E1 preflight must precede the output directory"
    );
}

#[test]
fn e1_refuses_a_repeated_treatment_role() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            &arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("alloy-default-2", "alloy", MODEL, "0.6", "default", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("e1.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(!out.exists());
}

#[test]
fn e1_requires_naive_to_be_the_first_data_row() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            &arm_row("alloy-autonomous", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("e1.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("first data row"),
        "{}",
        describe(&output)
    );
    assert!(
        !out.exists(),
        "baseline validation must precede output creation and model work"
    );
}

#[test]
fn e1_requires_equal_endpoint_values_across_arms() {
    for (label, naive, default, autonomous) in [
        (
            "model",
            arm_row("naive", "naive", "other-model", "0.6", "none", "1"),
            arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            arm_row("alloy-autonomous", "alloy", MODEL, "0.6", "autonomous", "1"),
        ),
        (
            "temperature",
            arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            arm_row("alloy-default", "alloy", MODEL, "0.7", "default", "1"),
            arm_row("alloy-autonomous", "alloy", MODEL, "0.6", "autonomous", "1"),
        ),
        (
            "repetitions",
            arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            arm_row("alloy-autonomous", "alloy", MODEL, "0.6", "autonomous", "2"),
        ),
    ] {
        let harness = Harness::new();
        let arms = arms_file(harness.root(), "arms.tsv", &[&naive, &default, &autonomous]);
        let out = harness.root().join("out");

        let output = harness.run("e1.sh", &arms, &out);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{label} mismatch must abort E1: {}",
            describe(&output)
        );
        assert!(
            stderr_of(&output).contains(label),
            "{label} mismatch must be named: {}",
            describe(&output)
        );
        assert!(!out.exists(), "{label}: preflight must precede any work");
    }
}

#[test]
fn e1_requires_one_base_url_across_arms() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            &arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            &format!(
                "alloy-autonomous\talloy\t{MODEL}\t0.6\tautonomous\thttp://127.0.0.1:9999/v1/\t1"
            ),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("e1.sh", &arms, &out);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("base_url"),
        "{}",
        describe(&output)
    );
    assert!(!out.exists());
}

#[test]
fn e1_runs_exactly_three_equal_arms_through_the_generic_matrix() {
    let harness = Harness::new();
    let arms = arms_file(
        harness.root(),
        "arms.tsv",
        &[
            &arm_row("naive", "naive", MODEL, "0.6", "none", "1"),
            &arm_row("alloy-default", "alloy", MODEL, "0.6", "default", "1"),
            &arm_row("alloy-autonomous", "alloy", MODEL, "0.6", "autonomous", "1"),
        ],
    );
    let out = harness.root().join("out");

    let output = harness.run("e1.sh", &arms, &out);

    assert!(output.status.success(), "{}", describe(&output));
    let matrix: LiveHoldoutMatrixComparison =
        serde_json::from_str(&fs::read_to_string(out.join("matrix.report.json")).unwrap()).unwrap();
    assert_eq!(matrix.baseline, "naive");
    assert_eq!(matrix.arms.len(), 3);

    let expected = bundle_sha256(&harness.bundle);
    for (arm, profile) in [
        ("naive", None),
        ("alloy-default", Some("default")),
        ("alloy-autonomous", Some("autonomous")),
    ] {
        let report = read_report(&out.join(format!("{arm}.report.json")));
        assert_eq!(report.endpoint.profile.as_deref(), profile, "{arm}");
        assert_eq!(report.endpoint.model, MODEL, "{arm}");
        assert_eq!(
            report.endpoint.harness.source_revision, harness.revision,
            "{arm}"
        );
        assert_eq!(
            report.endpoint.harness.binary_bundle_sha256, expected,
            "{arm}"
        );
        // Every arm ran the shared oracle path, not just a report writer.
        assert_eq!(report.overall.oracle.passes, 1, "{arm} strict oracle");
        assert_eq!(report.overall.model_calls_total, 1, "{arm} model calls");
        assert!(
            out.join(format!("{arm}.artifacts/pass_fixture/rep-1/cargo-test.log"))
                .is_file(),
            "{arm} must retain independent test evidence"
        );
    }
}

/// A minimal committed repository carrying the real `prepare.sh`, so its
/// refusal paths run without a cargo build.
fn prepare_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("eval/live-holdout")).unwrap();
    fs::copy(
        script("prepare.sh"),
        repo.join("eval/live-holdout/prepare.sh"),
    )
    .unwrap();
    make_executable(&repo.join("eval/live-holdout/prepare.sh"));
    fs::write(repo.join("README.md"), "fake repo\n").unwrap();
    git(&repo, &["init", "-q"]);
    git_commit(&repo, "root");
    repo
}

fn run_prepare(repo: &Path, bundle: &Path) -> Output {
    Command::new("bash")
        .arg(repo.join("eval/live-holdout/prepare.sh"))
        .arg(bundle)
        .output()
        .unwrap()
}

#[test]
fn prepare_refuses_a_dirty_worktree_and_a_non_empty_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let repo = prepare_repo(directory.path());

    // A clean worktree still refuses to overwrite an existing bundle.
    let occupied = directory.path().join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("manifest.tsv"), "stale\n").unwrap();
    let output = run_prepare(&repo, &occupied);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("empty"),
        "{}",
        describe(&output)
    );
    assert_eq!(
        fs::read_to_string(occupied.join("manifest.tsv")).unwrap(),
        "stale\n"
    );

    // A dirty worktree cannot produce a bundle at all.
    fs::write(repo.join("uncommitted.txt"), "dirty\n").unwrap();
    let bundle = directory.path().join("bundle");
    let output = run_prepare(&repo, &bundle);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("clean"),
        "{}",
        describe(&output)
    );
    assert!(!bundle.exists(), "a refused bundle must not be created");
}

#[test]
fn prepare_refuses_a_bundle_inside_the_repository() {
    let directory = tempfile::tempdir().unwrap();
    let repo = prepare_repo(directory.path());

    // A bundle under the repository would dirty the worktree with its own
    // build output — the state prepare.sh refuses to build from.
    for relative in ["bundle", "eval/live-holdout/bundle", "."] {
        let bundle = repo.join(relative);
        let output = run_prepare(&repo, &bundle);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{relative}: {}",
            describe(&output)
        );
        assert!(
            stderr_of(&output).contains("outside the repository"),
            "{relative}: {}",
            describe(&output)
        );
        assert!(
            !repo.join("bundle").exists() && !repo.join("eval/live-holdout/bundle").exists(),
            "{relative}: a rejected bundle directory must not be created"
        );
    }

    // Nothing was created and cargo was never invoked.
    assert!(!repo.join("target").exists(), "no build may have started");
    assert_eq!(
        git_status(&repo),
        "",
        "the worktree must be exactly as it was"
    );
}

#[test]
fn prepare_bundles_exactly_the_binaries_the_workspace_builds() {
    let text = fs::read_to_string(script("prepare.sh")).unwrap();
    for binary in BUNDLE_BINARIES {
        assert!(text.contains(binary), "prepare.sh must bundle {binary}");
    }
    for built in [EVALUATOR, SCORER, NAIVE] {
        let path = Path::new(built);
        assert!(path.is_file(), "{built} must be built for this suite");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            BUNDLE_BINARIES.contains(&name.as_str()),
            "{name} is built but not bundled"
        );
    }
    // matrix.sh must verify the manifest prepare.sh writes, field for field.
    let matrix = fs::read_to_string(script("matrix.sh")).unwrap();
    for field in ["source_revision", "worktree", "binary"] {
        assert!(text.contains(field), "prepare.sh must write {field}");
        assert!(matrix.contains(field), "matrix.sh must verify {field}");
    }
}
