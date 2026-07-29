//! The `syn` item/import deep pass (RFC-0014 §5, `model_version = 2`).
//!
//! Module inference is **declaration-driven** (amendment A-0014-2): roots
//! come from the manifest (IN7a/IN7b), then `mod foo;` / `mod foo { … }`
//! declarations decide children; `#[path]` is honoured; `cfg` is never
//! evaluated (SY7). IN7f survives verbatim — a missing node is acceptable,
//! an invented one is not (G7). The visitor walks items and nested `mod`
//! blocks only; it never descends into function bodies, expressions, or
//! macro invocations (SY10, SC3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use alloy_runtime::graph::{derive_node_id, GraphEdgeKind, GraphError, GraphNodeKind};
use alloy_runtime::types::ids::{Digest, GraphNodeId};

use crate::ingest::{
    crate_ident, module_key, rel_str, EdgeRow, FileRow, Member, NodeRow, ScanOutput,
};
use crate::layout::IngestLimits;

/// Editions the pinned `syn` grammar covers (TC3). Anything else parses
/// with the newest supported grammar and records a warning.
const KNOWN_EDITIONS: &[&str] = &["2015", "2018", "2021", "2024"];

/// One `use` leaf awaiting workspace-wide resolution (SY11–SY13).
#[derive(Debug)]
pub(crate) struct UseRecord {
    /// Ident of the crate the declaration appears in.
    crate_ident: String,
    /// Rust path of the importing module.
    module_path: String,
    /// Node id of the importing module.
    module_id: GraphNodeId,
    /// Path segments up to the target (renames dropped — `use a::b as c`
    /// targets `b`, SY12).
    segments: Vec<String>,
    /// `use a::*` — one edge to `a`'s module node (SY12).
    glob: bool,
    /// `use ::a::b` — extern-prelude only; relative resolution is skipped.
    leading_colon: bool,
}

/// Cross-crate state of one deep pass.
#[derive(Debug)]
pub(crate) struct DeepPass {
    file_budget: u32,
    item_budget: u32,
    /// `(kind_tag, rust_path)` already claimed — SY8 keep-first-and-warn.
    seen_paths: BTreeSet<(&'static str, String)>,
    /// `graph_files.path` already claimed (its PRIMARY KEY): a file shared
    /// by two module trees (lib and bin) keeps its first module.
    seen_files: BTreeSet<String>,
    uses: Vec<UseRecord>,
    /// Idents of every workspace member (SY13 leading-segment resolution).
    workspace_idents: BTreeSet<String>,
}

impl DeepPass {
    pub(crate) fn new(members: &[Member]) -> Self {
        Self {
            file_budget: 0,
            item_budget: 0,
            seen_paths: BTreeSet::new(),
            seen_files: BTreeSet::new(),
            uses: Vec::new(),
            workspace_idents: members.iter().map(|m| crate_ident(&m.package)).collect(),
        }
    }
}

/// Normalise a root-relative path, resolving `.`/`..` lexically. `None`
/// when it escapes the workspace root (IN4/SEC7 posture).
fn normalize_rel(rel: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Read and hash one file under the byte cap. `Ok((digest, len, None))`
/// when oversized: tracked by a marker digest, never parsed (SC3).
fn read_and_hash(
    abs: &Path,
    limits: &IngestLimits,
) -> Result<(Digest, u64, Option<Vec<u8>>), GraphError> {
    let meta = std::fs::metadata(abs)
        .map_err(|e| GraphError::Io(format!("stat {}: {e}", abs.display())))?;
    let byte_len = meta.len();
    if byte_len > limits.max_file_bytes {
        let marker = Digest::sha256(format!("alloyg1-oversize\0{byte_len}").as_bytes());
        return Ok((marker, byte_len, None));
    }
    let bytes =
        std::fs::read(abs).map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
    Ok((Digest::sha256(&bytes), byte_len, Some(bytes)))
}

/// A module file queued for descent.
struct FileSeed {
    module_path: String,
    file_rel: PathBuf,
    /// Directory owning this module's out-of-line children.
    owned_dir: PathBuf,
    parent: Option<(GraphNodeId, String)>,
    depth: u32,
}

/// Per-crate walk context.
struct CrateCtx<'a> {
    root: &'a Path,
    package: &'a str,
    crate_node_id: GraphNodeId,
    limits: &'a IngestLimits,
    queue: VecDeque<FileSeed>,
}

/// SY1–SY10 for one crate: declaration-driven modules, items, collected
/// `use` declarations.
pub(crate) fn ingest_crate(
    root: &Path,
    member: &Member,
    crate_node_id: GraphNodeId,
    limits: &IngestLimits,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    if let Some(edition) = member.edition.as_deref() {
        if !KNOWN_EDITIONS.contains(&edition) {
            // TC3: unknown edition parses with the newest supported grammar.
            out.warnings.push(format!(
                "{}: unknown edition {edition:?}; parsing with the newest supported grammar (TC3)",
                member.package
            ));
        }
    }

    let ident = crate_ident(&member.package);
    let dir = &member.dir_rel;
    let mut ctx = CrateCtx {
        root,
        package: &member.package,
        crate_node_id,
        limits,
        queue: VecDeque::new(),
    };

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
        // A crate root owns the directory containing it.
        let owned_dir = lib_rel.parent().map(Path::to_path_buf).unwrap_or_default();
        ctx.queue.push_back(FileSeed {
            module_path: ident.clone(),
            file_rel: lib_rel,
            owned_dir,
            parent: None,
            depth: 0,
        });
    }

