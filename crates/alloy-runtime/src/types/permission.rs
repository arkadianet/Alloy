//! Permission token shapes (Architecture V2 Appendix E).

use serde::{Deserialize, Serialize};

use super::ids::{ProfileId, RunId, Timestamp};

/// Glob pattern wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Glob(pub String);

/// Allowed executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecAllow {
    /// Binary name or path.
    pub binary: String,
    /// Optional argv glob.
    pub args_glob: Option<String>,
}

/// Allowed network host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAllow {
    /// Hostname or host pattern.
    pub host: String,
}

/// Capability grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    /// Read filesystem paths matching glob.
    FsRead(Glob),
    /// Write filesystem paths matching glob.
    FsWrite(Glob),
    /// Execute allowed binary.
    Exec(ExecAllow),
    /// Network egress to host.
    Network(HostAllow),
    /// Git write operations.
    GitWrite,
}

/// Permission token presented to tools (authorizer lands in later RFCs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionToken {
    /// Active profile.
    pub profile: ProfileId,
    /// Granted capabilities.
    pub grants: Vec<Grant>,
    /// Optional expiry.
    pub expires: Option<Timestamp>,
    /// Bound run id.
    pub run_id: RunId,
}
