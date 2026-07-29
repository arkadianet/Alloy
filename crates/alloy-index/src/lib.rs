//! Alloy ProjectGraph index (RFC-0011, RFC-0014, Architecture V2 §7/§16).
//!
//! The **deep** ProjectGraph: a derived, wipeable SQLite cache of workspace
//! structure (Workspace/Crate/Module/Item nodes; `Defines`/`Imports` plus best-effort
//! `References`/`Calls`/`Impls` edges)
//! plus diagnostic and fix ingest records, behind the `ProjectGraph` trait
//! seam that lives in `alloy-runtime::graph`.
//!
//! Facts come from `Cargo.toml` manifests, a bounded, sorted, symlink-free
//! filesystem walk, and the RFC-0014 `syn` item/import pass — no subprocess,
//! no network. `SimilarFixes` reads the recorded fixes back since
//! amendment A-0011-5 (Q6); `Refs`/`Impls`/`Callers` answer from the
//! best-effort `references`/`calls`/`impls` edges the syn pass records
//! since amendment A-0011-6 (Q4, Q5).
//! [`lang::rust::RustBackend`] implements the `LanguageBackend` seam over
//! this store (RFC-0014 LC2).

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod db;
mod ingest;
pub mod lang;
mod layout;
mod metrics;
mod migrate;
mod query;
mod store;

pub use lang::rust::{read_toolchain_hints, RustBackend};
pub use layout::{GraphLayout, GraphOpenOptions, IngestLimits};
// `GraphOpenOptions.synchronous` is this type; re-exported so configuring an
// open does not require a direct `alloy-runtime` import.
pub use alloy_runtime::SqliteSynchronous;
pub use metrics::GraphMetricsSnapshot;
pub use store::SqliteProjectGraph;
