//! `fs_read` builtin (RFC-0006 §5.8).
//!
//! Reads go through `PathPolicy::authorize(.., Read)` and then open the
//! **canonical** path that authorize returned — never the raw argument — so
//! deny globs (`.env`, keys, SSH/AWS material) cannot be dodged by a symlink or
//! a `..` segment. No sandbox exec is involved.
//!
//! Author: arkadianet

use std::path::{Path, PathBuf};

use alloy_runtime::{PermissionToken, ToolError, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::mcp::authz;
use crate::mcp::builtins::{
    object_args, optional_integer, required_string, BuiltinCtx, BuiltinToolId,
};
use crate::mcp::error::{map_sandbox_error, McpError, PermissionDenial};
use crate::sandbox::path::relative_for_matching;
use crate::sandbox::PathAccess;

/// Hard ceiling on a single read, independent of the requested `max_bytes`.
pub(crate) const FS_READ_HARD_MAX: usize = 1_048_576;

/// Default read cap when the caller omits `max_bytes`.
pub(crate) const FS_READ_DEFAULT_MAX: usize = 262_144;

/// `fs_read` arguments (schema: RFC-0006 §5.3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadArgs {
    /// Jail-relative or absolute path; must resolve inside the jail.
    pub path: String,
    /// Byte cap for the returned text.
    #[serde(default = "default_fs_read_max")]
    pub max_bytes: usize,
}

fn default_fs_read_max() -> usize {
    FS_READ_DEFAULT_MAX
}

const ALLOWED_KEYS: &[&str] = &["path", "max_bytes"];

/// An authorized read: the canonical path to open plus its jail-relative name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedRead {
    /// Canonical path returned by `PathPolicy::authorize`.
    pub(crate) canon: PathBuf,
    /// Jail-relative rendering used for grants and the result content.
    pub(crate) rel: String,
    /// Effective byte cap.
    pub(crate) cap: usize,
}

/// Parse and validate arguments without touching the filesystem.
pub(crate) fn parse(arguments: &Value) -> Result<FsReadArgs, McpError> {
    let obj = object_args(arguments, ALLOWED_KEYS)?;
    let max_bytes = optional_integer(obj, "max_bytes", 1, FS_READ_HARD_MAX as u64)?
        .map_or(FS_READ_DEFAULT_MAX, |n| {
            usize::try_from(n).unwrap_or(FS_READ_HARD_MAX)
        });
    Ok(FsReadArgs {
        path: required_string(obj, "path")?,
        max_bytes,
    })
}

/// Parse, authorize through [`PathPolicy`](crate::sandbox::PathPolicy), and
/// check the `FsRead` grant.
pub(crate) fn prepare(
    ctx: &BuiltinCtx<'_>,
    arguments: &Value,
    perms: &PermissionToken,
) -> Result<PreparedRead, McpError> {
    let args = parse(arguments)?;
    let jail = ctx.path_policy.jail();
    let raw = Path::new(&args.path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        jail.join(raw)
    };

    let canon = ctx
        .path_policy
        .authorize(&candidate, PathAccess::Read)
        .map_err(map_sandbox_error)?;

    // MVP keeps the content `path` jail-relative and well defined, so a
    // readable out-of-jail RO root is still refused here.
    if !canon.starts_with(jail) {
        return Err(McpError::PermissionDenied(
            PermissionDenial::PathNotCovered("outside jail".into()),
        ));
    }

    let rel = relative_for_matching(&canon, jail).map_err(map_sandbox_error)?;
    authz::authorize_fs_read(perms, &rel)?;

    Ok(PreparedRead {
        canon,
        rel,
        cap: args.max_bytes.min(FS_READ_HARD_MAX),
    })
}

/// Open the canonical path and return capped, validated UTF-8 text.
pub(crate) async fn execute(prepared: PreparedRead) -> Result<ToolResult, McpError> {
    let started = std::time::Instant::now();
    let name = BuiltinToolId::FsRead.name();

    let meta = match tokio::fs::metadata(&prepared.canon).await {
        Ok(m) => m,
        Err(e) => return Ok(io_error_result(&prepared, e, started)),
    };
    if !meta.is_file() {
        return Ok(tool_error(
            &prepared,
            ToolError::Permanent {
                code: "not_a_file".into(),
                message: "fs_read target is not a regular file".into(),
            },
            "not_a_file",
            started,
        ));
    }

    let file = match tokio::fs::File::open(&prepared.canon).await {
        Ok(f) => f,
        Err(e) => return Ok(io_error_result(&prepared, e, started)),
    };
    let mut raw = Vec::new();
    if let Err(e) = file.take(prepared.cap as u64).read_to_end(&mut raw).await {
        return Ok(io_error_result(&prepared, e, started));
    }

    let capped = meta.len() > raw.len() as u64;
    let Some((text, truncated)) = decode_utf8(&raw, capped) else {
        return Ok(tool_error(
            &prepared,
            ToolError::Permanent {
                code: "not_utf8".into(),
                message: "fs_read target is not valid UTF-8".into(),
            },
            "not_utf8",
            started,
        ));
    };

    let content = json!({
        "path": prepared.rel,
        "bytes": text.len(),
        "truncated": truncated,
        "text": text,
    });
    Ok(ToolResult::ok(name, content, elapsed_ms(started)))
}

