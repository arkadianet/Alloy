//! Worker-facing read-only graph handle (RFC-0011 §3.8, V2 §9).
//!
//! Rule SEC1: this type exposes only `new`, `null`, `query`, `version`,
//! `Clone` and `Debug`. There is deliberately no mutation method and no
//! accessor that yields the underlying `Arc<dyn ProjectGraph>`.

use std::sync::Arc;

use super::{GraphError, GraphQuery, GraphVersion, GraphView, ProjectGraph};

/// Read-only query handle handed to capability workers (V2 §9).
#[derive(Clone)]
pub struct GraphViewHandle {
    graph: Arc<dyn ProjectGraph>,
}

impl GraphViewHandle {
    /// Wrap a graph for read-only use.
    #[must_use]
    pub fn new(graph: Arc<dyn ProjectGraph>) -> Self {
        Self { graph }
    }

    /// A handle backed by [`super::NullProjectGraph`] (pre-wiring, tests,
    /// `--no-graph`).
    #[must_use]
    pub fn null() -> Self {
        Self {
            graph: super::null_graph_arc(),
        }
    }

    /// Run a read query.
    pub async fn query(&self, q: GraphQuery) -> Result<GraphView, GraphError> {
        self.graph.query(q).await
    }

    /// Current graph version.
    pub async fn version(&self) -> Result<GraphVersion, GraphError> {
        self.graph.version().await
    }
}

impl std::fmt::Debug for GraphViewHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Opaque: never leaks the implementation behind the handle.
        f.write_str("GraphViewHandle")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_handle_reads_empty_and_reports_version_zero() {
        let h = GraphViewHandle::null();
        let view = h
            .query(GraphQuery::Symbol { path: "a".into() })
            .await
            .unwrap();
        assert!(view.is_empty());
        assert_eq!(h.version().await.unwrap(), GraphVersion(0));
        assert_eq!(format!("{h:?}"), "GraphViewHandle");
    }
}
