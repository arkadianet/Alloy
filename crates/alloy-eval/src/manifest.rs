//! Strict fixture manifest DTOs and validated RFC-0016 manifest types.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use alloy_runtime::{
    CapabilityId, ChatMessage, ChatRole, CompletionRequest, EndpointId, ModelEndpoint,
    ModelResponse, ModelTier, NodeId, ProviderId, ResponseFormat, ToolChoice, Usage,
};
use serde::{Deserialize, Serialize};

use crate::error::{bound_message, EvalError};
use crate::fingerprint::RequestFingerprint;
use crate::license::validate_license;
pub use crate::license::{LicenseClass, LicenseMeta, PERMITTED_SPDX};
use crate::recording::{
    validate_expected_diagnostics, CargoJsonRecording, CARGO_RECORDING_FORMAT_VERSION,
};
use crate::scripted::{ScriptOutcome, ScriptedProviderError};

/// Manifest schema version accepted by RFC-0016.
pub const FIXTURE_MANIFEST_VERSION: u32 = 1;

/// Validated fixture identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FixtureId(String);

impl FixtureId {
    /// Construct a fixture id.
    ///
    /// Valid ids are non-empty, at most 128 UTF-8 bytes, use only
    /// `[a-z0-9_.-]`, and are never `.` or `..`.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Manifest`] when the id violates those constraints.
    pub fn new(s: impl Into<String>) -> Result<Self, EvalError> {
        let s = s.into();
        if s.is_empty()
            || s.len() > 128
            || s == "."
            || s == ".."
            || !s
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
        {
            return Err(EvalError::Manifest(format!("invalid fixture id: {s}")));
        }
        Ok(Self(s))
    }

    /// Borrow the id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for FixtureId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for FixtureId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fixture corpus partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureSet {
    /// Prompt-tuning-visible fixtures.
    Train,
    /// Held-out evaluation fixtures.
    Holdout,
}

impl FixtureSet {
    fn as_dir(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Holdout => "holdout",
        }
    }
}

impl std::fmt::Display for FixtureSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_dir())
    }
}

/// Validated fixture manifest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixtureManifest {
    /// Manifest schema version; always [`FIXTURE_MANIFEST_VERSION`].
    pub manifest_version: u32,
    /// Fixture id, matching its directory name.
    pub id: FixtureId,
    /// Fixture set, matching its parent directory and caller-selected set.
    pub set: FixtureSet,
    /// R17 license metadata.
    pub license: LicenseMeta,
    /// Captured Rust/Cargo toolchain.
    pub toolchain: ToolchainRecord,
    /// Workspace snapshot location and package.
    pub workspace: WorkspaceRef,
    /// Relative Rust source path replaced by the Day-1 patch oracle.
    pub naive_target_path: String,
    /// Day-1 patch mode; only full-file replace is accepted.
    pub naive_patch_mode: NaivePatchMode,
    /// Optional fixture-local endpoint prices.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_prices: Option<EndpointPrices>,
    /// Diagnostics expected before repair.
    pub expected_diagnostics: Vec<ExpectedDiagnostic>,
    /// Scripted model turns in declaration order.
    pub turns: Vec<ScriptTurn>,
    /// Paths to recorded pre/post cargo output.
    pub cargo_recordings: CargoRecordingRefs,
    /// Success criteria evaluated by drivers.
    pub success_criteria: Vec<SuccessCriterion>,
    /// Whether all installed scripted outcomes must be consumed.
    pub require_consume_all: bool,
    /// Driver used for this fixture.
    pub driver: FixtureDriverKind,
}

pub use alloy_runtime::ToolchainRecord;

/// Workspace snapshot reference inside a fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRef {
    /// Directory relative to the fixture root containing the Cargo project.
    pub path: String,
    /// Package name for replaying or recording cargo.
    pub package: String,
}

/// Patch interpretation for the naive baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NaivePatchMode {
    /// Replace the whole target file with the model response.
    FullFileReplace,
}

/// Optional fixture-local endpoint prices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointPrices {
    /// Input price per one million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_usd_per_mtok: Option<f64>,
    /// Output price per one million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_usd_per_mtok: Option<f64>,
}

/// Diagnostic expected in the pre-repair recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedDiagnostic {
    /// Exact diagnostic code, e.g. `E0502`.
    pub code: String,
    /// Required substring of the diagnostic message.
    pub message_contains: String,
}

/// Validated scripted turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScriptTurn {
    /// Human-stable turn id.
    pub turn_id: FixtureTurnId,
    /// Canonical completion request.
    pub request: CompletionRequest,
    /// Optional stored request fingerprint, validated when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    /// Scripted provider outcome.
    pub outcome: ScriptTurnOutcome,
}

/// Validated scripted turn outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptTurnOutcome {
    /// Successful model response.
    Response {
        /// Response text, when present.
        text: Option<String>,
        /// Parsed structured JSON response, when present.
        structured: Option<serde_json::Value>,
        /// Provider token usage.
        usage: Usage,
        /// Provider request id, when present.
        provider_request_id: Option<String>,
        /// Provider finish reason, when present.
        finish_reason: Option<String>,
    },
    /// Provider-level error.
    Error {
        /// Error returned by the scripted provider.
        error: ScriptedProviderError,
    },
}

