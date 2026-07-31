//! Live stack-driver options (RFC-0016 §5.9 / RFC-0012 weight arms).
//!
//! Kept additive so concurrent `stack.rs` work can merge against a thin
//! options surface. `context_profile: None` preserves the historical
//! `NullContextEngine` + keyed [`ScriptedProvider`] smoke path;
//! `Some(profile)` selects [`DefaultContextEngine`] and FIFO
//! [`RecordingModelProvider`] (fingerprints differ across weight arms).
//!
//! Author: arkadianet

use alloy_runtime::{ContextProfile, PlannerMode};

/// Options for [`super::stack::run_live_with_options`].
#[derive(Debug, Clone)]
pub struct StackLiveOptions {
    /// Planner mode (`Template` default; `Llm` is CapabilityPlanProposer +
    /// PlanningWorker smoke — not RFC-0017 §12.4 flip evidence).
    pub planner: PlannerMode,
    /// When `Some`, wire [`alloy_runtime::DefaultContextEngine`] with this
    /// profile (weight-measurement arms). When `None`, keep
    /// [`alloy_runtime::NullContextEngine`] (integration-smoke default).
    pub context_profile: Option<ContextProfile>,
    /// Bound forwarded to runtime config + [`alloy_runtime::GenerationPolicy`].
    pub max_repair_generations: u32,
}

impl Default for StackLiveOptions {
    fn default() -> Self {
        Self {
            planner: PlannerMode::Template,
            context_profile: None,
            max_repair_generations: 2,
        }
    }
}

impl StackLiveOptions {
    /// Template planner, null context (historical smoke default).
    #[must_use]
    pub fn template() -> Self {
        Self::default()
    }

    /// Template planner with an injectable context profile (weight arms).
    #[must_use]
    pub fn with_context_profile(profile: ContextProfile) -> Self {
        Self {
            context_profile: Some(profile),
            ..Self::default()
        }
    }

    /// Builder: set planner mode.
    #[must_use]
    pub fn planner(mut self, mode: PlannerMode) -> Self {
        self.planner = mode;
        self
    }

    /// Builder: set max repair generations.
    #[must_use]
    pub fn max_repair_generations(mut self, n: u32) -> Self {
        self.max_repair_generations = n;
        self
    }
}

/// Split into lines and whether the text ends with a trailing newline.
///
/// `str::lines()` drops the final-newline distinction; unified diffs need
/// `\ No newline at end of file` when a side lacks a terminating newline.
#[must_use]
#[allow(dead_code)] // pinned by unit tests; live path prefers recordings/
pub(crate) fn split_diff_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let ends_with_newline = text.ends_with('\n');
    (text.lines().collect(), ends_with_newline)
}

/// Full-file unified diff suitable for `apply_patch` / `GitEditEngine`.
///
/// Handles files with and without a final newline (emits
/// `\ No newline at end of file` per side when needed).
#[must_use]
#[allow(dead_code)] // pinned by unit tests; live path prefers recordings/
pub(crate) fn unified_diff(rel_path: &str, before: &str, after: &str) -> String {
    let (old_lines, old_nl) = split_diff_lines(before);
    let (new_lines, new_nl) = split_diff_lines(after);
    let old_n = old_lines.len();
    let new_n = new_lines.len();
    let mut out = String::new();
    out.push_str(&format!("--- a/{rel_path}\n+++ b/{rel_path}\n"));
    match (old_n, new_n) {
        (0, 0) => out.push_str("@@ -0,0 +0,0 @@\n"),
        (0, n) => out.push_str(&format!("@@ -0,0 +1,{n} @@\n")),
        (o, 0) => out.push_str(&format!("@@ -1,{o} +0,0 @@\n")),
        (o, n) => out.push_str(&format!("@@ -1,{o} +1,{n} @@\n")),
    }
    for line in &old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    if !old_lines.is_empty() && !old_nl {
        out.push_str("\\ No newline at end of file\n");
    }
    for line in &new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if !new_lines.is_empty() && !new_nl {
        out.push_str("\\ No newline at end of file\n");
    }
    out
}

#[cfg(test)]
mod unified_diff_tests {
    use super::unified_diff;

    #[test]
    fn preserves_trailing_newline_on_both_sides() {
        let diff = unified_diff("a.rs", "old\n", "new\n");
        assert!(diff.contains("-old\n"));
        assert!(diff.contains("+new\n"));
        assert!(!diff.contains("\\ No newline at end of file"));
    }

    #[test]
    fn marks_missing_final_newline_on_each_side() {
        let diff = unified_diff("a.rs", "old", "new");
        assert!(
            diff.contains(
                "-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n"
            ),
            "{diff}"
        );
    }

    #[test]
    fn marks_only_the_side_without_newline() {
        let diff = unified_diff("a.rs", "old\n", "new");
        assert!(
            diff.contains("-old\n+new\n\\ No newline at end of file\n"),
            "{diff}"
        );
    }
}
