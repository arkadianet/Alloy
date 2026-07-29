//! Live-repair fixture manifests.
//!
//! These manifests describe the *operator* live-endpoint benchmark corpus.
//! They are deliberately a different schema in a different file name
//! (`live-manifest.toml`) at a different corpus root than the offline
//! RFC-0016 train/holdout manifests (`manifest.toml` under
//! `crates/alloy-eval/fixtures/{train,holdout}/`), so neither loader can ever
//! read the other's corpus.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{bound_message, EvalError};
use crate::license::{validate_license, LicenseMeta};
use crate::manifest::{
    canonicalize_contained, validate_relative_path_string, FixtureId, WorkspaceRef,
};

/// Manifest schema version accepted by the live-repair benchmark.
pub const LIVE_REPAIR_MANIFEST_VERSION: u32 = 1;

/// Manifest file name for a live-repair fixture.
///
/// Intentionally *not* `manifest.toml`: the offline harness only ever opens
/// `manifest.toml`, so an offline set can never absorb a live fixture and the
/// live corpus can never absorb an offline one.
pub const LIVE_REPAIR_MANIFEST_FILE: &str = "live-manifest.toml";

/// Maximum UTF-8 bytes accepted for a fixture goal prompt.
pub const LIVE_REPAIR_GOAL_MAX_BYTES: usize = 512;

/// Maximum number of error-class tags per fixture.
pub const LIVE_REPAIR_MAX_TAGS: usize = 16;

/// Expected terminal outcome for a live-repair fixture.
///
/// Mirrors [`crate::SuccessCriterion::CompileClean`]; the live benchmark has
/// exactly one accepted outcome today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveRepairExpectedOutcome {
    /// The repaired workspace must compile cleanly.
    CompileClean,
}

/// Validated live-repair fixture manifest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveRepairManifest {
    /// Schema version; always [`LIVE_REPAIR_MANIFEST_VERSION`].
    pub live_manifest_version: u32,
    /// Fixture id, matching its directory name.
    pub id: FixtureId,
    /// Natural-language goal handed to the `alloy run` invocation.
    pub goal: String,
    /// Expected terminal outcome.
    pub expected_outcome: LiveRepairExpectedOutcome,
    /// Error-class tags, e.g. `["e0384", "mutability"]`.
    pub tags: Vec<String>,
    /// R17 license metadata, reusing the offline corpus rules.
    pub license: LicenseMeta,
    /// Workspace snapshot location and package.
    pub workspace: WorkspaceRef,
}

/// A loaded live-repair fixture with resolved on-disk paths.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRepairFixture {
    manifest: LiveRepairManifest,
    root: PathBuf,
    workspace_dir: PathBuf,
}

impl LiveRepairFixture {
    /// Borrow the validated manifest.
    #[must_use]
    pub fn manifest(&self) -> &LiveRepairManifest {
        &self.manifest
    }

    /// Canonical fixture directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical directory of the Cargo workspace snapshot to copy.
    #[must_use]
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }
}

/// The live-repair fixture corpus, loaded in deterministic id order.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRepairCorpus {
    fixtures: Vec<LiveRepairFixture>,
}

impl LiveRepairCorpus {
    /// Load every fixture directory under `root`.
    ///
    /// Ownership: borrows `root`; returns an owned corpus.
    ///
    /// # Errors
    ///
    /// - [`EvalError::Manifest`] when `root` is (or is inside) the offline
    ///   RFC-0016 train/holdout corpus, when a fixture carries an offline
    ///   `manifest.toml`, or when any manifest fails schema/path validation.
    /// - [`EvalError::LicenseForbidden`] for R17 violations.
    /// - [`EvalError::Io`] when the corpus directory cannot be enumerated.
    pub fn load(root: &Path) -> Result<Self, EvalError> {
        reject_offline_corpus_root(root)?;
        let canonical_root = root.canonicalize().map_err(|_| path_error(root))?;

        let mut fixtures = Vec::new();
        for entry in std::fs::read_dir(&canonical_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| EvalError::Manifest("fixture directory must be UTF-8".into()))?;
            let id = FixtureId::new(name)?;
            fixtures.push(load_fixture(&canonical_root, &path, &id)?);
        }

