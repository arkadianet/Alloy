//! Offline replay of recorded `cargo check --message-format=json` output.

use std::path::Path;

use alloy_runtime::Digest;
use serde::{Deserialize, Serialize};

use crate::error::{bound_message, EvalError};
use crate::manifest::{ExpectedDiagnostic, ToolchainRecord};

/// Cargo JSON recording schema version accepted by RFC-0016.
pub const CARGO_RECORDING_FORMAT_VERSION: u32 = 1;

/// Recorded `cargo check --message-format=json` stream and capture metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoJsonRecording {
    /// Recording file format version; must be [`CARGO_RECORDING_FORMAT_VERSION`].
    pub recording_format_version: u32,
    /// Toolchain used when the recording was captured.
    pub toolchain: ToolchainRecord,
    /// Exact argv conceptually recorded.
    pub argv: Vec<String>,
    /// Process exit code from the capture.
    pub exit_code: i32,
    /// Raw newline-delimited JSON lines from Cargo stdout.
    pub stdout_lines: Vec<String>,
    /// Optional stderr capture.
    #[serde(default)]
    pub stderr: String,
    /// SHA-256 of `stdout_lines.join("\n")`.
    pub content_digest: Digest,
}

/// Diagnostic extracted from a Cargo `compiler-message` JSON line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedDiagnostic {
    /// Optional Rust error code such as `E0502`.
    pub code: Option<String>,
    /// Cargo/rustc diagnostic level, for example `error`.
    pub level: String,
    /// Human-readable diagnostic message.
    pub message: String,
}

impl CargoJsonRecording {
    /// Load a recording JSON file and validate its format version and digest.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Io`] for read failures and
    /// [`EvalError::RecordingInvalid`] for JSON, version, or digest failures.
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        let bytes = std::fs::read(path)?;
        let recording: Self = serde_json::from_slice(&bytes).map_err(|err| {
            EvalError::RecordingInvalid(bound_message(format!("recording json: {err}")))
        })?;
        if recording.recording_format_version != CARGO_RECORDING_FORMAT_VERSION {
            return Err(EvalError::RecordingInvalid(format!(
                "recording format version must be {CARGO_RECORDING_FORMAT_VERSION}"
            )));
        }
        let digest = Digest::sha256(recording.stdout_lines.join("\n").as_bytes());
        if digest != recording.content_digest {
            return Err(EvalError::RecordingInvalid(
                "recording content digest mismatch".into(),
            ));
        }
        Ok(recording)
    }

    /// Validate this recording against the harness toolchain channel pin.
    ///
    /// Failure: [`EvalError::RecordingStale`] when the recording channel differs
    /// from `pin_channel`.
    pub fn validate_against_pin(&self, pin_channel: &str) -> Result<(), EvalError> {
        if self.toolchain.channel != pin_channel {
            return Err(EvalError::RecordingStale(format!(
                "recording channel {} does not match pin {pin_channel}",
                self.toolchain.channel
            )));
        }
        Ok(())
    }

    /// Parse all Cargo JSON lines and extract compiler diagnostics.
    ///
    /// Every line must be valid JSON before message classification. Malformed
    /// lines and malformed `compiler-message` payloads are
    /// [`EvalError::RecordingInvalid`].
    pub fn diagnostics(&self) -> Result<Vec<RecordedDiagnostic>, EvalError> {
        let mut diagnostics = Vec::new();
        for (idx, line) in self.stdout_lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
                EvalError::RecordingInvalid(bound_message(format!(
                    "malformed cargo json line {idx}: {err}"
                )))
            })?;
            if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
                continue;
            }
            let message = value.get("message").ok_or_else(|| {
                EvalError::RecordingInvalid(bound_message(format!(
                    "compiler-message line {idx} missing message"
                )))
            })?;
            let level = required_string(message, "level", idx)?.to_owned();
            let message_text = required_string(message, "message", idx)?.to_owned();
            let code = match message.get("code") {
                None | Some(serde_json::Value::Null) => None,
                Some(code) => Some(required_string(code, "code", idx)?.to_owned()),
            };
            diagnostics.push(RecordedDiagnostic {
                code,
                level,
                message: message_text,
            });
        }
        Ok(diagnostics)
    }

    /// Return whether this recording represents a clean compile.
    ///
    /// Diagnostics are parsed first, so malformed NDJSON is
    /// [`EvalError::RecordingInvalid`] even when `exit_code == 0`.
    ///
    /// Research §7.11 item 5: the pass/fail decision is
    /// [`alloy_runtime::cargo_exit_verdict`] — the same function the
    /// runtime's verify adapters use — so this crate and the runtime can no
    /// longer disagree about what "compiled" means.
    pub fn compile_clean(&self) -> Result<bool, EvalError> {
        Ok(matches!(
            self.verdict()?,
            alloy_runtime::VerdictOutcome::Pass
        ))
    }

    /// Three-valued verdict for this recording via the shared runtime
    /// decision (§7.11 items 5/6).
    pub fn verdict(&self) -> Result<alloy_runtime::VerdictOutcome, EvalError> {
        let diagnostics = self.diagnostics()?;
        let has_errors = diagnostics.iter().any(|diag| diag.level == "error");
        Ok(alloy_runtime::cargo_exit_verdict(
            Some(i64::from(self.exit_code)),
            has_errors,
            // Recordings are cargo-check JSON: a genuine compile failure
            // always carries error diagnostics, so a bare 101 is cargo's
            // own failure — Inconclusive, same as the runtime adapter.
            true,
        ))
    }
}

