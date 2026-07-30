//! Deterministic, offline, exec-free ingest (RFC-0011 §6, RFC-0014 §5).
//!
//! Facts come from exactly three sources: `Cargo.toml` manifests parsed
//! with `toml`, a bounded, sorted, symlink-free `std::fs` walk, and — since
//! `model_version = 2` — a `syn` item/import parse (RFC-0014 amendment
//! A-0014-2 supersedes IN7's layout-only module guessing with
//! declaration-driven inference). `model_version = 3` (RFC-0011 amendment
//! A-0011-6) extends the same parse with best-effort `references`/`calls`/
//! `impls` edges. No subprocess, no network.

use std::path::{Path, PathBuf};

use alloy_runtime::graph::{derive_node_id, GraphEdgeKind, GraphNodeKind};
use alloy_runtime::types::ids::{Digest, DigestHasher, GraphNodeId};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::layout::IngestLimits;
use alloy_runtime::graph::GraphError;

/// One node row, pre-sorted for deterministic insertion and digesting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeRow {
    pub(crate) id: GraphNodeId,
    pub(crate) kind: GraphNodeKind,
    pub(crate) path: String,
    pub(crate) crate_id: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) digest: Option<Digest>,
}

/// One edge row. `from_path`/`to_path` feed the §4.6 canonical rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeRow {
    pub(crate) from: GraphNodeId,
    pub(crate) to: GraphNodeId,
    pub(crate) from_path: String,
    pub(crate) to_path: String,
    pub(crate) kind: GraphEdgeKind,
}

/// One tracked file row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub(crate) path: String,
    pub(crate) crate_id: String,
    pub(crate) module_id: GraphNodeId,
    pub(crate) digest: Digest,
    pub(crate) byte_len: u64,
}

/// Full output of one scan over the workspace tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScanOutput {
    pub(crate) nodes: Vec<NodeRow>,
    pub(crate) edges: Vec<EdgeRow>,
    pub(crate) files: Vec<FileRow>,
    pub(crate) crates: u32,
    pub(crate) modules: u32,
    pub(crate) items: u32,
    pub(crate) imports: u32,
    pub(crate) references: u32,
    pub(crate) calls: u32,
    pub(crate) impls: u32,
    pub(crate) skipped: u32,
    pub(crate) warnings: Vec<String>,
}