        if fixtures.is_empty() {
            return Err(EvalError::FixtureNotFound(bound_message(format!(
                "no live-repair fixtures under {}",
                canonical_root.display()
            ))));
        }
        fixtures.sort_by(|left, right| left.manifest.id.as_str().cmp(right.manifest.id.as_str()));
        Ok(Self { fixtures })
    }

    /// Borrow the loaded fixtures in id order.
    #[must_use]
    pub fn fixtures(&self) -> &[LiveRepairFixture] {
        &self.fixtures
    }

    /// Look up one fixture by id.
    #[must_use]
    pub fn get(&self, id: &FixtureId) -> Option<&LiveRepairFixture> {
        self.fixtures
            .iter()
            .find(|fixture| fixture.manifest.id == *id)
    }
}

/// Reject a corpus root that is, or lives under, the offline fixture corpus.
///
/// This is the load-time half of the RFC-0016 §7.4 layer-1 separation: the
/// live benchmark must never be pointed at `train/` or `holdout/` fixtures.
fn reject_offline_corpus_root(root: &Path) -> Result<(), EvalError> {
    let mut components = root.components().filter_map(|component| match component {
        std::path::Component::Normal(part) => part.to_str(),
        _ => None,
    });
    if components.any(|part| part == "train" || part == "holdout") {
        return Err(EvalError::Manifest(bound_message(format!(
            "live-repair corpus must not be the offline train/holdout corpus: {}",
            root.display()
        ))));
    }
    Ok(())
}

fn load_fixture(
    canonical_root: &Path,
    fixture_dir: &Path,
    id: &FixtureId,
) -> Result<LiveRepairFixture, EvalError> {
    let root = canonicalize_contained(canonical_root, fixture_dir)?;
    if root.join("manifest.toml").exists() {
        return Err(EvalError::Manifest(bound_message(format!(
            "live-repair fixture must not carry an offline manifest.toml: {}",
            root.display()
        ))));
    }

    let manifest_path = root.join(LIVE_REPAIR_MANIFEST_FILE);
    let toml_src = match std::fs::read_to_string(&manifest_path) {
        Ok(src) => src,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(EvalError::FixtureNotFound(format!(
                "live-repair/{id}/{LIVE_REPAIR_MANIFEST_FILE}"
            )));
        }
        Err(err) => return Err(EvalError::Io(err)),
    };
    let manifest = parse_live_manifest_toml(&toml_src)?;
    if manifest.id != *id {
        return Err(EvalError::Manifest(
            "fixture directory name must match manifest id".into(),
        ));
    }
    validate_license(&root, &manifest.license)?;

    let workspace_dir = canonicalize_contained(&root, &root.join(&manifest.workspace.path))?;
    if !workspace_dir.is_dir() {
        return Err(path_error(&workspace_dir));
    }
    if !workspace_dir.join("Cargo.toml").is_file() {
        return Err(path_error(&workspace_dir.join("Cargo.toml")));
    }

    Ok(LiveRepairFixture {
        manifest,
        root,
        workspace_dir,
    })
}

/// Parse and validate one `live-manifest.toml` document.
pub(crate) fn parse_live_manifest_toml(toml_src: &str) -> Result<LiveRepairManifest, EvalError> {
    let wire: LiveManifestWire = toml::from_str(toml_src).map_err(|err| {
        EvalError::Manifest(bound_message(format!("live-repair manifest toml: {err}")))
    })?;
    validate_and_convert(wire)
}

fn validate_and_convert(wire: LiveManifestWire) -> Result<LiveRepairManifest, EvalError> {
    let LiveManifestWire {
        live_manifest_version,
        id,
        goal,
        expected_outcome,
        tags,
        license,
        workspace,
    } = wire;

    if live_manifest_version != LIVE_REPAIR_MANIFEST_VERSION {
        return Err(EvalError::Manifest(format!(
            "live_manifest_version must be {LIVE_REPAIR_MANIFEST_VERSION}"
        )));
    }
    let workspace = WorkspaceRef::from(workspace);
    validate_goal(&goal)?;
    validate_tags(&tags)?;
    validate_relative_path_string(&workspace.path)?;
    if workspace.package.trim().is_empty() {
        return Err(EvalError::Manifest(
            "workspace.package must be non-empty".into(),
        ));
    }

    Ok(LiveRepairManifest {
        live_manifest_version,
        id,
        goal,
        expected_outcome,
        tags,
        license,
        workspace,
    })
}

