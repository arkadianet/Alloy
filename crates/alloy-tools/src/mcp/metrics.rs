//! Host counters and the readable snapshot (RFC-0006 §9.3).
//!
//! Author: arkadianet

use std::sync::atomic::{AtomicU64, Ordering};

/// Point-in-time host counters. Pattern matches `StorageMetricsSnapshot`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpMetricsSnapshot {
    /// Calls returning `Ok(ToolResult)` with `is_error == false`.
    pub calls_ok: u64,
    /// Calls returning `Ok(ToolResult)` with `is_error == true`.
    pub calls_tool_error: u64,
    /// Calls returning `Err(McpError)` to a still-polled caller.
    pub calls_mcp_error: u64,
    /// Subset of `calls_mcp_error` that were `PermissionDenied`.
    pub denials: u64,
    /// Disclosures truncated at `MAX_TOOLS_PER_DISCLOSURE`.
    pub disclose_truncated: u64,
    /// Currently admitted calls (gauge).
    pub in_flight: u64,
}

/// Atomic counters backing [`McpMetricsSnapshot`].
#[derive(Debug, Default)]
pub(crate) struct McpMetrics {
    calls_ok: AtomicU64,
    calls_tool_error: AtomicU64,
    calls_mcp_error: AtomicU64,
    denials: AtomicU64,
    disclose_truncated: AtomicU64,
}

impl McpMetrics {
    pub(crate) fn call_ok(&self) {
        self.calls_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn call_tool_error(&self) {
        self.calls_tool_error.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn call_mcp_error(&self) {
        self.calls_mcp_error.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn denial(&self) {
        self.denials.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn disclose_truncated(&self) {
        self.disclose_truncated.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self, in_flight: u64) -> McpMetricsSnapshot {
        McpMetricsSnapshot {
            calls_ok: self.calls_ok.load(Ordering::Relaxed),
            calls_tool_error: self.calls_tool_error.load(Ordering::Relaxed),
            calls_mcp_error: self.calls_mcp_error.load(Ordering::Relaxed),
            denials: self.denials.load(Ordering::Relaxed),
            disclose_truncated: self.disclose_truncated.load(Ordering::Relaxed),
            in_flight,
        }
    }
}
