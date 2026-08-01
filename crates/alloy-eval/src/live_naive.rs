//! One-shot, tool-free naive replacement driver (E1 three-arm holdout, arm B).
//!
//! Exactly one model completion: no tools, no repository index, no
//! replanning, no retry. This module stays pure — request construction,
//! reply validation, and the target-file write — so it is fully testable
//! without a network. `bin/alloy-eval-live-naive.rs` owns the one network
//! call and the only read of `ALLOY_API_KEY`.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use alloy_runtime::{
    ChatMessage, ChatRole, CompletionRequest, ModelResponse, ProviderError, ResponseFormat,
    ToolChoice,
};
use serde::{Deserialize, Serialize};

use crate::byom_preflight::{ProviderFailure, WireMode};

/// Wire schema name carried on `response_format.json_schema.name`.
pub const NAIVE_SCHEMA_NAME: &str = "alloy_naive_replacement";
/// Replacement content must not exceed this many bytes.
pub const MAX_REPLACEMENT_BYTES: usize = 1024 * 1024;
/// Default whole-request deadline for the one permitted call.
///
/// Matches the Alloy arm's rendered `router.toml` deadline so a slow but
/// healthy model is not aborted by the baseline arm alone. The previous
/// hardcoded 120 s was shorter than the shell harness's 600 s budget, so a
/// slow 30B answer surfaced as "telemetry incomplete" instead of a timeout.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = crate::live_repair::LIVE_REPAIR_REQUEST_TIMEOUT_MS;
/// Default connect deadline for the one permitted call.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;

const SYSTEM_PROMPT: &str = "You are given one Rust source file to repair. \
You have exactly one attempt: no tools, no repository access beyond what is \
shown below, no retries, and no follow-up turns. Reply with only the JSON \
object the schema requires, containing the full replacement file contents.";

/// The model's single-attempt replacement for the target file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NaiveReplacement {
    /// Full replacement contents for the target file.
    pub replacement: String,
}

/// Bounded telemetry recorded from the one permitted model call.
///
/// One record is written for **every** attempt that reached the provider,
/// successful or not. A timeout is a spent call, so it is reported as a
/// timeout with its stage rather than as missing telemetry (E2 §d).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NaiveRunTelemetry {
    /// Always 1: exactly one completion is permitted, and an attempt that
    /// timed out still consumed it.
    pub model_calls: u32,
    /// Provider-reported input token count. `None` means unknown — the
    /// provider omitted or malformed it — and is never rendered as zero.
    pub tokens_in: Option<u64>,
    /// Provider-reported output token count. `None` means unknown.
    pub tokens_out: Option<u64>,
    /// Redacted, bounded provider request identifier.
    pub provider_request_id: Option<String>,
    /// Redacted, bounded provider finish reason.
    pub finish_reason: Option<String>,
    /// Wire `response_format` mode this attempt actually used.
    #[serde(default = "default_wire_mode")]
    pub wire_mode: String,
    /// Whole-request deadline the attempt ran under, in milliseconds.
    #[serde(default)]
    pub request_timeout_ms: u64,
    /// Populated when the single call failed; `None` on success.
    #[serde(default)]
    pub failure: Option<ProviderFailure>,
}

fn default_wire_mode() -> String {
    WireMode::JsonSchema.as_str().to_owned()
}

impl NaiveRunTelemetry {
    /// Record the one successful call.
    #[must_use]
    pub fn from_response(response: &ModelResponse, request_timeout: Duration) -> Self {
        Self {
            model_calls: 1,
            tokens_in: response.usage.input_tokens,
            tokens_out: response.usage.output_tokens,
            provider_request_id: response.provider_request_id.clone(),
            finish_reason: response.finish_reason.clone(),
            wire_mode: default_wire_mode(),
            request_timeout_ms: millis(request_timeout),
            failure: None,
        }
    }