    // IN7b: binary roots. Deduped by resolved file path (BTreeMap: sorted,
    // deterministic — IN5): conventional roots first, explicit `[[bin]]`
    // entries last so an explicit name wins for the same file.
    let mut bin_files: BTreeMap<PathBuf, String> = BTreeMap::new();
    let main_rel = dir.join("src/main.rs");
    if root.join(&main_rel).is_file() {
        bin_files.insert(main_rel, "main".into());
    }
    let bin_dir_abs = root.join(dir).join("src/bin");
    if bin_dir_abs.is_dir() {
        let entries = std::fs::read_dir(&bin_dir_abs)
            .map_err(|e| GraphError::Io(format!("read_dir {}: {e}", bin_dir_abs.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| GraphError::Io(format!("read_dir entry: {e}")))?;
            let file_type = entry
                .file_type()
                .map_err(|e| GraphError::Io(format!("file_type: {e}")))?;
            if file_type.is_symlink() {
                out.skipped += 1; // IN4
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_file() && name.ends_with(".rs") {
                let stem = name.trim_end_matches(".rs").to_string();
                bin_files.insert(dir.join("src/bin").join(&name), stem);
            }
        }
    }
    for (name, path) in &member.bin_paths {
        let rel = match path {
            Some(path) => dir.join(path),
            None => dir.join("src/bin").join(format!("{name}.rs")),
        };
        if root.join(&rel).is_file() {
            bin_files.insert(rel, name.clone());
        }
    }
    for (rel, bin_name) in bin_files {
        let owned_dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        ctx.queue.push_back(FileSeed {
            module_path: format!("{ident}::{bin_name}"),
            file_rel: rel,
            owned_dir,
            parent: None,
            depth: 0,
        });
    }

    while let Some(seed) = ctx.queue.pop_front() {
        process_file(seed, &mut ctx, pass, out)?;
    }
    Ok(())
}

/// One module file: node, file row, edge, then the item walk.
fn process_file(
    seed: FileSeed,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    if seed.depth > ctx.limits.max_depth {
        return Err(GraphError::LimitExceeded(format!(
            "max_depth {} exceeded under {}",
            ctx.limits.max_depth,
            seed.file_rel.display()
        )));
    }
    pass.file_budget += 1;
    if pass.file_budget > ctx.limits.max_files {
        return Err(GraphError::LimitExceeded(format!(
            "max_files {} exceeded",
            ctx.limits.max_files
        )));
    }

    // SY8 discipline extends to module paths: `#[path]`/`cfg` tricks can
    // duplicate one, and `UNIQUE (kind, path)` admits only the first.
    if !pass
        .seen_paths
        .insert((GraphNodeKind::Module.as_str(), seed.module_path.clone()))
    {
        out.warnings.push(format!(
            "colliding module path {:?}; keeping the first in traversal order (SY8)",
            seed.module_path
        ));
        return Ok(());
    }

    let file_rel_s = rel_str(&seed.file_rel);
    let abs = ctx.root.join(&seed.file_rel);
    let (digest, byte_len, bytes) = read_and_hash(&abs, ctx.limits)?;
    if bytes.is_none() {
        out.skipped += 1; // oversized: tracked, never parsed (SY15/SC3).
    }

    let node_id = derive_node_id(
        GraphNodeKind::Module,
        &module_key(ctx.package, &seed.module_path),
    );
    out.modules += 1;
    out.nodes.push(NodeRow {
        id: node_id,
        kind: GraphNodeKind::Module,
        path: seed.module_path.clone(),
        crate_id: Some(ctx.package.to_string()),
        file: Some(file_rel_s.clone()),
        digest: Some(digest.clone()),
    });
    if pass.seen_files.insert(file_rel_s.clone()) {
        out.files.push(FileRow {
            path: file_rel_s.clone(),
            crate_id: ctx.package.to_string(),
            module_id: node_id,
            digest: digest.clone(),
            byte_len,
        });
    }
    match &seed.parent {
        Some((parent_id, parent_path)) => out.edges.push(EdgeRow {
            from: *parent_id,
            to: node_id,
            from_path: parent_path.clone(),
            to_path: seed.module_path.clone(),
            kind: GraphEdgeKind::Defines,
        }),
        None => out.edges.push(EdgeRow {
            from: ctx.crate_node_id,
            to: node_id,
            from_path: ctx.package.to_string(),
            to_path: seed.module_path.clone(),
            kind: GraphEdgeKind::Defines,
        }),
    }

    let Some(bytes) = bytes else {
        return Ok(());
    };
    let text = String::from_utf8_lossy(&bytes);
    let ast = match syn::parse_file(&text) {
        Ok(ast) => ast,
        Err(e) => {
            // SY9: skipped, counted, warned — never fatal; path and reason
            // only, never source text.
            out.skipped += 1;
            out.warnings.push(format!(
                "{file_rel_s}: parse error: {e}; contents skipped (SY9)"
            ));
            return Ok(());
        }
    };

    let file_dir = seed
        .file_rel
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    walk_items(
        &ast.items,
        &ModScope {
            module_path: &seed.module_path,
            module_id: node_id,
            file_rel_s: &file_rel_s,
            file_digest: &digest,
            file_dir: &file_dir,
            owned_dir: &seed.owned_dir,
            depth: seed.depth,
        },
        ctx,
        pass,
        out,
    )
}

/// Position of the item walk inside one module.
struct ModScope<'a> {
    module_path: &'a str,
    module_id: GraphNodeId,
    file_rel_s: &'a str,
    file_digest: &'a Digest,
    /// Directory of the declaring file — `#[path]` resolves against it.
    file_dir: &'a Path,
    /// Directory owning out-of-line children of this module.
    owned_dir: &'a Path,
    depth: u32,
}

/// SY3/SY5/SY10: module-level items and nested `mod`s only; `impl` blocks,
/// macros and bodies are never entered.
fn walk_items(
    items: &[syn::Item],
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    for item in items {
        match item {
            syn::Item::Fn(i) => emit_item(&i.sig.ident, scope, ctx, pass, out)?,
            syn::Item::Struct(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Enum(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Union(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Trait(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Type(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Const(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Static(i) => emit_item(&i.ident, scope, ctx, pass, out)?,
            syn::Item::Mod(m) => walk_mod(m, scope, ctx, pass, out)?,
            syn::Item::Use(u) => collect_use(u, scope, ctx, pass),
            // SY5: impl blocks and associated items are deferred; macros,
            // extern blocks and the rest are never entered (SY10).
            _ => {}
        }
    }
    Ok(())
}

/// One item node plus its `Defines` edge (SY3, SY4, SY6).
fn emit_item(
    ident: &syn::Ident,
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    let path = format!("{}::{ident}", scope.module_path);
    if !pass
        .seen_paths
        .insert((GraphNodeKind::Item.as_str(), path.clone()))
    {
        // SY8: keep the first in traversal order; disambiguating suffixes
        // would make ids depend on sibling contents and break IN6.
        out.warnings.push(format!(
            "colliding item path {path:?}; keeping the first in traversal order (SY8)"
        ));
        return Ok(());
    }
    pass.item_budget += 1;
    if pass.item_budget > ctx.limits.max_items {
        return Err(GraphError::LimitExceeded(format!(
            "max_items {} exceeded",
            ctx.limits.max_items
        )));
    }
    let id = derive_node_id(GraphNodeKind::Item, &module_key(ctx.package, &path));
    out.items += 1;
    out.nodes.push(NodeRow {
        id,
        kind: GraphNodeKind::Item,
        path: path.clone(),
        crate_id: Some(ctx.package.to_string()),
        file: Some(scope.file_rel_s.to_string()),
        digest: Some(scope.file_digest.clone()),
    });
    out.edges.push(EdgeRow {
        from: scope.module_id,
        to: id,
        from_path: scope.module_path.to_string(),
        to_path: path,
        kind: GraphEdgeKind::Defines,
    });
    Ok(())
}

/// `#[path = "…"]` value, when present.
fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        if !a.path().is_ident("path") {
            return None;
        }
        match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        }
    })
}

fn is_cfg_gated(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("cfg"))
}

/// SY7: one `mod` declaration — inline recursion or out-of-line file
/// resolution with `#[path]` honoured and `cfg` unevaluated.
fn walk_mod(
    m: &syn::ItemMod,
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    let name = m.ident.to_string();
    let child_path = format!("{}::{name}", scope.module_path);
    let cfg_gated = is_cfg_gated(&m.attrs);
    let attr_path = path_attr(&m.attrs);

    if let Some((_, items)) = &m.content {
        // Inline block: a module node on the declaring file, no file row.
        if scope.depth + 1 > ctx.limits.max_depth {
            return Err(GraphError::LimitExceeded(format!(
                "max_depth {} exceeded at inline module {child_path}",
                ctx.limits.max_depth
            )));
        }
        if !pass
            .seen_paths
            .insert((GraphNodeKind::Module.as_str(), child_path.clone()))
        {
            out.warnings.push(format!(
                "colliding module path {child_path:?}; keeping the first in traversal order (SY8)"
            ));
            return Ok(());
        }
        if cfg_gated {
            out.warnings.push(format!(
                "cfg-gated module {child_path} emitted without evaluating cfg (SY7)"
            ));
        }
        let id = derive_node_id(GraphNodeKind::Module, &module_key(ctx.package, &child_path));
        out.modules += 1;
        out.nodes.push(NodeRow {
            id,
            kind: GraphNodeKind::Module,
            path: child_path.clone(),
            crate_id: Some(ctx.package.to_string()),
            file: Some(scope.file_rel_s.to_string()),
            digest: Some(scope.file_digest.clone()),
        });
        out.edges.push(EdgeRow {
            from: scope.module_id,
            to: id,
            from_path: scope.module_path.to_string(),
            to_path: child_path.clone(),
            kind: GraphEdgeKind::Defines,
        });
        let owned_dir = match &attr_path {
            Some(p) => scope.file_dir.join(p),
            None => scope.owned_dir.join(&name),
        };
        return walk_items(
            items,
            &ModScope {
                module_path: &child_path,
                module_id: id,
                file_rel_s: scope.file_rel_s,
                file_digest: scope.file_digest,
                file_dir: scope.file_dir,
                owned_dir: &owned_dir,
                depth: scope.depth + 1,
            },
            ctx,
            pass,
            out,
        );
    }

    // Out-of-line: resolve the declared file, missing-ok / invented-never.
    let (file_rel, owned_dir) = match &attr_path {
        Some(p) => {
            let Some(rel) = normalize_rel(&scope.file_dir.join(p)) else {
                out.warnings.push(format!(
                    "module {child_path}: #[path] escapes the workspace root; skipped (SEC7)"
                ));
                return Ok(());
            };
            let owned = if rel.file_name().is_some_and(|n| n == "mod.rs") {
                rel.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                rel.with_extension("")
            };
            (rel, owned)
        }
        None => {
            let sibling = scope.owned_dir.join(format!("{name}.rs"));
            let nested = scope.owned_dir.join(&name).join("mod.rs");
            let sibling_exists = ctx.root.join(&sibling).is_file();
            let nested_exists = ctx.root.join(&nested).is_file();
            if sibling_exists && nested_exists {
                out.warnings.push(format!(
                    "{}: both {name}.rs and {name}/mod.rs exist; {name}.rs wins (IN7d)",
                    rel_str(scope.owned_dir)
                ));
            }
            let rel = if sibling_exists { sibling } else { nested };
            (rel, scope.owned_dir.join(&name))
        }
    };

    let abs = ctx.root.join(&file_rel);
    let is_symlink = std::fs::symlink_metadata(&abs)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        out.skipped += 1; // IN4: never follow symlinks.
        out.warnings.push(format!(
            "module {child_path}: {} is a symlink; skipped (IN4)",
            rel_str(&file_rel)
        ));
        return Ok(());
    }
    if !abs.is_file() {
        // IN7f: missing is acceptable, invented is not.
        out.warnings.push(format!(
            "declared module {child_path} has no file at {}; skipped (IN7f)",
            rel_str(&file_rel)
        ));
        return Ok(());
    }
    if cfg_gated {
        out.warnings.push(format!(
            "cfg-gated module {child_path} emitted without evaluating cfg (SY7)"
        ));
    }
    ctx.queue.push_back(FileSeed {
        module_path: child_path,
        file_rel,
        owned_dir,
        parent: Some((scope.module_id, scope.module_path.to_string())),
        depth: scope.depth + 1,
    });
    Ok(())
}

/// Flatten one `use` declaration into leaves (SY12). `cfg` is not
/// evaluated: gated imports are collected like any other.
fn collect_use(u: &syn::ItemUse, scope: &ModScope<'_>, ctx: &CrateCtx<'_>, pass: &mut DeepPass) {
    fn flatten(
        tree: &syn::UseTree,
        prefix: &mut Vec<String>,
        leaves: &mut Vec<(Vec<String>, bool)>,
    ) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                flatten(&p.tree, prefix, leaves);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                let mut segs = prefix.clone();
                let ident = n.ident.to_string();
                // `use a::b::{self}` targets `a::b` itself.
                if ident != "self" || segs.is_empty() {
                    segs.push(ident);
                }
                leaves.push((segs, false));
            }
            syn::UseTree::Rename(r) => {
                // `use a::b as c` targets `b` (SY12); the rename is dropped.
                let mut segs = prefix.clone();
                let ident = r.ident.to_string();
                if ident != "self" || segs.is_empty() {
                    segs.push(ident);
                }
                leaves.push((segs, false));
            }
            syn::UseTree::Glob(_) => leaves.push((prefix.clone(), true)),
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    flatten(item, prefix, leaves);
                }
            }
        }
    }
    let mut leaves = Vec::new();
    flatten(&u.tree, &mut Vec::new(), &mut leaves);
    for (segments, glob) in leaves {
        if segments.is_empty() {
            continue;
        }
        pass.uses.push(UseRecord {
            crate_ident: crate_ident(ctx.package),
            module_path: scope.module_path.to_string(),
            module_id: scope.module_id,
            segments,
            glob,
            leading_colon: u.leading_colon.is_some(),
        });
    }
}