impl From<ScriptTurnOutcome> for ScriptOutcome {
    fn from(value: ScriptTurnOutcome) -> Self {
        match value {
            ScriptTurnOutcome::Response {
                text,
                structured,
                usage,
                provider_request_id,
                finish_reason,
            } => Self::Response(ModelResponse {
                text,
                structured,
                tool_calls: vec![],
                usage,
                provider_request_id,
                finish_reason,
            }),
            ScriptTurnOutcome::Error { error } => Self::Error(error),
        }
    }
}

/// Cargo recording manifest references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoRecordingRefs {
    /// Relative path to pre-repair failing cargo JSON recording.
    pub pre_repair: String,
    /// Relative path to post-repair passing cargo JSON recording.
    pub post_repair: String,
    /// Recording format version; must be [`CARGO_RECORDING_FORMAT_VERSION`].
    pub recording_format_version: u32,
}

/// Success criterion selected by a fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuccessCriterion {
    /// Repaired workspace compiles cleanly.
    CompileClean,
    /// Candidate introduces no new `unsafe` usage.
    NoNewUnsafe,
    /// Expected pre-repair diagnostics are absent after repair.
    ExpectedDiagnosticsCleared,
    /// Required scripted turns are consumed.
    ScriptTurnsConsumed,
}

/// Fixture driver kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureDriverKind {
    /// Day-1 scripted replay driver.
    SkeletonReplay,
    /// Full control-plane driver; deferred.
    ControlPlane,
    /// Naive single-turn baseline driver.
    NaiveBaseline,
}

/// Human-stable turn id inside a fixture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FixtureTurnId {
    /// Capability the turn belongs to, e.g. `repair`.
    pub capability: CapabilityId,
    /// Optional DAG node id for full-stack attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// 0-based ordinal within `(capability, node)`.
    pub ordinal: u32,
}

