//! CLI smoke tests.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_exits_zero() {
    Command::cargo_bin("alloy")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Alloy"));
}

#[test]
fn version_exits_zero() {
    Command::cargo_bin("alloy")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("alloy"));
}
