//! Opaque and catalog identifier types.

use serde::{Deserialize, Serialize};
use sha2::{Digest as Sha2Digest, Sha256};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Allocate a new random identifier.
            ///
            /// Explicit only — these IDs do **not** implement [`Default`] so
            /// `..Default::default()` cannot silently mint random UUIDs.
            #[must_use]
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Parse from a canonical UUID string.
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                Ok(Self(Uuid::parse_str(s)?))
            }

            /// Borrow the inner UUID.
            #[must_use]
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl TryFrom<&str> for $name {
            type Error = uuid::Error;

            fn try_from(s: &str) -> Result<Self, Self::Error> {
                Self::parse(s)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

macro_rules! name_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Construct a validated catalog name (1..=128 bytes).
            pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
                let s = s.into();
                if s.is_empty() || s.len() > 128 {
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

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $name::new(s).map_err(serde::de::Error::custom)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

uuid_id!(
    /// Session identifier.
    SessionId
);
uuid_id!(
    /// Run identifier.
    RunId
);
uuid_id!(
    /// Task DAG identifier.
    DagId
);
uuid_id!(
    /// DAG node identifier.
    NodeId
);
uuid_id!(
    /// Human/approval gate identifier.
    GateId
);
uuid_id!(
    /// Artifact blob identifier.
    ArtifactId
);
uuid_id!(
    /// Edit/transaction identifier.
    TransactionId
);
uuid_id!(
    /// Git checkpoint identifier.
    CheckpointId
);
uuid_id!(
    /// Project graph node identifier.
    GraphNodeId
);
uuid_id!(
    /// Diagnostic event identifier.
    DiagnosticId
);
uuid_id!(
    /// Out-of-process MCP server identifier (RFC-0006).
    ServerId
);

name_id!(
    /// Profile catalog id (`default`, `autonomous`, `readonly`).
    ProfileId
);
name_id!(
    /// Language backend catalog id (MVP: `rust`).
    LanguageId
);
name_id!(
    /// Capability catalog id (`repair`, `edit`, …).
    CapabilityId
);
name_id!(
    /// Model provider catalog id from router config.
    ProviderId
);
name_id!(
    /// Catalog id for a model endpoint row in `router.toml` (RFC-0007).
    EndpointId
);

/// Invalid catalog name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// Empty or overlong name.
    #[error("invalid name id")]
    InvalidName,
}

/// Monotonic graph schema/version token.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphVersion(pub u64);

/// Lowercase hex SHA-256 digest (64 chars).
///
/// Construct only via [`Digest::sha256`] / [`Digest::try_from_hex`]. Deserialize validates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Hash `bytes` with SHA-256 and return lowercase hex.
    #[must_use]
    pub fn sha256(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex::encode(hasher.finalize()))
    }

    /// Parse a lowercase hex SHA-256 string.
    pub fn try_from_hex(s: impl AsRef<str>) -> Result<Self, DigestError> {
        let s = s.as_ref();
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(DigestError::InvalidHex);
        }
        Ok(Self(s.to_owned()))
    }

    /// Borrow the hex string.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::try_from_hex(s).map_err(serde::de::Error::custom)
    }
}

/// Digest parse/validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    /// Not exactly 64 hex characters.
    #[error("digest must be 64 lowercase hex chars")]
    InvalidHex,
}

/// Append-only event sequence number (per session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

/// UTC timestamp (RFC3339 on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp(#[serde(with = "time::serde::rfc3339")] pub time::OffsetDateTime);

impl Timestamp {
    /// Current UTC time.
    #[must_use]
    pub fn now() -> Self {
        Self(time::OffsetDateTime::now_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_id_rejects_empty_and_overlong() {
        assert!(ProfileId::new("").is_err());
        assert!(ProfileId::new("a".repeat(129)).is_err());
        assert_eq!(ProfileId::new("default").unwrap().as_str(), "default");
    }

    #[test]
    fn name_id_serde_validates() {
        assert!(serde_json::from_str::<ProfileId>("\"\"").is_err());
        assert!(serde_json::from_str::<ProfileId>(&format!("\"{}\"", "x".repeat(129))).is_err());
        let id: ProfileId = serde_json::from_str("\"default\"").unwrap();
        assert_eq!(id.as_str(), "default");
    }

    #[test]
    fn digest_round_trip_and_rejects() {
        let d = Digest::sha256(b"alloy");
        assert_eq!(d.as_hex().len(), 64);
        let parsed = Digest::try_from_hex(d.as_hex()).unwrap();
        assert_eq!(parsed, d);
        assert!(Digest::try_from_hex("abcd").is_err());
        assert!(Digest::try_from_hex("A".repeat(64)).is_err());
        assert!(serde_json::from_str::<Digest>("\"not-a-digest\"").is_err());
        assert!(serde_json::from_str::<Digest>(&format!("\"{}\"", "A".repeat(64))).is_err());
        let ok: Digest = serde_json::from_str(&format!("\"{}\"", d.as_hex())).unwrap();
        assert_eq!(ok, d);
    }

    #[test]
    fn uuid_id_display() {
        let s = SessionId::new();
        assert!(!s.to_string().is_empty());
    }
}