impl ScanOutput {
    /// §4.6 canonical rendering: nodes sorted by (kind, path), then edges by
    /// (from_path, to_path, kind); fields `\0`-delimited, rows `\n`-delimited.
    pub(crate) fn content_digest(&self) -> Digest {
        let mut hasher = DigestHasher::new();
        for n in &self.nodes {
            hasher.update(n.kind.as_str().as_bytes());
            hasher.update(b"\0");
            hasher.update(n.path.as_bytes());
            hasher.update(b"\0");
            hasher.update(n.crate_id.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\0");
            hasher.update(n.file.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\0");
            hasher.update(
                n.digest
                    .as_ref()
                    .map(|d| d.as_hex())
                    .unwrap_or("")
                    .as_bytes(),
            );
            hasher.update(b"\n");
        }
        for e in &self.edges {
            hasher.update(e.from_path.as_bytes());
            hasher.update(b"\0");
            hasher.update(e.to_path.as_bytes());
            hasher.update(b"\0");
            hasher.update(e.kind.as_str().as_bytes());
            hasher.update(b"\n");
        }
        hasher.finish()
    }
}

/// G4 stable keys.
fn workspace_key() -> String {
    ".".into()
}
fn crate_key(package: &str, manifest_rel: &str) -> String {
    format!("{package}\0{manifest_rel}")
}
/// G4/SY4 stable key for module and item nodes alike:
/// `<crate_id>\0<rust_path>`.
pub(crate) fn module_key(package: &str, module_path: &str) -> String {
    format!("{package}\0{module_path}")
}

/// `crate_ident`: package name with `-` → `_` (§4.2).
pub(crate) fn crate_ident(package: &str) -> String {
    package.replace('-', "_")
}

/// Normalise a path to workspace-relative `/`-separated form (G12).
pub(crate) fn rel_str(rel: &Path) -> String {
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    out
}

/// Sorted directory entries (IN5); symlinks are skipped and counted (IN4).
fn sorted_entries(dir: &Path, skipped: &mut u32) -> Result<Vec<(String, PathBuf)>, GraphError> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| GraphError::Io(format!("read_dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("read_dir entry: {e}")))?;
        let file_type = entry
            .file_type()
            .map_err(|e| GraphError::Io(format!("file_type: {e}")))?;
        if file_type.is_symlink() {
            *skipped += 1; // IN4: never follow symlinks.
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push((name, entry.path()));
    }
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

/// A discovered workspace member.
#[derive(Debug)]
pub(crate) struct Member {
    pub(crate) package: String,
    pub(crate) dir_rel: PathBuf,
    pub(crate) lib_path: Option<String>,
    /// `[[bin]]` entries: name plus explicit `path` when present. Name-only
    /// bins resolve to the conventional `src/bin/<name>.rs` at seed time.
    pub(crate) bin_paths: Vec<(String, Option<String>)>,
    /// `[package] edition`, used to pick the parse target (TC3).
    pub(crate) edition: Option<String>,
}

/// Parse the root manifest and discover members (§6.3).
fn discover_members(
    root: &Path,
    limits: &IngestLimits,
    out: &mut ScanOutput,
) -> Result<Vec<Member>, GraphError> {
    let root_manifest_path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&root_manifest_path)
        .map_err(|e| GraphError::Workspace(format!("not a cargo workspace: {e}")))?;
    let doc: toml::Value = text
        .parse()
        .map_err(|e: toml::de::Error| GraphError::Manifest {
            path: "Cargo.toml".into(),
            reason: e.to_string(),
        })?;

    let workspace = doc.get("workspace");
    let root_package = doc.get("package");
    if workspace.is_none() && root_package.is_none() {
        return Err(GraphError::Workspace(
            "not a cargo workspace: Cargo.toml has neither [workspace] nor [package]".into(),
        ));
    }

    let build_globset = |key: &str| -> Result<Option<GlobSet>, GraphError> {
        let Some(list) = workspace
            .and_then(|w| w.get(key))
            .and_then(|m| m.as_array())
        else {
            return Ok(None);
        };
        let mut builder = GlobSetBuilder::new();
        for pat in list {
            let Some(pat) = pat.as_str() else { continue };
            let glob = Glob::new(pat).map_err(|e| GraphError::Manifest {
                path: "Cargo.toml".into(),
                reason: format!("bad {key} glob {pat:?}: {e}"),
            })?;
            builder.add(glob);
        }
        Ok(Some(builder.build().map_err(|e| GraphError::Manifest {
            path: "Cargo.toml".into(),
            reason: format!("{key} globset: {e}"),
        })?))
    };
    let members = build_globset("members")?;
    let exclude = build_globset("exclude")?;

    // Candidate member dirs: bounded walk collecting directories that hold a
    // Cargo.toml, matched against the member globs.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(members) = &members {
        let mut stack = vec![(root.to_path_buf(), 0u32)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > limits.max_depth {
                return Err(GraphError::LimitExceeded(format!(
                    "max_depth {} exceeded during member discovery",
                    limits.max_depth
                )));
            }
            for (name, path) in sorted_entries(&dir, &mut out.skipped)? {
                if !path.is_dir() {
                    continue;
                }
                if name.starts_with('.') || name == "target" {
                    out.skipped += 1;
                    continue;
                }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                let rel_s = rel_str(&rel);
                if members.is_match(&rel_s)
                    && !exclude.as_ref().is_some_and(|x| x.is_match(&rel_s))
                    && path.join("Cargo.toml").is_file()
                {
                    candidates.push(rel);
                }
                stack.push((path, depth + 1));
            }
        }
    }
    candidates.sort();

    // Root package is itself a member (§6.3).
    let mut members_out: Vec<Member> = Vec::new();
    let mut member_dirs: Vec<PathBuf> = Vec::new();
    if root_package.is_some() {
        member_dirs.push(PathBuf::new());
    }
    member_dirs.extend(candidates);

    if member_dirs.len() as u32 > limits.max_crates {
        return Err(GraphError::LimitExceeded(format!(
            "max_crates {} exceeded: {} members",
            limits.max_crates,
            member_dirs.len()
        )));
    }

    for dir_rel in member_dirs {
        let manifest_rel = if dir_rel.as_os_str().is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{}/Cargo.toml", rel_str(&dir_rel))
        };
        let manifest_abs = root.join(&dir_rel).join("Cargo.toml");
        let member = match parse_member_manifest(&manifest_abs, &manifest_rel, dir_rel.clone()) {
            Ok(Some(m)) => m,
            Ok(None) => {
                // IN12: member without [package].name — warn and continue.
                out.warnings
                    .push(format!("{manifest_rel}: missing [package].name; skipped"));
                continue;
            }
            Err(reason) => {
                if manifest_rel == "Cargo.toml" {
                    // IN12: malformed root manifest is fatal (already parsed
                    // above; this covers root-as-package field errors).
                    return Err(GraphError::Manifest {
                        path: manifest_rel,
                        reason,
                    });
                }
                out.warnings
                    .push(format!("{manifest_rel}: {reason}; skipped"));
                continue;
            }
        };
        members_out.push(member);
    }

    // §6.3: duplicate package names break G9.
    members_out.sort_by(|a, b| a.package.cmp(&b.package));
    for pair in members_out.windows(2) {
        if pair[0].package == pair[1].package {
            return Err(GraphError::Manifest {
                path: "Cargo.toml".into(),
                reason: format!("duplicate package name {:?}", pair[0].package),
            });
        }
    }
    Ok(members_out)
}

/// Parse one member manifest. `Ok(None)` = no `[package].name` (skip).
fn parse_member_manifest(
    abs: &Path,
    rel: &str,
    dir_rel: PathBuf,
) -> Result<Option<Member>, String> {
    let text = std::fs::read_to_string(abs).map_err(|e| format!("read: {e}"))?;
    let doc: toml::Value = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let Some(package) = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    else {
        return Ok(None);
    };
    let _ = rel;
    let lib_path = doc
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
        .map(str::to_string);
    let mut bin_paths = Vec::new();
    if let Some(bins) = doc.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            let name = bin.get("name").and_then(|n| n.as_str());
            let path = bin.get("path").and_then(|p| p.as_str());
            if let Some(name) = name {
                bin_paths.push((name.to_string(), path.map(str::to_string)));
            }
        }
    }
    let edition = doc
        .get("package")
        .and_then(|p| p.get("edition"))
        .and_then(|e| e.as_str())
        .map(str::to_string);
    Ok(Some(Member {
        package: package.to_string(),
        dir_rel,
        lib_path,
        bin_paths,
        edition,
    }))
}

