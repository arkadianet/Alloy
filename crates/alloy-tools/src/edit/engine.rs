//! Concrete git-backed EditEngine (RFC-0008).
//!
//! Author: arkadianet

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use alloy_runtime::{
    ArtifactKind, ArtifactPut, ArtifactStore, CheckpointId, EditAppliedPayload, EditContext,
    EditEngine, EditError, EditRequest, EditRequestKind, EditTransaction, EditValidation,
    EventSink, NewSessionEvent, SessionEventType, Timestamp, TransactionId, TxState,
    EDIT_APPLIED_SCHEMA,
};
use async_trait::async_trait;
use serde_json::json;

use crate::edit::apply::{apply_file_patches, ApplyProgress, FileApplyOutcome};
use crate::edit::checkpoint::{
    create_checkpoint, preflight_git, resolve_checkpoint, restore_checkpoint, tracked_set,
    CreatedCheckpoint,
};
use crate::edit::digest::compute_workspace_digest;
use crate::edit::map_error::map_store;
use crate::edit::patch_parse::{
    check_expiry, check_run, reject_semantic, require_git_write, validate_patchset_local,
};
use crate::edit::tx::{AbandonedCheckpoint, TxRecord};
use crate::sandbox::{ExecClass, PathPolicy, SandboxBroker};

const DEFAULT_MAX_DIGEST_FILES: u64 = 50_000;
const DEFAULT_MAX_DIGEST_BYTES: u64 = 512 * 1024 * 1024;

/// Concrete MVP EditEngine: PathPolicy writes + sandboxed git checkpoints.
pub struct GitEditEngine {
    broker: Arc<dyn SandboxBroker>,
    path_policy: PathPolicy,
    trusted_path: Vec<PathBuf>,
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<dyn EventSink>,
    tx_store: Mutex<HashMap<TransactionId, TxRecord>>,
    abandoned: Mutex<Option<AbandonedCheckpoint>>,
    write_lock: tokio::sync::Mutex<()>,
    max_digest_files: u64,
    max_digest_bytes: u64,
}

/// Configuration for [`GitEditEngine`].
pub struct GitEditEngineConfig {
    /// Sandbox broker used for all git execution.
    pub broker: Arc<dyn SandboxBroker>,
    /// Path policy used for host-side writes.
    pub path_policy: PathPolicy,
    /// Trusted executable roots used for `Exec(git)` preflight.
    pub trusted_path: Vec<PathBuf>,
    /// Artifact store for canonical PatchSet JSON.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Event sink for best-effort EditApplied.
    pub events: Arc<dyn EventSink>,
    /// Soft cap on files walked for WorkspaceDigest.
    pub max_digest_files: u64,
    /// Soft cap on total bytes hashed for WorkspaceDigest.
    pub max_digest_bytes: u64,
}

impl GitEditEngineConfig {
    /// Build config with default digest caps.
    #[must_use]
    pub fn new(
        broker: Arc<dyn SandboxBroker>,
        path_policy: PathPolicy,
        trusted_path: Vec<PathBuf>,
        artifacts: Arc<dyn ArtifactStore>,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            broker,
            path_policy,
            trusted_path,
            artifacts,
            events,
            max_digest_files: DEFAULT_MAX_DIGEST_FILES,
            max_digest_bytes: DEFAULT_MAX_DIGEST_BYTES,
        }
    }
}

impl GitEditEngine {
    /// Construct an engine after verifying jail alignment.
    pub fn new(config: GitEditEngineConfig) -> Result<Self, EditError> {
        let policy_jail = config
            .path_policy
            .jail()
            .canonicalize()
            .map_err(|e| EditError::Internal(format!("path policy jail: {e}")))?;
        let broker_jail = config
            .broker
            .profile()
            .fs_jail
            .canonicalize()
            .map_err(|e| EditError::Internal(format!("broker jail: {e}")))?;
        if policy_jail != broker_jail {
            return Err(EditError::Internal(
                "path_policy jail != broker jail".into(),
            ));
        }
        Ok(Self {
            broker: config.broker,
            path_policy: config.path_policy,
            trusted_path: config.trusted_path,
            artifacts: config.artifacts,
            events: config.events,
            tx_store: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(None),
            write_lock: tokio::sync::Mutex::new(()),
            max_digest_files: config.max_digest_files,
            max_digest_bytes: config.max_digest_bytes,
        })
    }

