//! RFC-0016 separation checks between the offline holdout gates and the
//! live-endpoint operator benchmark.
//!
//! These assertions are the reason the live benchmark can share `alloy-eval`'s
//! vocabulary without eroding §7.4 holdout discipline or §10.2's offline
//! guarantee.

use std::path::{Path, PathBuf};

use alloy_eval::{
    EvalError, EvalHarness, EvalHarnessConfig, FixtureId, FixtureSet, LiveRepairCorpus,
    LIVE_REPAIR_MANIFEST_FILE,
};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_root()
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn live_fixture_root() -> PathBuf {
    workspace_root().join("eval/live-repair/fixtures")
}

#[test]
fn live_corpus_lives_outside_the_offline_fixture_root() {
    let offline_root = crate_root().join("fixtures");
    let live_root = live_fixture_root();
    assert!(live_root.is_dir(), "live corpus must exist");
    assert!(
        !live_root.starts_with(&offline_root),
        "live fixtures must not live under {}",
        offline_root.display()
    );

    // The offline corpus keeps exactly the two RFC-0016 §7.1 partitions.
    let mut sets: Vec<String> = std::fs::read_dir(&offline_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    sets.sort();
    assert_eq!(sets, vec!["holdout".to_owned(), "train".to_owned()]);
}

#[test]
fn the_two_corpora_use_disjoint_manifest_file_names() {
    assert_ne!(LIVE_REPAIR_MANIFEST_FILE, "manifest.toml");

    for entry in std::fs::read_dir(live_fixture_root()).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        assert!(
            dir.join(LIVE_REPAIR_MANIFEST_FILE).is_file(),
            "{} must carry {LIVE_REPAIR_MANIFEST_FILE}",
            dir.display()
        );
        assert!(
            !dir.join("manifest.toml").exists(),
            "{} must not carry an offline manifest.toml",
            dir.display()
        );
        assert!(
            dir.join("LICENSE").is_file(),
            "{} must carry a LICENSE (R17)",
            dir.display()
        );
    }
}

#[test]
fn the_offline_harness_cannot_load_a_live_fixture() {
    let harness = EvalHarness::new(EvalHarnessConfig::skeleton(live_fixture_root())).unwrap();
    let id = FixtureId::new("missing_mut").unwrap();
    for set in [FixtureSet::Train, FixtureSet::Holdout] {
        let loaded = harness.load_fixture(set, &id);
        assert!(
            matches!(loaded, Err(EvalError::FixtureNotFound(_))),
            "offline harness must not load live fixtures as {set}"
        );
    }
}

#[test]
fn the_live_loader_refuses_the_offline_train_and_holdout_corpora() {
    for set in ["train", "holdout"] {
        let root = crate_root().join("fixtures").join(set);
        assert!(root.is_dir());
        let loaded = LiveRepairCorpus::load(&root);
        assert!(
            matches!(loaded, Err(EvalError::Manifest(_))),
            "live loader must refuse the offline {set} corpus"
        );
    }
}

#[test]
fn the_real_live_corpus_loads_with_manifests_and_licences() {
    let corpus = LiveRepairCorpus::load(&live_fixture_root()).unwrap();
    assert_eq!(corpus.fixtures().len(), 10, "10 live-repair fixtures");
    for fixture in corpus.fixtures() {
        let manifest = fixture.manifest();
        assert!(!manifest.goal.trim().is_empty());
        assert!(!manifest.tags.is_empty());
        assert_eq!(
            manifest.expected_outcome,
            alloy_eval::LiveRepairExpectedOutcome::CompileClean
        );
        assert!(fixture.workspace_dir().join("Cargo.toml").is_file());
        assert!(fixture.workspace_dir().join("src/main.rs").is_file());
        assert!(fixture.root().join("LICENSE").is_file());
    }
}

#[test]
fn live_repair_sources_never_spawn_or_reach_the_network() {
    // Restates RFC-0016 §10.2 for the new module: the library and its binary
    // stay pure; only `eval/live-repair/run.sh` executes anything.
    fn collect(dir: &Path, sources: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                collect(&path, sources);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                sources.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
            }
        }
    }

    let mut sources = Vec::new();
    collect(&crate_root().join("src/live_repair"), &mut sources);
    collect(&crate_root().join("src/bin"), &mut sources);
    assert!(!sources.is_empty());
    for (path, source) in sources {
        for forbidden in [
            "std::process::Command",
            "Command::new",
            "reqwest",
            "TcpStream",
            "live-provider",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} must not contain {forbidden}",
                path.display()
            );
        }
    }
}

#[test]
fn the_shell_wrapper_documents_that_it_is_not_a_gate() {
    let script = std::fs::read_to_string(workspace_root().join("eval/live-repair/run.sh")).unwrap();
    assert!(script.contains("NOT an RFC-0016 holdout gate"));
    let readme = std::fs::read_to_string(workspace_root().join("eval/live-repair/README.md"))
        .expect("eval/live-repair/README.md");
    assert!(readme.contains("not a gate") || readme.contains("NOT a gate"));
    assert!(readme.contains("RFC-0016"));
}
