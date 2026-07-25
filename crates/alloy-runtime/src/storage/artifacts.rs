//! Filesystem content-addressed artifact store + SQLite index.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::codec::{parse_artifact_id, parse_run_id, parse_session_id, ts_from_text, ts_to_text};
use super::error::StoreError;
use super::gate::StorageGate;
use super::metrics::StorageMetrics;
use super::open::{spawn_db, DbHandle};
use super::paths::StorageLayout;
use crate::types::ids::{ArtifactId, Digest, RunId, SessionId, Timestamp};

/// Artifact kind classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Opaque blob.
    Blob,
    /// Patch / diff.
    Patch,
    /// Log body.
    Log,
    /// Prompt pack.
    PromptPack,
    /// Decision record body.
    Decision,
    /// Extension kind.
    Other(String),
}

/// Artifact metadata (no raw secrets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactMeta {
    /// Kind.
    pub kind: ArtifactKind,
    /// Optional content type.
    pub content_type: Option<String>,
    /// Byte length.
    pub byte_len: u64,
    /// Content digest.
    pub digest: Digest,
    /// Creation time.
    pub created_at: Timestamp,
    /// Optional owning session.
    pub session_id: Option<SessionId>,
    /// Optional owning run.
    pub run_id: Option<RunId>,
    /// Free-form non-secret metadata.
    pub labels: serde_json::Map<String, serde_json::Value>,
}

/// Artifact id + meta + bytes.
#[derive(Debug, Clone)]
pub struct ArtifactBlob {
    /// Artifact id.
    pub id: ArtifactId,
    /// Metadata.
    pub meta: ArtifactMeta,
    /// Raw bytes.
    pub bytes: Vec<u8>,
}

/// Request to store a new artifact.
#[derive(Debug, Clone)]
pub struct ArtifactPut {
    /// Raw bytes.
    pub bytes: Vec<u8>,
    /// Kind.
    pub kind: ArtifactKind,
    /// Optional content type.
    pub content_type: Option<String>,
    /// Optional session attribution.
    pub session_id: Option<SessionId>,
    /// Optional run attribution.
    pub run_id: Option<RunId>,
    /// Free-form labels.
    pub labels: serde_json::Map<String, serde_json::Value>,
}

/// Artifact store trait.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Put bytes; returns a new [`ArtifactId`] (MVP always allocates a new row).
    async fn put(&self, req: ArtifactPut) -> Result<ArtifactId, StoreError>;
    /// Get blob by id (verifies digest).
    async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError>;
    /// Metadata only.
    async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError>;
    /// Oldest non-deleted row for digest (`created_at ASC, id ASC`), or `None`.
    async fn get_by_digest(&self, digest: &Digest) -> Result<Option<ArtifactId>, StoreError>;
    /// Soft-delete the artifact index row only.
    ///
    /// CAS blobs are always retained; reference-counted reclamation / GC is
    /// out of scope for RFC-0002.
    async fn delete(&self, id: ArtifactId) -> Result<(), StoreError>;
}

/// Filesystem CAS + SQLite index.
pub struct FsArtifactStore {
    db: Arc<DbHandle>,
    layout: StorageLayout,
    metrics: Arc<StorageMetrics>,
    gate: Arc<StorageGate>,
}

impl FsArtifactStore {
    pub(crate) fn new(
        db: Arc<DbHandle>,
        layout: StorageLayout,
        metrics: Arc<StorageMetrics>,
        gate: Arc<StorageGate>,
    ) -> Self {
        Self {
            db,
            layout,
            metrics,
            gate,
        }
    }

    fn map_busy(&self, err: StoreError) -> StoreError {
        if matches!(err, StoreError::Busy) {
            self.metrics.inc_busy_errors();
        }
        err
    }
}