/// Full scan of `root` (§6.5 steps 1–6, plus the RFC-0014 §5 deep pass).
/// Pure with respect to the database.
#[tracing::instrument(skip_all, fields(root = tracing::field::Empty), name = "index.rebuild")]
pub(crate) fn scan_workspace(root: &Path, limits: &IngestLimits) -> Result<ScanOutput, GraphError> {
    if !root.is_dir() {
        return Err(GraphError::Workspace(format!(
            "not a directory: {}",
            root.display()
        )));
    }
    tracing::Span::current().record("root", tracing::field::debug(root));

    let mut out = ScanOutput::default();
    let members = discover_members(root, limits, &mut out)?;

    let workspace_id = derive_node_id(GraphNodeKind::Workspace, &workspace_key());
    out.nodes.push(NodeRow {
        id: workspace_id,
        kind: GraphNodeKind::Workspace,
        path: ".".into(),
        crate_id: None,
        file: Some("Cargo.toml".into()),
        digest: None,
    });

    // RFC-0014 §5.5: the syn deep pass composes with this manifest pass in
    // the same scan, behind the same single-writer transaction (X1).
    let mut pass = crate::lang::rust::pass::DeepPass::new(&members);
    for member in &members {
        out.crates += 1;
        let manifest_rel = if member.dir_rel.as_os_str().is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{}/Cargo.toml", rel_str(&member.dir_rel))
        };
        let crate_node_id = derive_node_id(
            GraphNodeKind::Crate,
            &crate_key(&member.package, &manifest_rel),
        );
        out.nodes.push(NodeRow {
            id: crate_node_id,
            kind: GraphNodeKind::Crate,
            path: member.package.clone(),
            crate_id: Some(member.package.clone()),
            file: Some(manifest_rel),
            digest: None,
        });
        out.edges.push(EdgeRow {
            from: workspace_id,
            to: crate_node_id,
            from_path: ".".into(),
            to_path: member.package.clone(),
            kind: GraphEdgeKind::Defines,
        });

        crate::lang::rust::pass::ingest_crate(
            root,
            member,
            crate_node_id,
            limits,
            &mut pass,
            &mut out,
        )?;
    }
    crate::lang::rust::pass::resolve_semantics(&pass, &mut out);

    // Deterministic final order (Q8 / §4.6): nodes by (kind, path), edges by
    // (from_path, to_path, kind).
    out.nodes
        .sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));
    out.edges.sort_by(|a, b| {
        (&a.from_path, &a.to_path, a.kind).cmp(&(&b.from_path, &b.to_path, b.kind))
    });
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Parse the stored kind tag.
pub(crate) fn parse_kind(s: &str) -> Result<GraphNodeKind, GraphError> {
    match s {
        "workspace" => Ok(GraphNodeKind::Workspace),
        "crate" => Ok(GraphNodeKind::Crate),
        "module" => Ok(GraphNodeKind::Module),
        "item" => Ok(GraphNodeKind::Item),
        other => Err(GraphError::Corrupt(format!("unknown node kind {other:?}"))),
    }
}