    /// Record the one call that failed.
    ///
    /// Usage stays unknown: a failed attempt reports no tokens, and inventing
    /// zeros would read as a measured zero-token call.
    #[must_use]
    pub fn from_failure(error: &ProviderError, request_timeout: Duration) -> Self {
        Self {
            model_calls: 1,
            tokens_in: None,
            tokens_out: None,
            provider_request_id: None,
            finish_reason: None,
            wire_mode: default_wire_mode(),
            request_timeout_ms: millis(request_timeout),
            failure: Some(ProviderFailure::classify(error)),
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn replacement_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "replacement": { "type": "string", "minLength": 1 }
        },
        "required": ["replacement"],
        "additionalProperties": false
    })
}

/// Build the pure, tool-free one-shot completion request.
///
/// Only `goal`, `target_path`, `target_source`, and `diagnostics` reach the
/// prompt — no hidden oracle inputs (`.post` reference files,
/// `oracle-tests/`) are ever consulted here.
///
/// # Errors
///
/// Returns `Err` when `goal` or `target_path` is empty, or `temperature` is
/// not finite.
pub fn build_naive_request(
    goal: &str,
    target_path: &str,
    target_source: &str,
    diagnostics: &str,
    temperature: f64,
) -> Result<CompletionRequest, String> {
    if goal.trim().is_empty() {
        return Err("goal must not be empty".to_owned());
    }
    if target_path.trim().is_empty() {
        return Err("target path must not be empty".to_owned());
    }
    if !temperature.is_finite() {
        return Err("temperature must be finite".to_owned());
    }
    let user_prompt = format!(
        "Goal: {goal}\n\n\
         Target file: {target_path}\n\n\
         Current contents:\n```rust\n{target_source}\n```\n\n\
         Compiler diagnostics:\n{diagnostics}\n"
    );
    Ok(CompletionRequest {
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: SYSTEM_PROMPT.to_owned(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: user_prompt,
            },
        ],
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: ResponseFormat::JsonSchema {
            name: NAIVE_SCHEMA_NAME.to_owned(),
            schema: replacement_schema(),
        },
        temperature: Some(temperature as f32),
        max_output_tokens: None,
    })
}

fn validate_replacement_bytes(replacement: &str) -> Result<(), String> {
    if replacement.is_empty() {
        return Err("replacement must not be empty".to_owned());
    }
    if replacement.len() > MAX_REPLACEMENT_BYTES {
        return Err(format!(
            "replacement must not exceed {MAX_REPLACEMENT_BYTES} bytes, got {}",
            replacement.len()
        ));
    }
    Ok(())
}

/// Parse and validate the model's structured reply.
///
/// # Errors
///
/// Returns `Err` when `content` does not match [`NaiveReplacement`], or the
/// replacement is empty or exceeds [`MAX_REPLACEMENT_BYTES`].
pub fn parse_replacement(content: &str) -> Result<NaiveReplacement, String> {
    let parsed: NaiveReplacement =
        serde_json::from_str(content).map_err(|error| format!("parse model reply: {error}"))?;
    validate_replacement_bytes(&parsed.replacement)?;
    Ok(parsed)
}

/// Resolve `target` inside `workspace`, rejecting absolute paths, parent
/// traversal, and symlink targets.
///
/// # Errors
///
/// Returns `Err` describing the rejected path.
pub fn resolve_target(workspace: &Path, target: &str) -> Result<PathBuf, String> {
    let relative = Path::new(target);
    if target.is_empty() || relative.is_absolute() {
        return Err(format!(
            "target must be a non-empty relative path, got {target:?}"
        ));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "target must not traverse parent directories, got {target:?}"
        ));
    }
    let resolved = workspace.join(relative);
    if resolved
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!("target must not be a symlink: {target:?}"));
    }
    Ok(resolved)
}

/// Write `replacement` into `workspace`/`target` through a sibling temp file
/// followed by `rename`, so a crash never leaves a partially written target.
///
/// Resolves `target` itself; callers that already hold a path from
/// [`resolve_target`] (for example because they read the target before
/// writing it) should call [`write_resolved_replacement`] instead, so the
/// same path is validated once and reused for both operations.
///
/// # Errors
///
/// Returns `Err` for a rejected target (see [`resolve_target`]), an empty or
/// oversized replacement, or an I/O failure.
pub fn write_replacement(workspace: &Path, target: &str, replacement: &str) -> Result<(), String> {
    let resolved = resolve_target(workspace, target)?;
    write_resolved_replacement(&resolved, replacement)
}

