//! EditEngine failure taxonomy (RFC-0008 §3.6 / §8).
//!
//! Author: arkadianet

use thiserror::Error;

use crate::edit::types::TxState;
use crate::types::ids::{CheckpointId, TransactionId};

/// EditEngine failure taxonomy (RFC-0008 §8).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EditError {
    /// Semantic op not implemented in MVP.
    #[error("unsupported op: {op}")]
    UnsupportedOp {
        /// Serde tag string (e.g. `"rename_type"`).
        op: String,
    },

    /// Bad request envelope (not a patch-body defect).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Malformed patch structure or parse failure.
    #[error("invalid patch: {0}")]
    InvalidPatch(String),

    /// PatchSet with zero files.
    #[error("empty patch")]
    EmptyPatch,

    /// Path shape / jail / deny / symlink rejection.
    #[error("path denied: {path}: {reason}")]
    PathDenied {
        /// Jail-relative path when available (never absolute operator layout).
        path: String,
        /// Short reason code.
        reason: String,
    },

    /// FsWrite grants exist but none cover this path.
    #[error("path not covered by FsWrite grant: {path}")]
    PathNotCovered {
        /// Jail-relative path.
        path: String,
    },

    /// Required grant missing (`fs_write`, `git_write`, `exec:git`, …).
    #[error("missing grant: {0}")]
    MissingGrant(String),

    /// Create exists / delete missing / unclean repo state.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Hunk context lines disagree with the file.
    #[error("context mismatch: {path}: {detail}")]
    ContextMismatch {
        /// Jail-relative path.
        path: String,
        /// Short detail.
        detail: String,
    },

    /// Two hunks consume overlapping old-side ranges.
    #[error("overlapping hunks: {path}")]
    OverlappingHunks {
        /// Jail-relative path.
        path: String,
    },

    /// Modify/Delete path is not in the git tracked set.
    #[error("untracked path in patch: {path}")]
    UntrackedPath {
        /// Jail-relative path.
        path: String,
    },

    /// Create path is already in the git tracked set (RFC-0008 §5.6.1
    /// item 4, split from `UntrackedPath` by amendment — the file exists;
    /// the honest correction is Modify, not a different path).
    #[error("create targets a tracked path (file exists): {path}")]
    CreateOnTrackedPath {
        /// Jail-relative path.
        path: String,
    },

    /// A tracked path matches deny-globs (secrets fail closed).
    #[error("tracked deny-glob path present: {path}")]
    TrackedDeniedPath {
        /// Jail-relative path.
        path: String,
    },

    /// Transient git checkpoint create failure (pre-mutate).
    #[error("checkpoint failed: {0}")]
    CheckpointFailed(String),

    /// Restore failed after mutation (FailedDirty).
    #[error("rollback failed: tx={tx} checkpoint={checkpoint_id}: {detail}")]
    RollbackFailed {
        /// Transaction that remains Open.
        tx: TransactionId,
        /// Checkpoint ref retained for recovery.
        checkpoint_id: CheckpointId,
        /// Short detail (no absolute paths).
        detail: String,
    },

    /// Rollback requested for an unknown in-process transaction.
    #[error("unknown transaction: {0}")]
    UnknownTransaction(TransactionId),

    /// Transaction exists but is not eligible for rollback.
    #[error("transaction not eligible for rollback: {tx}: state={state:?}: {reason}")]
    RollbackNotEligible {
        /// Transaction id.
        tx: TransactionId,
        /// Current state.
        state: TxState,
        /// Static reason: `"not newest"` | `"not abandon target"`.
        reason: &'static str,
    },

    /// Workspace digest drifted since the transaction.
    #[error("workspace drifted since transaction: {0}")]
    WorkspaceDrifted(TransactionId),

    /// Digest soft caps exceeded.
    #[error("digest limit exceeded: {0}")]
    DigestLimitExceeded(String),

    /// Filesystem IO error.
    #[error("io: {0}")]
    Io(String),

    /// Transient git / sandbox child failure.
    #[error("git: {0}")]
    Git(String),

    /// Permanent operator/environment misconfiguration (not retryable).
    #[error("environment: {0}")]
    Environment(String),

    /// ArtifactStore failure.
    #[error("storage: {0}")]
    Storage(String),

    /// EventSink failure (reserved; not returned after commit).
    #[error("event sink: {0}")]
    Event(String),

    /// Concurrent try_lock helper only (not on production MCP path).
    #[error("busy: edit already in progress")]
    Busy,

    /// Cooperative cancel (reserved).
    #[error("cancelled")]
    Cancelled,

    /// Permission token past expiry.
    #[error("token expired")]
    TokenExpired,

    /// Internal invariant violation.
    #[error("internal: {0}")]
    Internal(String),
}