fn validate_goal(goal: &str) -> Result<(), EvalError> {
    if goal.trim().is_empty() {
        return Err(EvalError::Manifest("goal must be non-empty".into()));
    }
    if goal.len() > LIVE_REPAIR_GOAL_MAX_BYTES {
        return Err(EvalError::Manifest(format!(
            "goal must be at most {LIVE_REPAIR_GOAL_MAX_BYTES} bytes"
        )));
    }
    // The plan output is line- and tab-delimited for shell consumption.
    if goal.contains(['\n', '\r', '\t']) {
        return Err(EvalError::Manifest(
            "goal must not contain newlines or tabs".into(),
        ));
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), EvalError> {
    if tags.is_empty() {
        return Err(EvalError::Manifest("tags must be non-empty".into()));
    }
    if tags.len() > LIVE_REPAIR_MAX_TAGS {
        return Err(EvalError::Manifest(format!(
            "tags must be at most {LIVE_REPAIR_MAX_TAGS} entries"
        )));
    }
    let mut seen = HashSet::new();
    for tag in tags {
        if tag.is_empty()
            || tag.len() > 64
            || !tag
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
        {
            return Err(EvalError::Manifest(bound_message(format!(
                "invalid tag: {tag}"
            ))));
        }
        if !seen.insert(tag.as_str()) {
            return Err(EvalError::Manifest(
                "tags must not contain duplicates".into(),
            ));
        }
    }
    Ok(())
}

fn path_error(path: &Path) -> EvalError {
    EvalError::Manifest(bound_message(format!("path: {}", path.display())))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveManifestWire {
    live_manifest_version: u32,
    id: FixtureId,
    goal: String,
    expected_outcome: LiveRepairExpectedOutcome,
    tags: Vec<String>,
    license: LicenseMeta,
    workspace: LiveWorkspaceWire,
}

/// Manifest-owned workspace DTO.
///
/// The reused [`WorkspaceRef`] does not carry `deny_unknown_fields` and must
/// therefore never be a direct deserialization target (RFC-0016 §7.2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveWorkspaceWire {
    path: String,
    package: String,
}

impl From<LiveWorkspaceWire> for WorkspaceRef {
    fn from(value: LiveWorkspaceWire) -> Self {
        Self {
            path: value.path,
            package: value.package,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const VALID: &str = r#"
live_manifest_version = 1
id = "missing_mut"
goal = "fix the compile error in src/main.rs"
expected_outcome = "compile_clean"
tags = ["e0384", "mutability"]

[license]
class = "permitted"
spdx = "Alloy-Original"
source_note = "Alloy-original live-repair fixture by arkadianet."

[workspace]
path = "workspace"
package = "missing-mut"
"#;

    fn with_line(replace: &str, with: &str) -> String {
        VALID.replace(replace, with)
    }

    #[test]
    fn live_manifest_parses_valid_document() {
        let manifest = parse_live_manifest_toml(VALID).unwrap();
        assert_eq!(manifest.live_manifest_version, LIVE_REPAIR_MANIFEST_VERSION);
        assert_eq!(manifest.id.as_str(), "missing_mut");
        assert_eq!(manifest.goal, "fix the compile error in src/main.rs");
        assert_eq!(
            manifest.expected_outcome,
            LiveRepairExpectedOutcome::CompileClean
        );
        assert_eq!(manifest.tags, vec!["e0384", "mutability"]);
        assert_eq!(manifest.workspace.path, "workspace");
        assert_eq!(manifest.workspace.package, "missing-mut");
        assert_eq!(manifest.license.spdx, "Alloy-Original");
    }

    #[test]
    fn live_manifest_rejects_unknown_fields() {
        let src = format!("{VALID}\nunexpected = true\n");
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_offline_set_field() {
        // `set = "holdout"` is the offline schema's discriminator; a live
        // manifest that carries it must be a hard error, never a silent
        // accept, so the two corpora cannot blur.
        let src = with_line(
            "id = \"missing_mut\"",
            "id = \"missing_mut\"\nset = \"holdout\"",
        );
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_wrong_version() {
        let src = with_line("live_manifest_version = 1", "live_manifest_version = 2");
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_invalid_id() {
        let src = with_line("id = \"missing_mut\"", "id = \"Missing Mut\"");
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_bad_goal() {
        for goal in ["", "   ", "has\ttab", "has\nnewline"] {
            let src = with_line(
                "goal = \"fix the compile error in src/main.rs\"",
                &format!("goal = {}", toml_string(goal)),
            );
            assert!(
                matches!(parse_live_manifest_toml(&src), Err(EvalError::Manifest(_))),
                "goal {goal:?} must be rejected"
            );
        }
        let long = "x".repeat(LIVE_REPAIR_GOAL_MAX_BYTES + 1);
        let src = with_line(
            "goal = \"fix the compile error in src/main.rs\"",
            &format!("goal = \"{long}\""),
        );
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_bad_tags() {
        for tags in [
            "[]",
            "[\"E0384\"]",
            "[\"has space\"]",
            "[\"dup\", \"dup\"]",
            "[\"\"]",
        ] {
            let src = with_line(
                "tags = [\"e0384\", \"mutability\"]",
                &format!("tags = {tags}"),
            );
            assert!(
                matches!(parse_live_manifest_toml(&src), Err(EvalError::Manifest(_))),
                "tags {tags} must be rejected"
            );
        }
    }

    #[test]
    fn live_manifest_rejects_expected_outcome_other_than_compile_clean() {
        let src = with_line(
            "expected_outcome = \"compile_clean\"",
            "expected_outcome = \"tests_pass\"",
        );
        assert!(matches!(
            parse_live_manifest_toml(&src),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn live_manifest_rejects_escaping_workspace_path() {
        for path in ["/abs", "../escape", "", "."] {
            let src = with_line(
                "path = \"workspace\"",
                &format!("path = {}", toml_string(path)),
            );
            assert!(
                matches!(parse_live_manifest_toml(&src), Err(EvalError::Manifest(_))),
                "workspace path {path:?} must be rejected"
            );
        }
    }

    fn toml_string(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn write_fixture(root: &Path, id: &str, manifest: &str) {
        let dir = root.join(id);
        fs::create_dir_all(dir.join("workspace/src")).unwrap();
        fs::write(dir.join(LIVE_REPAIR_MANIFEST_FILE), manifest).unwrap();
        fs::write(dir.join("LICENSE"), "license text\n").unwrap();
        fs::write(dir.join("workspace/Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.join("workspace/src/main.rs"), "fn main() {}\n").unwrap();
    }

    #[test]
    fn corpus_loads_fixtures_in_id_order() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), "missing_mut", VALID);
        write_fixture(
            dir.path(),
            "a_first",
            &with_line("id = \"missing_mut\"", "id = \"a_first\""),
        );

        let corpus = LiveRepairCorpus::load(dir.path()).unwrap();
        let ids: Vec<&str> = corpus
            .fixtures()
            .iter()
            .map(|fixture| fixture.manifest().id.as_str())
            .collect();
        assert_eq!(ids, vec!["a_first", "missing_mut"]);
        let id = FixtureId::new("missing_mut").unwrap();
        let fixture = corpus.get(&id).unwrap();
        assert!(fixture.workspace_dir().join("Cargo.toml").is_file());
        assert!(fixture.workspace_dir().starts_with(fixture.root()));
    }

    #[test]
    fn corpus_rejects_offline_train_or_holdout_root() {
        for set in ["train", "holdout"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("fixtures").join(set);
            fs::create_dir_all(&root).unwrap();
            write_fixture(&root, "missing_mut", VALID);
            assert!(
                matches!(LiveRepairCorpus::load(&root), Err(EvalError::Manifest(_))),
                "{set} root must be rejected"
            );
        }
    }

    #[test]
    fn corpus_rejects_fixture_carrying_offline_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), "missing_mut", VALID);
        fs::write(dir.path().join("missing_mut/manifest.toml"), "id = 'x'\n").unwrap();
        assert!(matches!(
            LiveRepairCorpus::load(dir.path()),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn corpus_requires_license_file() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), "missing_mut", VALID);
        fs::remove_file(dir.path().join("missing_mut/LICENSE")).unwrap();
        assert!(matches!(
            LiveRepairCorpus::load(dir.path()),
            Err(EvalError::LicenseForbidden(_))
        ));
    }

    #[test]
    fn corpus_requires_directory_name_to_match_id() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), "other_name", VALID);
        assert!(matches!(
            LiveRepairCorpus::load(dir.path()),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn corpus_rejects_empty_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            LiveRepairCorpus::load(dir.path()),
            Err(EvalError::FixtureNotFound(_))
        ));
    }
}
