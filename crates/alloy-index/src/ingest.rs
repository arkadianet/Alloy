//! Deterministic, offline, exec-free ingest (RFC-0011 §6).
//!
//! Facts come from exactly two sources (IN — §6.2): `Cargo.toml` manifests
//! parsed with `toml`, and a bounded, sorted, symlink-free `std::fs` walk.
//! No subprocess, no network, no Rust parsing (IN7 is file-layout only).

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
fn module_key(package: &str, module_path: &str) -> String {
    format!("{package}\0{module_path}")
}

/// `crate_ident`: package name with `-` → `_` (§4.2).
fn crate_ident(package: &str) -> String {
    package.replace('-', "_")
}

/// Normalise a path to workspace-relative `/`-separated form (G12).
fn rel_str(rel: &Path) -> String {
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
struct Member {
    package: String,
    dir_rel: PathBuf,
    lib_path: Option<String>,
    bin_paths: Vec<(String, String)>, // (bin name, path)
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
            if let (Some(name), Some(path)) = (name, path) {
                bin_paths.push((name.to_string(), path.to_string()));
            }
        }
    }
    Ok(Some(Member {
        package: package.to_string(),
        dir_rel,
        lib_path,
        bin_paths,
    }))
}

/// One inferred module, queued for descent.
struct ModuleSeed {
    module_path: String,
    file_rel: PathBuf,
    /// Directory that holds this module's children (IN7c), when it may have
    /// any.
    children_dir: Option<PathBuf>,
}

/// Full scan of `root` (§6.5 steps 1–6). Pure with respect to the database.
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

    let mut file_budget = 0u32;
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

        ingest_crate_modules(
            root,
            member,
            crate_node_id,
            limits,
            &mut file_budget,
            &mut out,
        )?;
    }

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

