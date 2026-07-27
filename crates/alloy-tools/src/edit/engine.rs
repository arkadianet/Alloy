//! Concrete git-backed EditEngine (RFC-0008).
//!
//! Author: arkadianet

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use alloy_runtime::{
    ArtifactId, ArtifactKind, ArtifactPut, ArtifactStore, CheckpointId, Digest, EditAppliedPayload,
    EditContext, EditEngine, EditError, EditRequest, EditRequestKind, EditTransaction,
    EditValidation, EventSink, NewSessionEvent, PatchSet, PermissionToken, SessionEventType,
    SessionId, Timestamp, TransactionId, TxState, WorkspaceDigest, EDIT_APPLIED_SCHEMA,
};
use async_trait::async_trait;
use serde_json::json;
use tracing::field::{display, Empty};
use tracing::Instrument;

use crate::edit::apply::{apply_file_patches, ApplyProgress, FileApplyError, FileApplyOutcome};
use crate::edit::checkpoint::{
    create_checkpoint, ensure_no_tracked_denied, preflight_git, prepare_repo_for_edit,
    resolve_checkpoint, restore_checkpoint, tracked_set, CreatedCheckpoint,
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
    /// Shared with the blocking apply task so path bookkeeping survives a
    /// cancelled `apply` future (RFC-0008 §5.11 step 5).
    abandoned: Arc<Mutex<Option<AbandonedCheckpoint>>>,
    /// Serializes every mutating operation.
    ///
    /// `Arc` because `apply` hands an owned guard to its blocking task: work on
    /// the blocking pool cannot be aborted, so a cancelled `apply` future must
    /// not release the lock while its task is still writing files.
    write_lock: Arc<tokio::sync::Mutex<()>>,
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

/// Identity and pre-image of the mutating transaction currently in flight.
///
/// Every post-mutate failure path needs the same five values to restore the
/// checkpoint and verify the pre-image digest, so they travel together.
struct ApplyState {
    tx_id: TransactionId,
    checkpoint_id: CheckpointId,
    checkpoint_sha: String,
    pre_digest: WorkspaceDigest,
    tracked: BTreeSet<String>,
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
            abandoned: Arc::new(Mutex::new(None)),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
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
        refuse_tracked_denied(&self.path_policy, &tracked)?;
        let sha = resolve_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            &ctx.perms,
            checkpoint_id,
        )
        .await?;
        check_expiry(&ctx.perms)?;
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

    fn require_git(&self, perms: &PermissionToken) -> Result<(), EditError> {
        require_git_write(perms)?;
        preflight_git(
            perms,
            self.broker.profile().backend_for(ExecClass::Check),
            self.path_policy.jail(),
            &self.trusted_path,
        )
    }

    /// Compute a workspace digest under an `edit.digest` span (RFC §9.1).
    ///
    /// Hashing walks every tracked file, so it runs on the blocking pool: an
    /// engine sharing a runtime with request handlers must not stall them for the
    /// length of a workspace walk. The caller's `write_lock` guard stays on the
    /// async side across the await, so the serialization contract is unchanged.
    async fn workspace_digest(
        &self,
        phase: &'static str,
        tracked: &BTreeSet<String>,
        created_paths: &[String],
    ) -> Result<WorkspaceDigest, EditError> {
        let span = tracing::debug_span!(
            "edit.digest",
            phase = phase,
            file_count = Empty,
            total_bytes = Empty
        );
        let policy = self.path_policy.clone();
        let tracked = tracked.clone();
        let created_paths = created_paths.to_vec();
        let max_files = self.max_digest_files;
        let max_bytes = self.max_digest_bytes;
        let task_span = span.clone();
        let digest = tokio::task::spawn_blocking(move || {
            let _entered = task_span.enter();
            compute_workspace_digest(&policy, &tracked, &created_paths, max_files, max_bytes)
        })
        .await
        .map_err(|err| EditError::Internal(format!("digest task: {err}")))??;
        span.record("file_count", digest.file_count);
        span.record("total_bytes", digest.total_bytes);
        Ok(digest)
    }

    /// Create the checkpoint ref under an `edit.checkpoint` span (RFC §9.1).
    async fn create_checkpoint_spanned(
        &self,
        perms: &PermissionToken,
        checkpoint_id: CheckpointId,
        head_sha: &str,
        tracked: BTreeSet<String>,
    ) -> Result<CreatedCheckpoint, EditError> {
        let span = tracing::info_span!(
            "edit.checkpoint",
            checkpoint_id = %checkpoint_id,
            sha = Empty,
            git.exit = Empty
        );
        let created = create_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            perms,
            checkpoint_id,
            head_sha,
            tracked,
        )
        .instrument(span.clone())
        .await;
        if let Ok(created) = &created {
            span.record("sha", created.checkpoint_sha.as_str());
            span.record("git.exit", 0);
        }
        created
    }

    async fn reconcile_abandoned(&self, perms: &PermissionToken) -> Result<(), EditError> {
        let abandoned = { lock(&self.abandoned)?.clone() };
        let Some(abandoned) = abandoned else {
            return Ok(());
        };
        let span = tracing::info_span!(
            "edit.reconcile_abandoned",
            checkpoint_id = %abandoned.checkpoint_id,
            result = Empty
        );
        let outcome = self
            .reconcile_one(&abandoned, perms)
            .instrument(span.clone())
            .await;
        match &outcome {
            Ok(result) => {
                span.record("result", *result);
            }
            Err(err) => {
                span.record("result", display(err));
            }
        }
        outcome.map(|_| ())
    }

    /// Reconcile one abandoned checkpoint; returns the span `result` label.
    async fn reconcile_one(
        &self,
        abandoned: &AbandonedCheckpoint,
        perms: &PermissionToken,
    ) -> Result<&'static str, EditError> {
        tracing::warn!(
            tx = %abandoned.transaction_id,
            checkpoint_id = %abandoned.checkpoint_id,
            "abandon reconcile invoked"
        );
        check_expiry(perms)?;
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
            return Ok("cleared_committed");
        }
        if state != Some(TxState::Open) {
            *lock(&self.abandoned)? = None;
            return Ok("cleared_not_open");
        }
        // V17: a whole-tree restore would rewrite deny-glob paths through the
        // sandbox binds, so refuse before touching the worktree.
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, perms).await?;
        refuse_tracked_denied(&self.path_policy, &tracked)?;
        check_expiry(perms)?;
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
            return Err(rollback_failed(
                abandoned.transaction_id,
                abandoned.checkpoint_id,
                err.to_string(),
            ));
        }
        self.verify_restored_digest(
            abandoned.transaction_id,
            abandoned.checkpoint_id,
            &abandoned.pre_digest,
            perms,
        )
        .await?;
        self.mark_rolled_back(abandoned.transaction_id)?;
        Ok("restored")
    }

    async fn rollback_record(
        &self,
        record: TxRecord,
        perms: &PermissionToken,
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
        // RFC §5.11 step 5 arms `abandoned` before the restore so a dropped
        // rollback future is reconciled like a dropped apply. §6.4 forbids
        // reconcile from restoring a `Committed` transaction, so the record must
        // leave `Committed` first: otherwise a cancelled rollback of a committed
        // edit clears the abandon record and leaves the tree half restored.
        {
            let mut slot = lock(&self.abandoned)?;
            if let Some(stored) = lock(&self.tx_store)?.get_mut(&record.id) {
                stored.state = TxState::Open;
            }
            *slot = Some(abandoned);
        }
        check_expiry(perms)?;
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
            return Err(rollback_failed(
                record.id,
                record.checkpoint_id,
                err.to_string(),
            ));
        }
        self.verify_restored_digest(record.id, record.checkpoint_id, &record.pre_digest, perms)
            .await?;
        self.mark_rolled_back(record.id)
    }

    /// Restore must land exactly on the pre-image (RFC §5.11 step 7).
    ///
    /// Leaves the transaction `Open` with its abandon record on any failure so
    /// the next apply/rollback reconcile can retry.
    async fn verify_restored_digest(
        &self,
        tx: TransactionId,
        checkpoint_id: CheckpointId,
        pre_digest: &WorkspaceDigest,
        perms: &PermissionToken,
    ) -> Result<(), EditError> {
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, perms)
            .await
            .map_err(|err| rollback_failed(tx, checkpoint_id, err.to_string()))?;
        let digest = self
            .workspace_digest("post", &tracked, &[])
            .await
            .map_err(|err| rollback_failed(tx, checkpoint_id, err.to_string()))?;
        if &digest != pre_digest {
            return Err(rollback_failed(
                tx,
                checkpoint_id,
                "digest mismatch after restore",
            ));
        }
        Ok(())
    }

    /// Mark `tx` rolled back and clear its abandon record (verified restore only).
    fn mark_rolled_back(&self, tx: TransactionId) -> Result<(), EditError> {
        let mut slot = lock(&self.abandoned)?;
        if let Some(record) = lock(&self.tx_store)?.get_mut(&tx) {
            record.state = TxState::RolledBack;
        }
        if slot.as_ref().is_some_and(|a| a.transaction_id == tx) {
            *slot = None;
        }
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
        let span = tracing::info_span!("edit.validate", file_count = Empty, error = Empty);
        let result = self.validate_inner(req, ctx).instrument(span.clone()).await;
        record_error(&span, result)
    }

    async fn apply(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditTransaction, EditError> {
        let span = tracing::info_span!(
            "edit.apply",
            tx.id = Empty,
            file_count = Empty,
            checkpoint_id = Empty,
            error = Empty
        );
        let result = self.apply_inner(req, ctx).instrument(span.clone()).await;
        record_error(&span, result)
    }

    async fn rollback(&self, tx: TransactionId, ctx: &EditContext) -> Result<(), EditError> {
        let span = tracing::info_span!(
            "edit.rollback",
            tx.id = %tx,
            checkpoint_id = Empty,
            sha = Empty,
            files_touched = Empty,
            error = Empty
        );
        let result = self.rollback_inner(tx, ctx).instrument(span.clone()).await;
        record_error(&span, result)
    }
}

