//! Offline CI surface checks for RFC-0016 §11.7.

use alloy_eval::EvalHarnessConfig;

#[test]
fn offline_ci_has_no_live_provider_api() {
    // Construction only requires a fixture root; there is no provider URL,
    // secret, mode enum, or live-provider feature on Day-1 config.
    let config = EvalHarnessConfig::skeleton("/tmp/alloy-eval-fixtures");
    assert!(config.artifact_dir.is_none());
    assert!(config.cancel.is_none());
    assert_eq!(config.pin_toolchain_channel, "1.97.1");
    assert_eq!(config.max_concurrency, 4);
    assert_eq!(config.max_retained_runs, 32);
}

#[test]
fn alloy_eval_does_not_link_reqwest() {
    let output = std::process::Command::new("cargo")
        .args([
            "tree",
            "-p",
            "alloy-eval",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR").to_string() + "/../..")
        .output()
        .expect("cargo tree");
    assert!(output.status.success(), "cargo tree failed: {output:?}");
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        !tree.lines().any(|line| line.starts_with("reqwest ")),
        "alloy-eval must not link reqwest:\n{tree}"
    );
}

#[test]
fn public_reexports_complete() {
    use alloy_eval::*;
    let _ = COST_DISCLAIMER;
    let _ = NAIVE_BASELINE_LABEL;
    let _ = PERMITTED_SPDX;
    let _ = FIXTURE_MANIFEST_VERSION;
    let _ = CARGO_RECORDING_FORMAT_VERSION;
    let _ = EVAL_MAX_CONCURRENCY;
    let _ = evaluate_gate;
}
