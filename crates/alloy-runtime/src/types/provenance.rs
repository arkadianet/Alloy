//! Session provenance and consent (research §7.11 item 4; RFC-0018 will
//! extend this).
//!
//! Stored as `sessions.provenance_json`. The reason this exists now rather
//! than with RFC-0018: consent cannot be obtained retroactively, and the
//! repository identity / SPDX state at the time of a session cannot be
//! reconstructed later. A missing record means "no consent, provenance
//! unknown" — the fail-closed reading.

use serde::{Deserialize, Serialize};

/// Version of the [`SessionProvenance`] JSON shape.
pub const PROVENANCE_SCHEMA_VERSION: u32 = 1;

/// What the operator has consented to for this session's captured data.
///
/// Everything defaults to `false`: absence of consent is never consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConsentRecord {
    /// Captured trajectories from this session may enter a training corpus.
    pub corpus_ok: bool,
    /// Captured trajectories may be shared outside the operator's own
    /// infrastructure (implies nothing about `corpus_ok`).
    pub share_ok: bool,
}

/// Repository identity and consent captured at session creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProvenance {
    /// Shape version — bump on any field change.
    pub schema_version: u32,
    /// Repository identity (a remote URL or a stable local identifier),
    /// when known.
    pub repo: Option<String>,
    /// Workspace HEAD commit SHA at session creation, when in a git repo.
    pub head_sha: Option<String>,
    /// SPDX license expressions observed in the workspace at session
    /// creation (e.g. `MIT OR Apache-2.0`). Empty means "not scanned".
    pub spdx: Vec<String>,
    /// Operator consent for captured data.
    pub consent: ConsentRecord,
}

impl SessionProvenance {
    /// A fail-closed record: nothing known, nothing consented.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            schema_version: PROVENANCE_SCHEMA_VERSION,
            repo: None,
            head_sha: None,
            spdx: Vec::new(),
            consent: ConsentRecord::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_defaults_to_false_everywhere() {
        let p = SessionProvenance::unknown();
        assert!(!p.consent.corpus_ok);
        assert!(!p.consent.share_ok);
        // A consent field missing from stored JSON also reads as false.
        let sparse: ConsentRecord = serde_json::from_str("{}").unwrap();
        assert!(!sparse.corpus_ok && !sparse.share_ok);
    }

    #[test]
    fn serde_round_trip() {
        let p = SessionProvenance {
            schema_version: PROVENANCE_SCHEMA_VERSION,
            repo: Some("https://github.com/arkadianet/Alloy".into()),
            head_sha: Some("4b8dfd7".into()),
            spdx: vec!["MIT OR Apache-2.0".into()],
            consent: ConsentRecord {
                corpus_ok: true,
                share_ok: false,
            },
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(p, serde_json::from_str(&json).unwrap());
    }
}
