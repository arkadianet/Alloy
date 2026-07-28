//! Alloy ProjectGraph index (RFC-0011, Architecture V2 §7).
//!
//! The **thin** MVP ProjectGraph: a derived, wipeable SQLite cache of
//! workspace structure (Workspace/Crate/Module nodes, `Defines` edges) plus
//! diagnostic and fix ingest records, behind the `ProjectGraph` trait seam
//! that lives in `alloy-runtime::graph`.
//!
//! Facts come from `Cargo.toml` manifests and a bounded, sorted, symlink-free
//! filesystem walk — no subprocess, no network, no Rust parsing. `Item`
//! nodes, `Imports` edges, and the `Callers`/`SimilarFixes`/`Refs`/`Impls`
//! query answers are **Stub** surfaces reserved for the Beta deepening
//! (rules IN8, IN9, Q4–Q6).

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod db;
mod ingest;
mod layout;
mod metrics;
mod migrate;
mod query;
mod store;

pub use layout::{GraphLayout, GraphOpenOptions, IngestLimits};
pub use metrics::GraphMetricsSnapshot;
pub use store::SqliteProjectGraph;
