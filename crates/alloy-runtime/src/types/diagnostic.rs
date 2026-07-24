//! Diagnostic and failure IR (Architecture V2 Appendix D).

use serde::{Deserialize, Serialize};

use super::ids::{DiagnosticId, Digest, NodeId};

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    /// Hard error.
    Error,
    /// Warning.
    Warning,
    /// Informational note.
    Note,
    /// Help/suggestion.
    Help,
}

/// Source span reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanRef {
    /// Path relative to workspace or absolute.
    pub path: String,
    /// Start line (1-based).
    pub start_line: u32,
    /// Start column (1-based).
    pub start_col: u32,
    /// End line (1-based).
    pub end_line: u32,
    /// End column (1-based).
    pub end_col: u32,
}

/// Structured compiler/tool diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    /// Diagnostic id.
    pub id: DiagnosticId,
    /// Optional error code (`E0502`, …).
    pub code: Option<String>,
    /// Severity.
    pub level: DiagnosticLevel,
    /// Human-readable message.
    pub message: String,
    /// Related spans.
    pub spans: Vec<SpanRef>,
    /// Nested diagnostics.
    pub children: Vec<DiagnosticEvent>,
    /// Optional package name.
    pub package: Option<String>,
    /// Stable fingerprint for dedupe.
    pub fingerprint: Digest,
    /// Optional raw JSON from the tool.
    pub raw_json: Option<serde_json::Value>,
}

/// Failure classification for scheduler/repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Compile failure.
    Compile,
    /// Test failure.
    Test,
    /// Tool/MCP failure.
    Tool,
    /// Model/provider failure.
    Model,
    /// Budget exhausted.
    Budget,
    /// Approval denied/timeout.
    Approval,
    /// Internal runtime error.
    Internal,
    /// Deadline exceeded.
    Timeout,
    /// Cancellation.
    Cancelled,
}

/// Structured node failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureIr {
    /// Failing node.
    pub node: NodeId,
    /// Error class.
    pub error_class: ErrorClass,
    /// Related diagnostics.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Free-form notes.
    pub notes: String,
}
