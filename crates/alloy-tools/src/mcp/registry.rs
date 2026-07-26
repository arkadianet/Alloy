//! Immutable builtin registration table (RFC-0006 §5.2).
//!
//! Built once in [`InProcessMcpHost::new`](crate::mcp::InProcessMcpHost::new)
//! and never mutated, so disclosure has no stale-cache problem and there is no
//! seam through which a worker could register `graph_query`, `bash`, or any
//! raw-shell tool.
//!
//! Author: arkadianet

use std::collections::BTreeMap;

use alloy_runtime::{ToolName, ToolView};

use crate::mcp::builtins::BuiltinToolId;
use crate::mcp::schema;

/// Name → handler table plus the disclosed views, sorted by name.
#[derive(Debug)]
pub(crate) struct Registry {
    views: Vec<ToolView>,
    by_name: BTreeMap<ToolName, BuiltinToolId>,
}

impl Registry {
    /// Register exactly the four MVP builtins.
    pub(crate) fn builtins() -> Self {
        let mut by_name = BTreeMap::new();
        let mut views = Vec::with_capacity(BuiltinToolId::ALL.len());
        for id in BuiltinToolId::ALL {
            let name = id.name();
            views.push(ToolView::new(
                name.clone(),
                schema::description(id),
                schema::input_schema(id),
                id.tags().iter().map(|t| (*t).to_string()).collect(),
                true,
            ));
            by_name.insert(name, id);
        }
        views.sort_by(|a, b| a.name.cmp(&b.name));
        Self { views, by_name }
    }

    /// Disclosable views (sorted ascending by name).
    pub(crate) fn views(&self) -> &[ToolView] {
        &self.views
    }

    /// Resolve a call name to its handler.
    pub(crate) fn lookup(&self, name: &ToolName) -> Option<BuiltinToolId> {
        self.by_name.get(name).copied()
    }

    /// Registered names, sorted ascending.
    pub(crate) fn names(&self) -> Vec<ToolName> {
        self.by_name.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_exactly_four_builtins() {
        let registry = Registry::builtins();
        let names: Vec<String> = registry.names().iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            vec!["apply_patch", "cargo_check", "cargo_test", "fs_read"]
        );
        assert_eq!(registry.views().len(), 4);
        assert!(registry.views().iter().all(|v| v.builtin));
    }

    #[test]
    fn no_forbidden_registrations() {
        let registry = Registry::builtins();
        for forbidden in [
            "graph_query",
            "bash",
            "sh",
            "shell",
            "raw_exec",
            "clippy_lint",
            "miri_test",
        ] {
            let name = ToolName::new(forbidden).unwrap();
            assert!(registry.lookup(&name).is_none(), "{forbidden} registered");
        }
    }

    #[test]
    fn views_carry_schema_and_description() {
        let registry = Registry::builtins();
        let view = registry
            .views()
            .iter()
            .find(|v| v.name.as_str() == "cargo_check")
            .unwrap();
        assert_eq!(
            view.description,
            "Run cargo check and return structured rustc messages"
        );
        assert_eq!(view.input_schema["additionalProperties"], false);
        assert_eq!(view.tags, vec!["sel.compiler".to_string()]);
    }
}
