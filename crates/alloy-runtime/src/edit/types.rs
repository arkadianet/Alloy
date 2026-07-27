//! EditEngine IR types (RFC-0008 §3.2–§3.5, §9.3).
//!
//! Author: arkadianet

use serde::{Deserialize, Serialize};

use crate::types::ids::{
    ArtifactId, CheckpointId, Digest, RunId, SessionId, Timestamp, TransactionId,
};
use crate::types::permission::PermissionToken;

fn default_true() -> bool {
    true
}

/// Workspace edit envelope (Architecture V2 §13.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditRequest {
    /// Unified-diff / structured text patch (MVP path).
    TextPatch {
        /// Structured patch set.
        patch: PatchSet,
    },
    /// Semantic ops envelope — MVP fail closed (RFC-0008 §5.10).
    SemanticOps {
        /// Ordered semantic operations.
        ops: Vec<SemanticEditOp>,
    },
}

/// Structured patch set. Paths are jail-relative (`/`-separated, no leading `/`, no `..`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchSet {
    /// Ordered file patches. Apply order is vector order.
    pub files: Vec<FilePatch>,
}

/// One file operation inside a [`PatchSet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FilePatch {
    /// Modify an existing tracked file.
    Modify {
        /// Jail-relative path.
        path: String,
        /// Hunks to apply in ascending `old_start` order.
        hunks: Vec<Hunk>,
    },
    /// Create a new file. Parent directories are created as needed.
    Create {
        /// Jail-relative path.
        path: String,
        /// Exactly one create-shaped hunk (RFC-0008 V27).
        hunks: Vec<Hunk>,
    },
    /// Delete an existing file.
    Delete {
        /// Jail-relative path.
        path: String,
        /// Optional hunks retained from unified-diff parse for context validation
        /// (RFC-0008 V5/V9). Structured JSON omits this (default empty); apply
        /// ignores hunks and unlinks the path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hunks: Vec<Hunk>,
    },
}

impl FilePatch {
    /// Jail-relative path for this operation.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Modify { path, .. } | Self::Create { path, .. } | Self::Delete { path, .. } => {
                path
            }
        }
    }
}

/// One unified-diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// 1-based start line in the current file (0 only for create-from-empty old side).
    pub old_start: u32,
    /// Line count on the old side (context + deletions).
    pub old_lines: u32,
    /// 1-based start line in the new file.
    pub new_start: u32,
    /// Line count on the new side (context + insertions).
    pub new_lines: u32,
    /// Unified diff lines including leading ' ', '-', '+' only (no embedded NUL or raw `\n`).
    pub lines: Vec<String>,
    /// Whether the **new** file ends with `\n` after this hunk is applied (when this is
    /// the last hunk that contributes new-side lines).
    #[serde(default = "default_true")]
    pub eof_newline: bool,
    /// When true, the current (old) file must lack a trailing newline at EOF
    /// (unified-diff `\ No newline at end of file` after a `-` or context line).
    /// Structured JSON patches omit this (default `false`).
    #[serde(default)]
    pub old_eof_no_newline: bool,
}

