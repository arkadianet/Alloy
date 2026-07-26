//! Pure lazy-disclosure filter/sort/cap over `&[ToolView]` (RFC-0006 §4.1 / §5.4).
//!
//! Kept free of IO and host state so the cap is unit-testable with synthetic
//! views.
//!
//! Author: arkadianet

use std::collections::BTreeMap;

use alloy_runtime::{ToolName, ToolSelector, ToolView};

/// Hard cap on tools returned by a single disclosure.
///
/// A safety valve against schema-tax context exhaustion, not pagination.
pub const MAX_TOOLS_PER_DISCLOSURE: usize = 32;

/// Filter `views` by `selectors`, dedupe by name, sort ascending, cap at
/// [`MAX_TOOLS_PER_DISCLOSURE`].
///
/// Returns the disclosed views and whether the cap truncated the result.
/// Empty `selectors` disclose nothing. Selectors matching no view are ignored.
pub(crate) fn disclose(views: &[ToolView], selectors: &[ToolSelector]) -> (Vec<ToolView>, bool) {
    if selectors.is_empty() {
        return (Vec::new(), false);
    }

    let mut out: BTreeMap<ToolName, ToolView> = BTreeMap::new();
    for selector in selectors {
        match selector {
            ToolSelector::Name { name } => {
                if let Some(view) = views.iter().find(|v| &v.name == name) {
                    out.insert(view.name.clone(), view.clone());
                }
            }
            ToolSelector::Tag { tag } => {
                for view in views.iter().filter(|v| v.tags.iter().any(|t| t == tag)) {
                    out.insert(view.name.clone(), view.clone());
                }
            }
        }
    }

    let mut disclosed: Vec<ToolView> = out.into_values().collect();
    if disclosed.len() > MAX_TOOLS_PER_DISCLOSURE {
        disclosed.truncate(MAX_TOOLS_PER_DISCLOSURE);
        return (disclosed, true);
    }
    (disclosed, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(name: &str, tags: &[&str]) -> ToolView {
        ToolView::new(
            ToolName::new(name).unwrap(),
            "d",
            json!({}),
            tags.iter().map(|t| (*t).to_string()).collect(),
            true,
        )
    }

    fn builtin_views() -> Vec<ToolView> {
        vec![
            view("cargo_check", &["sel.compiler"]),
            view("cargo_test", &["sel.test"]),
            view("fs_read", &["sel.fs"]),
            view("apply_patch", &["sel.edit"]),
        ]
    }

    #[test]
    fn disclose_empty_selectors_empty() {
        let (out, truncated) = disclose(&builtin_views(), &[]);
        assert!(out.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn disclose_by_name() {
        let sel = [ToolSelector::name(ToolName::new("fs_read").unwrap())];
        let (out, _) = disclose(&builtin_views(), &sel);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name.as_str(), "fs_read");
    }

    #[test]
    fn disclose_by_tag_compiler() {
        let sel = [ToolSelector::tag("sel.compiler")];
        let (out, _) = disclose(&builtin_views(), &sel);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name.as_str(), "cargo_check");
    }

    #[test]
    fn disclose_unknown_name_ignored() {
        let sel = [
            ToolSelector::name(ToolName::new("graph_query").unwrap()),
            ToolSelector::tag("sel.nothing"),
        ];
        let (out, truncated) = disclose(&builtin_views(), &sel);
        assert!(out.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn disclose_dedupe_and_sort() {
        let sel = [
            ToolSelector::tag("sel.fs"),
            ToolSelector::name(ToolName::new("cargo_check").unwrap()),
            ToolSelector::name(ToolName::new("fs_read").unwrap()),
            ToolSelector::tag("sel.compiler"),
        ];
        let (out, _) = disclose(&builtin_views(), &sel);
        let names: Vec<&str> = out.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["cargo_check", "fs_read"]);
    }

    #[test]
    fn disclose_cap_truncates() {
        let views: Vec<ToolView> = (0..40)
            .map(|i| view(&format!("tool_{i:03}"), &["sel.many"]))
            .collect();
        let (out, truncated) = disclose(&views, &[ToolSelector::tag("sel.many")]);
        assert_eq!(out.len(), MAX_TOOLS_PER_DISCLOSURE);
        assert!(truncated);
        // Sorted ascending, so the cap keeps the lexicographically first 32.
        assert_eq!(out[0].name.as_str(), "tool_000");
        assert_eq!(out[31].name.as_str(), "tool_031");
    }
}