    /// Operator / post-restart recovery for a checkpoint ref.
    pub async fn recover_checkpoint(
        &self,
        checkpoint_id: CheckpointId,
        ctx: &EditContext,
    ) -> Result<(), EditError> {
        let _guard = self.write_lock.lock().await;
        check_expiry(&ctx.perms)?;
        check_run(ctx.run_id, &ctx.perms)?;
        self.require_git(&ctx.perms)?;
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, &ctx.perms).await?;
        crate::edit::checkpoint::ensure_no_tracked_denied(&self.path_policy, &tracked)?;
        let sha = resolve_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            &ctx.perms,
            checkpoint_id,
        )
        .await?;
        restore_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            &ctx.perms,
            &sha,
            &[],
            &[],
            &[],
        )
        .await?;
        {
            let mut txs = lock(&self.tx_store)?;
            for record in txs.values_mut() {
                if record.checkpoint_id == checkpoint_id && record.state == TxState::Open {
                    record.state = TxState::RolledBack;
                }
            }
        }
        {
            let mut abandoned = lock(&self.abandoned)?;
            if abandoned
                .as_ref()
                .is_some_and(|a| a.checkpoint_id == checkpoint_id)
            {
                *abandoned = None;
            }
        }
        Ok(())
    }

    fn require_git(&self, perms: &alloy_runtime::PermissionToken) -> Result<(), EditError> {
        require_git_write(perms)?;
        preflight_git(
            perms,
            self.broker.profile().backend_for(ExecClass::Check),
            self.path_policy.jail(),
            &self.trusted_path,
        )
    }

    async fn reconcile_abandoned(
        &self,
        perms: &alloy_runtime::PermissionToken,
    ) -> Result<(), EditError> {
        let abandoned = { lock(&self.abandoned)?.clone() };
        let Some(abandoned) = abandoned else {
            return Ok(());
        };
        self.require_git(perms)?;
        let state = {
            lock(&self.tx_store)?
                .get(&abandoned.transaction_id)
                .map(|r| r.state)
        };
        if state == Some(TxState::Committed) {
            tracing::warn!(
                checkpoint_id = %abandoned.checkpoint_id,
                "clearing stale abandoned checkpoint for committed edit"
            );
            *lock(&self.abandoned)? = None;
            return Ok(());
        }
        if state != Some(TxState::Open) {
            *lock(&self.abandoned)? = None;
            return Ok(());
        }
        if let Err(err) = restore_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            perms,
            &abandoned.checkpoint_sha,
            &abandoned.created_paths,
            &abandoned.temp_paths,
            &abandoned.created_dirs,
        )
        .await
        {
            return Err(EditError::RollbackFailed {
                tx: abandoned.transaction_id,
                checkpoint_id: abandoned.checkpoint_id,
                detail: err.to_string(),
            });
        }
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, perms).await?;
        let digest = compute_workspace_digest(
            &self.path_policy,
            &tracked,
            &[],
            self.max_digest_files,
            self.max_digest_bytes,
        )?;
        if digest != abandoned.pre_digest {
            return Err(EditError::RollbackFailed {
                tx: abandoned.transaction_id,
                checkpoint_id: abandoned.checkpoint_id,
                detail: "digest mismatch after restore".into(),
            });
        }
        if let Some(record) = lock(&self.tx_store)?.get_mut(&abandoned.transaction_id) {
            record.state = TxState::RolledBack;
        }
        *lock(&self.abandoned)? = None;
        Ok(())
    }

    async fn rollback_record(
        &self,
        record: TxRecord,
        perms: &alloy_runtime::PermissionToken,
    ) -> Result<(), EditError> {
        let abandoned = AbandonedCheckpoint {
            transaction_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            created_paths: record.created_paths.clone(),
            temp_paths: record.temp_paths.clone(),
            created_dirs: record.created_dirs.clone(),
            pre_digest: record.pre_digest.clone(),
        };
        *lock(&self.abandoned)? = Some(abandoned.clone());
        if let Err(err) = restore_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            perms,
            &record.checkpoint_sha,
            &record.created_paths,
            &record.temp_paths,
            &record.created_dirs,
        )
        .await
        {
            return Err(EditError::RollbackFailed {
                tx: record.id,
                checkpoint_id: record.checkpoint_id,
                detail: err.to_string(),
            });
        }
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, perms).await?;
        let digest = compute_workspace_digest(
            &self.path_policy,
            &tracked,
            &[],
            self.max_digest_files,
            self.max_digest_bytes,
        )?;
        if digest != record.pre_digest {
            return Err(EditError::RollbackFailed {
                tx: record.id,
                checkpoint_id: record.checkpoint_id,
                detail: "digest mismatch after restore".into(),
            });
        }
        if let Some(stored) = lock(&self.tx_store)?.get_mut(&record.id) {
            stored.state = TxState::RolledBack;
        }
        *lock(&self.abandoned)? = None;
        Ok(())
    }

    fn record_to_tx(record: &TxRecord) -> EditTransaction {
        EditTransaction {
            id: record.id,
            state: record.state,
            request_kind: EditRequestKind::TextPatch,
            pre_digest: record.pre_digest.clone(),
            post_digest: record.post_digest.clone(),
            checkpoint_id: Some(record.checkpoint_id),
            checkpoint_sha: Some(record.checkpoint_sha.clone()),
            files_touched: record.files_touched.clone(),
            created_paths: record.created_paths.clone(),
            patch_artifact_id: record.patch_artifact_id,
            patch_content_hash: record.patch_content_hash.clone(),
            created_at: record.created_at.clone(),
        }
    }
}