/// Parse the stored edge kind tag.
pub(crate) fn parse_edge_kind(s: &str) -> Result<GraphEdgeKind, GraphError> {
    match s {
        "defines" => Ok(GraphEdgeKind::Defines),
        "imports" => Ok(GraphEdgeKind::Imports),
        "references" => Ok(GraphEdgeKind::References),
        "calls" => Ok(GraphEdgeKind::Calls),
        "impls" => Ok(GraphEdgeKind::Impls),
        other => Err(GraphError::Corrupt(format!("unknown edge kind {other:?}"))),
    }
}

/// IN11: validate an incoming workspace-relative `/`-separated change path.
pub(crate) fn validate_change_path(path: &str) -> Result<(), GraphError> {
    let p = Path::new(path);
    if p.is_absolute() || path.starts_with('/') || path.contains('\\') {
        return Err(GraphError::InvalidQuery(format!(
            "file change path must be workspace-relative with '/' separators: {path:?}"
        )));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(GraphError::InvalidQuery(format!(
            "file change path escapes the workspace root: {path:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_change_path_rejects_absolute_and_escaping() {
        assert!(validate_change_path("crates/x/src/lib.rs").is_ok());
        assert!(validate_change_path("/etc/passwd").is_err());
        assert!(validate_change_path("../outside.rs").is_err());
        assert!(validate_change_path("a/../../b.rs").is_err());
        assert!(validate_change_path("a\\b.rs").is_err());
    }
}