impl GitEditEngine {
    async fn validate_inner(
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
        tracing::Span::current().record("file_count", files_touched.len());
        Ok(EditValidation { files_touched })
    }

    async fn apply_inner(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditTransaction, EditError> {
        let guard = Arc::clone(&self.write_lock).lock_owned().await;
        check_expiry(&ctx.perms)?;
        check_run(ctx.run_id, &ctx.perms)?;
        self.reconcile_abandoned(&ctx.perms).await?;
        reject_semantic(&req)?;
        let EditRequest::TextPatch { patch } = req else {
            unreachable!("semantic rejected");
        };
        let files_touched = validate_patchset_local(&patch, &self.path_policy, &ctx.perms)?;
        self.require_git(&ctx.perms)?;
        let span = tracing::Span::current();
        span.record("file_count", files_touched.len());

        // Serialized before the first mutation so that no failure path after the
        // checkpoint has to unwind work which could have happened up front.
        let patch_bytes = serde_json::to_vec(&patch)
            .map_err(|e| EditError::Internal(format!("patch serde: {e}")))?;
        let patch_hash = Digest::sha256(&patch_bytes);

        let (head_sha, tracked) =
            prepare_repo_for_edit(self.broker.as_ref(), &self.path_policy, &ctx.perms, &patch)
                .await?;
        let pre_digest = self.workspace_digest("pre", &tracked, &[]).await?;

        let tx_id = TransactionId::new();
        let checkpoint_id = CheckpointId::new();
        span.record("tx.id", display(tx_id));
        span.record("checkpoint_id", display(checkpoint_id));
        check_expiry(&ctx.perms)?;
        let CreatedCheckpoint {
            checkpoint_sha,
            head_sha,
            tracked,
        } = self
            .create_checkpoint_spanned(&ctx.perms, checkpoint_id, &head_sha, tracked)
            .await?;
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
            created_at: Timestamp::now(),
        };
        {
            let mut slot = lock(&self.abandoned)?;
            lock(&self.tx_store)?.insert(tx_id, record);
            *slot = Some(AbandonedCheckpoint {
                transaction_id: tx_id,
                checkpoint_id,
                checkpoint_sha: checkpoint_sha.clone(),
                created_paths: Vec::new(),
                temp_paths: Vec::new(),
                created_dirs: Vec::new(),
                pre_digest: pre_digest.clone(),
            });
        }
        let state = ApplyState {
            tx_id,
            checkpoint_id,
            checkpoint_sha,
            pre_digest,
            tracked,
        };

        // From here the workspace is mutated: every failure restores first. The
        // write-lock guard round-trips through the blocking task, so it is still
        // held for the commit steps below.
        let (_guard, apply_result, progress_error) =
            match self.spawn_apply(patch, &state, &ctx.perms, guard).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    // The task panicked, taking the guard with it: retake the
                    // lock before touching the workspace again. Its progress
                    // callbacks already recorded whatever it created, so restore
                    // against the abandon record rather than nothing.
                    let _guard = Arc::clone(&self.write_lock).lock_owned().await;
                    let partial = self.partial_from_abandoned(tx_id);
                    return Err(self
                        .restore_after_failure(err, &state, &partial, &ctx.perms)
                        .await);
                }
            };
        let file_out = match apply_result {
            Ok(out) => out,
            Err(failure) => {
                let failure = *failure;
                return Err(self
                    .restore_after_failure(failure.error, &state, &failure.partial, &ctx.perms)
                    .await);
            }
        };
        if let Some(err) = progress_error {
            tracing::error!(
                tx = %tx_id,
                "apply path bookkeeping failed; restoring checkpoint"
            );
            return Err(self
                .restore_after_failure(err, &state, &file_out, &ctx.perms)
                .await);
        }

        let (post_digest, artifact_id) = match self
            .stage_commit_inputs(ctx, &state, &file_out, patch_bytes)
            .await
        {
            Ok(staged) => staged,
            Err(err) => {
                return Err(self
                    .restore_after_failure(err, &state, &file_out, &ctx.perms)
                    .await)
            }
        };

        let facts = EditAppliedFacts {
            files_touched,
            post_digest,
            artifact_id,
            patch_hash,
        };
        // Encoded before the commit point: after CAS + `Committed` + `abandoned
        // = None`, no serde or event failure may turn a committed edit into
        // `Err` (RFC §5.1 / AC 2), so nothing below may use `?`.
        let event = edit_applied_event(ctx, &state, &facts);

        let committed = match self.commit_transaction(&state, &facts, &file_out) {
            Ok(committed) => committed,
            Err(err) => {
                return Err(self
                    .restore_after_failure(err, &state, &file_out, &ctx.perms)
                    .await)
            }
        };

        if let Some(event) = event {
            if let Err(err) = self.events.append_session(event).await {
                let mapped = crate::edit::map_error::map_event(err);
                tracing::error!(error = %mapped, tx = %tx_id, "EditApplied append failed after commit");
            }
        }
        tracing::info!(
            tx = %tx_id,
            checkpoint_id = %checkpoint_id,
            file_count = facts.files_touched.len(),
            "edit applied"
        );
        Ok(committed)
    }

    async fn rollback_inner(&self, tx: TransactionId, ctx: &EditContext) -> Result<(), EditError> {
        let _guard = self.write_lock.lock().await;
        check_expiry(&ctx.perms)?;
        check_run(ctx.run_id, &ctx.perms)?;
        self.reconcile_abandoned(&ctx.perms).await?;
        let record = lock(&self.tx_store)?
            .get(&tx)
            .cloned()
            .ok_or(EditError::UnknownTransaction(tx))?;
        self.require_git(&ctx.perms)?;
        let span = tracing::Span::current();
        span.record("checkpoint_id", display(record.checkpoint_id));
        span.record("sha", record.checkpoint_sha.as_str());
        span.record("files_touched", record.files_touched.len());
        let tracked = tracked_set(self.broker.as_ref(), &self.path_policy, &ctx.perms).await?;
        refuse_tracked_denied(&self.path_policy, &tracked)?;

        match record.state {
            TxState::RolledBack => {
                let digest = self.workspace_digest("post", &tracked, &[]).await?;
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
                let digest = self
                    .workspace_digest("post", &tracked, &record.created_paths)
                    .await?;
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

    /// Apply the patch on the blocking pool, keeping progress bookkeeping there.
    ///
    /// Patch application is synchronous file I/O over every patched file, so it
    /// belongs off the async worker. Two things travel into the task because
    /// blocking work cannot be aborted and may outlive a cancelled `apply`:
    ///
    /// - the abandon record (an `Arc`), so the task records the created / temp
    ///   paths it is responsible for, and reconcile can clean up after it;
    /// - the `write_lock` guard, so no other mutation starts while an orphaned
    ///   task is still writing. It is returned to the caller for the commit steps.
    #[allow(clippy::type_complexity)]
    async fn spawn_apply(
        &self,
        patch: PatchSet,
        state: &ApplyState,
        perms: &PermissionToken,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<
        (
            tokio::sync::OwnedMutexGuard<()>,
            Result<FileApplyOutcome, Box<FileApplyError>>,
            Option<EditError>,
        ),
        EditError,
    > {
        let policy = self.path_policy.clone();
        let perms = perms.clone();
        let abandoned = Arc::clone(&self.abandoned);
        let tx_id = state.tx_id;
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let mut progress_error: Option<EditError> = None;
            let result = apply_file_patches(&patch, &policy, &perms, tx_id, |progress| {
                if progress_error.is_some() {
                    return;
                }
                match lock(&abandoned) {
                    Ok(mut abandoned) => {
                        if let Some(a) = abandoned.as_mut() {
                            match progress {
                                ApplyProgress::TempPath(path) => a.temp_paths.push(path),
                                ApplyProgress::CreatedPath(path) => a.created_paths.push(path),
                                ApplyProgress::CreatedDir(path) => a.created_dirs.push(path),
                            }
                        }
                    }
                    Err(err) => progress_error = Some(err),
                }
            });
            (guard, result, progress_error)
        })
        .await
        .map_err(|err| EditError::Internal(format!("apply task: {err}")))
    }

    /// Paths the armed abandon record says this transaction created.
    ///
    /// Used when the apply task died without returning an outcome: a poisoned or
    /// cleared record yields an empty set, which still restores tracked files.
    fn partial_from_abandoned(&self, tx: TransactionId) -> FileApplyOutcome {
        match lock(&self.abandoned) {
            Ok(slot) => slot
                .as_ref()
                .filter(|a| a.transaction_id == tx)
                .map(|a| FileApplyOutcome {
                    files_touched: Vec::new(),
                    created_paths: a.created_paths.clone(),
                    temp_paths: a.temp_paths.clone(),
                    created_dirs: a.created_dirs.clone(),
                })
                .unwrap_or_default(),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    tx = %tx,
                    "apply path bookkeeping unreadable; restoring tracked files only"
                );
                FileApplyOutcome::default()
            }
        }
    }

    /// Post-mutate, pre-commit work: record paths, post digest, CAS put.
    ///
    /// Every fallible step lives here so `apply` has exactly one restore-on-error
    /// seam between the first mutation and the commit point.
    async fn stage_commit_inputs(
        &self,
        ctx: &EditContext,
        state: &ApplyState,
        file_out: &FileApplyOutcome,
        patch_bytes: Vec<u8>,
    ) -> Result<(WorkspaceDigest, ArtifactId), EditError> {
        update_record_paths(&self.tx_store, state.tx_id, file_out)?;
        let post_digest = self
            .workspace_digest("post", &state.tracked, &file_out.created_paths)
            .await?;
        let mut labels = serde_json::Map::new();
        labels.insert("transaction_id".into(), json!(state.tx_id.to_string()));
        labels.insert(
            "checkpoint_id".into(),
            json!(state.checkpoint_id.to_string()),
        );
        labels.insert("pre_digest".into(), json!(state.pre_digest.tree.as_hex()));
        labels.insert("post_digest".into(), json!(post_digest.tree.as_hex()));
        labels.insert("schema".into(), json!("alloy.patch_set.v1"));
        let artifact_id = self
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
            .map_err(map_store)?;
        Ok((post_digest, artifact_id))
    }

    /// Commit point: `TxRecord = Committed` and the abandon record cleared.
    fn commit_transaction(
        &self,
        state: &ApplyState,
        facts: &EditAppliedFacts,
        file_out: &FileApplyOutcome,
    ) -> Result<EditTransaction, EditError> {
        let mut slot = lock(&self.abandoned)?;
        let mut txs = lock(&self.tx_store)?;
        let record = txs
            .get_mut(&state.tx_id)
            .ok_or_else(|| EditError::Internal("missing tx record".into()))?;
        record.state = TxState::Committed;
        record.post_digest = Some(facts.post_digest.clone());
        record.patch_artifact_id = Some(facts.artifact_id);
        record.patch_content_hash = Some(facts.patch_hash.clone());
        record.files_touched = facts.files_touched.clone();
        record.created_paths = file_out.created_paths.clone();
        record.temp_paths = file_out.temp_paths.clone();
        record.created_dirs = file_out.created_dirs.clone();
        *slot = None;
        Ok(Self::record_to_tx(record))
    }

    /// Restore the checkpoint after a mutation failed, then return the error.
    ///
    /// Returns the restore failure when the restore itself fails, so the caller
    /// always surfaces the most serious problem.
    async fn restore_after_failure(
        &self,
        err: EditError,
        state: &ApplyState,
        paths: &FileApplyOutcome,
        perms: &PermissionToken,
    ) -> EditError {
        match self.restore_checkpoint_verified(state, paths, perms).await {
            Ok(()) => err,
            Err(restore_err) => restore_err,
        }
    }

    /// Restore `state`'s checkpoint and prove the workspace equals `pre_digest`.
    ///
    /// FailedDirty on any failure: the transaction stays `Open`, the abandon
    /// record and checkpoint ref survive, and `TokenExpired` is preserved as the
    /// public "recover under a fresh token" signal (RFC §5.2 / AC 39).
    async fn restore_checkpoint_verified(
        &self,
        state: &ApplyState,
        paths: &FileApplyOutcome,
        perms: &PermissionToken,
    ) -> Result<(), EditError> {
        // V21: never start a restore with an expired token.
        check_expiry(perms).inspect_err(|_| {
            tracing::error!(
                tx = %state.tx_id,
                checkpoint_id = %state.checkpoint_id,
                "FailedDirty: token expired before restore; reconcile under a fresh token"
            );
        })?;
        match restore_checkpoint(
            self.broker.as_ref(),
            &self.path_policy,
            perms,
            &state.checkpoint_sha,
            &paths.created_paths,
            &paths.temp_paths,
            &paths.created_dirs,
        )
        .await
        {
            Ok(()) => {}
            Err(EditError::TokenExpired) => {
                tracing::error!(
                    tx = %state.tx_id,
                    checkpoint_id = %state.checkpoint_id,
                    "FailedDirty: token expired during restore; reconcile under a fresh token"
                );
                return Err(EditError::TokenExpired);
            }
            Err(err) => {
                return Err(rollback_failed(
                    state.tx_id,
                    state.checkpoint_id,
                    err.to_string(),
                ))
            }
        }
        self.verify_restored_digest(state.tx_id, state.checkpoint_id, &state.pre_digest, perms)
            .await?;
        self.mark_rolled_back(state.tx_id)
    }
}