/// SY11–SY13: syntactic resolution of the collected `use` leaves against
/// the module/item trees just built. Anything unresolved produces nothing —
/// cross-workspace targets (`std::`, registry crates) get no edge and no
/// node (G7).
pub(crate) fn resolve_imports(pass: &DeepPass, out: &mut ScanOutput) {
    let mut modules: BTreeMap<&str, GraphNodeId> = BTreeMap::new();
    let mut items: BTreeMap<&str, GraphNodeId> = BTreeMap::new();
    for n in &out.nodes {
        match n.kind {
            GraphNodeKind::Module => {
                modules.insert(n.path.as_str(), n.id);
            }
            GraphNodeKind::Item => {
                items.insert(n.path.as_str(), n.id);
            }
            // Workspace/Crate nodes (and any future kind) never resolve a
            // `use` target.
            _ => {}
        }
    }

    // SY12: duplicate (from, to, imports) rows collapse.
    let mut seen: BTreeSet<(GraphNodeId, GraphNodeId)> = BTreeSet::new();
    for record in &pass.uses {
        let Some((to_id, to_path)) = resolve_use(record, pass, &modules, &items) else {
            continue;
        };
        if to_id == record.module_id {
            continue; // a module never imports itself.
        }
        if !seen.insert((record.module_id, to_id)) {
            continue;
        }
        out.imports += 1;
        out.edges.push(EdgeRow {
            from: record.module_id,
            to: to_id,
            from_path: record.module_path.clone(),
            to_path,
            kind: GraphEdgeKind::Imports,
        });
    }
}

