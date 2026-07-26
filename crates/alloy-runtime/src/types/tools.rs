//! Shared MCP / tool IR (RFC-0006).
//!
//! Author: arkadianet

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{IdError, NodeId, RunId, SessionId, Timestamp};

/// Catalog tool name (`cargo_check`, `fs_read`, …).
///
/// Validation (enforced by [`ToolName::new`] **and** by `Deserialize`):
/// non-empty, ≤128 bytes, ASCII `[a-z0-9_]` only.
/// Length **and** charset failures both return [`IdError::InvalidName`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    /// Construct a validated tool name.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() || s.len() > 128 {
            return Err(IdError::InvalidName);
        }
        if !s
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        {
            return Err(IdError::InvalidName);
        }
        Ok(Self(s))
    }

    /// Borrow the name string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ToolName::new(s).map_err(serde::de::Error::custom)
    }
}

/// Lazy-disclosure selector (capability `required_tools` / host `tools_for`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolSelector {
    /// Exact tool name.
    Name {
        /// Tool name.
        name: ToolName,
    },
    /// Tag / group id (e.g. `sel.compiler`). Opaque, case-sensitive, exact equality.
    Tag {
        /// Tag string.
        tag: String,
    },
}

impl ToolSelector {
    /// Name selector.
    #[must_use]
    pub fn name(name: ToolName) -> Self {
        Self::Name { name }
    }

    /// Tag selector.
    #[must_use]
    pub fn tag(tag: impl Into<String>) -> Self {
        Self::Tag { tag: tag.into() }
    }
}

/// One tool invocation request (model or adapter → host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCall {
    /// Tool to invoke.
    pub name: ToolName,
    /// JSON arguments matching the tool's input schema.
    pub arguments: Value,
    /// Optional call id for correlation.
    pub call_id: Option<String>,
    /// Optional session attribution.
    pub session: Option<SessionId>,
    /// Optional run attribution.
    pub run: Option<RunId>,
    /// Optional node attribution.
    pub node: Option<NodeId>,
}

impl ToolCall {
    /// Build a call with no attribution.
    #[must_use]
    pub fn new(name: ToolName, arguments: Value) -> Self {
        Self {
            name,
            arguments,
            call_id: None,
            session: None,
            run: None,
            node: None,
        }
    }

    /// Attach a call id.
    #[must_use]
    pub fn with_call_id(mut self, id: impl Into<String>) -> Self {
        self.call_id = Some(id.into());
        self
    }

    /// Attach session/run/node attribution.
    #[must_use]
    pub fn with_attribution(
        mut self,
        session: Option<SessionId>,
        run: Option<RunId>,
        node: Option<NodeId>,
    ) -> Self {
        self.session = session;
        self.run = run;
        self.node = node;
        self
    }
}

/// Disclosed tool schema view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolView {
    /// Tool name.
    pub name: ToolName,
    /// Human description.
    pub description: String,
    /// JSON Schema object for arguments.
    pub input_schema: Value,
    /// Disclosure tags (sorted ascending at construction).
    pub tags: Vec<String>,
    /// `true` for in-process builtins.
    pub builtin: bool,
}

impl ToolView {
    /// Constructor for tests / eval fixtures (`#[non_exhaustive]`).
    #[must_use]
    pub fn new(
        name: ToolName,
        description: impl Into<String>,
        input_schema: Value,
        tags: Vec<String>,
        builtin: bool,
    ) -> Self {
        let mut tags = tags;
        tags.sort();
        tags.dedup();
        Self {
            name,
            description: description.into(),
            input_schema,
            tags,
            builtin,
        }
    }
}

/// Successful or tool-level-failed invocation payload.
///
/// `is_error` / `error` are private so callers cannot break
/// `is_error == error.is_some()` by field assignment.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolResult {
    /// Tool name.
    pub name: ToolName,
    /// Optional call id echo.
    pub call_id: Option<String>,
    /// Structured content.
    pub content: Value,
    is_error: bool,
    error: Option<ToolError>,
    /// Wall time inside the host dispatch (ms).
    pub duration_ms: u64,
}

impl ToolResult {
    /// Success result.
    #[must_use]
    pub fn ok(name: ToolName, content: Value, duration_ms: u64) -> Self {
        Self {
            name,
            call_id: None,
            content,
            is_error: false,
            error: None,
            duration_ms,
        }
    }

    /// Tool-level error result (still `Ok` at the MCP boundary).
    #[must_use]
    pub fn err(name: ToolName, content: Value, error: ToolError, duration_ms: u64) -> Self {
        Self {
            name,
            call_id: None,
            content,
            is_error: true,
            error: Some(error),
            duration_ms,
        }
    }