/// Values only known once the mutation and CAS put succeeded.
struct EditAppliedFacts {
    files_touched: Vec<String>,
    post_digest: WorkspaceDigest,
    artifact_id: ArtifactId,
    patch_hash: Digest,
}

/// Encode the `EditApplied` event, or `None` when there is nothing to send.
///
/// A serde failure is logged and dropped: the caller is about to commit, and a
/// committed edit MUST NOT surface as `Err` (RFC §5.1).
fn edit_applied_event(
    ctx: &EditContext,
    state: &ApplyState,
    facts: &EditAppliedFacts,
) -> Option<NewSessionEvent> {
    let session_id: SessionId = ctx.session_id?;
    let payload = EditAppliedPayload {
        schema: EDIT_APPLIED_SCHEMA.into(),
        transaction_id: state.tx_id,
        checkpoint_id: state.checkpoint_id,
        checkpoint_sha: state.checkpoint_sha.clone(),
        pre_digest: state.pre_digest.clone(),
        post_digest: facts.post_digest.clone(),
        files_touched: facts.files_touched.clone(),
        patch_artifact_id: facts.artifact_id,
        patch_content_hash: facts.patch_hash.clone(),
        request_kind: EditRequestKind::TextPatch,
    };
    match serde_json::to_value(payload) {
        Ok(payload) => Some(NewSessionEvent {
            session_id,
            run_id: Some(ctx.run_id.unwrap_or(ctx.perms.run_id)),
            type_: SessionEventType::EditApplied,
            payload,
        }),
        Err(err) => {
            tracing::error!(
                error = %err,
                tx = %state.tx_id,
                "EditApplied payload encoding failed; skipping event"
            );
            None
        }
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

/// Refuse to restore while a deny-glob path is tracked (V17 / RFC §5.11).
fn refuse_tracked_denied(policy: &PathPolicy, tracked: &BTreeSet<String>) -> Result<(), EditError> {
    ensure_no_tracked_denied(policy, tracked).inspect_err(|err| {
        tracing::warn!(error = %err, "refusing restore: deny-glob path is tracked");
    })
}

/// Build and log a `RollbackFailed`; the checkpoint ref is always retained.
fn rollback_failed(
    tx: TransactionId,
    checkpoint_id: CheckpointId,
    detail: impl Into<String>,
) -> EditError {
    let err = EditError::RollbackFailed {
        tx,
        checkpoint_id,
        detail: detail.into(),
    };
    tracing::error!(error = %err, "rollback failed; checkpoint ref retained");
    err
}

fn record_error<T>(span: &tracing::Span, result: Result<T, EditError>) -> Result<T, EditError> {
    if let Err(err) = &result {
        span.record("error", display(err));
    }
    result
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, EditError> {
    mutex
        .lock()
        .map_err(|_| EditError::Internal("mutex poisoned".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{
        RecordingSandboxBroker, SandboxBackend, SandboxExecResult, SandboxProfile,
    };
    use alloy_runtime::{
        ArtifactBlob, ArtifactMeta, ExecAllow, Glob, Grant, InMemoryEventSink, ProfileId, RunId,
        StoreError,
    };
    use std::path::Path;

    /// Accepts every put; nothing here exercises CAS storage itself.
    struct NoopArtifacts;

    #[async_trait]
    impl ArtifactStore for NoopArtifacts {
        async fn put(&self, _req: ArtifactPut) -> Result<ArtifactId, StoreError> {
            Ok(ArtifactId::new())
        }

        async fn get(&self, _id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
            Err(StoreError::NotFound("test".into()))
        }

        async fn meta(&self, _id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
            Err(StoreError::NotFound("test".into()))
        }

        async fn get_by_digest(&self, _digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
            Ok(None)
        }

        async fn delete(&self, _id: ArtifactId) -> Result<(), StoreError> {
            Ok(())
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        jail: PathBuf,
        broker: Arc<RecordingSandboxBroker>,
        engine: GitEditEngine,
    }

    /// A hermetic engine over a temp jail with a scripted broker.
    ///
    /// `git` preflight resolves against a stand-in binary on a temp trusted
    /// root, so these tests never depend on the host's git.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let jail = root.path().join("repo");
        std::fs::create_dir_all(&jail).unwrap();
        let jail = jail.canonicalize().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_stand_in_git(&bin.join("git"));

        let profile = SandboxProfile::default_for_jail(jail.clone()).unwrap();
        let policy = PathPolicy::from_profile(&profile, Vec::new()).unwrap();
        let broker = Arc::new(RecordingSandboxBroker::new(profile));
        let engine = GitEditEngine::new(GitEditEngineConfig::new(
            broker.clone() as Arc<dyn SandboxBroker>,
            policy,
            vec![bin],
            Arc::new(NoopArtifacts),
            Arc::new(InMemoryEventSink::new()),
        ))
        .unwrap();
        Fixture {
            _root: root,
            jail,
            broker,
            engine,
        }
    }

    fn write_stand_in_git(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    fn edit_token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: vec![
                Grant::FsWrite(Glob("**".into())),
                Grant::GitWrite,
                Grant::Exec(ExecAllow {
                    binary: "git".into(),
                    args_glob: None,
                }),
            ],
            expires: None,
            run_id: RunId::new(),
        }
    }

    fn exit(code: i32) -> Result<SandboxExecResult, crate::sandbox::SandboxError> {
        Ok(SandboxExecResult::synthetic(
            Some(code),
            None,
            SandboxBackend::Landlock,
            Digest::sha256(b"policy"),
        ))
    }

    fn ls_files(stdout: &[u8]) -> Result<SandboxExecResult, crate::sandbox::SandboxError> {
        Ok(SandboxExecResult::synthetic(
            Some(0),
            None,
            SandboxBackend::Landlock,
            Digest::sha256(b"policy"),
        )
        .with_stdio(stdout.to_vec(), Vec::new()))
    }

    fn tx_record(state: TxState, pre_digest: WorkspaceDigest) -> TxRecord {
        TxRecord {
            id: TransactionId::new(),
            state,
            checkpoint_id: CheckpointId::new(),
            checkpoint_sha: "0".repeat(40),
            head_sha_at_checkpoint: "0".repeat(40),
            pre_digest,
            post_digest: None,
            files_touched: vec!["a.txt".into()],
            created_paths: Vec::new(),
            temp_paths: Vec::new(),
            created_dirs: Vec::new(),
            patch_artifact_id: None,
            patch_content_hash: None,
            session_id: None,
            run_id: None,
            created_at: Timestamp::now(),
        }
    }

    fn abandon_for(record: &TxRecord) -> AbandonedCheckpoint {
        AbandonedCheckpoint {
            transaction_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            created_paths: Vec::new(),
            temp_paths: Vec::new(),
            created_dirs: Vec::new(),
            pre_digest: record.pre_digest.clone(),
        }
    }

    fn arm(fx: &Fixture, record: &TxRecord) {
        fx.engine
            .tx_store
            .lock()
            .unwrap()
            .insert(record.id, record.clone());
        *fx.engine.abandoned.lock().unwrap() = Some(abandon_for(record));
    }

    fn state_of(fx: &Fixture, tx: TransactionId) -> TxState {
        fx.engine.tx_store.lock().unwrap()[&tx].state
    }

    fn tracked(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    /// Digest of the current jail contents, as the engine would compute it.
    async fn jail_digest(fx: &Fixture, paths: &[&str]) -> WorkspaceDigest {
        fx.engine
            .workspace_digest("pre", &tracked(paths), &[])
            .await
            .unwrap()
    }

    #[test]
    fn newest_tx_filters_state_and_breaks_ties_by_uuid() {
        let digest = WorkspaceDigest {
            tree: Digest::sha256(b""),
            file_count: 0,
            total_bytes: 0,
        };
        let mut open_a = tx_record(TxState::Open, digest.clone());
        let mut open_b = tx_record(TxState::Open, digest.clone());
        let committed = tx_record(TxState::Committed, digest);
        // Identical timestamps isolate the UUID tie-break from wall-clock luck.
        open_b.created_at = open_a.created_at.clone();
        open_a.created_at = open_b.created_at.clone();
        let expected = if open_a.id.as_uuid() > open_b.id.as_uuid() {
            open_a.id
        } else {
            open_b.id
        };
        let mut txs = HashMap::new();
        for record in [open_a, open_b, committed.clone()] {
            txs.insert(record.id, record);
        }
        assert_eq!(newest_tx_with_state(&txs, TxState::Open), Some(expected));
        assert_eq!(
            newest_tx_with_state(&txs, TxState::Committed),
            Some(committed.id)
        );
    }

    /// The blocking apply task owns the write lock while it runs, so a cancelled
    /// `apply` cannot let a second mutation start under an orphaned task.
    #[tokio::test]
    async fn apply_task_owns_the_write_lock_and_hands_it_back() {
        let fx = fixture();
        let record = tx_record(TxState::Open, jail_digest(&fx, &[]).await);
        let state = ApplyState {
            tx_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            pre_digest: record.pre_digest.clone(),
            tracked: BTreeSet::new(),
        };
        let guard = Arc::clone(&fx.engine.write_lock).lock_owned().await;

        let (returned, result, progress_error) = fx
            .engine
            .spawn_apply(PatchSet { files: vec![] }, &state, &edit_token(), guard)
            .await
            .unwrap();

        assert!(result.is_ok());
        assert!(progress_error.is_none());
        assert!(
            fx.engine.write_lock.try_lock().is_err(),
            "the guard must survive the round trip through the blocking task"
        );
        drop(returned);
        assert!(fx.engine.write_lock.try_lock().is_ok());
    }

    /// A blocking apply task that dies mid-mutation leaves its progress in the
    /// abandon record; the restore path has to use it or created files linger.
    #[tokio::test]
    async fn partial_paths_come_from_the_armed_abandon_record() {
        let fx = fixture();
        let record = tx_record(TxState::Open, jail_digest(&fx, &[]).await);
        arm(&fx, &record);
        {
            let mut slot = fx.engine.abandoned.lock().unwrap();
            let armed = slot.as_mut().unwrap();
            armed.created_paths.push("new.txt".into());
            armed.temp_paths.push(".new.txt.alloy-tmp-1".into());
            armed.created_dirs.push("sub".into());
        }

        let partial = fx.engine.partial_from_abandoned(record.id);

        assert_eq!(partial.created_paths, vec!["new.txt"]);
        assert_eq!(partial.temp_paths, vec![".new.txt.alloy-tmp-1"]);
        assert_eq!(partial.created_dirs, vec!["sub"]);
        assert!(fx
            .engine
            .partial_from_abandoned(TransactionId::new())
            .created_paths
            .is_empty());
    }

    #[tokio::test]
    async fn reconcile_refuses_restore_when_deny_glob_path_is_tracked() {
        let fx = fixture();
        let record = tx_record(TxState::Open, jail_digest(&fx, &[]).await);
        arm(&fx, &record);
        fx.broker.push(ls_files(b".env\0a.txt\0"));

        let err = fx
            .engine
            .reconcile_abandoned(&edit_token())
            .await
            .unwrap_err();

        assert!(matches!(err, EditError::TrackedDeniedPath { ref path } if path == ".env"));
        assert_eq!(
            fx.broker.recorded().len(),
            1,
            "only the tracked-set probe may run; restore must not"
        );
        assert_eq!(state_of(&fx, record.id), TxState::Open);
        assert!(
            fx.engine.abandoned.lock().unwrap().is_some(),
            "the abandon record survives so recovery can retry"
        );
    }

    #[tokio::test]
    async fn reconcile_refuses_expired_token_before_any_git() {
        let fx = fixture();
        let record = tx_record(TxState::Open, jail_digest(&fx, &[]).await);
        arm(&fx, &record);
        let perms = PermissionToken {
            expires: Some(Timestamp::now()),
            ..edit_token()
        };

        let err = fx.engine.reconcile_abandoned(&perms).await.unwrap_err();

        assert!(matches!(err, EditError::TokenExpired));
        assert!(
            fx.broker.recorded().is_empty(),
            "no git before expiry check"
        );
        assert!(fx.engine.abandoned.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn reconcile_clears_without_restore_for_committed_tx() {
        let fx = fixture();
        let record = tx_record(TxState::Committed, jail_digest(&fx, &[]).await);
        arm(&fx, &record);

        fx.engine.reconcile_abandoned(&edit_token()).await.unwrap();

        assert!(
            fx.broker.recorded().is_empty(),
            "committed edits never restore"
        );
        assert_eq!(state_of(&fx, record.id), TxState::Committed);
        assert!(fx.engine.abandoned.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn restore_after_mutate_requires_digest_match_before_clearing_abandon() {
        let fx = fixture();
        std::fs::write(fx.jail.join("a.txt"), b"restored\n").unwrap();
        let mut record = tx_record(TxState::Open, jail_digest(&fx, &["a.txt"]).await);
        // Pre-image the restore cannot reach: the file on disk says otherwise.
        record.pre_digest.total_bytes += 1;
        arm(&fx, &record);
        let state = ApplyState {
            tx_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            pre_digest: record.pre_digest.clone(),
            tracked: tracked(&["a.txt"]),
        };
        fx.broker.push(exit(0)); // git restore
        fx.broker.push(ls_files(b"a.txt\0")); // tracked set for the verify

        let err = fx
            .engine
            .restore_checkpoint_verified(&state, &FileApplyOutcome::default(), &edit_token())
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            EditError::RollbackFailed { ref detail, .. } if detail == "digest mismatch after restore"
        ));
        assert_eq!(state_of(&fx, record.id), TxState::Open);
        assert!(
            fx.engine.abandoned.lock().unwrap().is_some(),
            "FailedDirty keeps Open + abandoned"
        );
    }

    #[tokio::test]
    async fn restore_after_mutate_marks_rolled_back_on_verified_restore() {
        let fx = fixture();
        std::fs::write(fx.jail.join("a.txt"), b"restored\n").unwrap();
        let record = tx_record(TxState::Open, jail_digest(&fx, &["a.txt"]).await);
        arm(&fx, &record);
        let state = ApplyState {
            tx_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            pre_digest: record.pre_digest.clone(),
            tracked: tracked(&["a.txt"]),
        };
        fx.broker.push(exit(0));
        fx.broker.push(ls_files(b"a.txt\0"));

        fx.engine
            .restore_checkpoint_verified(&state, &FileApplyOutcome::default(), &edit_token())
            .await
            .unwrap();

        assert_eq!(state_of(&fx, record.id), TxState::RolledBack);
        assert!(fx.engine.abandoned.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn restore_after_mutate_refuses_expired_token_and_stays_dirty() {
        let fx = fixture();
        let record = tx_record(TxState::Open, jail_digest(&fx, &[]).await);
        arm(&fx, &record);
        let state = ApplyState {
            tx_id: record.id,
            checkpoint_id: record.checkpoint_id,
            checkpoint_sha: record.checkpoint_sha.clone(),
            pre_digest: record.pre_digest.clone(),
            tracked: BTreeSet::new(),
        };
        let perms = PermissionToken {
            expires: Some(Timestamp::now()),
            ..edit_token()
        };

        let err = fx
            .engine
            .restore_checkpoint_verified(&state, &FileApplyOutcome::default(), &perms)
            .await
            .unwrap_err();

        assert!(matches!(err, EditError::TokenExpired));
        assert!(fx.broker.recorded().is_empty());
        assert_eq!(state_of(&fx, record.id), TxState::Open);
        assert!(fx.engine.abandoned.lock().unwrap().is_some());
    }

    /// A cancelled or failed rollback of a *committed* edit must stay
    /// reconcilable: the record leaves `Committed` before the restore starts, so
    /// the next reconcile restores instead of clearing the abandon record.
    #[tokio::test]
    async fn failed_rollback_of_committed_tx_is_reconcilable() {
        let fx = fixture();
        std::fs::write(fx.jail.join("a.txt"), b"restored\n").unwrap();
        let mut record = tx_record(TxState::Committed, jail_digest(&fx, &["a.txt"]).await);
        record.state = TxState::Committed;
        fx.engine
            .tx_store
            .lock()
            .unwrap()
            .insert(record.id, record.clone());
        fx.broker.push(exit(1)); // git restore fails

        let err = fx
            .engine
            .rollback_record(record.clone(), &edit_token())
            .await
            .unwrap_err();

        assert!(matches!(err, EditError::RollbackFailed { .. }));
        assert_eq!(
            state_of(&fx, record.id),
            TxState::Open,
            "mid-rollback state must not stay Committed, or reconcile would skip the restore"
        );
        let armed = fx.engine.abandoned.lock().unwrap().clone().unwrap();
        assert_eq!(armed.transaction_id, record.id);

        // The next apply/rollback reconcile now finishes the rollback.
        fx.broker.push(ls_files(b"a.txt\0")); // tracked-deny scan
        fx.broker.push(exit(0)); // git restore
        fx.broker.push(ls_files(b"a.txt\0")); // digest verify
        fx.engine.reconcile_abandoned(&edit_token()).await.unwrap();

        assert_eq!(state_of(&fx, record.id), TxState::RolledBack);
        assert!(fx.engine.abandoned.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn rollback_clears_abandon_only_after_digest_verify() {
        let fx = fixture();
        std::fs::write(fx.jail.join("a.txt"), b"restored\n").unwrap();
        let mut record = tx_record(TxState::Open, jail_digest(&fx, &["a.txt"]).await);
        record.pre_digest.file_count += 1;
        fx.engine
            .tx_store
            .lock()
            .unwrap()
            .insert(record.id, record.clone());
        fx.broker.push(exit(0)); // git restore
        fx.broker.push(ls_files(b"a.txt\0")); // digest verify

        let err = fx
            .engine
            .rollback_record(record.clone(), &edit_token())
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            EditError::RollbackFailed { ref detail, .. } if detail == "digest mismatch after restore"
        ));
        assert_eq!(state_of(&fx, record.id), TxState::Open);
        assert!(
            fx.engine.abandoned.lock().unwrap().is_some(),
            "abandon record must survive a failed digest verification"
        );
    }
}