impl FixtureTurnId {
    /// Render as `capability/node/ordinal` or `capability/-/ordinal`.
    #[must_use]
    pub fn render(&self) -> String {
        match self.node {
            Some(node) => format!("{}/{}/{}", self.capability, node, self.ordinal),
            None => format!("{}/-/{}", self.capability, self.ordinal),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedFixtureParts {
    pub(crate) manifest: FixtureManifest,
    pub(crate) root: PathBuf,
    pub(crate) paths: FixturePaths,
    pub(crate) pre_repair: CargoJsonRecording,
    pub(crate) post_repair: CargoJsonRecording,
    pub(crate) endpoint: ModelEndpoint,
    pub(crate) script_entries: Vec<(RequestFingerprint, ScriptOutcome)>,
}

#[derive(Debug, Clone)]
pub(crate) struct FixturePaths {
    pub(crate) target: PathBuf,
    pub(crate) golden: PathBuf,
    pub(crate) pre_repair: PathBuf,
    pub(crate) post_repair: PathBuf,
}

/// Parse and validate a manifest TOML document that has no filesystem fields.
///
/// This is crate-internal because full fixture loading must also enforce path,
/// license, and recording checks.
pub(crate) fn parse_manifest_toml(toml_src: &str) -> Result<FixtureManifest, EvalError> {
    let wire: ManifestWire = toml::from_str(toml_src)
        .map_err(|err| EvalError::Manifest(bound_message(format!("manifest toml: {err}"))))?;
    validate_and_convert_manifest(wire)
}

pub(crate) fn load_fixture(
    fixture_root: &Path,
    set: FixtureSet,
    id: &FixtureId,
    pin_toolchain_channel: &str,
) -> Result<LoadedFixtureParts, EvalError> {
    let fixture_dir = fixture_root.join(set.as_dir()).join(id.as_str());
    match std::fs::metadata(&fixture_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(EvalError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("not a directory: {}", fixture_dir.display()),
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(EvalError::FixtureNotFound(format!("{set}/{id}")));
        }
        Err(err) => return Err(EvalError::Io(err)),
    }

    let canonical_fixture_root = fixture_root
        .canonicalize()
        .map_err(|_| path_error(fixture_root))?;
    let canonical_fixture_dir = fixture_dir
        .canonicalize()
        .map_err(|_| path_error(&fixture_dir))?;
    if !canonical_fixture_dir.starts_with(&canonical_fixture_root) {
        return Err(path_error(&fixture_dir));
    }

    let manifest_path = fixture_dir.join("manifest.toml");
    let toml_src = match std::fs::read_to_string(&manifest_path) {
        Ok(src) => src,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(EvalError::FixtureNotFound(format!("{set}/{id}")));
        }
        Err(err) => return Err(EvalError::Io(err)),
    };
    let manifest = parse_manifest_toml(&toml_src)?;
    validate_physical_identity(&fixture_dir, set, id, &manifest)?;
    let root = canonical_fixture_dir;
    validate_license(&root, &manifest.license)?;
    let paths = validate_manifest_paths(&root, &manifest)?;
    let (pre_repair, post_repair) =
        load_and_validate_recordings(&paths, &manifest, pin_toolchain_channel)?;
    let endpoint = build_fixture_endpoint(&manifest)?;
    let script_entries = script_entries(&manifest.turns);
    Ok(LoadedFixtureParts {
        manifest,
        root,
        paths,
        pre_repair,
        post_repair,
        endpoint,
        script_entries,
    })
}

fn validate_and_convert_manifest(wire: ManifestWire) -> Result<FixtureManifest, EvalError> {
    let ManifestWire {
        manifest_version,
        id,
        set,
        license,
        toolchain,
        workspace,
        naive_target_path,
        naive_patch_mode,
        endpoint_prices,
        expected_diagnostics,
        turns: wire_turns,
        cargo_recordings,
        success_criteria,
        require_consume_all,
        driver,
    } = wire;

    if manifest_version != FIXTURE_MANIFEST_VERSION {
        return Err(EvalError::Manifest(format!(
            "manifest_version must be {FIXTURE_MANIFEST_VERSION}"
        )));
    }
    if wire_turns.is_empty() {
        return Err(EvalError::Manifest("turns must be non-empty".into()));
    }
    if expected_diagnostics.is_empty() {
        return Err(EvalError::Manifest(
            "expected_diagnostics must be non-empty".into(),
        ));
    }
    for expected in &expected_diagnostics {
        if expected.code.is_empty() || expected.message_contains.is_empty() {
            return Err(EvalError::Manifest(
                "expected diagnostics must have non-empty code and message_contains".into(),
            ));
        }
    }
    let endpoint_prices = endpoint_prices.map(EndpointPrices::from);
    validate_endpoint_prices(endpoint_prices.as_ref())?;
    validate_criteria(&success_criteria)?;

    let mut turns = Vec::with_capacity(wire_turns.len());
    for turn in wire_turns {
        let request = turn.request.into_request();
        if !request.tools.is_empty() {
            return Err(EvalError::Manifest(
                "manifest requests must not use tools".into(),
            ));
        }
        if request
            .temperature
            .map(|temperature| !temperature.is_finite())
            .unwrap_or(false)
        {
            return Err(EvalError::Manifest(
                "request temperature must be finite".into(),
            ));
        }
        let computed = RequestFingerprint::of(&request);
        if let Some(stored) = &turn.request_fingerprint {
            RequestFingerprint::from_hex(stored)?;
            if stored != computed.as_hex() {
                return Err(EvalError::Manifest(
                    "request_fingerprint must match request".into(),
                ));
            }
        }
        turns.push(ScriptTurn {
            turn_id: turn.turn_id.into_turn_id()?,
            request,
            request_fingerprint: turn.request_fingerprint,
            outcome: turn.outcome.into_outcome(),
        });
    }
    validate_turn_ids(&turns)?;
    validate_naive_selector(&turns)?;

    Ok(FixtureManifest {
        manifest_version,
        id,
        set,
        license: license.into(),
        toolchain: toolchain.into(),
        workspace: workspace.into(),
        naive_target_path,
        naive_patch_mode,
        endpoint_prices,
        expected_diagnostics: expected_diagnostics
            .into_iter()
            .map(ExpectedDiagnostic::from)
            .collect(),
        turns,
        cargo_recordings: cargo_recordings.into(),
        success_criteria,
        require_consume_all,
        driver,
    })
}

fn validate_endpoint_prices(prices: Option<&EndpointPrices>) -> Result<(), EvalError> {
    for price in prices
        .into_iter()
        .flat_map(|prices| [prices.input_usd_per_mtok, prices.output_usd_per_mtok])
        .flatten()
    {
        if !price.is_finite() || price < 0.0 {
            return Err(EvalError::Manifest(
                "endpoint price must be finite and non-negative".into(),
            ));
        }
    }
    Ok(())
}

fn validate_criteria(criteria: &[SuccessCriterion]) -> Result<(), EvalError> {
    if criteria.is_empty() {
        return Err(EvalError::Manifest(
            "success_criteria must be non-empty".into(),
        ));
    }
    let mut seen = HashSet::new();
    for criterion in criteria {
        if !seen.insert(*criterion) {
            return Err(EvalError::Manifest(
                "success_criteria must not contain duplicates".into(),
            ));
        }
    }
    Ok(())
}

fn validate_turn_ids(turns: &[ScriptTurn]) -> Result<(), EvalError> {
    let mut full = HashSet::new();
    for turn in turns {
        let node = turn.turn_id.node.map(|node| node.to_string());
        let full_key = (
            turn.turn_id.capability.as_str().to_owned(),
            node,
            turn.turn_id.ordinal,
        );
        if !full.insert(full_key) {
            return Err(EvalError::Manifest("duplicate turn_id".into()));
        }
    }
    Ok(())
}

/// Require exactly one `repair` turn with ordinal 0 (naive baseline selector).
pub(crate) fn require_single_repair_ordinal_zero(
    turns: &[ScriptTurn],
) -> Result<&ScriptTurn, EvalError> {
    let mut matches = turns
        .iter()
        .filter(|turn| turn.turn_id.capability.as_str() == "repair" && turn.turn_id.ordinal == 0);
    match (matches.next(), matches.next()) {
        (Some(turn), None) => Ok(turn),
        _ => Err(EvalError::Manifest(
            "exactly one repair ordinal 0 turn is required".into(),
        )),
    }
}

fn validate_naive_selector(turns: &[ScriptTurn]) -> Result<(), EvalError> {
    require_single_repair_ordinal_zero(turns).map(|_| ())
}

fn validate_physical_identity(
    fixture_dir: &Path,
    set: FixtureSet,
    id: &FixtureId,
    manifest: &FixtureManifest,
) -> Result<(), EvalError> {
    let dir_name = fixture_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| EvalError::Manifest("fixture directory name must be UTF-8".into()))?;
    let parent_name = fixture_dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| EvalError::Manifest("fixture set directory name must be UTF-8".into()))?;
    if dir_name != id.as_str() || manifest.id != *id {
        return Err(EvalError::Manifest(
            "fixture directory name must match manifest id".into(),
        ));
    }
    if parent_name != set.as_dir() || manifest.set != set {
        return Err(EvalError::Manifest(
            "fixture parent, caller set, and manifest set must agree".into(),
        ));
    }
    Ok(())
}

