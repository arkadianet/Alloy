//! In-process edit transaction bookkeeping.
//!
//! Author: arkadianet

use alloy_runtime::{
    ArtifactId, CheckpointId, Digest, RunId, SessionId, Timestamp, TransactionId, TxState,
    WorkspaceDigest,
};

/// In-process transaction record (RFC-0008 §4.4).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct TxRecord {
    pub id: TransactionId,
    pub state: TxState,
    pub checkpoint_id: CheckpointId,
    pub checkpoint_sha: String,
    pub head_sha_at_checkpoint: String,
    pub pre_digest: WorkspaceDigest,
    pub post_digest: Option<WorkspaceDigest>,
    pub files_touched: Vec<String>,
    pub created_paths: Vec<String>,
    pub temp_paths: Vec<String>,
    pub created_dirs: Vec<String>,
    pub patch_artifact_id: Option<ArtifactId>,
    pub patch_content_hash: Option<Digest>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
    pub created_at: Timestamp,
}

/// Checkpoint left open by a dropped or failed mutating operation.
#[derive(Debug, Clone)]
pub(crate) struct AbandonedCheckpoint {
    pub transaction_id: TransactionId,
    pub checkpoint_id: CheckpointId,
    pub checkpoint_sha: String,
    pub created_paths: Vec<String>,
    pub temp_paths: Vec<String>,
    pub created_dirs: Vec<String>,
    pub pre_digest: WorkspaceDigest,
}