/// IN7a–IN7g for one crate.
fn ingest_crate_modules(
    root: &Path,
    member: &Member,
    crate_node_id: GraphNodeId,
    limits: &IngestLimits,
    file_budget: &mut u32,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    let ident = crate_ident(&member.package);
    let dir = &member.dir_rel;
    let mut seeds: Vec<ModuleSeed> = Vec::new();

    // IN7a: library root.
    let lib_rel = member
        .lib_path
        .as_deref()
        .map(|p| dir.join(p))
        .filter(|p| root.join(p).is_file())
        .or_else(|| {
            let conventional = dir.join("src/lib.rs");
            root.join(&conventional).is_file().then_some(conventional)
        });
    if let Some(lib_rel) = lib_rel {
        let children_dir = lib_rel.parent().map(Path::to_path_buf);
        seeds.push(ModuleSeed {
            module_path: ident.clone(),
            file_rel: lib_rel,
            children_dir,
        });
    }

    // IN7b: binary roots. Bin roots never descend — layout inference cannot
    // attribute `src/` children between a lib root and a bin root without
    // parsing, and G7 forbids inventing the answer.
    let mut bin_seeds: Vec<(String, PathBuf)> = Vec::new();
    for (name, path) in &member.bin_paths {
        let rel = dir.join(path);
        if root.join(&rel).is_file() {
            bin_seeds.push((name.clone(), rel));
        }
    }
    if bin_seeds.is_empty() {
        let main_rel = dir.join("src/main.rs");
        if root.join(&main_rel).is_file() {
            bin_seeds.push(("main".into(), main_rel));
        }
        let bin_dir = root.join(dir).join("src/bin");
        if bin_dir.is_dir() {
            for (name, path) in sorted_entries(&bin_dir, &mut out.skipped)? {
                if path.is_file() && name.ends_with(".rs") {
                    let stem = name.trim_end_matches(".rs").to_string();
                    bin_seeds.push((stem, dir.join("src/bin").join(&name)));
                }
            }
        }
    }
    for (bin_name, rel) in bin_seeds {
        seeds.push(ModuleSeed {
            module_path: format!("{ident}::{bin_name}"),
            file_rel: rel,
            children_dir: None,
        });
    }

    // Descend (IN7c–IN7e), breadth-first over a work queue, entries sorted.
    // (seed, parent (id, module path), depth)
    type QueueEntry = (ModuleSeed, Option<(GraphNodeId, String)>, u32);
    let mut queue: std::collections::VecDeque<QueueEntry> = seeds
        .into_iter()
        .map(|s| (s, None::<(GraphNodeId, String)>, 0u32))
        .collect();

    while let Some((seed, parent, depth)) = queue.pop_front() {
        if depth > limits.max_depth {
            return Err(GraphError::LimitExceeded(format!(
                "max_depth {} exceeded under {}",
                limits.max_depth,
                seed.file_rel.display()
            )));
        }
        *file_budget += 1;
        if *file_budget > limits.max_files {
            return Err(GraphError::LimitExceeded(format!(
                "max_files {} exceeded",
                limits.max_files
            )));
        }

        let file_rel_s = rel_str(&seed.file_rel);
        let (digest, byte_len, oversized) = hash_file(&root.join(&seed.file_rel), limits)?;
        if oversized {
            out.skipped += 1;
        }
        let node_id = derive_node_id(
            GraphNodeKind::Module,
            &module_key(&member.package, &seed.module_path),
        );
        out.modules += 1;
        out.nodes.push(NodeRow {
            id: node_id,
            kind: GraphNodeKind::Module,
            path: seed.module_path.clone(),
            crate_id: Some(member.package.clone()),
            file: Some(file_rel_s.clone()),
            digest: Some(digest.clone()),
        });
        out.files.push(FileRow {
            path: file_rel_s,
            crate_id: member.package.clone(),
            module_id: node_id,
            digest,
            byte_len,
        });
        match parent {
            Some((parent_id, parent_path)) => out.edges.push(EdgeRow {
                from: parent_id,
                to: node_id,
                from_path: parent_path,
                to_path: seed.module_path.clone(),
                kind: GraphEdgeKind::Defines,
            }),
            None => out.edges.push(EdgeRow {
                from: crate_node_id,
                to: node_id,
                from_path: member.package.clone(),
                to_path: seed.module_path.clone(),
                kind: GraphEdgeKind::Defines,
            }),
        }

        let Some(children_dir) = seed.children_dir else {
            continue;
        };
        let children_abs = root.join(&children_dir);
        if !children_abs.is_dir() {
            continue;
        }

        // Which sibling files are already claimed as roots of this crate?
        // (`src/lib.rs` descends `src/`; `main.rs` / bin roots must not
        // reappear as child modules of the lib.)
        let claimed: Vec<String> = ["lib.rs", "main.rs"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        let mut child_names: Vec<(String, PathBuf, PathBuf)> = Vec::new(); // (module name, file rel, child children dir)
        let entries = sorted_entries(&children_abs, &mut out.skipped)?;
        let dir_names: std::collections::BTreeSet<String> = entries
            .iter()
            .filter(|(_, p)| p.is_dir())
            .map(|(n, _)| n.clone())
            .collect();
        for (name, path) in &entries {
            if path.is_file() {
                if !name.ends_with(".rs") || *name == "mod.rs" || claimed.contains(name) {
                    continue;
                }
                let stem = name.trim_end_matches(".rs").to_string();
                let file_rel = children_dir.join(name);
                // IN7d: `name.rs` wins over `name/mod.rs`.
                if dir_names.contains(&stem) && children_abs.join(&stem).join("mod.rs").is_file() {
                    out.warnings.push(format!(
                        "{}: both {stem}.rs and {stem}/mod.rs exist; {stem}.rs wins (IN7d)",
                        rel_str(&children_dir)
                    ));
                }
                child_names.push((stem.clone(), file_rel, children_dir.join(&stem)));
            } else if path.is_dir() {
                if name.starts_with('.') || name == "target" {
                    out.skipped += 1;
                    continue;
                }
                // IN7d handled above; only a dir with mod.rs and no sibling
                // `name.rs` becomes a module here (IN7c/IN7e).
                let sibling = format!("{name}.rs");
                let has_sibling = entries.iter().any(|(n, p)| p.is_file() && *n == sibling);
                if !has_sibling && path.join("mod.rs").is_file() {
                    child_names.push((
                        name.clone(),
                        children_dir.join(name).join("mod.rs"),
                        children_dir.join(name),
                    ));
                }
            }
        }

        for (child, file_rel, child_dir) in child_names {
            queue.push_back((
                ModuleSeed {
                    module_path: format!("{}::{child}", seed.module_path),
                    file_rel,
                    children_dir: Some(child_dir),
                },
                Some((node_id, seed.module_path.clone())),
                depth + 1,
            ));
        }
    }
    Ok(())
}

/// Hash one file's bytes; oversized files get a size-only marker digest and
/// are counted as skipped by the caller (IN3).
pub(crate) fn hash_file(
    abs: &Path,
    limits: &IngestLimits,
) -> Result<(Digest, u64, bool), GraphError> {
    let meta = std::fs::metadata(abs)
        .map_err(|e| GraphError::Io(format!("stat {}: {e}", abs.display())))?;
    let byte_len = meta.len();
    if byte_len > limits.max_file_bytes {
        let marker = Digest::sha256(format!("alloyg1-oversize\0{byte_len}").as_bytes());
        return Ok((marker, byte_len, true));
    }
    let bytes =
        std::fs::read(abs).map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
    Ok((Digest::sha256(&bytes), byte_len, false))
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
