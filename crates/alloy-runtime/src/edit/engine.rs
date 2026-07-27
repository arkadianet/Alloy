//! [`EditEngine`] trait (RFC-0008 §3.5).
//!
//! Author: arkadianet

use async_trait::async_trait;

use crate::edit::error::EditError;
use crate::edit::types::{EditContext, EditRequest, EditTransaction, EditValidation};
use crate::types::ids::TransactionId;

/// Transactional workspace edit apply + rollback.
///
/// Implementors MUST be `Send + Sync`. Methods are async and MAY perform
/// filesystem and sandboxed git I/O. The trait object is shared as
/// `Arc<dyn EditEngine>`.
///
/// **Permissions are explicit arguments** via [`EditContext`]. There is no
/// ambient token slot, no `task_local!`, and no `apply_with_perms` twin API.
#[async_trait]
pub trait EditEngine: Send + Sync {
    /// Validate `req` without mutating the workspace or creating a checkpoint.
    ///
    /// MUST enforce the **validate** column of RFC-0008 §5.5.1.
    /// MUST NOT write files, refs, CAS edit artifacts, or session events.
    /// MUST NOT run abandon reconcile (that is `apply`/`rollback` only).
    async fn validate(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditValidation, EditError>;

    /// Validate and apply `req`. On success returns a committed transaction.
    async fn apply(
        &self,
        req: EditRequest,
        ctx: &EditContext,
    ) -> Result<EditTransaction, EditError>;

    /// Restore the checkpoint associated with `tx` when eligible (RFC-0008 §5.11).
    async fn rollback(&self, tx: TransactionId, ctx: &EditContext) -> Result<(), EditError>;
}