#[async_trait]
impl EditEngine for GitEditEngine {
    async fn validate(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditValidation, EditError> {
        let _guard = self.write_lock.lock().await;
        reject_semantic(&req)?;
        let EditRequest::TextPatch { patch } = req else {
            unreachable!("semantic rejected");
        };
        let files_touched = validate_patchset_local(&patch, &self.path_policy, &ctx.perms)?;
        Ok(EditValidation { files_touched })
    }

    async fn apply(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditTransaction, EditError> {
        let _guard = self.write_lock.lock().await;
        check_expiry(&ctx.perms)?;
        check_run(ctx.run_id, &ctx.perms)?;
        self.reconcile_abandoned(&ctx.perms).await?;
        reject_semantic(&req)?;
        let EditRequest::TextPatch { patch } = req else {
            unreachable!("semantic rejected");
        };
        let files_touched = validate_patchset_local(&patch, &self.path_policy, &ctx.perms)?;
        self.require_git(&ctx.perms)?;

        let tx_id = TransactionId::new();
        let checkpoint_id = CheckpointId::new();
        let CreatedCheckpoint {
            checkpoint_sha,
            head_sha,
            tracked,
        } = create_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            &ctx.perms,
            checkpoint_id,
            &patch,
        )
        .await?;
        let pre_digest = compute_workspace_digest(
            &self.path_policy,
            &tracked,
            &[],
            self.max_digest_files,
            self.max_digest_bytes,
        )?;
        let created_at = Timestamp::now();
        let record = TxRecord {
            id: tx_id,
            state: TxState::Open,
            checkpoint_id,
            checkpoint_sha: checkpoint_sha.clone(),
            head_sha_at_checkpoint: head_sha,
            pre_digest: pre_digest.clone(),
            post_digest: None,
            files_touched: files_touched.clone(),
            created_paths: Vec::new(),
            temp_paths: Vec::new(),
            created_dirs: Vec::new(),
            patch_artifact_id: None,
            patch_content_hash: None,
            session_id: ctx.session_id,
            run_id: Some(ctx.run_id.unwrap_or(ctx.perms.run_id)),
            created_at: created_at.clone(),
        };
        lock(&self.tx_store)?.insert(tx_id, record);
        *lock(&self.abandoned)? = Some(AbandonedCheckpoint {
            transaction_id: tx_id,
            checkpoint_id,
            checkpoint_sha: checkpoint_sha.clone(),
            created_paths: Vec::new(),
            temp_paths: Vec::new(),
            created_dirs: Vec::new(),
            pre_digest: pre_digest.clone(),
        });

        let apply_result =
            apply_file_patches(&patch, &self.path_policy, &ctx.perms, tx_id, |progress| {
                if let Ok(mut abandoned) = self.abandoned.lock() {
                    if let Some(a) = abandoned.as_mut() {
                        match progress {
                            ApplyProgress::TempPath(path) => a.temp_paths.push(path),
                            ApplyProgress::CreatedPath(path) => a.created_paths.push(path),
                            ApplyProgress::CreatedDir(path) => a.created_dirs.push(path),
                        }
                    }
                }
            });
        let file_out = match apply_result {
            Ok(out) => out,
            Err(err) => {
                let partial = err.partial;
                let _ = self
                    .restore_after_failure(
                        tx_id,
                        checkpoint_id,
                        &checkpoint_sha,
                        &partial,
                        &ctx.perms,
                    )
                    .await;
                return Err(err.error);
            }
        };
        update_record_paths(&self.tx_store, tx_id, &file_out)?;

        let post_digest = match compute_workspace_digest(
            &self.path_policy,
            &tracked,
            &file_out.created_paths,
            self.max_digest_files,
            self.max_digest_bytes,
        ) {
            Ok(d) => d,
            Err(err) => {
                self.restore_after_failure(
                    tx_id,
                    checkpoint_id,
                    &checkpoint_sha,
                    &file_out,
                    &ctx.perms,
                )
                .await?;
                return Err(err);
            }
        };

        let patch_bytes = serde_json::to_vec(&patch)
            .map_err(|e| EditError::Internal(format!("patch serde: {e}")))?;
        let patch_hash = alloy_runtime::Digest::sha256(&patch_bytes);
        let mut labels = serde_json::Map::new();
        labels.insert("transaction_id".into(), json!(tx_id.to_string()));
        labels.insert("checkpoint_id".into(), json!(checkpoint_id.to_string()));
        labels.insert("pre_digest".into(), json!(pre_digest.tree.as_hex()));
        labels.insert("post_digest".into(), json!(post_digest.tree.as_hex()));
        labels.insert("schema".into(), json!("alloy.patch_set.v1"));
        let artifact_id = match self
            .artifacts
            .put(ArtifactPut {
                bytes: patch_bytes,
                kind: ArtifactKind::Patch,
                content_type: Some("application/json".into()),
                session_id: ctx.session_id,
                run_id: Some(ctx.run_id.unwrap_or(ctx.perms.run_id)),
                labels,
            })
            .await
        {
            Ok(id) => id,
            Err(err) => {
                self.restore_after_failure(
                    tx_id,
                    checkpoint_id,
                    &checkpoint_sha,
                    &file_out,
                    &ctx.perms,
                )
                .await?;
                return Err(map_store(err));
            }
        };

        let committed = {
            let mut abandoned = lock(&self.abandoned)?;
            let mut txs = lock(&self.tx_store)?;
            let record = txs
                .get_mut(&tx_id)
                .ok_or_else(|| EditError::Internal("missing tx record".into()))?;
            record.state = TxState::Committed;
            record.post_digest = Some(post_digest.clone());
            record.patch_artifact_id = Some(artifact_id);
            record.patch_content_hash = Some(patch_hash.clone());
            record.files_touched = files_touched.clone();
            record.created_paths = file_out.created_paths.clone();
            record.temp_paths = file_out.temp_paths.clone();
            record.created_dirs = file_out.created_dirs.clone();
            *abandoned = None;
            Self::record_to_tx(record)
        };

        if let Some(session_id) = ctx.session_id {
            let payload = EditAppliedPayload {
                schema: EDIT_APPLIED_SCHEMA.into(),
                transaction_id: tx_id,
                checkpoint_id,
                checkpoint_sha: checkpoint_sha.clone(),
                pre_digest,
                post_digest,
                files_touched,
                patch_artifact_id: artifact_id,
                patch_content_hash: patch_hash,
                request_kind: EditRequestKind::TextPatch,
            };
            if let Err(err) = self
                .events
                .append_session(NewSessionEvent {
                    session_id,
                    run_id: Some(ctx.run_id.unwrap_or(ctx.perms.run_id)),
                    type_: SessionEventType::EditApplied,
                    payload: serde_json::to_value(payload).map_err(|e| {
                        EditError::Internal(format!("EditAppliedPayload serde: {e}"))
                    })?,
                })
                .await
            {
                let mapped = crate::edit::map_error::map_event(err);
                tracing::error!(error = %mapped, tx = %tx_id, "EditApplied append failed after commit");
            }
        }
        tracing::info!(tx = %tx_id, checkpoint_id = %checkpoint_id, "edit applied");
        Ok(committed)
    }

    async fn rollback(&self, tx: TransactionId, ctx: &EditContext) -> Result<(), EditError> {
        let _guard = self.write_lock.lock().await;
        check_expiry(&ctx.perms)?;
        check_run(ctx.run_id, &ctx.perms)?;
        self.reconcile_abandoned(&ctx.perms).await?;
        let record = lock(&self.tx_store)?
            .get(&tx)
            .cloned()
            .ok_or(EditError::UnknownTransaction(tx))?;
        self.require_git(&ctx.perms)?;
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, &ctx.perms).await?;
        crate::edit::checkpoint::ensure_no_tracked_denied(&self.path_policy, &tracked)?;

        match record.state {
            TxState::RolledBack => {
                let digest = compute_workspace_digest(
                    &self.path_policy,
                    &tracked,
                    &[],
                    self.max_digest_files,
                    self.max_digest_bytes,
                )?;
                if digest == record.pre_digest {
                    return Ok(());
                }
                return Err(EditError::WorkspaceDrifted(tx));
            }
            TxState::Committed => {
                let newest = {
                    let txs = lock(&self.tx_store)?;
                    newest_tx_with_state(&txs, TxState::Committed)
                };
                if Some(tx) != newest {
                    return Err(EditError::RollbackNotEligible {
                        tx,
                        state: record.state,
                        reason: "not newest",
                    });
                }
                let digest = compute_workspace_digest(
                    &self.path_policy,
                    &tracked,
                    &record.created_paths,
                    self.max_digest_files,
                    self.max_digest_bytes,
                )?;
                if Some(&digest) != record.post_digest.as_ref() {
                    return Err(EditError::WorkspaceDrifted(tx));
                }
            }
            TxState::Open => {
                let abandoned = lock(&self.abandoned)?.clone();
                if let Some(a) = abandoned {
                    if a.transaction_id != tx {
                        return Err(EditError::RollbackNotEligible {
                            tx,
                            state: record.state,
                            reason: "not abandon target",
                        });
                    }
                } else {
                    let newest = {
                        let txs = lock(&self.tx_store)?;
                        newest_tx_with_state(&txs, TxState::Open)
                    };
                    if Some(tx) != newest {
                        return Err(EditError::RollbackNotEligible {
                            tx,
                            state: record.state,
                            reason: "not newest",
                        });
                    }
                }
            }
        }
        self.rollback_record(record, &ctx.perms).await
    }
}