/// Semantic edit ops (V2 §13). Serde-stable; MVP returns UnsupportedOp for all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SemanticEditOp {
    /// Rename a type and optionally update references.
    RenameType {
        /// Source path of the type.
        from_path: String,
        /// New type name.
        to_name: String,
        /// Whether to rewrite references.
        update_references: bool,
    },
    /// Add/remove imports in a file.
    UpdateImports {
        /// Target file.
        file: String,
        /// Imports to add.
        add: Vec<String>,
        /// Imports to remove.
        remove: Vec<String>,
    },
    /// Replace an item body.
    ReplaceBody {
        /// Item path.
        item_path: String,
        /// Replacement body source.
        new_body: String,
    },
    /// Insert an `impl` block.
    InsertImpl {
        /// Target file.
        file: String,
        /// Type path for the impl.
        type_path: String,
        /// Impl body source.
        body: String,
    },
    /// Add a method to an item.
    AddMethod {
        /// Item path.
        item_path: String,
        /// Method source.
        method_source: String,
    },
    /// Move a module between paths.
    MoveModule {
        /// Source module path.
        from_path: String,
        /// Destination module path.
        to_path: String,
    },
    /// Extract a trait from a type.
    ExtractTrait {
        /// Type path.
        type_path: String,
        /// New trait name.
        trait_name: String,
        /// Methods to move onto the trait.
        method_names: Vec<String>,
    },
    /// Split a crate.
    SplitCrate {
        /// Source crate name.
        source_crate: String,
        /// New crate name.
        new_crate: String,
        /// Paths to move into the new crate.
        move_paths: Vec<String>,
    },
    /// Add a field to a type.
    AddField {
        /// Type path.
        type_path: String,
        /// Field source.
        field_source: String,
    },
}

impl SemanticEditOp {
    /// Stable serde tag string for this variant (also used in `UnsupportedOp.op`).
    #[must_use]
    pub fn op_tag(&self) -> &'static str {
        match self {
            Self::RenameType { .. } => "rename_type",
            Self::UpdateImports { .. } => "update_imports",
            Self::ReplaceBody { .. } => "replace_body",
            Self::InsertImpl { .. } => "insert_impl",
            Self::AddMethod { .. } => "add_method",
            Self::MoveModule { .. } => "move_module",
            Self::ExtractTrait { .. } => "extract_trait",
            Self::SplitCrate { .. } => "split_crate",
            Self::AddField { .. } => "add_field",
        }
    }
}

/// Digest over the authorized workspace snapshot (RFC-0008 §5.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDigest {
    /// SHA-256 hex of the canonical tree encoding.
    pub tree: Digest,
    /// Number of files included in the tree encoding.
    pub file_count: u64,
    /// Total bytes hashed (file contents only).
    pub total_bytes: u64,
}

/// Lifecycle of a recorded edit transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxState {
    /// Checkpoint created; mutate not yet committed.
    Open,
    /// Mutate + CAS committed (`EditApplied` attempted when session present).
    Committed,
    /// Rollback restored pre-image.
    RolledBack,
}

/// Wire/request kind without embedding bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditRequestKind {
    /// Text patch path.
    TextPatch,
    /// Semantic ops path (fail closed in MVP).
    SemanticOps,
}

/// Committed or open edit transaction returned by [`super::EditEngine::apply`].
///
/// Persistence and session events MUST NOT store raw patch bodies — only ids,
/// digests, and hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTransaction {
    /// Transaction id.
    pub id: TransactionId,
    /// Lifecycle state.
    pub state: TxState,
    /// Request kind (no body).
    pub request_kind: EditRequestKind,
    /// Pre-apply workspace digest.
    pub pre_digest: WorkspaceDigest,
    /// Always `Some` when `state == Committed`.
    pub post_digest: Option<WorkspaceDigest>,
    /// Always `Some` after checkpoint creation on the mutating path.
    pub checkpoint_id: Option<CheckpointId>,
    /// Git commit SHA recorded at checkpoint (40 lowercase hex).
    pub checkpoint_sha: Option<String>,
    /// Jail-relative paths touched (sorted, deduped).
    pub files_touched: Vec<String>,
    /// Subset of `files_touched` that were created by this tx (for rollback unlink).
    pub created_paths: Vec<String>,
    /// CAS artifact id for the canonical PatchSet JSON, when stored (Committed).
    pub patch_artifact_id: Option<ArtifactId>,
    /// `Digest::sha256` of the canonical PatchSet JSON bytes.
    pub patch_content_hash: Option<Digest>,
    /// Creation time.
    pub created_at: Timestamp,
}