/// Decode `raw` under the RFC-0006 §5.8 step 6 rules.
///
/// Returns `None` for interior corruption — a clipped trailing code point is
/// an artefact of `max_bytes`, but invalid bytes in the middle of the buffer
/// are a real property of the file and must not be silently trimmed.
fn decode_utf8(raw: &[u8], capped: bool) -> Option<(&str, bool)> {
    match std::str::from_utf8(raw) {
        Ok(text) => Some((text, capped)),
        Err(e) => {
            if !capped || e.error_len().is_some() {
                return None;
            }
            let valid = e.valid_up_to();
            // `valid == 0` means the cap split a leading multibyte sequence:
            // an empty truncated read, not a decoding failure.
            std::str::from_utf8(&raw[..valid]).ok().map(|t| (t, true))
        }
    }
}

fn io_error_result(
    prepared: &PreparedRead,
    err: std::io::Error,
    started: std::time::Instant,
) -> ToolResult {
    let (code, error) = match err.kind() {
        std::io::ErrorKind::NotFound => (
            "not_found",
            ToolError::Permanent {
                code: "not_found".into(),
                message: "fs_read target not found".into(),
            },
        ),
        std::io::ErrorKind::PermissionDenied => (
            "io_denied",
            ToolError::Permanent {
                code: "io_denied".into(),
                message: "fs_read target not readable".into(),
            },
        ),
        // Raw OS strings can embed absolute paths; use a fixed message.
        _ => (
            "io",
            ToolError::Transient {
                code: "io".into(),
                message: "fs_read io error".into(),
            },
        ),
    };
    tool_error(prepared, error, code, started)
}

fn tool_error(
    prepared: &PreparedRead,
    error: ToolError,
    code: &str,
    started: std::time::Instant,
) -> ToolResult {
    ToolResult::err(
        BuiltinToolId::FsRead.name(),
        json!({ "path": prepared.rel, "code": code }),
        error,
        elapsed_ms(started),
    )
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fs_read_defaults_and_bounds() {
        let args = parse(&json!({ "path": "src/main.rs" })).unwrap();
        assert_eq!(args.max_bytes, FS_READ_DEFAULT_MAX);

        assert!(matches!(
            parse(&json!({ "path": "a", "max_bytes": 0 })),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("out of range")
        ));
        assert!(matches!(
            parse(&json!({ "path": "a", "max_bytes": FS_READ_HARD_MAX + 1 })),
            Err(McpError::InvalidArguments(ref m)) if m.starts_with("out of range")
        ));
        assert!(matches!(
            parse(&json!({ "path": "" })),
            Err(McpError::InvalidArguments(ref m)) if m == "empty string: path"
        ));
    }

    #[test]
    fn fs_read_utf8_trim_on_truncate() {
        // "aé" clipped mid-`é`: valid_up_to == 1, cap-induced, so trim.
        let raw = b"a\xC3";
        let (text, truncated) = decode_utf8(raw, true).unwrap();
        assert_eq!(text, "a");
        assert!(truncated);
    }

    #[test]
    fn fs_read_utf8_cap_splits_leading_multibyte() {
        let raw = b"\xC3";
        let (text, truncated) = decode_utf8(raw, true).unwrap();
        assert_eq!(text, "");
        assert!(truncated);
    }

    #[test]
    fn fs_read_utf8_interior_invalid() {
        // Interior corruption is rejected even when the read was capped.
        assert!(decode_utf8(b"a\xFFb", true).is_none());
        assert!(decode_utf8(b"a\xFFb", false).is_none());
        // A trailing incomplete sequence on a complete read is still invalid.
        assert!(decode_utf8(b"a\xC3", false).is_none());
    }

    #[test]
    fn fs_read_utf8_clean_buffer() {
        let (text, truncated) = decode_utf8("héllo".as_bytes(), false).unwrap();
        assert_eq!(text, "héllo");
        assert!(!truncated);
        let (_, truncated) = decode_utf8("héllo".as_bytes(), true).unwrap();
        assert!(truncated);
    }
}
