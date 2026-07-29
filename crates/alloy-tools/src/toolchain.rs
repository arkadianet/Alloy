//! Host toolchain probe (research §7.11 item 3).
//!
//! `ToolchainRecord` says `rustc -V` / `cargo -V` belong to the CLI/tools
//! layer, and RFC-0015 T1 forbids process spawning inside `alloy-cli`, so
//! the probe lives here. Failures degrade to `"unknown"` rather than
//! failing the run — the digests exist for cache keys, which day-1
//! templates do not use, and an honest "unknown" is still a stable input.
//!
//! Author: arkadianet

// A read-only version probe of trusted binaries, not workspace execution —
// the sandbox broker seam (RFC-0005 §6.4) is for tool runs, not `-V`.
#![allow(clippy::disallowed_methods)]

use std::process::Command;

use alloy_runtime::ToolchainRecord;

fn stdout_line(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

/// Capture the host toolchain identity via `rustc -V` / `cargo -V`.
#[must_use]
pub fn capture_toolchain() -> ToolchainRecord {
    let rustc_version = stdout_line("rustc", &["-V"]).unwrap_or_else(|| "unknown".to_owned());
    let cargo_version = stdout_line("cargo", &["-V"]).unwrap_or_else(|| "unknown".to_owned());
    // `rustc 1.97.1 (abcdef 2026-06-01)` → channel `1.97.1`.
    let channel = rustc_version
        .split_whitespace()
        .nth(1)
        .unwrap_or("unknown")
        .to_owned();
    ToolchainRecord {
        channel,
        rustc_version,
        cargo_version,
    }
}

/// Host target triple from `rustc -vV`'s `host:` line.
#[must_use]
pub fn host_triple() -> String {
    Command::new("rustc")
        .args(["-vV"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("host: ").map(|h| h.trim().to_owned()))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_never_panics_and_yields_nonempty_fields() {
        let record = capture_toolchain();
        assert!(!record.channel.is_empty());
        assert!(!record.rustc_version.is_empty());
        assert!(!record.cargo_version.is_empty());
        assert!(!host_triple().is_empty());
    }
}
