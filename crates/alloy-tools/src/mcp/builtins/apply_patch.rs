//! `apply_patch` builtin (RFC-0006 §5.9).
//!
//! The host owns the output boundary: every value coming back from a
//! [`PatchApplyBackend`] is re-validated and sanitized before it reaches a
//! model, whether it is a success outcome or an error string. RFC-0008 swaps
//! the backend without changing anything here.
//!
//! Author: arkadianet

use std::time::Instant;

use alloy_runtime::{PermissionToken, ToolError, ToolResult};
use serde_json::{json, Value};

use crate::mcp::authz;
use crate::mcp::builtins::{
    object_args, optional_bool, BuiltinCtx, BuiltinToolId, MAX_ARG_STRING_BYTES,
};
use crate::mcp::error::McpError;
use crate::mcp::patch::{
    ApplyPatchArgs, ApplyPatchOutcome, PatchApplyError, EDIT_ENGINE_UNWIRED_CODE,
    EDIT_ENGINE_UNWIRED_MESSAGE,
};

/// Max length of any message forwarded from the backend.
const MAX_BACKEND_MESSAGE_BYTES: usize = 512;

/// Fallback used when a backend success message fails sanitization.
const SAFE_SUCCESS_MESSAGE: &str = "apply_patch completed";

const ALLOWED_KEYS: &[&str] = &["patch", "dry_run"];

/// Parse and validate arguments without touching the filesystem.
pub(crate) fn parse(arguments: &Value) -> Result<ApplyPatchArgs, McpError> {
    let obj = object_args(arguments, ALLOWED_KEYS)?;
    let patch = obj
        .get("patch")
        .ok_or_else(|| McpError::InvalidArguments("missing property: patch".into()))?
        .clone();
    Ok(ApplyPatchArgs {
        patch,
        dry_run: optional_bool(obj, "dry_run", false)?,
    })
}

/// Parse then require at least one `FsWrite` grant.
pub(crate) fn prepare(
    arguments: &Value,
    perms: &PermissionToken,
) -> Result<ApplyPatchArgs, McpError> {
    let args = parse(arguments)?;
    authz::authorize_fs_write(perms)?;
    Ok(args)
}

/// Call the injected backend and map the outcome through the output boundary.
pub(crate) async fn execute(
    ctx: &BuiltinCtx<'_>,
    args: ApplyPatchArgs,
) -> Result<ToolResult, McpError> {
    let started = Instant::now();
    let dry_run = args.dry_run;
    let outcome = ctx.patch_backend.apply(args).await;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(map_outcome(outcome, dry_run, elapsed))
}

fn map_outcome(
    outcome: Result<ApplyPatchOutcome, PatchApplyError>,
    dry_run: bool,
    duration_ms: u64,
) -> ToolResult {
    let name = BuiltinToolId::ApplyPatch.name();
    match outcome {
        Ok(outcome) => match sanitize_outcome(outcome) {
            Some(sanitized) => {
                let content = serde_json::to_value(&sanitized).unwrap_or_else(|_| json!({}));
                ToolResult::ok(name, content, duration_ms)
            }
            None => ToolResult::err(
                name,
                json!({ "code": "unsafe_backend_output" }),
                ToolError::Permanent {
                    code: "unsafe_backend_output".into(),
                    message: "files_touched failed validation".into(),
                },
                duration_ms,
            ),
        },
        Err(err) => {
            let (code, error) = map_backend_error(err);
            ToolResult::err(
                name,
                json!({ "code": code, "dry_run": dry_run }),
                error,
                duration_ms,
            )
        }
    }
}

fn map_backend_error(err: PatchApplyError) -> (&'static str, ToolError) {
    match err {
        PatchApplyError::Unsupported(msg) if msg == EDIT_ENGINE_UNWIRED_MESSAGE => (
            EDIT_ENGINE_UNWIRED_CODE,
            ToolError::Permanent {
                code: EDIT_ENGINE_UNWIRED_CODE.into(),
                message: EDIT_ENGINE_UNWIRED_MESSAGE.into(),
            },
        ),
        PatchApplyError::Unsupported(msg) => (
            "unsupported",
            ToolError::Permanent {
                code: "unsupported".into(),
                message: sanitize_msg(&msg)
                    .unwrap_or_else(|| "apply_patch unsupported".to_string()),
            },
        ),
        PatchApplyError::InvalidPatch(msg) => (
            "invalid_patch",
            ToolError::InvalidArgs {
                message: sanitize_msg(&msg)
                    .unwrap_or_else(|| "apply_patch invalid patch".to_string()),
            },
        ),
        PatchApplyError::Conflict(msg) => (
            "conflict",
            ToolError::Permanent {
                code: "conflict".into(),
                message: sanitize_msg(&msg).unwrap_or_else(|| "apply_patch conflict".to_string()),
            },
        ),
        // Backend IO / internal detail is dropped wholesale: fixed messages.
        PatchApplyError::Io(_) => (
            "io",
            ToolError::Transient {
                code: "io".into(),
                message: "apply_patch io error".into(),
            },
        ),
        PatchApplyError::Internal(_) => (
            "internal",
            ToolError::Permanent {
                code: "internal".into(),
                message: "apply_patch internal error".into(),
            },
        ),
    }
}

/// Re-validate `files_touched` and sanitize `message`.
///
/// Returns `None` when any path fails validation — the outcome is then not
/// forwarded at all, rather than partially trusted.
fn sanitize_outcome(mut outcome: ApplyPatchOutcome) -> Option<ApplyPatchOutcome> {
    if !outcome.files_touched.iter().all(|p| is_jail_relative(p)) {
        return None;
    }
    outcome.message =
        sanitize_msg(&outcome.message).unwrap_or_else(|| SAFE_SUCCESS_MESSAGE.to_string());
    Some(outcome)
}