/// Write `replacement` to an already-[`resolve_target`]-validated path
/// through a sibling temp file followed by `rename`.
///
/// # Errors
///
/// Returns `Err` for an empty or oversized replacement, or an I/O failure.
pub fn write_resolved_replacement(resolved: &Path, replacement: &str) -> Result<(), String> {
    validate_replacement_bytes(replacement)?;
    let parent = resolved
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| format!("target has no parent directory: {}", resolved.display()))?;
    let file_name = resolved
        .file_name()
        .ok_or_else(|| format!("target has no file name: {}", resolved.display()))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".alloy-naive.tmp");
    let tmp_path = parent.join(tmp_name);
    fs::write(&tmp_path, replacement)
        .map_err(|error| format!("write {}: {error}", tmp_path.display()))?;
    fs::rename(&tmp_path, resolved).map_err(|error| {
        format!(
            "rename {} -> {}: {error}",
            tmp_path.display(),
            resolved.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_and_schema_expose_no_oracle_inputs() {
        let request = build_naive_request(
            "fix the compile error",
            "src/lib.rs",
            "pub fn broken() { missing }",
            "error[E0425]: cannot find value `missing`",
            0.6,
        )
        .unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains(".post"));
        assert!(!encoded.contains("oracle-tests"));
        assert!(encoded.contains("src/lib.rs"));
    }

    #[test]
    fn request_carries_no_tools_and_the_naive_schema() {
        let request = build_naive_request("g", "src/lib.rs", "src", "diag", 0.6).unwrap();
        assert!(request.tools.is_empty());
        assert_eq!(request.tool_choice, ToolChoice::None);
        match &request.response_format {
            ResponseFormat::JsonSchema { name, schema } => {
                assert_eq!(name, NAIVE_SCHEMA_NAME);
                assert_eq!(schema["required"], serde_json::json!(["replacement"]));
                assert_eq!(schema["additionalProperties"], false);
            }
            other => panic!("expected JsonSchema response format, got {other:?}"),
        }
    }

    #[test]
    fn build_naive_request_rejects_empty_goal_target_or_nonfinite_temperature() {
        assert!(build_naive_request("", "src/lib.rs", "s", "d", 0.6).is_err());
        assert!(build_naive_request("g", "", "s", "d", 0.6).is_err());
        assert!(build_naive_request("g", "src/lib.rs", "s", "d", f64::NAN).is_err());
    }

    #[test]
    fn parse_replacement_rejects_empty_and_oversized_content() {
        assert!(parse_replacement(r#"{"replacement":""}"#).is_err());
        let huge = "a".repeat(MAX_REPLACEMENT_BYTES + 1);
        let payload = serde_json::to_string(&NaiveReplacement { replacement: huge }).unwrap();
        assert!(parse_replacement(&payload).is_err());
        assert_eq!(
            parse_replacement(r#"{"replacement":"ok\n"}"#).unwrap(),
            NaiveReplacement {
                replacement: "ok\n".to_owned()
            }
        );
    }

    #[test]
    fn resolve_target_rejects_absolute_traversal_empty_and_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_target(dir.path(), "/etc/passwd").is_err());
        assert!(resolve_target(dir.path(), "../escape.rs").is_err());
        assert!(resolve_target(dir.path(), "").is_err());

        fs::write(dir.path().join("real.rs"), "x").unwrap();
        assert!(resolve_target(dir.path(), "real.rs").is_ok());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("real.rs"), dir.path().join("link.rs"))
                .unwrap();
            assert!(resolve_target(dir.path(), "link.rs").is_err());
        }
    }

    #[test]
    fn write_replacement_replaces_via_rename_with_no_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "old").unwrap();
        write_replacement(dir.path(), "lib.rs", "new\n").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            "new\n"
        );
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp"));
        assert!(!leftover, "no sibling temp file must remain");
    }

    /// E2 (b) — a provider that omits usage must leave `tokens_in` /
    /// `tokens_out` null all the way into the result file. A zero there would
    /// be read as a measured zero-token call.
    #[test]
    fn absent_usage_stays_null_in_the_result_file() {
        let response = ModelResponse {
            text: Some("{\"replacement\":\"x\"}".to_owned()),
            structured: None,
            tool_calls: vec![],
            usage: alloy_runtime::Usage {
                input_tokens: None,
                output_tokens: None,
            },
            provider_request_id: None,
            finish_reason: Some("stop".to_owned()),
        };
        let telemetry = NaiveRunTelemetry::from_response(&response, Duration::from_secs(600));
        assert_eq!(telemetry.model_calls, 1);
        assert_eq!(telemetry.tokens_in, None);
        assert_eq!(telemetry.tokens_out, None);
        assert_eq!(telemetry.request_timeout_ms, 600_000);
        assert!(telemetry.failure.is_none());

        let json = serde_json::to_value(&telemetry).unwrap();
        assert!(json["tokens_in"].is_null());
        assert!(json["tokens_out"].is_null());
        assert_eq!(json["wire_mode"], "json_schema");

        // A provider that genuinely reports usage keeps it verbatim.
        let measured = ModelResponse {
            usage: alloy_runtime::Usage {
                input_tokens: Some(0),
                output_tokens: Some(7),
            },
            ..response
        };
        let telemetry = NaiveRunTelemetry::from_response(&measured, Duration::from_secs(600));
        assert_eq!(telemetry.tokens_in, Some(0));
        assert_eq!(telemetry.tokens_out, Some(7));
    }

    /// E2 (d) — a timed-out attempt still spent the one permitted call, so it
    /// is recorded as one call that timed out, naming the stage. Reporting it
    /// as missing telemetry would blame the harness for a provider deadline.
    #[test]
    fn timeout_is_recorded_as_a_spent_call_with_its_stage() {
        let telemetry = NaiveRunTelemetry::from_failure(
            &ProviderError::TimeoutAt {
                stage: alloy_runtime::TimeoutStage::Read,
            },
            Duration::from_millis(600_000),
        );
        assert_eq!(
            telemetry.model_calls, 1,
            "the call was spent even though it produced nothing"
        );
        assert_eq!(telemetry.tokens_in, None);
        assert_eq!(telemetry.tokens_out, None);
        let failure = telemetry.failure.as_ref().expect("failure is recorded");
        assert_eq!(failure.kind, "timeout");
        assert_eq!(failure.error_class, "timeout");
        assert_eq!(failure.timeout_stage.as_deref(), Some("read"));
        assert_eq!(telemetry.request_timeout_ms, 600_000);

        // The document round-trips, and remains parseable by the holdout
        // reader, which requires exactly one model call.
        let text = serde_json::to_string(&telemetry).unwrap();
        let parsed: NaiveRunTelemetry = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, telemetry);
        let loose: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(loose["model_calls"], 1);

        // An HTTP failure keeps its status instead of collapsing to a timeout.
        let http = NaiveRunTelemetry::from_failure(
            &ProviderError::HttpStatus {
                status: 503,
                message: "unavailable".to_owned(),
            },
            Duration::from_secs(600),
        );
        let failure = http.failure.as_ref().unwrap();
        assert_eq!(failure.kind, "http_status");
        assert_eq!(failure.http_status, Some(503));
        assert_eq!(failure.timeout_stage, None);
    }

    /// The naive arm's default deadline must not be tighter than the Alloy
    /// arm's, or the baseline aborts on responses the agent arm tolerates.
    #[test]
    fn default_request_timeout_matches_the_alloy_arm() {
        assert_eq!(
            DEFAULT_REQUEST_TIMEOUT_MS,
            crate::LIVE_REPAIR_REQUEST_TIMEOUT_MS
        );
        const { assert!(DEFAULT_CONNECT_TIMEOUT_MS < DEFAULT_REQUEST_TIMEOUT_MS) };
    }

    #[test]
    fn write_replacement_rejects_empty_and_oversized_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "old").unwrap();
        assert!(write_replacement(dir.path(), "lib.rs", "").is_err());
        let huge = "a".repeat(MAX_REPLACEMENT_BYTES + 1);
        assert!(write_replacement(dir.path(), "lib.rs", &huge).is_err());
        // Rejected writes must not touch the original file.
        assert_eq!(
            fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            "old"
        );
    }
}
