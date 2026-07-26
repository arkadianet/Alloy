//! Normative builtin JSON Schemas and descriptions (RFC-0006 §5.3).
//!
//! Schemas are built in Rust and snapshot-tested against the committed JSON in
//! `snapshots/`, so a drifting schema fails the suite instead of silently
//! changing the model-facing contract.
//!
//! Author: arkadianet

use serde_json::{json, Value};

use crate::mcp::builtins::fs_read::{FS_READ_DEFAULT_MAX, FS_READ_HARD_MAX};
use crate::mcp::builtins::{BuiltinToolId, MAX_ARG_STRING_BYTES, MAX_FEATURES};

/// Committed snapshot for `cargo_check`.
#[cfg(test)]
const CARGO_CHECK_SNAPSHOT: &str = include_str!("snapshots/cargo_check.json");
/// Committed snapshot for `cargo_test`.
#[cfg(test)]
const CARGO_TEST_SNAPSHOT: &str = include_str!("snapshots/cargo_test.json");
/// Committed snapshot for `fs_read`.
#[cfg(test)]
const FS_READ_SNAPSHOT: &str = include_str!("snapshots/fs_read.json");
/// Committed snapshot for `apply_patch`.
#[cfg(test)]
const APPLY_PATCH_SNAPSHOT: &str = include_str!("snapshots/apply_patch.json");

/// Model-facing input schema for a builtin.
pub(crate) fn input_schema(id: BuiltinToolId) -> Value {
    match id {
        BuiltinToolId::CargoCheck => json!({
            "type": "object",
            "properties": {
                "workspace_root": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "package": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "features": {
                    "type": "array",
                    "maxItems": MAX_FEATURES,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_ARG_STRING_BYTES
                    }
                },
                "all_features": { "type": "boolean", "default": false },
                "message_format": { "type": "string", "enum": ["json"], "default": "json" }
            },
            "required": ["workspace_root"],
            "additionalProperties": false
        }),
        BuiltinToolId::CargoTest => json!({
            "type": "object",
            "properties": {
                "workspace_root": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "package": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "test_name_filter": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "jobs": { "type": ["integer", "null"], "minimum": 1 }
            },
            "required": ["workspace_root"],
            "additionalProperties": false
        }),
        BuiltinToolId::FsRead => json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_ARG_STRING_BYTES
                },
                "max_bytes": {
                    "type": "integer",
                    "default": FS_READ_DEFAULT_MAX,
                    "minimum": 1,
                    "maximum": FS_READ_HARD_MAX
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        BuiltinToolId::ApplyPatch => json!({
            "type": "object",
            "properties": {
                "patch": {},
                "dry_run": { "type": "boolean", "default": false }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
    }
}

/// Exact model-facing description for a builtin.
pub(crate) fn description(id: BuiltinToolId) -> &'static str {
    match id {
        BuiltinToolId::CargoCheck => "Run cargo check and return structured rustc messages",
        BuiltinToolId::CargoTest => "Run cargo test and return structured results",
        BuiltinToolId::FsRead => "Read a UTF-8 text file under the workspace jail",
        BuiltinToolId::ApplyPatch => "Apply a unified diff / TextPatch via EditEngine",
    }
}

/// Committed snapshot text for a builtin.
#[cfg(test)]
fn snapshot(id: BuiltinToolId) -> &'static str {
    match id {
        BuiltinToolId::CargoCheck => CARGO_CHECK_SNAPSHOT,
        BuiltinToolId::CargoTest => CARGO_TEST_SNAPSHOT,
        BuiltinToolId::FsRead => FS_READ_SNAPSHOT,
        BuiltinToolId::ApplyPatch => APPLY_PATCH_SNAPSHOT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_snapshots() {
        for id in BuiltinToolId::ALL {
            let committed: Value = serde_json::from_str(snapshot(id))
                .unwrap_or_else(|e| panic!("snapshot for {} is not JSON: {e}", id.name()));
            assert_eq!(
                input_schema(id),
                committed,
                "schema drift for {}",
                id.name()
            );
        }
    }

    #[test]
    fn schemas_are_closed_objects() {
        for id in BuiltinToolId::ALL {
            let schema = input_schema(id);
            assert_eq!(schema["type"], "object", "{}", id.name());
            assert_eq!(schema["additionalProperties"], false, "{}", id.name());
        }
    }
}
