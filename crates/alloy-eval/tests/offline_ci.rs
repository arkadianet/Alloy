//! Offline CI surface checks for RFC-0016 §11.7.

use std::path::{Path, PathBuf};

use alloy_eval::EvalHarnessConfig;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_root().join("../..")
}

fn rust_sources() -> Vec<(PathBuf, String)> {
    fn collect(dir: &Path, sources: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let entry = entry.expect("read source entry");
            let path = entry.path();
            if path.is_dir() {
                collect(&path, sources);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                let source = std::fs::read_to_string(&path).expect("read Rust source");
                sources.push((path, source));
            }
        }
    }

    let mut sources = Vec::new();
    collect(&crate_root().join("src"), &mut sources);
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    sources
}

#[test]
fn offline_ci_has_no_live_provider_api() {
    // Construction only requires a fixture root; there is no provider URL,
    // secret, mode enum, or live-provider feature on Day-1 config.
    let config = EvalHarnessConfig::skeleton("/tmp/alloy-eval-fixtures");
    let EvalHarnessConfig {
        fixture_root,
        thresholds: _,
        max_concurrency,
        pin_toolchain_channel,
        cancel,
        artifact_dir,
        max_retained_runs,
    } = config;
    assert_eq!(fixture_root, PathBuf::from("/tmp/alloy-eval-fixtures"));
    assert!(artifact_dir.is_none());
    assert!(cancel.is_none());
    assert_eq!(pin_toolchain_channel, "1.97.1");
    assert_eq!(max_concurrency, 4);
    assert_eq!(max_retained_runs, 32);
}

