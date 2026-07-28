//! Artifacts domain builder (RFC-0012 §4.4).
//!
//! Unpinned artifacts contribute metadata + digest only — the prompt
//! carries *references* and RFC-0006's tools are the fetch (V2 §20 R5).
//! Bodies are admitted for unpinned artifacts only when `kind` is `Patch`
//! and the body fits the remaining allowance.

use time::format_description::well_known::Rfc3339;

use crate::storage::{ArtifactKind, ArtifactMeta, ArtifactStore, StoreError};
use crate::types::ids::ArtifactId;

use super::conversation::EventArtifactRef;
use super::render::{bound_bytes, sanitize_line, sanitize_untrusted};
use super::types::{Degradation, DegradationReason, DomainId};

/// One artifact candidate: metadata always, body when admitted.
#[derive(Debug, Clone)]
pub(super) struct ArtifactCandidate {
    /// Artifact id.
    pub id: ArtifactId,
    /// Store metadata.
    pub meta: ArtifactMeta,
    /// Pinned via `must_include` (B11).
    pub pinned: bool,
    /// Sanitised body, fetched for pins and admitted `Patch` kinds.
    pub body: Option<String>,
}

/// Raw Artifacts inputs before clamping.
#[derive(Debug, Default)]
pub(super) struct ArtifactsRaw {
    /// Candidates in D12 order, pins first.
    pub candidates: Vec<ArtifactCandidate>,
    /// Candidates that could not be resolved (counted as omitted).
    pub unresolved: usize,
    /// Store failures, as degradations (E1).
    pub degradations: Vec<Degradation>,
}

/// `true` when the D12 kind filter admits this artifact. `PromptPack` and
/// `Other(_)` are excluded — no prompt-in-prompt recursion, no
/// unclassified bodies (SEC10) — and this exclusion outranks B11.
pub(super) fn kind_admitted(kind: &ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::Patch | ArtifactKind::Log | ArtifactKind::Decision | ArtifactKind::Blob
    )
}

/// Stable lowercase label for a rendered artifact line.
pub(super) fn kind_label(kind: &ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Blob => "blob",
        ArtifactKind::Patch => "patch",
        ArtifactKind::Log => "log",
        ArtifactKind::PromptPack => "prompt_pack",
        ArtifactKind::Decision => "decision",
        ArtifactKind::Other(_) => "other",
    }
}

/// Resolve unpinned candidates from event references and predecessor
/// outputs; order by D12 `(created_at DESC, ArtifactId ASC)`.
pub(super) async fn fetch_unpinned(
    store: &dyn ArtifactStore,
    refs: &[EventArtifactRef],
    predecessor_ids: &[ArtifactId],
    already_pinned: &[ArtifactId],
) -> ArtifactsRaw {
    let mut raw = ArtifactsRaw::default();
    let mut ids: Vec<ArtifactId> = Vec::new();

    for r in refs {
        match r {
            EventArtifactRef::Id(id) => ids.push(*id),
            EventArtifactRef::ContentDigest(digest) => match store.get_by_digest(digest).await {
                Ok(Some(id)) => ids.push(id),
                Ok(None) => raw.unresolved += 1,
                Err(e) => raw.degradations.push(store_degradation(&e)),
            },
        }
    }
    ids.extend_from_slice(predecessor_ids);
    ids.sort();
    ids.dedup();
    ids.retain(|id| !already_pinned.contains(id));

    for id in ids {
        match store.meta(id).await {
            Ok(meta) => {
                if kind_admitted(&meta.kind) {
                    raw.candidates.push(ArtifactCandidate {
                        id,
                        meta,
                        pinned: false,
                        body: None,
                    });
                } else {
                    raw.unresolved += 1;
                }
            }
            Err(StoreError::NotFound(_)) => raw.unresolved += 1,
            Err(e) => raw.degradations.push(store_degradation(&e)),
        }
    }

    // D12: created_at DESC (recorded RFC-3339 value, never a wall clock),
    // ArtifactId ASC.
    raw.candidates.sort_by(|a, b| {
        b.meta
            .created_at
            .0
            .cmp(&a.meta.created_at.0)
            .then_with(|| a.id.cmp(&b.id))
    });
    raw
}

/// Fetch an unpinned `Patch` body when the caller decides it fits (§4.4).
pub(super) async fn fetch_patch_body(store: &dyn ArtifactStore, id: ArtifactId) -> Option<String> {
    let blob = store.get(id).await.ok()?;
    if blob.bytes.iter().take(8 * 1024).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8(blob.bytes).ok()?;
    Some(sanitize_untrusted(&text))
}

/// Render the metadata line for one artifact.
#[must_use]
pub(super) fn render_meta_line(c: &ArtifactCandidate) -> String {
    let created = c
        .meta
        .created_at
        .0
        .format(&Rfc3339)
        .unwrap_or_else(|_| "-".into());
    let labels = if c.meta.labels.is_empty() {
        String::new()
    } else {
        let json = serde_json::to_string(&c.meta.labels).unwrap_or_default();
        format!(" labels={}", bound_bytes(&sanitize_line(&json), 200))
    };
    format!(
        "{} {} {}B sha256:{} {created}{labels}",
        kind_label(&c.meta.kind),
        c.id,
        c.meta.byte_len,
        &c.meta.digest.as_hex()[..12],
    )
}

fn store_degradation(e: &StoreError) -> Degradation {
    Degradation {
        domain: DomainId::Artifacts,
        reason: DegradationReason::StoreUnavailable,
        detail: bound_bytes(&sanitize_line(&e.to_string()), 200),
    }
}
