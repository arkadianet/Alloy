//! EditEngine shared IR and trait (RFC-0008).
//!
//! Types and the [`EditEngine`] trait live here so `alloy-tools` can implement
//! the engine without introducing a sixth crate or a reverse dependency.
//! Concrete `GitEditEngine` / MCP adapter live in `alloy-tools::edit`.
//!
//! Author: arkadianet

mod engine;
mod error;
mod types;

pub use engine::EditEngine;
pub use error::EditError;
pub use types::{
    EditAppliedPayload, EditContext, EditRequest, EditRequestKind, EditTransaction,
    EditValidation, FilePatch, Hunk, PatchSet, SemanticEditOp, TxState, WorkspaceDigest,
    EDIT_APPLIED_SCHEMA,
};
