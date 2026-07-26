//! Eval-local trajectory retention and optional JSONL artifacts.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use alloy_runtime::{
    classify_provider_error, Digest, EndpointId, ErrorClass, ModelEndpoint, ModelResponse,
    ModelTier, ModelUsdSource, ProviderError, ProviderId,
};
use serde::{Deserialize, Serialize};

use crate::cost_claim::derive_eval_usd;
use crate::error::EvalError;
use crate::fingerprint::RequestFingerprint;
use crate::manifest::{FixtureId, FixtureSet, FixtureTurnId};
use crate::report::{EvalReport, FixtureStatus};

/// One retained decision tuple for later offline grouping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalTrajectoryRecord {
    /// Fixture id.
    pub fixture_id: FixtureId,
    /// Fixture set.
    pub set: FixtureSet,
    /// Manifest turn id.
    pub turn_id: FixtureTurnId,
    /// Request fingerprint.
    pub request_fingerprint: RequestFingerprint,
    /// Day-1 content hash, equal to the request fingerprint digest.
    pub request_content_hash: Digest,
    /// Endpoint catalog id.
    pub endpoint_id: EndpointId,
    /// Provider catalog id.
    pub provider_id: ProviderId,
    /// Model tier; Day-1 scripted path uses `Standard`.
    pub model_tier: ModelTier,
    /// Input token count, if known.
    pub input_tokens: Option<u64>,
    /// Output token count, if known.
    pub output_tokens: Option<u64>,
    /// Uncalibrated operator-price estimate.
    pub usd: Option<f64>,
    /// USD source when `usd` is present.
    pub usd_source: Option<ModelUsdSource>,
    /// Observed completion duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Confidence score; always `None` in Day-1 scripted path.
    pub confidence: Option<f32>,
    /// Classified provider error or cancellation.
    pub error_class: Option<ErrorClass>,
    /// Whether the complete call returned `Ok`.
    pub complete_ok: bool,
    /// Final fixture status stamped before report assembly.
    pub fixture_status: FixtureStatus,
    /// Final compile observation stamped before report assembly.
    pub compile_clean: Option<bool>,
}

impl EvalTrajectoryRecord {
    /// Construct a trajectory row for a successful provider response.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn from_response(
        fixture_id: FixtureId,
        set: FixtureSet,
        turn_id: FixtureTurnId,
        request_fingerprint: RequestFingerprint,
        endpoint: &ModelEndpoint,
        response: &ModelResponse,
        duration_ms: Option<u64>,
        fixture_status: FixtureStatus,
        compile_clean: Option<bool>,
    ) -> Self {
        let usd = derive_eval_usd(endpoint, &response.usage);
        Self::base(
            fixture_id,
            set,
            turn_id,
            request_fingerprint,
            endpoint,
            duration_ms,
            fixture_status,
            compile_clean,
        )
        .with_success(
            response.usage.input_tokens,
            response.usage.output_tokens,
            usd,
        )
    }

    /// Construct a trajectory row for a provider error.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn from_provider_error(
        fixture_id: FixtureId,
        set: FixtureSet,
        turn_id: FixtureTurnId,
        request_fingerprint: RequestFingerprint,
        endpoint: &ModelEndpoint,
        error: &ProviderError,
        duration_ms: Option<u64>,
        fixture_status: FixtureStatus,
        compile_clean: Option<bool>,
    ) -> Self {
        let mut row = Self::base(
            fixture_id,
            set,
            turn_id,
            request_fingerprint,
            endpoint,
            duration_ms,
            fixture_status,
            compile_clean,
        );
        row.error_class = Some(classify_provider_error(error).class);
        row
    }

    /// Construct a trajectory row for cancellation after dispatch.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn cancelled(
        fixture_id: FixtureId,
        set: FixtureSet,
        turn_id: FixtureTurnId,
        request_fingerprint: RequestFingerprint,
        endpoint: &ModelEndpoint,
        duration_ms: Option<u64>,
        fixture_status: FixtureStatus,
        compile_clean: Option<bool>,
    ) -> Self {
        let mut row = Self::base(
            fixture_id,
            set,
            turn_id,
            request_fingerprint,
            endpoint,
            duration_ms,
            fixture_status,
            compile_clean,
        );
        row.error_class = Some(ErrorClass::Cancelled);
        row
    }

    #[allow(clippy::too_many_arguments)]
    fn base(
        fixture_id: FixtureId,
        set: FixtureSet,
        turn_id: FixtureTurnId,
        request_fingerprint: RequestFingerprint,
        endpoint: &ModelEndpoint,
        duration_ms: Option<u64>,
        fixture_status: FixtureStatus,
        compile_clean: Option<bool>,
    ) -> Self {
        let request_content_hash = request_fingerprint.as_digest().clone();
        Self {
            fixture_id,
            set,
            turn_id,
            request_fingerprint,
            request_content_hash,
            endpoint_id: endpoint.id.clone(),
            provider_id: endpoint.provider.clone(),
            model_tier: ModelTier::Standard,
            input_tokens: None,
            output_tokens: None,
            usd: None,
            usd_source: None,
            duration_ms,
            confidence: None,
            error_class: None,
            complete_ok: false,
            fixture_status,
            compile_clean,
        }
    }

    fn with_success(
        mut self,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        usd: Option<f64>,
    ) -> Self {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.usd = usd;
        self.usd_source = usd.map(|_| ModelUsdSource::OperatorPriceTable);
        self.complete_ok = true;
        self
    }
}