#[test]
fn alloy_eval_does_not_link_reqwest() {
    // RFC-0016 checks the package-scoped dependency graph, not the workspace
    // aggregate, and checks the minimal feature set independently.
    let output = std::process::Command::new("cargo")
        .args([
            "tree",
            "-p",
            "alloy-eval",
            "--no-default-features",
            "-e",
            "all",
            "--prefix",
            "none",
        ])
        .current_dir(workspace_root())
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
fn source_has_no_network_or_dotenv_writes() {
    for (path, source) in rust_sources() {
        assert!(
            !source.contains("reqwest::"),
            "{} must not reference reqwest",
            path.display()
        );
        assert!(
            !source.contains(".env"),
            "{} must not read or write .env",
            path.display()
        );
    }
}

#[test]
fn harness_driver_public_path_has_no_toolchain_spawn() {
    for (path, source) in rust_sources() {
        let relative = path.strip_prefix(crate_root()).expect("source under crate");
        if relative != Path::new("src/harness.rs") && !relative.starts_with(Path::new("src/driver"))
        {
            continue;
        }
        // Live stack-driver (compiled only under `--features stack-driver`) may
        // orchestrate sandbox/`cargo check` via alloy-tools. It must still never
        // shell out with std::process::Command — the broker owns process spawn.
        if relative == Path::new("src/driver/stack.rs") {
            assert!(
                source.contains("feature `stack-driver`")
                    || source.contains("stack-driver"),
                "stack.rs must document the stack-driver feature gate"
            );
            for forbidden in ["Command::new(\"cargo\")", "std::process::Command"] {
                assert!(
                    !source.contains(forbidden),
                    "{} must not use {}; sandbox broker owns process spawn",
                    path.display(),
                    forbidden
                );
            }
            continue;
        }
        for forbidden in ["Command::new(\"cargo\")", "std::process::Command"] {
            assert!(
                !source.contains(forbidden),
                "{} must not spawn toolchains from the harness/driver path",
                path.display()
            );
        }
    }
}

#[test]
fn alloy_eval_src_has_no_process_command() {
    for (path, source) in rust_sources() {
        let relative = path.strip_prefix(crate_root()).expect("source under crate");
        // Optional live stack-driver sources may mention process seams only if
        // they stay free of std::process::Command (alloy-tools owns spawn).
        if relative == Path::new("src/driver/stack.rs") {
            assert!(
                !source.contains("std::process::Command"),
                "{} must not use std::process::Command",
                path.display()
            );
            continue;
        }
        assert!(
            !source.contains("std::process::Command"),
            "{} must not use std::process::Command",
            path.display()
        );
    }
}

#[test]
fn stack_driver_feature_is_optional_and_default_off() {
    let manifest =
        std::fs::read_to_string(crate_root().join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        manifest.contains("stack-driver"),
        "stack-driver feature must be declared for thesis runs"
    );
    assert!(
        manifest.contains("default = []"),
        "default features must stay empty so offline CI never pulls alloy-tools"
    );
}

#[test]
fn package_has_no_live_provider_or_unicode_normalization_feature() {
    let manifest =
        std::fs::read_to_string(crate_root().join("Cargo.toml")).expect("read Cargo.toml");
    for forbidden in ["live-provider", "unicode-normalization", "uni_normalize"] {
        assert!(
            !manifest.contains(forbidden),
            "alloy-eval Cargo.toml contains forbidden {forbidden}"
        );
    }
    for (path, source) in rust_sources() {
        assert!(
            !source.contains("live-provider"),
            "{} contains forbidden live-provider feature string",
            path.display()
        );
    }
}

#[test]
fn config_requires_root_constructor() {
    let source =
        std::fs::read_to_string(crate_root().join("src/harness.rs")).expect("read harness source");
    assert!(!source.contains("impl Default for EvalHarnessConfig"));
    let declaration_prefix = source
        .split_once("pub struct EvalHarnessConfig")
        .expect("EvalHarnessConfig declaration")
        .0;
    let derive = declaration_prefix
        .rsplit_once("#[derive(")
        .expect("EvalHarnessConfig derive")
        .1
        .lines()
        .next()
        .unwrap();
    assert!(
        !derive.contains("Default"),
        "EvalHarnessConfig must require an explicit fixture root"
    );

    let config = EvalHarnessConfig::skeleton("/explicit/root");
    assert_eq!(config.fixture_root, PathBuf::from("/explicit/root"));
}

#[test]
fn public_reexports_complete() {
    use alloy_eval::*;

    let _: Option<RequestFingerprint> = None;
    let _: Option<ScriptedProvider> = None;
    let _: Option<ScriptOutcome> = None;
    let _: Option<ScriptTurnOutcome> = None;
    let _: Option<ScriptedProviderError> = None;
    let _: Option<ScriptedInvocation> = None;

    let _: Option<FixtureTurnId> = None;
    let _: Option<FixtureId> = None;
    let _: Option<FixtureSet> = None;
    let _: Option<FixtureManifest> = None;
    let _: Option<LicenseClass> = None;
    let _: Option<LicenseMeta> = None;
    let _: Option<ToolchainRecord> = None;
    let _: Option<WorkspaceRef> = None;
    let _: Option<NaivePatchMode> = None;
    let _: Option<EndpointPrices> = None;
    let _: Option<ExpectedDiagnostic> = None;
    let _: Option<ScriptTurn> = None;
    let _: Option<CargoRecordingRefs> = None;
    let _: Option<SuccessCriterion> = None;
    let _: Option<FixtureDriverKind> = None;

    let _: Option<CargoJsonRecording> = None;
    let _: Option<RecordedDiagnostic> = None;

    let _: Option<FixtureStatus> = None;
    let _: Option<FixtureOutcome> = None;
    let _: Option<CriterionResult> = None;
    let _: Option<ReportError> = None;
    let _: Option<EvalMetrics> = None;
    let _: Option<MetricField<f64>> = None;
    let _: Option<UnmeasuredReason> = None;
    let _: Option<EvalTrajectoryRecord> = None;
    let _: Option<EvalReport> = None;
    let _: Option<CostClaimGrade> = None;
    let _: Option<CostClaimEnvelope> = None;

    let _: Option<LoadedFixture> = None;
    let _: Option<EvalHarness> = None;
    let _: Option<EvalHarnessConfig> = None;
    let _: Option<GateThresholds> = None;
    let _: Option<GateResult> = None;
    let _: Option<GateFailure> = None;
    let _: Option<NaiveComparisonResult> = None;
    let _: Option<EvalError> = None;

    let _ = COST_DISCLAIMER;
    let _ = NAIVE_BASELINE_LABEL;
    let _ = PERMITTED_SPDX;
    let _ = FIXTURE_MANIFEST_VERSION;
    let _ = CARGO_RECORDING_FORMAT_VERSION;
    let _ = EVAL_MAX_CONCURRENCY;
    let _ = evaluate_gate;
}
