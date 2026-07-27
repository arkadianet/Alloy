//! Git-backed EditEngine implementation (RFC-0008).
//!
//! The public surface is intentionally small: callers inject
//! [`GitEditEngine`] into MCP through [`EditEnginePatchBackend`]. Parser,
//! digest, checkpoint, and transaction helpers stay crate-private.
//!
//! Author: arkadianet

pub(crate) mod apply;
pub(crate) mod backend;
pub(crate) mod checkpoint;
pub(crate) mod digest;
pub(crate) mod engine;
pub(crate) mod map_error;
pub(crate) mod patch_parse;
pub(crate) mod tx;

pub use backend::EditEnginePatchBackend;
pub use engine::{GitEditEngine, GitEditEngineConfig};