impl EvalReport {
    /// Group retained control trajectories by an exact key extractor.
    pub fn group_trajectories_by<K, F>(&self, key: F) -> BTreeMap<K, Vec<&EvalTrajectoryRecord>>
    where
        K: Ord,
        F: Fn(&EvalTrajectoryRecord) -> K,
    {
        let mut grouped: BTreeMap<K, Vec<&EvalTrajectoryRecord>> = BTreeMap::new();
        for row in &self.trajectories {
            grouped.entry(key(row)).or_default().push(row);
        }
        grouped
    }
}

/// Stable-sort trajectories by fixture id and turn ordinal.
pub fn sort_trajectories_stable(rows: &mut [EvalTrajectoryRecord]) {
    rows.sort_by(|left, right| {
        left.fixture_id
            .as_str()
            .cmp(right.fixture_id.as_str())
            .then_with(|| left.turn_id.ordinal.cmp(&right.turn_id.ordinal))
    });
}

/// Write control trajectories as JSONL and rotate retained run directories.
pub fn write_trajectory_artifacts(
    report: &EvalReport,
    artifact_dir: Option<&Path>,
    max_retained_runs: u32,
) -> Result<(), EvalError> {
    validate_run_id(&report.run_id)?;
    if max_retained_runs == 0 {
        return Err(EvalError::Manifest(
            "max_retained_runs must be at least 1".to_owned(),
        ));
    }

    let Some(artifact_dir) = artifact_dir else {
        return Ok(());
    };

    fs::create_dir_all(artifact_dir)?;
    let run_dir = artifact_dir.join(report.run_id.as_str());
    match fs::symlink_metadata(&run_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(EvalError::Io(std::io::Error::other(
                "trajectory run directory is a symlink",
            )));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(EvalError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "trajectory run path is not a directory",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&run_dir)?;
        }
        Err(error) => return Err(EvalError::Io(error)),
    }

    let jsonl_path = run_dir.join("trajectories.jsonl");
    match fs::symlink_metadata(&jsonl_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(EvalError::Io(std::io::Error::other(
                "trajectory jsonl is a symlink",
            )));
        }
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(EvalError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "trajectory jsonl path is not a file",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(EvalError::Io(error)),
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&jsonl_path)?;
    for row in &report.trajectories {
        let line =
            serde_json::to_string(row).map_err(|error| EvalError::Json(error.to_string()))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    drop(file);

    rotate_runs(artifact_dir, &report.run_id, max_retained_runs)
}

