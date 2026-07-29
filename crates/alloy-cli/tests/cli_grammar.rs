//! RFC-0015 §12.2 — argv and rendering tests (`assert_cmd` style).
//!
//! Author: arkadianet

mod common;

use predicates::prelude::*;

const SUBCOMMANDS: &[&str] = &[
    "run", "review", "events", "approve", "cancel", "resume", "index", "host",
];

/// CL10 — top-level and per-subcommand help match snapshots.
#[test]
fn help_snapshot_matches() {
    let snap_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    let bless = std::env::var_os("ALLOY_BLESS").is_some();
    let mut targets: Vec<Vec<&str>> = vec![vec!["--help"]];
    for sub in SUBCOMMANDS {
        targets.push(vec![sub, "--help"]);
    }
    for argv in targets {
        let name = if argv.len() == 1 {
            "top".to_owned()
        } else {
            argv[0].to_owned()
        };
        let out = assert_cmd::Command::cargo_bin("alloy")
            .unwrap()
            .args(&argv)
            .output()
            .unwrap();
        assert!(out.status.success(), "{argv:?} help failed");
        let text = String::from_utf8(out.stdout).unwrap();
        let path = snap_dir.join(format!("help_{name}.txt"));
        if bless {
            std::fs::create_dir_all(&snap_dir).unwrap();
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing snapshot {}; run with ALLOY_BLESS=1",
                path.display()
            )
        });
        assert_eq!(
            text, expected,
            "help for {argv:?} changed; deliberate changes update the snapshot (ALLOY_BLESS=1)"
        );
    }
}

/// CL2/CL3 — every subcommand parses `--json --workspace X`.
#[test]
fn every_subcommand_accepts_json_and_workspace() {
    for sub in SUBCOMMANDS {
        // Parse-only proof: `--help` after the flags exits 0 without I/O.
        assert_cmd::Command::cargo_bin("alloy")
            .unwrap()
            .args([sub, "--json", "--workspace", "somewhere", "--help"])
            .assert()
            .success();
    }
}

/// CL4 — unknown profile exits 2 and names the catalog.
#[test]
fn unknown_profile_is_usage_error() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .args(["events", "--profile", "weird"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("default"))
        .stderr(predicate::str::contains("readonly"));
}

/// CL5 — malformed ids are usage errors, not not-found.
#[test]
fn malformed_id_is_usage_not_not_found() {
    assert_cmd::Command::cargo_bin("alloy")
        .unwrap()
        .args([
            "approve",
            "--run",
            "notauuid",
            "--gate",
            "also-not",
            "--decision",
            "allow",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("malformed run id"));
}

/// `--yes` and `--no-input` are mutually exclusive.
#[test]
fn yes_and_no_input_conflict() {
    assert_cmd::Command::cargo_bin("alloy")
        .unwrap()
        .args(["run", "goal", "--yes", "--no-input"])
        .assert()
        .code(2);
}

/// PF11 — `--max-usd` above the profile ceiling exits 2 naming both numbers.
#[test]
fn max_usd_above_profile_ceiling_rejected() {
    let dir = common::workspace();
    common::alloy_in(dir.path())
        .args(["run", "goal", "--max-usd", "50"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("50"))
        .stderr(predicate::str::contains("5"));
}

/// CL9 — help works in an empty directory with no config at all.
#[test]
fn help_works_without_any_config() {
    let dir = tempfile::tempdir().unwrap();
    assert_cmd::Command::cargo_bin("alloy")
        .unwrap()
        .current_dir(dir.path())
        .arg("--help")
        .assert()
        .success();
    assert_cmd::Command::cargo_bin("alloy")
        .unwrap()
        .current_dir(dir.path())
        .arg("--version")
        .assert()
        .success();
}