/// Per-call attribution and authorization for EditEngine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditContext {
    /// Session for EditApplied; if `None`, mutating apply still proceeds but
    /// skips EditApplied emission.
    pub session_id: Option<SessionId>,
    /// Run attribution. If `None`, use `perms.run_id` when emitting events.
    pub run_id: Option<RunId>,
    /// Caller grants for this invocation.
    pub perms: PermissionToken,
}

/// Result of a validation-only (dry-run) pass — never allocated a TransactionId.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditValidation {
    /// Jail-relative paths that would be touched (sorted, deduped).
    pub files_touched: Vec<String>,
}

/// Typed `EditApplied` session event payload (RFC-0008 §9.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditAppliedPayload {
    /// Must be `"alloy.edit_applied.v1"`.
    pub schema: String,
    /// Transaction id.
    pub transaction_id: TransactionId,
    /// Checkpoint id.
    pub checkpoint_id: CheckpointId,
    /// Checkpoint SHA (40 lowercase hex).
    pub checkpoint_sha: String,
    /// Pre-apply digest.
    pub pre_digest: WorkspaceDigest,
    /// Post-apply digest.
    pub post_digest: WorkspaceDigest,
    /// Jail-relative paths touched.
    pub files_touched: Vec<String>,
    /// CAS artifact id for PatchSet JSON.
    pub patch_artifact_id: ArtifactId,
    /// Digest of canonical PatchSet JSON (same as artifact meta.digest).
    pub patch_content_hash: Digest,
    /// Request kind.
    pub request_kind: EditRequestKind,
}

/// Schema string for [`EditAppliedPayload`].
pub const EDIT_APPLIED_SCHEMA: &str = "alloy.edit_applied.v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_op_tags_match_serde() {
        let cases: &[(SemanticEditOp, &str)] = &[
            (
                SemanticEditOp::RenameType {
                    from_path: "a".into(),
                    to_name: "B".into(),
                    update_references: true,
                },
                "rename_type",
            ),
            (
                SemanticEditOp::UpdateImports {
                    file: "a".into(),
                    add: vec![],
                    remove: vec![],
                },
                "update_imports",
            ),
            (
                SemanticEditOp::ReplaceBody {
                    item_path: "a".into(),
                    new_body: "b".into(),
                },
                "replace_body",
            ),
            (
                SemanticEditOp::InsertImpl {
                    file: "a".into(),
                    type_path: "T".into(),
                    body: "{}".into(),
                },
                "insert_impl",
            ),
            (
                SemanticEditOp::AddMethod {
                    item_path: "a".into(),
                    method_source: "fn x() {}".into(),
                },
                "add_method",
            ),
            (
                SemanticEditOp::MoveModule {
                    from_path: "a".into(),
                    to_path: "b".into(),
                },
                "move_module",
            ),
            (
                SemanticEditOp::ExtractTrait {
                    type_path: "T".into(),
                    trait_name: "Tr".into(),
                    method_names: vec![],
                },
                "extract_trait",
            ),
            (
                SemanticEditOp::SplitCrate {
                    source_crate: "a".into(),
                    new_crate: "b".into(),
                    move_paths: vec![],
                },
                "split_crate",
            ),
            (
                SemanticEditOp::AddField {
                    type_path: "T".into(),
                    field_source: "x: u8".into(),
                },
                "add_field",
            ),
        ];
        for (op, tag) in cases {
            assert_eq!(op.op_tag(), *tag);
            let v = serde_json::to_value(op).unwrap();
            assert_eq!(v["op"], *tag);
        }
    }

    #[test]
    fn hunk_eof_newline_defaults_true() {
        let v = serde_json::json!({
            "old_start": 1,
            "old_lines": 1,
            "new_start": 1,
            "new_lines": 1,
            "lines": [" context"]
        });
        let h: Hunk = serde_json::from_value(v).unwrap();
        assert!(h.eof_newline);
    }

    #[test]
    fn file_patch_path_accessor() {
        assert_eq!(
            FilePatch::Delete {
                path: "a.rs".into(),
                hunks: vec![],
            }
            .path(),
            "a.rs"
        );
    }
}