/// One leaf: resolve the leading segment (SY13 — `crate`/`self`/`super`, an
/// in-workspace crate ident, or a declared child module of the importing
/// module), then look the full path up as a module or item.
fn resolve_use(
    record: &UseRecord,
    pass: &DeepPass,
    modules: &BTreeMap<&str, GraphNodeId>,
    items: &BTreeMap<&str, GraphNodeId>,
) -> Option<(GraphNodeId, String)> {
    let segs = &record.segments;
    let mut rest = segs.as_slice();
    let mut base: String;
    let head = segs.first()?.as_str();
    if record.leading_colon {
        // `use ::a::…` — extern prelude only: a workspace crate or nothing.
        if !pass.workspace_idents.contains(head) {
            return None;
        }
        base = head.to_string();
        rest = &rest[1..];
    } else {
        match head {
            "crate" => {
                base = record.crate_ident.clone();
                rest = &rest[1..];
            }
            "self" => {
                base = record.module_path.clone();
                rest = &rest[1..];
            }
            "super" => {
                base = record.module_path.clone();
                while rest.first().is_some_and(|s| s == "super") {
                    let (parent, _) = base.rsplit_once("::")?;
                    base = parent.to_string();
                    rest = &rest[1..];
                }
            }
            s if pass.workspace_idents.contains(s) => {
                base = s.to_string();
                rest = &rest[1..];
            }
            s => {
                // A declared child module of the importing module (the
                // `mod reader;` + `use reader::Reader` shape).
                let candidate = format!("{}::{s}", record.module_path);
                if !modules.contains_key(candidate.as_str()) {
                    return None;
                }
                base = candidate;
                rest = &rest[1..];
            }
        }
    }
    let full = if rest.is_empty() {
        base
    } else {
        format!("{base}::{}", rest.join("::"))
    };
    if record.glob {
        // SY12: `use a::*` produces one edge to `a`'s module node.
        return modules.get(full.as_str()).map(|id| (*id, full));
    }
    modules
        .get(full.as_str())
        .or_else(|| items.get(full.as_str()))
        .map(|id| (*id, full))
}
