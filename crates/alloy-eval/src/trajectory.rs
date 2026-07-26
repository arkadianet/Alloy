//! Eval-local trajectory retention and optional JSONL artifacts.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

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
pub(crate) fn sort_trajectories_stable(rows: &mut [EvalTrajectoryRecord]) {
    rows.sort_by(|left, right| {
        left.fixture_id
            .as_str()
            .cmp(right.fixture_id.as_str())
            .then_with(|| left.turn_id.ordinal.cmp(&right.turn_id.ordinal))
    });
}

/// Write control trajectories as JSONL and rotate retained run directories.
pub(crate) fn write_trajectory_artifacts(
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
    ensure_run_directory(&run_dir)?;

    let jsonl_path = run_dir.join("trajectories.jsonl");
    ensure_jsonl_target(&jsonl_path)?;
    write_jsonl_atomically(&run_dir, &jsonl_path, report)?;

    rotate_runs(artifact_dir, &report.run_id, max_retained_runs)
}

fn ensure_run_directory(run_dir: &Path) -> Result<(), EvalError> {
    match fs::symlink_metadata(run_dir) {
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
            fs::create_dir(run_dir)?;
        }
        Err(error) => return Err(EvalError::Io(error)),
    }
    Ok(())
}

fn ensure_jsonl_target(jsonl_path: &Path) -> Result<(), EvalError> {
    match fs::symlink_metadata(jsonl_path) {
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
    Ok(())
}

fn write_jsonl_atomically(
    run_dir: &Path,
    jsonl_path: &Path,
    report: &EvalReport,
) -> Result<(), EvalError> {
    let temp_name = format!(
        ".trajectories.jsonl.{}.tmp",
        uuid::Uuid::new_v4().hyphenated()
    );
    let temp_path = run_dir.join(temp_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;

    if let Err(error) = ensure_run_directory(run_dir) {
        drop(file);
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    let write_result = (|| {
        for row in &report.trajectories {
            let line =
                serde_json::to_string(row).map_err(|error| EvalError::Json(error.to_string()))?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        Ok(())
    })();
    drop(file);

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = ensure_run_directory(run_dir).and_then(|()| ensure_jsonl_target(jsonl_path))
    {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    // Path-based std APIs leave a residual parent-directory replacement race before rename.
    if let Err(error) = fs::rename(&temp_path, jsonl_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(EvalError::Io(error));
    }
    Ok(())
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
        let modified = metadata.modified()?;
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
    if bytes[14] != b'4' || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b') {
        return false;
    }
    for (idx, byte) in bytes.iter().copied().enumerate() {
        match idx {
            8 | 13 | 18 | 23 if byte == b'-' => {}
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
        let run_id = "00000000-0000-4000-8000-000000000000";
        let first = report(run_id, vec![row("a", 0), row("b", 0)]);
        write_trajectory_artifacts(&first, Some(dir.path()), 2).unwrap();
        let second = report(run_id, vec![row("c", 0)]);
        write_trajectory_artifacts(&second, Some(dir.path()), 2).unwrap();
        let path = dir.path().join(run_id).join("trajectories.jsonl");
        let data = std::fs::read_to_string(path).unwrap();
        assert_eq!(data.lines().count(), 1);
        assert!(data.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(data.trim_end()).unwrap();
        assert_eq!(value["fixture_id"], "c");
    }

    #[test]
    fn trajectory_artifact_run_id_validation() {
        for run_id in [
            "00000000-0000-1000-8000-000000000000",
            "00000000-0000-4000-7000-000000000000",
            "00000000-0000-4000-8000-00000000000A",
            "{00000000-0000-4000-8000-000000000000}",
            ".",
            "..",
            "../00000000-0000-4000-8000-000000000000",
            "00000000-0000-4000-8000-000000000000/child",
        ] {
            let invalid = report(run_id, vec![]);
            assert!(
                matches!(
                    write_trajectory_artifacts(&invalid, None, 2),
                    Err(EvalError::Manifest(message)) if message == "invalid run_id for artifacts"
                ),
                "{run_id} must be rejected"
            );
        }
        let valid = report("00000000-0000-4000-8000-000000000000", vec![]);
        assert!(write_trajectory_artifacts(&valid, None, 2).is_ok());
    }

    #[test]
    fn trajectory_disk_rotation_ignores_non_uuid_directories() {
        let dir = tempfile::tempdir().unwrap();
        let old_run_ids = [
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
        ];
        for run_id in old_run_ids {
            std::fs::create_dir(dir.path().join(run_id)).unwrap();
        }
        let ignored = dir.path().join("not-a-run");
        std::fs::create_dir(&ignored).unwrap();

        let current_run_id = "00000000-0000-4000-8000-000000000003";
        write_trajectory_artifacts(&report(current_run_id, vec![]), Some(dir.path()), 1).unwrap();

        for run_id in old_run_ids {
            assert!(!dir.path().join(run_id).exists());
        }
        assert!(ignored.is_dir());
        assert!(dir.path().join(current_run_id).is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn trajectory_artifact_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let run_id = "00000000-0000-4000-8000-000000000000";
        symlink(outside.path(), dir.path().join(run_id)).unwrap();

        assert!(matches!(
            write_trajectory_artifacts(&report(run_id, vec![row("a", 0)]), Some(dir.path()), 2),
            Err(EvalError::Io(_))
        ));
        assert!(!outside.path().join("trajectories.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn trajectory_jsonl_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let run_id = "00000000-0000-4000-8000-000000000000";
        let run_dir = dir.path().join(run_id);
        std::fs::create_dir(&run_dir).unwrap();
        let outside_jsonl = outside.path().join("trajectories.jsonl");
        std::fs::write(&outside_jsonl, "do not overwrite\n").unwrap();
        symlink(&outside_jsonl, run_dir.join("trajectories.jsonl")).unwrap();

        assert!(matches!(
            write_trajectory_artifacts(&report(run_id, vec![row("a", 0)]), Some(dir.path()), 2),
            Err(EvalError::Io(_))
        ));
        assert_eq!(
            std::fs::read_to_string(outside_jsonl).unwrap(),
            "do not overwrite\n"
        );
    }

    #[test]
    fn trajectories_omit_prompt_and_response_bodies() {
        let value = serde_json::to_value(row("a", 0)).unwrap();
        let keys: std::collections::BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected = std::collections::BTreeSet::from([
            "compile_clean",
            "complete_ok",
            "confidence",
            "duration_ms",
            "endpoint_id",
            "error_class",
            "fixture_id",
            "fixture_status",
            "input_tokens",
            "model_tier",
            "output_tokens",
            "provider_id",
            "request_content_hash",
            "request_fingerprint",
            "set",
            "turn_id",
            "usd",
            "usd_source",
        ]);
        assert_eq!(keys, expected);
    }
}