pub(crate) fn validate_expected_diagnostics(
    recording: &CargoJsonRecording,
    expected: &[ExpectedDiagnostic],
) -> Result<(), EvalError> {
    let diagnostics = recording.diagnostics()?;
    for expected in expected {
        let present = diagnostics.iter().any(|actual| {
            actual.code.as_deref() == Some(expected.code.as_str())
                && actual.message.contains(&expected.message_contains)
        });
        if !present {
            return Err(EvalError::RecordingInvalid(bound_message(format!(
                "missing expected diagnostic {} containing {:?}",
                expected.code, expected.message_contains
            ))));
        }
    }
    Ok(())
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
    line: usize,
) -> Result<&'a str, EvalError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            EvalError::RecordingInvalid(bound_message(format!(
                "compiler-message line {line} field {field} must be a string"
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn toolchain(channel: &str) -> ToolchainRecord {
        ToolchainRecord {
            channel: channel.into(),
            rustc_version: "rustc 1.97.1".into(),
            cargo_version: "cargo 1.97.1".into(),
        }
    }

    fn line(level: &str, code: Option<&str>, message: &str) -> String {
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

    fn recording(exit_code: i32, stdout_lines: Vec<String>) -> CargoJsonRecording {
        CargoJsonRecording {
            recording_format_version: CARGO_RECORDING_FORMAT_VERSION,
            toolchain: toolchain("1.97.1"),
            argv: vec!["cargo".into(), "check".into()],
            exit_code,
            content_digest: Digest::sha256(stdout_lines.join("\n").as_bytes()),
            stdout_lines,
            stderr: String::new(),
        }
    }

    fn write_recording(recording: &CargoJsonRecording) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.json");
        fs::write(&path, serde_json::to_vec(recording).unwrap()).unwrap();
        (dir, path)
    }

    #[test]
    fn recording_format_and_digest() {
        let rec = recording(0, vec![json!({ "reason": "build-finished" }).to_string()]);
        let (_dir, path) = write_recording(&rec);
        assert_eq!(CargoJsonRecording::load(&path).unwrap(), rec);

        let mut stale_version = rec.clone();
        stale_version.recording_format_version = 2;
        let (_dir, path) = write_recording(&stale_version);
        assert!(matches!(
            CargoJsonRecording::load(&path),
            Err(EvalError::RecordingInvalid(_))
        ));

        let mut bad_digest = rec;
        bad_digest.content_digest = Digest::sha256(b"different");
        let (_dir, path) = write_recording(&bad_digest);
        assert!(matches!(
            CargoJsonRecording::load(&path),
            Err(EvalError::RecordingInvalid(_))
        ));
    }

    #[test]
    fn recording_rejects_unknown_fields() {
        let rec = recording(0, vec![json!({ "reason": "build-finished" }).to_string()]);
        let base = serde_json::to_value(&rec).unwrap();
        let cases = [
            {
                let mut value = base.clone();
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("extra".into(), json!(true));
                ("recording", value)
            },
            {
                let mut value = base;
                value["toolchain"]
                    .as_object_mut()
                    .unwrap()
                    .insert("extra".into(), json!(true));
                ("recording toolchain", value)
            },
        ];
        for (field, value) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("recording.json");
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(
                matches!(
                    CargoJsonRecording::load(&path),
                    Err(EvalError::RecordingInvalid(_))
                ),
                "{field} accepted an unknown key"
            );
        }
    }

    #[test]
    fn recording_toolchain_triplet_matches() {
        let rec = recording(0, vec![]);
        rec.validate_against_pin("1.97.1").unwrap();
        assert!(matches!(
            rec.validate_against_pin("1.98.0"),
            Err(EvalError::RecordingStale(_))
        ));
    }

    #[test]
    fn recording_exit_code_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.json");
        let stdout_lines = Vec::<String>::new();
        let digest = Digest::sha256(stdout_lines.join("\n").as_bytes());
        fs::write(
            &path,
            json!({
                "recording_format_version": CARGO_RECORDING_FORMAT_VERSION,
                "toolchain": toolchain("1.97.1"),
                "argv": ["cargo", "check"],
                "stdout_lines": stdout_lines,
                "content_digest": digest,
            })
            .to_string(),
        )
        .unwrap();
        assert!(matches!(
            CargoJsonRecording::load(&path),
            Err(EvalError::RecordingInvalid(_))
        ));
    }

    #[test]
    fn compile_clean_parses_before_classifying() {
        let rec = recording(0, vec!["not json".into()]);
        assert!(matches!(
            rec.compile_clean(),
            Err(EvalError::RecordingInvalid(_))
        ));
    }

    #[test]
    fn pre_repair_expected_diagnostic_pairs() {
        let rec = recording(
            1,
            vec![line("error", Some("E0502"), "cannot borrow `x` as mutable")],
        );
        validate_expected_diagnostics(
            &rec,
            &[ExpectedDiagnostic {
                code: "E0502".into(),
                message_contains: "borrow `x`".into(),
            }],
        )
        .unwrap();
        assert!(matches!(
            validate_expected_diagnostics(
                &rec,
                &[ExpectedDiagnostic {
                    code: "E0502".into(),
                    message_contains: "not present".into(),
                }],
            ),
            Err(EvalError::RecordingInvalid(_))
        ));
    }

    #[test]
    fn golden_pre_repair_fails_compile() {
        let rec = recording(
            1,
            vec![line("error", Some("E0502"), "cannot borrow as mutable")],
        );
        assert!(!rec.compile_clean().unwrap());
        assert_eq!(rec.diagnostics().unwrap()[0].code.as_deref(), Some("E0502"));
    }

    #[test]
    fn golden_post_repair_passes_compile() {
        let rec = recording(0, vec![json!({ "reason": "build-finished" }).to_string()]);
        assert!(rec.compile_clean().unwrap());
    }
}