    /// Attach call id.
    #[must_use]
    pub fn with_call_id(mut self, id: Option<String>) -> Self {
        self.call_id = id;
        self
    }

    /// Whether this is a tool-level error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// Borrow the tool error if present.
    #[must_use]
    pub fn error(&self) -> Option<&ToolError> {
        self.error.as_ref()
    }

    /// Replace content while preserving the ok/err discriminant.
    #[must_use]
    pub fn with_content(mut self, content: Value) -> Self {
        self.content = content;
        self
    }
}

#[derive(Deserialize)]
struct ToolResultDe {
    name: ToolName,
    #[serde(default)]
    call_id: Option<String>,
    content: Value,
    is_error: bool,
    #[serde(default)]
    error: Option<ToolError>,
    duration_ms: u64,
}

impl<'de> Deserialize<'de> for ToolResult {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = ToolResultDe::deserialize(d)?;
        if raw.is_error != raw.error.is_some() {
            return Err(serde::de::Error::custom(
                "ToolResult invariant violated: is_error must equal error.is_some()",
            ));
        }
        Ok(Self {
            name: raw.name,
            call_id: raw.call_id,
            content: raw.content,
            is_error: raw.is_error,
            error: raw.error,
            duration_ms: raw.duration_ms,
        })
    }
}

/// Tool-level failure taxonomy (consumed by RFC-0010 retry policy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// Transient infrastructure / IO style failure worth retry.
    #[error("transient: {code}: {message}")]
    Transient {
        /// Stable code.
        code: String,
        /// Safe message.
        message: String,
    },
    /// Permanent business / policy / stub failure.
    #[error("permanent: {code}: {message}")]
    Permanent {
        /// Stable code.
        code: String,
        /// Safe message.
        message: String,
    },
    /// Arguments failed validation after schema parse (backend patch body, etc.).
    #[error("invalid_args: {message}")]
    InvalidArgs {
        /// Safe message.
        message: String,
    },
    /// Tool executed but the underlying command failed.
    #[error("execution_failed: exit={exit_code:?} signal={signal:?}: {message}")]
    ExecutionFailed {
        /// Process exit code if exited.
        exit_code: Option<i32>,
        /// Signal if killed.
        signal: Option<i32>,
        /// Safe message.
        message: String,
    },
}

/// Out-of-process server spec (unstable — MVP always Unsupported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct McpServerSpec {
    /// Logical server name.
    pub name: String,
    /// Transport.
    pub transport: McpTransport,
}

impl McpServerSpec {
    /// Constructor required because the struct is `#[non_exhaustive]`.
    #[must_use]
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
        }
    }
}

/// MCP transport (deferred — accepted only to return Unsupported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpTransport {
    /// Stdio subprocess (deferred).
    Stdio {
        /// Command.
        command: String,
        /// Args.
        args: Vec<String>,
    },
}

/// Token expiry helper used by MCP and sandbox consumers.
#[must_use]
pub fn token_expired(expires: Option<&Timestamp>) -> bool {
    match expires {
        Some(t) => Timestamp::now().0 >= t.0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_name_rejects_invalid() {
        assert!(ToolName::new("").is_err());
        assert!(ToolName::new("Cargo").is_err());
        assert!(ToolName::new("cargo-check").is_err());
        assert!(ToolName::new("cargo_check").is_ok());
    }

    #[test]
    fn tool_name_serde_validates() {
        assert!(serde_json::from_str::<ToolName>("\"Cargo Check\"").is_err());
        assert!(serde_json::from_str::<ToolName>("\"cargo_check\"").is_ok());
    }

    #[test]
    fn tool_result_invariant_deserialize() {
        let bad = json!({
            "name": "cargo_check",
            "content": {},
            "is_error": false,
            "error": { "kind": "permanent", "code": "x", "message": "y" },
            "duration_ms": 1
        });
        assert!(serde_json::from_value::<ToolResult>(bad).is_err());

        let bad2 = json!({
            "name": "cargo_check",
            "content": {},
            "is_error": true,
            "duration_ms": 1
        });
        assert!(serde_json::from_value::<ToolResult>(bad2).is_err());

        let ok = ToolResult::ok(ToolName::new("cargo_check").unwrap(), json!({}), 1);
        let round =
            serde_json::from_value::<ToolResult>(serde_json::to_value(&ok).unwrap()).unwrap();
        assert!(!round.is_error());
        assert!(round.error().is_none());
    }
}