fn is_jail_relative(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_ARG_STRING_BYTES {
        return false;
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return false;
    }
    // Windows drive-letter form is also absolute.
    if has_drive_prefix(path) {
        return false;
    }
    path.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn has_drive_prefix(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Strip absolute-path tokens and enforce the length / NUL limits.
///
/// `None` means the caller must substitute a fixed message.
fn sanitize_msg(msg: &str) -> Option<String> {
    if msg.len() > MAX_BACKEND_MESSAGE_BYTES || msg.contains('\0') {
        return None;
    }
    let redacted: Vec<&str> = msg
        .split_whitespace()
        .map(|token| {
            if token.starts_with('/') || has_drive_prefix(token) {
                "<path>"
            } else {
                token
            }
        })
        .collect();
    Some(redacted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::TransactionId;

    fn outcome(files: Vec<&str>, message: &str) -> ApplyPatchOutcome {
        ApplyPatchOutcome {
            dry_run: false,
            files_touched: files.into_iter().map(String::from).collect(),
            transaction_id: Some(TransactionId::new()),
            message: message.into(),
        }
    }

    #[test]
    fn apply_patch_stub_deterministic() {
        let result = map_outcome(
            Err(PatchApplyError::Unsupported(
                EDIT_ENGINE_UNWIRED_MESSAGE.into(),
            )),
            true,
            7,
        );
        assert!(result.is_error());
        assert_eq!(result.name.as_str(), "apply_patch");
        assert_eq!(
            result.content,
            json!({ "code": "edit_engine_unwired", "dry_run": true })
        );
        assert_eq!(
            result.error(),
            Some(&ToolError::Permanent {
                code: EDIT_ENGINE_UNWIRED_CODE.into(),
                message: EDIT_ENGINE_UNWIRED_MESSAGE.into(),
            })
        );
        assert_eq!(result.duration_ms, 7);
    }

    #[test]
    fn apply_patch_error_map_all_variants() {
        let cases: Vec<(PatchApplyError, ToolError)> = vec![
            (
                PatchApplyError::Unsupported("other dialect".into()),
                ToolError::Permanent {
                    code: "unsupported".into(),
                    message: "other dialect".into(),
                },
            ),
            (
                PatchApplyError::InvalidPatch("bad hunk".into()),
                ToolError::InvalidArgs {
                    message: "bad hunk".into(),
                },
            ),
            (
                PatchApplyError::Conflict("hunk 3".into()),
                ToolError::Permanent {
                    code: "conflict".into(),
                    message: "hunk 3".into(),
                },
            ),
            (
                PatchApplyError::Io("/home/op/x: EACCES".into()),
                ToolError::Transient {
                    code: "io".into(),
                    message: "apply_patch io error".into(),
                },
            ),
            (
                PatchApplyError::Internal("/home/op panic".into()),
                ToolError::Permanent {
                    code: "internal".into(),
                    message: "apply_patch internal error".into(),
                },
            ),
        ];
        for (err, expect) in cases {
            let result = map_outcome(Err(err), false, 0);
            assert!(result.is_error());
            assert_eq!(result.error(), Some(&expect));
        }
    }

    #[test]
    fn apply_patch_rejects_abs_files_touched() {
        for bad in [
            vec!["/etc/passwd"],
            vec!["../escape.rs"],
            vec!["src/../../escape.rs"],
            vec!["src\\main.rs"],
            vec!["C:/Windows/system32"],
            vec![""],
            vec!["./src/main.rs"],
        ] {
            let result = map_outcome(Ok(outcome(bad.clone(), "ok")), false, 0);
            assert!(result.is_error(), "expected rejection for {bad:?}");
            assert_eq!(result.content, json!({ "code": "unsafe_backend_output" }));
            assert!(matches!(
                result.error(),
                Some(ToolError::Permanent { code, .. }) if code == "unsafe_backend_output"
            ));
        }
    }

    #[test]
    fn apply_patch_success_is_sanitized() {
        let result = map_outcome(
            Ok(outcome(
                vec!["src/main.rs", "crates/a/src/lib.rs"],
                "wrote /home/op/work/src/main.rs",
            )),
            false,
            3,
        );
        assert!(!result.is_error());
        assert_eq!(result.content["message"], "wrote <path>");
        assert_eq!(result.content["files_touched"][0], "src/main.rs");
    }

    #[test]
    fn apply_patch_overlong_message_replaced() {
        let long = "x".repeat(MAX_BACKEND_MESSAGE_BYTES + 1);
        let result = map_outcome(Ok(outcome(vec!["a.rs"], &long)), false, 0);
        assert_eq!(result.content["message"], SAFE_SUCCESS_MESSAGE);

        let result = map_outcome(Err(PatchApplyError::Conflict(long)), false, 0);
        assert_eq!(
            result.error(),
            Some(&ToolError::Permanent {
                code: "conflict".into(),
                message: "apply_patch conflict".into(),
            })
        );
    }

    #[test]
    fn apply_patch_parse_requires_patch() {
        assert!(matches!(
            parse(&json!({ "dry_run": true })),
            Err(McpError::InvalidArguments(ref m)) if m == "missing property: patch"
        ));
        assert!(matches!(
            parse(&json!({ "patch": "x", "dry_run": "yes" })),
            Err(McpError::InvalidArguments(ref m)) if m == "type error: dry_run"
        ));
        let args = parse(&json!({ "patch": { "hunks": [] } })).unwrap();
        assert!(!args.dry_run);
    }
}