fn validate_manifest_paths(
    fixture_root: &Path,
    manifest: &FixtureManifest,
) -> Result<FixturePaths, EvalError> {
    validate_relative_path_string(&manifest.workspace.path)?;
    validate_relative_path_string(&manifest.naive_target_path)?;
    validate_relative_path_string(&manifest.cargo_recordings.pre_repair)?;
    validate_relative_path_string(&manifest.cargo_recordings.post_repair)?;

    let workspace_dir =
        canonicalize_contained(fixture_root, &fixture_root.join(&manifest.workspace.path))?;
    if !workspace_dir.is_dir() {
        return Err(path_error(&workspace_dir));
    }

    let target = canonicalize_contained(
        fixture_root,
        &workspace_dir.join(&manifest.naive_target_path),
    )?;
    ensure_regular_utf8_file(&target)?;
    let golden = canonicalize_contained(
        fixture_root,
        &workspace_dir.join(format!("{}.post", manifest.naive_target_path)),
    )?;
    ensure_regular_utf8_file(&golden)?;
    let pre_repair = canonicalize_contained(
        fixture_root,
        &fixture_root.join(&manifest.cargo_recordings.pre_repair),
    )?;
    ensure_regular_file(&pre_repair)?;
    let post_repair = canonicalize_contained(
        fixture_root,
        &fixture_root.join(&manifest.cargo_recordings.post_repair),
    )?;
    ensure_regular_file(&post_repair)?;

    Ok(FixturePaths {
        target,
        golden,
        pre_repair,
        post_repair,
    })
}

pub(crate) fn validate_relative_path_string(path: &str) -> Result<(), EvalError> {
    if path.is_empty() || path.contains('\\') || has_windows_drive(path) {
        return Err(path_error(Path::new(path)));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(path_error(path));
    }
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(path_error(path)),
        }
    }
    if !saw_component {
        return Err(path_error(path));
    }
    Ok(())
}

pub(crate) fn canonicalize_contained(
    canonical_fixture_root: &Path,
    path: &Path,
) -> Result<PathBuf, EvalError> {
    let canonical = path.canonicalize().map_err(|_| path_error(path))?;
    if !canonical.starts_with(canonical_fixture_root) {
        return Err(path_error(path));
    }
    Ok(canonical)
}

fn has_windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn ensure_regular_file(path: &Path) -> Result<(), EvalError> {
    let meta = std::fs::metadata(path).map_err(|_| path_error(path))?;
    if !meta.is_file() {
        return Err(path_error(path));
    }
    Ok(())
}

fn ensure_regular_utf8_file(path: &Path) -> Result<(), EvalError> {
    ensure_regular_file(path)?;
    std::fs::read_to_string(path).map_err(|_| path_error(path))?;
    Ok(())
}

fn path_error(path: &Path) -> EvalError {
    EvalError::Manifest(bound_message(format!("path: {}", path.display())))
}

fn load_and_validate_recordings(
    paths: &FixturePaths,
    manifest: &FixtureManifest,
    pin_toolchain_channel: &str,
) -> Result<(CargoJsonRecording, CargoJsonRecording), EvalError> {
    if manifest.cargo_recordings.recording_format_version != CARGO_RECORDING_FORMAT_VERSION {
        return Err(EvalError::RecordingInvalid(format!(
            "recording reference version must be {CARGO_RECORDING_FORMAT_VERSION}"
        )));
    }
    let pre = CargoJsonRecording::load(&paths.pre_repair)?;
    let post = CargoJsonRecording::load(&paths.post_repair)?;
    if pre.toolchain != manifest.toolchain || post.toolchain != manifest.toolchain {
        return Err(EvalError::RecordingInvalid(
            "manifest and recording toolchains must agree".into(),
        ));
    }
    pre.validate_against_pin(pin_toolchain_channel)?;
    post.validate_against_pin(pin_toolchain_channel)?;
    if pre.compile_clean()? {
        return Err(EvalError::RecordingInvalid(
            "pre-repair recording must fail compile".into(),
        ));
    }
    validate_expected_diagnostics(&pre, &manifest.expected_diagnostics)?;
    if !post.compile_clean()? {
        return Err(EvalError::RecordingInvalid(
            "post-repair recording must compile clean".into(),
        ));
    }
    Ok((pre, post))
}

fn build_fixture_endpoint(manifest: &FixtureManifest) -> Result<ModelEndpoint, EvalError> {
    let provider_id = ProviderId::new("eval-script")
        .map_err(|err| EvalError::Internal(format!("eval provider id: {err}")))?;
    let endpoint = ModelEndpoint {
        id: EndpointId::new("eval-script")
            .map_err(|err| EvalError::Internal(format!("eval endpoint id: {err}")))?,
        provider: provider_id,
        display_name: "eval-script".into(),
        model: "scripted".into(),
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: false,
        max_context: 8192,
        input_usd_per_mtok: manifest
            .endpoint_prices
            .as_ref()
            .and_then(|prices| prices.input_usd_per_mtok),
        output_usd_per_mtok: manifest
            .endpoint_prices
            .as_ref()
            .and_then(|prices| prices.output_usd_per_mtok),
    };
    Ok(endpoint)
}