fn rotate_runs(
    artifact_dir: &Path,
    current_run_id: &str,
    max_retained_runs: u32,
) -> Result<(), EvalError> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(artifact_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_valid_run_id(name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, name.to_owned(), entry.path()));
    }

    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    while candidates.len() > max_retained_runs as usize {
        let Some(index) = candidates
            .iter()
            .position(|(_, name, _)| name != current_run_id)
        else {
            break;
        };
        let (_, _, path) = candidates.remove(index);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> Result<(), EvalError> {
    if is_valid_run_id(run_id) {
        Ok(())
    } else {
        Err(EvalError::Manifest(
            "invalid run_id for artifacts".to_owned(),
        ))
    }
}

fn is_valid_run_id(run_id: &str) -> bool {
    let bytes = run_id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().copied().enumerate() {
        match idx {
            8 | 13 | 18 | 23 if byte == b'-' => {}
            14 if byte == b'4' => {}
            19 if matches!(byte, b'8' | b'9' | b'a' | b'b') => {}
            8 | 13 | 18 | 23 => return false,
            _ if byte.is_ascii_digit() || matches!(byte, b'a'..=b'f') => {}
            _ => return false,
        }
    }
    uuid::Uuid::parse_str(run_id)
        .map(|uuid| uuid.to_string() == run_id)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_claim::CostClaimEnvelope;
    use crate::manifest::{FixtureSet, ToolchainRecord};
    use crate::metrics::{EvalMetrics, MetricField, UnmeasuredReason};
    use alloy_runtime::{CapabilityId, CompletionRequest, EndpointId, ProviderId};

    fn turn(ordinal: u32) -> FixtureTurnId {
        FixtureTurnId {
            capability: CapabilityId::new("repair").unwrap(),
            node: None,
            ordinal,
        }
    }

    fn endpoint() -> ModelEndpoint {
        ModelEndpoint {
            id: EndpointId::new("eval-script").unwrap(),
            provider: ProviderId::new("eval-script").unwrap(),
            display_name: "eval".to_owned(),
            model: "scripted".to_owned(),
            tiers: vec![ModelTier::Standard],
            supports_tools: false,
            supports_structured_output: false,
            max_context: 8192,
            input_usd_per_mtok: Some(1.0),
            output_usd_per_mtok: Some(1.0),
        }
    }

    fn fingerprint() -> RequestFingerprint {
        RequestFingerprint::of(&CompletionRequest {
            messages: vec![],
            tools: vec![],
            tool_choice: alloy_runtime::ToolChoice::None,
            response_format: alloy_runtime::ResponseFormat::Text,
            temperature: None,
            max_output_tokens: None,
        })
    }

    fn row(id: &str, ordinal: u32) -> EvalTrajectoryRecord {
        EvalTrajectoryRecord::cancelled(
            FixtureId::new(id).unwrap(),
            FixtureSet::Train,
            turn(ordinal),
            fingerprint(),
            &endpoint(),
            Some(1),
            FixtureStatus::Error,
            None,
        )
    }

    fn report(run_id: &str, trajectories: Vec<EvalTrajectoryRecord>) -> EvalReport {
        EvalReport {
            schema_version: 1,
            run_id: run_id.to_owned(),
            offline: true,
            toolchain: ToolchainRecord {
                channel: "1.97.1".to_owned(),
                rustc_version: "none".to_owned(),
                cargo_version: "none".to_owned(),
            },
            fixtures: vec![],
            trajectories,
            naive_fixtures: None,
            naive_trajectories: None,
            metrics: EvalMetrics::empty(),
            cost_claim: CostClaimEnvelope::uncalibrated(MetricField::Unmeasured {
                reason: UnmeasuredReason::CostInputsIncomplete,
            }),
            gate: None,
            naive_comparison: None,
        }
    }

    #[test]
    fn trajectory_sort_key_is_pinned() {
        let mut rows = vec![row("b", 0), row("a", 2), row("a", 1), row("a", 1)];
        sort_trajectories_stable(&mut rows);
        assert_eq!(rows[0].fixture_id.as_str(), "a");
        assert_eq!(rows[0].turn_id.ordinal, 1);
        assert_eq!(rows[1].fixture_id.as_str(), "a");
        assert_eq!(rows[1].turn_id.ordinal, 1);
        assert_eq!(rows[2].turn_id.ordinal, 2);
        assert_eq!(rows[3].fixture_id.as_str(), "b");
    }

    #[test]
    fn trajectories_survive_batch_and_group() {
        let report = report(
            "00000000-0000-4000-8000-000000000000",
            vec![row("a", 0), row("a", 1), row("b", 0)],
        );
        let grouped = report.group_trajectories_by(|row| row.fixture_id.as_str().to_owned());
        assert_eq!(grouped["a"].len(), 2);
        assert_eq!(grouped["b"].len(), 1);
    }

    #[test]
    fn trajectory_jsonl_contract() {
        let dir = tempfile::tempdir().unwrap();
        let report = report("00000000-0000-4000-8000-000000000000", vec![row("a", 0)]);
        write_trajectory_artifacts(&report, Some(dir.path()), 2).unwrap();
        let path = dir
            .path()
            .join("00000000-0000-4000-8000-000000000000")
            .join("trajectories.jsonl");
        let data = std::fs::read_to_string(path).unwrap();
        assert_eq!(data.lines().count(), 1);
        assert!(data.ends_with('\n'));
    }

    #[test]
    fn trajectory_artifact_run_id_validation() {
        let invalid = report("../bad", vec![]);
        assert!(matches!(
            write_trajectory_artifacts(&invalid, None, 2),
            Err(EvalError::Manifest(message)) if message == "invalid run_id for artifacts"
        ));
        let valid = report("00000000-0000-4000-8000-000000000000", vec![]);
        assert!(write_trajectory_artifacts(&valid, None, 2).is_ok());
    }
}