impl GitEditEngine {
    async fn restore_after_failure(
        &self,
        tx: TransactionId,
        checkpoint_id: CheckpointId,
        checkpoint_sha: &str,
        paths: &FileApplyOutcome,
        perms: &alloy_runtime::PermissionToken,
    ) -> Result<(), EditError> {
        if let Err(err) = restore_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            perms,
            checkpoint_sha,
            &paths.created_paths,
            &paths.temp_paths,
            &paths.created_dirs,
        )
        .await
        {
            return Err(EditError::RollbackFailed {
                tx,
                checkpoint_id,
                detail: err.to_string(),
            });
        }
        if let Some(record) = lock(&self.tx_store)?.get_mut(&tx) {
            record.state = TxState::RolledBack;
        }
        *lock(&self.abandoned)? = None;
        Ok(())
    }
}

fn update_record_paths(
    store: &Mutex<HashMap<TransactionId, TxRecord>>,
    tx: TransactionId,
    paths: &FileApplyOutcome,
) -> Result<(), EditError> {
    let mut txs = lock(store)?;
    let record = txs
        .get_mut(&tx)
        .ok_or_else(|| EditError::Internal("missing tx record".into()))?;
    record.created_paths = paths.created_paths.clone();
    record.temp_paths = paths.temp_paths.clone();
    record.created_dirs = paths.created_dirs.clone();
    Ok(())
}

fn newest_tx_with_state(
    txs: &HashMap<TransactionId, TxRecord>,
    state: TxState,
) -> Option<TransactionId> {
    txs.values()
        .filter(|r| r.state == state)
        .max_by(|a, b| (a.created_at.0, *a.id.as_uuid()).cmp(&(b.created_at.0, *b.id.as_uuid())))
        .map(|r| r.id)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, EditError> {
    mutex
        .lock()
        .map_err(|_| EditError::Internal("mutex poisoned".into()))
}

#[allow(dead_code)]
fn _tracked_from_slice(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}