#[async_trait]
impl ArtifactStore for FsArtifactStore {
    #[tracing::instrument(
        skip(self, req),
        fields(
            byte_len = req.bytes.len(),
            digest = tracing::field::Empty,
            id = tracing::field::Empty
        ),
        name = "storage.artifact_put",
        level = "debug"
    )]
    async fn put(&self, req: ArtifactPut) -> Result<ArtifactId, StoreError> {
        let _permit = self.gate.enter()?;
        let layout = self.layout.clone();
        let byte_len = req.bytes.len() as u64;
        let bytes = req.bytes;
        let kind = req.kind;
        let content_type = req.content_type;
        let session_id = req.session_id;
        let run_id = req.run_id;
        let labels = req.labels;
        let db = Arc::clone(&self.db);
        let metrics = Arc::clone(&self.metrics);

        // Hash + CAS write on a blocking thread (unbounded CPU/IO off the reactor).
        let (digest, rel, id, created_at) = tokio::task::spawn_blocking(move || {
            let digest = Digest::sha256(&bytes);
            let digest_hex = digest.as_hex().to_owned();
            let rel = StorageLayout::cas_rel_path(&digest_hex)?;
            let cas_path = layout.cas_path(&digest_hex)?;
            let tmp_path = layout
                .artifacts_dir
                .join("tmp")
                .join(uuid::Uuid::new_v4().to_string());
            write_cas_blob(&layout, &cas_path, &tmp_path, &bytes, &digest_hex)?;
            Ok::<_, StoreError>((digest, rel, ArtifactId::new(), Timestamp::now()))
        })
        .await??;

        tracing::Span::current().record("digest", digest.as_hex());
        tracing::Span::current().record("id", id.to_string());

        let digest_hex = digest.as_hex().to_owned();
        let id_str = id.to_string();

        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let kind_json = serde_json::to_string(&kind)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let labels_json = serde_json::to_string(&labels)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let ts_text = ts_to_text(&created_at)?;
                conn.execute(
                    "INSERT INTO artifacts (
                        id, digest, kind, content_type, byte_len, rel_path,
                        session_id, run_id, labels_json, created_at, deleted_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
                    params![
                        id_str,
                        digest_hex,
                        kind_json,
                        content_type,
                        byte_len as i64,
                        rel,
                        session_id.map(|s| s.to_string()),
                        run_id.map(|r| r.to_string()),
                        labels_json,
                        ts_text,
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))?;

        metrics.inc_artifacts_put();
        Ok(id)
    }

    async fn get(&self, id: ArtifactId) -> Result<ArtifactBlob, StoreError> {
        let _permit = self.gate.enter()?;
        let meta = self.meta_unlocked(id).await?;
        let path = self.layout.cas_path(meta.digest.as_hex())?;
        let digest = meta.digest.clone();
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, StoreError> {
            if !path.is_file() {
                return Err(StoreError::Corrupt(format!(
                    "artifact index without blob at {}",
                    path.display()
                )));
            }
            let bytes = std::fs::read(&path)?;
            let actual = Digest::sha256(&bytes);
            if actual != digest {
                return Err(StoreError::DigestMismatch);
            }
            Ok(bytes)
        })
        .await??;
        self.metrics.inc_artifacts_get();
        Ok(ArtifactBlob { id, meta, bytes })
    }

    async fn meta(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
        let _permit = self.gate.enter()?;
        self.meta_unlocked(id).await
    }

    async fn get_by_digest(&self, digest: &Digest) -> Result<Option<ArtifactId>, StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let digest_hex = digest.as_hex().to_owned();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let id: Option<String> = conn
                    .query_row(
                        "SELECT id FROM artifacts
                         WHERE digest = ?1 AND deleted_at IS NULL
                         ORDER BY created_at ASC, id ASC LIMIT 1",
                        [&digest_hex],
                        |r| r.get(0),
                    )
                    .optional()?;
                match id {
                    None => Ok(None),
                    Some(s) => Ok(Some(parse_artifact_id(&s)?)),
                }
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }

    async fn delete(&self, id: ArtifactId) -> Result<(), StoreError> {
        let _permit = self.gate.enter()?;
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        let ts = Timestamp::now();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let ts_text = ts_to_text(&ts)?;
                let n = conn.execute(
                    "UPDATE artifacts SET deleted_at = ?1
                     WHERE id = ?2 AND deleted_at IS NULL",
                    params![ts_text, id_str],
                )?;
                if n == 0 {
                    return Err(StoreError::NotFound(format!("artifact {id}")));
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }
}

impl FsArtifactStore {
    /// Meta lookup without taking another gate permit (caller already holds one).
    async fn meta_unlocked(&self, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
        let db = Arc::clone(&self.db);
        let id_str = id.to_string();
        spawn_db(db, move |handle| {
            handle.with(|conn| {
                let row = conn
                    .query_row(
                        "SELECT digest, kind, content_type, byte_len, session_id, run_id,
                                labels_json, created_at, deleted_at
                         FROM artifacts WHERE id = ?1",
                        [&id_str],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, i64>(3)?,
                                r.get::<_, Option<String>>(4)?,
                                r.get::<_, Option<String>>(5)?,
                                r.get::<_, String>(6)?,
                                r.get::<_, String>(7)?,
                                r.get::<_, Option<String>>(8)?,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("artifact {id_str}")))?;

                let (
                    digest_hex,
                    kind_json,
                    content_type,
                    byte_len,
                    session_id,
                    run_id,
                    labels_json,
                    created_at,
                    deleted_at,
                ) = row;

                if deleted_at.is_some() {
                    return Err(StoreError::NotFound(format!("artifact {id_str} deleted")));
                }

                Ok(ArtifactMeta {
                    kind: serde_json::from_str(&kind_json)
                        .map_err(|e| StoreError::Corrupt(format!("artifact kind: {e}")))?,
                    content_type,
                    byte_len: byte_len as u64,
                    digest: Digest::try_from_hex(&digest_hex)
                        .map_err(|e| StoreError::Corrupt(format!("artifact digest: {e}")))?,
                    created_at: ts_from_text(&created_at)?,
                    session_id: session_id.map(|s| parse_session_id(&s)).transpose()?,
                    run_id: run_id.map(|s| parse_run_id(&s)).transpose()?,
                    labels: serde_json::from_str(&labels_json)
                        .map_err(|e| StoreError::Corrupt(format!("artifact labels: {e}")))?,
                })
            })
        })
        .await
        .map_err(|e| self.map_busy(e))
    }
}

fn write_cas_blob(
    layout: &StorageLayout,
    cas_path: &std::path::Path,
    tmp_path: &std::path::Path,
    bytes: &[u8],
    digest_hex: &str,
) -> Result<(), StoreError> {
    if cas_path.is_file() {
        // Reuse existing blob; if corrupt, rewrite with known-good bytes (self-heal).
        match std::fs::read(cas_path) {
            Ok(existing) if Digest::sha256(&existing).as_hex() == digest_hex => return Ok(()),
            Ok(_) | Err(_) => {
                tracing::warn!(
                    path = %cas_path.display(),
                    "replacing corrupt or unreadable CAS blob"
                );
            }
        }
    }

    if let Some(parent) = cas_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(layout.artifacts_dir.join("tmp"))?;

    {
        let mut f = std::fs::File::create(tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    // Atomic rename into CAS path. On Unix this replaces an existing file.
    std::fs::rename(tmp_path, cas_path).map_err(|e| {
        let _ = std::fs::remove_file(tmp_path);
        StoreError::Io(e.to_string())
    })?;

    if let Some(parent) = cas_path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(dir: &std::path::Path) -> Result<(), StoreError> {
    let f = std::fs::File::open(dir)?;
    f.sync_all()?;
    Ok(())
}