fn script_entries(turns: &[ScriptTurn]) -> Vec<(RequestFingerprint, ScriptOutcome)> {
    turns
        .iter()
        .map(|turn| {
            (
                RequestFingerprint::of(&turn.request),
                ScriptOutcome::from(turn.outcome.clone()),
            )
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    manifest_version: u32,
    id: FixtureId,
    set: FixtureSet,
    license: ManifestLicenseWire,
    toolchain: ManifestToolchainWire,
    workspace: ManifestWorkspaceWire,
    naive_target_path: String,
    naive_patch_mode: NaivePatchMode,
    #[serde(default)]
    endpoint_prices: Option<ManifestEndpointPricesWire>,
    expected_diagnostics: Vec<ManifestExpectedDiagnosticWire>,
    turns: Vec<ScriptTurnWire>,
    cargo_recordings: ManifestCargoRecordingRefsWire,
    success_criteria: Vec<SuccessCriterion>,
    #[serde(default = "default_require_consume_all")]
    require_consume_all: bool,
    driver: FixtureDriverKind,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestLicenseWire {
    class: LicenseClass,
    spdx: String,
    source_note: String,
}

impl From<ManifestLicenseWire> for LicenseMeta {
    fn from(value: ManifestLicenseWire) -> Self {
        Self {
            class: value.class,
            spdx: value.spdx,
            source_note: value.source_note,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestToolchainWire {
    channel: String,
    rustc_version: String,
    cargo_version: String,
}

impl From<ManifestToolchainWire> for ToolchainRecord {
    fn from(value: ManifestToolchainWire) -> Self {
        Self {
            channel: value.channel,
            rustc_version: value.rustc_version,
            cargo_version: value.cargo_version,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestWorkspaceWire {
    path: String,
    package: String,
}

impl From<ManifestWorkspaceWire> for WorkspaceRef {
    fn from(value: ManifestWorkspaceWire) -> Self {
        Self {
            path: value.path,
            package: value.package,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEndpointPricesWire {
    #[serde(default)]
    input_usd_per_mtok: Option<f64>,
    #[serde(default)]
    output_usd_per_mtok: Option<f64>,
}

impl From<ManifestEndpointPricesWire> for EndpointPrices {
    fn from(value: ManifestEndpointPricesWire) -> Self {
        Self {
            input_usd_per_mtok: value.input_usd_per_mtok,
            output_usd_per_mtok: value.output_usd_per_mtok,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestExpectedDiagnosticWire {
    code: String,
    message_contains: String,
}

impl From<ManifestExpectedDiagnosticWire> for ExpectedDiagnostic {
    fn from(value: ManifestExpectedDiagnosticWire) -> Self {
        Self {
            code: value.code,
            message_contains: value.message_contains,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCargoRecordingRefsWire {
    pre_repair: String,
    post_repair: String,
    recording_format_version: u32,
}

impl From<ManifestCargoRecordingRefsWire> for CargoRecordingRefs {
    fn from(value: ManifestCargoRecordingRefsWire) -> Self {
        Self {
            pre_repair: value.pre_repair,
            post_repair: value.post_repair,
            recording_format_version: value.recording_format_version,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptTurnWire {
    turn_id: ManifestFixtureTurnIdWire,
    request: ManifestCompletionRequest,
    #[serde(default)]
    request_fingerprint: Option<String>,
    outcome: ScriptTurnOutcomeWire,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFixtureTurnIdWire {
    capability: String,
    #[serde(default)]
    node: Option<String>,
    ordinal: u32,
}

impl ManifestFixtureTurnIdWire {
    fn into_turn_id(self) -> Result<FixtureTurnId, EvalError> {
        let capability = CapabilityId::new(self.capability).map_err(|err| {
            EvalError::Manifest(bound_message(format!("turn_id capability: {err}")))
        })?;
        let node = self
            .node
            .map(|node| {
                NodeId::parse(&node).map_err(|err| {
                    EvalError::Manifest(bound_message(format!("turn_id node: {err}")))
                })
            })
            .transpose()?;
        Ok(FixtureTurnId {
            capability,
            node,
            ordinal: self.ordinal,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCompletionRequest {
    messages: Vec<ManifestChatMessage>,
    #[serde(default)]
    tools: Vec<serde_json::Value>,
    #[serde(default)]
    tool_choice: ToolChoice,
    #[serde(default)]
    response_format: ResponseFormat,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
}

impl ManifestCompletionRequest {
    fn into_request(self) -> CompletionRequest {
        CompletionRequest {
            messages: self
                .messages
                .into_iter()
                .map(ManifestChatMessage::into_message)
                .collect(),
            tools: self.tools,
            tool_choice: self.tool_choice,
            response_format: self.response_format,
            temperature: self.temperature,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestChatMessage {
    role: ChatRole,
    content: String,
}

impl ManifestChatMessage {
    fn into_message(self) -> ChatMessage {
        ChatMessage {
            role: self.role,
            content: self.content,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl ManifestUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ScriptTurnOutcomeWire {
    Response {
        text: Option<String>,
        #[serde(default)]
        structured: Option<serde_json::Value>,
        usage: ManifestUsage,
        #[serde(default)]
        provider_request_id: Option<String>,
        #[serde(default)]
        finish_reason: Option<String>,
    },
    Error {
        error: ManifestScriptedProviderError,
    },
}

impl ScriptTurnOutcomeWire {
    fn into_outcome(self) -> ScriptTurnOutcome {
        match self {
            Self::Response {
                text,
                structured,
                usage,
                provider_request_id,
                finish_reason,
            } => ScriptTurnOutcome::Response {
                text,
                structured,
                usage: usage.into_usage(),
                provider_request_id,
                finish_reason,
            },
            Self::Error { error } => ScriptTurnOutcome::Error {
                error: error.into(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ManifestScriptedProviderError {
    Auth,
    RateLimit,
    ContextLength,
    Timeout,
    MalformedResponse { message: String },
    HttpStatus { status: u16, message: String },
    Tls { message: String },
    Transport { message: String },
    Internal { message: String },
}

impl From<ManifestScriptedProviderError> for ScriptedProviderError {
    fn from(value: ManifestScriptedProviderError) -> Self {
        match value {
            ManifestScriptedProviderError::Auth => Self::Auth,
            ManifestScriptedProviderError::RateLimit => Self::RateLimit,
            ManifestScriptedProviderError::ContextLength => Self::ContextLength,
            ManifestScriptedProviderError::Timeout => Self::Timeout,
            ManifestScriptedProviderError::MalformedResponse { message } => {
                Self::MalformedResponse { message }
            }
            ManifestScriptedProviderError::HttpStatus { status, message } => {
                Self::HttpStatus { status, message }
            }
            ManifestScriptedProviderError::Tls { message } => Self::Tls { message },
            ManifestScriptedProviderError::Transport { message } => Self::Transport { message },
            ManifestScriptedProviderError::Internal { message } => Self::Internal { message },
        }
    }
}

fn default_require_consume_all() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::Digest;
    use serde_json::json;
    use std::fs;

    fn toolchain() -> ToolchainRecord {
        ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1".into(),
            cargo_version: "cargo 1.97.1".into(),
        }
    }

    fn diagnostic_line(level: &str, code: Option<&str>, message: &str) -> String {
        let code = code
            .map(|code| json!({ "code": code }))
            .unwrap_or(serde_json::Value::Null);
        json!({
            "reason": "compiler-message",
            "message": {
                "level": level,
                "message": message,
                "code": code
            }
        })
        .to_string()
    }

    fn write_recording(path: &Path, exit_code: i32, stdout_lines: Vec<String>) {
        let digest = Digest::sha256(stdout_lines.join("\n").as_bytes());
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "recording_format_version": CARGO_RECORDING_FORMAT_VERSION,
                "toolchain": toolchain(),
                "argv": ["cargo", "check", "--message-format=json"],
                "exit_code": exit_code,
                "stdout_lines": stdout_lines,
                "stderr": "",
                "content_digest": digest,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn manifest_toml(id: &str, set: FixtureSet) -> String {
        let set = set.as_dir();
        format!(
            r#"
manifest_version = 1
id = "{id}"
set = "{set}"
naive_target_path = "src/lib.rs"
naive_patch_mode = "full_file_replace"
success_criteria = ["compile_clean", "expected_diagnostics_cleared", "script_turns_consumed", "no_new_unsafe"]
driver = "skeleton_replay"

[license]
class = "permitted"
spdx = "MIT"
source_note = "test provenance"

[toolchain]
channel = "1.97.1"
rustc_version = "rustc 1.97.1"
cargo_version = "cargo 1.97.1"

[workspace]
path = "workspace"
package = "fixture"

[[expected_diagnostics]]
code = "E0502"
message_contains = "borrow"

[cargo_recordings]
pre_repair = "recordings/pre.json"
post_repair = "recordings/post.json"
recording_format_version = 1

[[turns]]
request_fingerprint = "4e68ffe37fd31000068a317bf27e389a0cb8f9d9a01031f6d42cd9e8559e7d05"

[turns.turn_id]
capability = "repair"
ordinal = 0

[turns.request]
messages = [{{ role = "user", content = "hello" }}]

[turns.outcome]
type = "response"
text = "fixed"

[turns.outcome.usage]
input_tokens = 1
output_tokens = 2
"#
        )
    }

    fn create_fixture(root: &Path, set: FixtureSet, id: &str) -> PathBuf {
        let dir = root.join(set.as_dir()).join(id);
        fs::create_dir_all(dir.join("workspace/src")).unwrap();
        fs::create_dir_all(dir.join("recordings")).unwrap();
        fs::write(dir.join("LICENSE"), "MIT license text\n").unwrap();
        fs::write(dir.join("workspace/src/lib.rs"), "pub fn broken() {}\n").unwrap();
        fs::write(dir.join("workspace/src/lib.rs.post"), "pub fn fixed() {}\n").unwrap();
        write_recording(
            &dir.join("recordings/pre.json"),
            1,
            vec![diagnostic_line("error", Some("E0502"), "borrow failed")],
        );
        write_recording(
            &dir.join("recordings/post.json"),
            0,
            vec![json!({ "reason": "build-finished" }).to_string()],
        );
        fs::write(dir.join("manifest.toml"), manifest_toml(id, set)).unwrap();
        dir
    }

    fn load(root: &Path, set: FixtureSet, id: &str) -> Result<LoadedFixtureParts, EvalError> {
        load_fixture(root, set, &FixtureId::new(id).unwrap(), "1.97.1")
    }

    #[test]
    fn load_fixture_is_set_qualified() {
        let root = tempfile::tempdir().unwrap();
        create_fixture(root.path(), FixtureSet::Train, "same");
        create_fixture(root.path(), FixtureSet::Holdout, "same");
        assert_eq!(
            load(root.path(), FixtureSet::Train, "same")
                .unwrap()
                .manifest
                .set,
            FixtureSet::Train
        );
        assert_eq!(
            load(root.path(), FixtureSet::Holdout, "same")
                .unwrap()
                .manifest
                .set,
            FixtureSet::Holdout
        );
    }

    #[test]
    fn load_fixture_rejects_set_or_directory_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let dir = create_fixture(root.path(), FixtureSet::Train, "mismatch");
        fs::write(
            dir.join("manifest.toml"),
            manifest_toml("other", FixtureSet::Train),
        )
        .unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "mismatch"),
            Err(EvalError::Manifest(_))
        ));
        fs::write(
            dir.join("manifest.toml"),
            manifest_toml("mismatch", FixtureSet::Holdout),
        )
        .unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "mismatch"),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn fixture_id_rejects_dot_components() {
        for id in ["", ".", "..", "Bad", "bad/name", "bad name"] {
            assert!(matches!(FixtureId::new(id), Err(EvalError::Manifest(_))));
        }
        assert!(matches!(
            FixtureId::new("a".repeat(129)),
            Err(EvalError::Manifest(_))
        ));
        assert_eq!(FixtureId::new("ok-id_1.2").unwrap().as_str(), "ok-id_1.2");
    }

    #[test]
    fn fixture_id_deserialize_validates() {
        assert!(serde_json::from_str::<FixtureId>("\"..\"").is_err());
        let id: FixtureId = serde_json::from_str("\"valid-id\"").unwrap();
        assert_eq!(id.as_str(), "valid-id");
    }

    #[test]
    fn manifest_toml_only() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("train/jsononly");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.json"), "{}").unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "jsononly"),
            Err(EvalError::FixtureNotFound(id)) if id == "train/jsononly"
        ));
    }

    #[test]
    fn manifest_request_usage_dtos_deny_unknown() {
        let base = manifest_toml("strict", FixtureSet::Train);
        assert!(parse_manifest_toml(&base).is_ok());

        let request_unknown = base.replace(
            "messages = [{ role = \"user\", content = \"hello\" }]",
            "messages = [{ role = \"user\", content = \"hello\" }]\nunknown = true",
        );
        assert!(matches!(
            parse_manifest_toml(&request_unknown),
            Err(EvalError::Manifest(_))
        ));

        let message_unknown = base.replace(
            "{ role = \"user\", content = \"hello\" }",
            "{ role = \"user\", content = \"hello\", extra = true }",
        );
        assert!(matches!(
            parse_manifest_toml(&message_unknown),
            Err(EvalError::Manifest(_))
        ));

        let usage_unknown = base.replace("output_tokens = 2", "output_tokens = 2\nextra = true");
        assert!(matches!(
            parse_manifest_toml(&usage_unknown),
            Err(EvalError::Manifest(_))
        ));

        let nested_unknowns = [
            (
                "license",
                base.replace(
                    "source_note = \"test provenance\"",
                    "source_note = \"test provenance\"\nextra = true",
                ),
            ),
            (
                "toolchain",
                base.replace(
                    "cargo_version = \"cargo 1.97.1\"",
                    "cargo_version = \"cargo 1.97.1\"\nextra = true",
                ),
            ),
            (
                "workspace",
                base.replace(
                    "package = \"fixture\"",
                    "package = \"fixture\"\nextra = true",
                ),
            ),
            (
                "endpoint_prices",
                base.replace(
                    "[workspace]",
                    "[endpoint_prices]\ninput_usd_per_mtok = 1.0\nextra = true\n\n[workspace]",
                ),
            ),
            (
                "expected_diagnostics",
                base.replace(
                    "message_contains = \"borrow\"",
                    "message_contains = \"borrow\"\nextra = true",
                ),
            ),
            (
                "cargo_recordings",
                base.replace(
                    "recording_format_version = 1",
                    "recording_format_version = 1\nextra = true",
                ),
            ),
            (
                "turn_id",
                base.replace("ordinal = 0", "ordinal = 0\nextra = true"),
            ),
        ];
        for (table, manifest) in nested_unknowns {
            assert!(
                matches!(parse_manifest_toml(&manifest), Err(EvalError::Manifest(_))),
                "{table} accepted an unknown key"
            );
        }
    }

    #[test]
    fn manifest_types_expose_no_deserialize() {
        fn assert_serialize<T: Serialize>(value: &T) {
            serde_json::to_value(value).unwrap();
        }

        let manifest = parse_manifest_toml(&manifest_toml("serialize-only", FixtureSet::Train))
            .expect("wire parser constructs validated manifest types");
        assert_serialize(&manifest);
        assert_serialize(&manifest.turns[0]);
        assert_serialize(&manifest.turns[0].outcome);

        // Compile-fail negative trait bound: these types must not implement Deserialize.
        let t = trybuild::TestCases::new();
        t.compile_fail("tests/ui/manifest_no_deserialize.rs");
    }

    #[test]
    fn manifest_turn_identity_validation() {
        let base = manifest_toml("turns", FixtureSet::Train);
        let missing = base.replace("capability = \"repair\"", "capability = \"edit\"");
        assert!(matches!(
            parse_manifest_toml(&missing),
            Err(EvalError::Manifest(_))
        ));

        let second = r#"

[[turns]]
[turns.turn_id]
capability = "repair"
node = "550e8400-e29b-41d4-a716-446655440000"
ordinal = 0
[turns.request]
messages = [{ role = "user", content = "hello" }]
[turns.outcome]
type = "response"
[turns.outcome.usage]
"#;
        assert!(matches!(
            parse_manifest_toml(&(base.clone() + second)),
            Err(EvalError::Manifest(_))
        ));

        let duplicate = base.clone() + &base[base.find("[[turns]]").unwrap()..];
        assert!(matches!(
            parse_manifest_toml(&duplicate),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn script_turn_outcome_conversion() {
        let outcome = ScriptTurnOutcome::Response {
            text: Some("t".into()),
            structured: None,
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
            },
            provider_request_id: None,
            finish_reason: None,
        };
        match ScriptOutcome::from(outcome) {
            ScriptOutcome::Response(response) => {
                assert!(response.tool_calls.is_empty());
                assert_eq!(response.text.as_deref(), Some("t"));
            }
            ScriptOutcome::Error(_) => panic!("expected response"),
        }

        let error = ScriptTurnOutcome::Error {
            error: ScriptedProviderError::RateLimit,
        };
        assert!(matches!(
            ScriptOutcome::from(error),
            ScriptOutcome::Error(ScriptedProviderError::RateLimit)
        ));
    }

    #[test]
    fn manifest_criteria_validation() {
        let base = manifest_toml("criteria", FixtureSet::Train);
        let empty = base.replace(
            "success_criteria = [\"compile_clean\", \"expected_diagnostics_cleared\", \"script_turns_consumed\", \"no_new_unsafe\"]",
            "success_criteria = []",
        );
        assert!(matches!(
            parse_manifest_toml(&empty),
            Err(EvalError::Manifest(_))
        ));
        let dup = base.replace(
            "success_criteria = [\"compile_clean\", \"expected_diagnostics_cleared\", \"script_turns_consumed\", \"no_new_unsafe\"]",
            "success_criteria = [\"compile_clean\", \"compile_clean\"]",
        );
        assert!(matches!(
            parse_manifest_toml(&dup),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn manifest_fingerprint_validation() {
        let base = manifest_toml("fingerprint", FixtureSet::Train);
        let bad = base.replace(
            "4e68ffe37fd31000068a317bf27e389a0cb8f9d9a01031f6d42cd9e8559e7d05",
            "71ab8ab13b7cb4a68d7727e6268d8793fc4f41506cac57ef15cb7c1931ef7d36",
        );
        assert!(matches!(
            parse_manifest_toml(&bad),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn path_rejects_absolute_parent_and_symlink_escape() {
        assert!(matches!(
            validate_relative_path_string("/abs"),
            Err(EvalError::Manifest(_))
        ));
        assert!(matches!(
            validate_relative_path_string("../escape"),
            Err(EvalError::Manifest(_))
        ));
        assert!(matches!(
            validate_relative_path_string("C:\\escape"),
            Err(EvalError::Manifest(_))
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let root_canon = root.path().canonicalize().unwrap();
            symlink(outside.path(), root.path().join("link")).unwrap();
            assert!(matches!(
                canonicalize_contained(&root_canon, &root.path().join("link")),
                Err(EvalError::Manifest(_))
            ));

            let escaped_fixture = create_fixture(outside.path(), FixtureSet::Train, "escaped");
            fs::create_dir_all(root.path().join("train")).unwrap();
            symlink(&escaped_fixture, root.path().join("train/escaped")).unwrap();
            assert!(matches!(
                load(root.path(), FixtureSet::Train, "escaped"),
                Err(EvalError::Manifest(message)) if message.starts_with("path: ")
            ));
        }
    }

    #[test]
    fn load_fixture_missing_id_is_fixture_not_found() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "missing"),
            Err(EvalError::FixtureNotFound(id)) if id == "train/missing"
        ));
        fs::create_dir_all(root.path().join("train/no-manifest")).unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "no-manifest"),
            Err(EvalError::FixtureNotFound(id)) if id == "train/no-manifest"
        ));
        create_fixture(root.path(), FixtureSet::Holdout, "other-set");
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "other-set"),
            Err(EvalError::FixtureNotFound(id)) if id == "train/other-set"
        ));
    }

    #[test]
    fn manifest_rejects_invalid_endpoint_prices() {
        let base = manifest_toml("prices", FixtureSet::Train);
        let bad = base.replace(
            "[workspace]",
            "[endpoint_prices]\ninput_usd_per_mtok = -1.0\n\n[workspace]",
        );
        assert!(matches!(
            parse_manifest_toml(&bad),
            Err(EvalError::Manifest(msg)) if msg == "endpoint price must be finite and non-negative"
        ));
    }

    #[test]
    fn golden_path_is_workspace_relative_post_suffix() {
        let root = tempfile::tempdir().unwrap();
        let dir = create_fixture(root.path(), FixtureSet::Train, "golden");
        fs::remove_file(dir.join("workspace/src/lib.rs.post")).unwrap();
        assert!(matches!(
            load(root.path(), FixtureSet::Train, "golden"),
            Err(EvalError::Manifest(_))
        ));
    }

    #[test]
    fn turn_node_absent_in_day1_fixtures() {
        let base = manifest_toml("node", FixtureSet::Train);
        assert_eq!(
            parse_manifest_toml(&base).unwrap().turns[0].turn_id.node,
            None
        );
        let invalid = base.replace("ordinal = 0", "node = \"not-a-uuid\"\nordinal = 0");
        assert!(matches!(
            parse_manifest_toml(&invalid),
            Err(EvalError::Manifest(_))
        ));
        let valid = base.replace(
            "ordinal = 0",
            "node = \"550e8400-e29b-41d4-a716-446655440000\"\nordinal = 0",
        );
        assert!(parse_manifest_toml(&valid).is_ok());
    }
}
