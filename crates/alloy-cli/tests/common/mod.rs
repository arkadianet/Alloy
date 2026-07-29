//! Shared workspace fixtures for the RFC-0015 integration tests.
//!
//! Author: arkadianet

#![allow(dead_code)] // each test binary uses a subset

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Repo-root `profiles/` sources, compiled in.
pub const DEFAULT_PROFILE: &str = include_str!("../../../../profiles/default.toml");
pub const AUTONOMOUS_PROFILE: &str = include_str!("../../../../profiles/autonomous.toml");
pub const READONLY_PROFILE: &str = include_str!("../../../../profiles/readonly.toml");

/// A workspace with the three catalog profiles, an active `router.toml`
/// (keyed to `key_env`), and `example.env`.
pub fn workspace_with_key_env(key_env: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_profiles(dir.path());
    let router = alloy_runtime::default_router_toml().replace("ALLOY_API_KEY", key_env);
    std::fs::write(dir.path().join("router.toml"), router).unwrap();
    std::fs::write(dir.path().join("example.env"), format!("{key_env}=\n")).unwrap();
    dir
}

/// Default-keyed workspace (`ALLOY_API_KEY`) containing a minimal crate so
/// graph ingest has a manifest to read.
pub fn workspace() -> TempDir {
    let dir = workspace_with_key_env("ALLOY_API_KEY");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "// fixture\n").unwrap();
    dir
}

pub fn write_profiles(root: &Path) {
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(root.join("profiles/default.toml"), DEFAULT_PROFILE).unwrap();
    std::fs::write(root.join("profiles/autonomous.toml"), AUTONOMOUS_PROFILE).unwrap();
    std::fs::write(root.join("profiles/readonly.toml"), READONLY_PROFILE).unwrap();
}

/// Overwrite `profiles/default.toml` with `body`.
pub fn set_default_profile(root: &Path, body: &str) {
    std::fs::write(root.join("profiles/default.toml"), body).unwrap();
}

/// An `alloy` command in `dir` with a scrubbed Alloy environment.
pub fn alloy_in(dir: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("alloy").unwrap();
    cmd.current_dir(dir)
        .env_remove("ALLOY_DATA_DIR")
        .env_remove("ALLOY_PROFILE")
        .env_remove("ALLOY_ROUTER")
        .env_remove("ALLOY_API_KEY");
    cmd
}

/// `git init && commit` via std::process (test-only; the binary itself
/// spawns nothing outside the broker — rule B7).
pub fn git_init_commit(root: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=alloy",
            "-c",
            "user.email=alloy@localhost",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(root)
            .status()
            .expect("git available in tests");
        assert!(status.success(), "git {args:?} failed");
    }
}

/// Copy a directory tree (skipping `target/`).
pub fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_os_str() == "target" {
            continue;
        }
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// The `alloy-tools` repair fixture crate (a lib with one type error).
pub fn fixture_crate_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../alloy-tools/tests/fixtures/sbx_repair")
}
