//! The `syn` item/import/reference deep pass (RFC-0014 §5, RFC-0011
//! amendment A-0011-6 — `model_version = 3`).
//!
//! Module inference is **declaration-driven** (amendment A-0014-2): roots
//! come from the manifest (IN7a/IN7b), then `mod foo;` / `mod foo { … }`
//! declarations decide children; `#[path]` is honoured; `cfg` is never
//! evaluated (SY7). IN7f survives verbatim — a missing node is acceptable,
//! an invented one is not (G7).
//!
//! Since amendment A-0011-6 the pass also records **semantic edges**
//! between the item nodes it already emits: `References` (type usages and
//! multi-segment path expressions), `Calls` (fn calls whose callee resolves
//! to a workspace `fn` item), and `Impls` (`impl Trait for Type` blocks).
//! Resolution is **syntactic and best-effort** — `syn` performs no name
//! resolution, so the pass resolves only through the scopes it can see:
//! `crate`/`self`/`super` prefixes, the module's own `use` bindings, the
//! module's declared children, workspace crate idents, and unambiguous glob
//! imports. Everything else — `std`/registry paths, method calls (no type
//! inference), locals, generic parameters, macro-generated code — records
//! **nothing**. A missing edge is acceptable; an invented one is not (G7).
//! Known accepted imprecision, documented in RFC-0011 §2.3b: a body-local
//! binding that shadows an in-scope single-segment fn name can attribute a
//! call to the workspace item, and `#[cfg]` variants are all recorded.

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
    /// Name the leaf binds in the importing module (`c` for `use a::b as
    /// c`), feeding the A-0011-6 alias table. `None` for globs.
    alias: Option<String>,
}

/// A raw, unresolved path as it appeared in source (A-0011-6).
#[derive(Debug, Clone)]
pub(crate) struct RawPath {
    segments: Vec<String>,
    leading_colon: bool,
}

fn raw_of(path: &syn::Path) -> RawPath {
    RawPath {
        segments: path.segments.iter().map(|s| s.ident.to_string()).collect(),
        leading_colon: path.leading_colon.is_some(),
    }
}

/// Where a pending reference originates.
#[derive(Debug)]
enum RefOrigin {
    /// A module-level item node already emitted.
    Node { id: GraphNodeId, path: String },
    /// The self type of an `impl` block — impl-block bodies have no node of
    /// their own, so their references attribute to the self-type item once
    /// it resolves (A-0011-6).
    SelfType(RawPath),
}

/// One reference or call awaiting workspace-wide resolution (A-0011-6).
#[derive(Debug)]
struct PendingRef {
    crate_ident: String,
    module_path: String,
    origin: RefOrigin,
    target: RawPath,
    /// `true` when the path was a call expression's callee.
    is_call: bool,
}

/// One `impl Trait for Type` block awaiting resolution (A-0011-6).
#[derive(Debug)]
struct ImplRecord {
    crate_ident: String,
    module_path: String,
    self_ty: RawPath,
    trait_ty: RawPath,
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
    /// Full rust paths of emitted module-level `fn` items — the only
    /// admissible `Calls` targets (A-0011-6).
    fn_items: BTreeSet<String>,
    /// References/calls collected during the walk, resolved at the end.
    refs: Vec<PendingRef>,
    /// Trait impl blocks collected during the walk.
    impls: Vec<ImplRecord>,
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
            fn_items: BTreeSet::new(),
            refs: Vec::new(),
            impls: Vec::new(),
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

/// SY3/SY10: module-level items and nested `mod`s only; macros are never
/// entered. Since A-0011-6, item bodies and signatures are walked for
/// reference/call collection, and `impl` blocks yield `Impls` edges plus
/// references attributed to their self type — but still no item nodes of
/// their own (SY5).
fn walk_items(
    items: &[syn::Item],
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    for item in items {
        match item {
            syn::Item::Fn(i) => {
                if let Some((id, path)) = emit_item(&i.sig.ident, scope, ctx, pass, out)? {
                    pass.fn_items.insert(path.clone());
                    collect_refs(item, Some(&i.sig.generics), id, path, scope, ctx, pass);
                }
            }
            syn::Item::Struct(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Enum(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Union(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Trait(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Type(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Const(i) => {
                emit_with_refs(item, &i.ident, Some(&i.generics), scope, ctx, pass, out)?
            }
            syn::Item::Static(i) => emit_with_refs(item, &i.ident, None, scope, ctx, pass, out)?,
            syn::Item::Mod(m) => walk_mod(m, scope, ctx, pass, out)?,
            syn::Item::Use(u) => collect_use(u, scope, ctx, pass),
            syn::Item::Impl(i) => collect_impl(i, scope, ctx, pass),
            // Macros, extern blocks and the rest are never entered (SY10).
            _ => {}
        }
    }
    Ok(())
}

/// One item node plus its `Defines` edge (SY3, SY4, SY6). `Ok(None)` when a
/// colliding path kept its first claimant (SY8).
fn emit_item(
    ident: &syn::Ident,
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<Option<(GraphNodeId, String)>, GraphError> {
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
        return Ok(None);
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
        to_path: path.clone(),
        kind: GraphEdgeKind::Defines,
    });
    Ok(Some((id, path)))
}

/// Emit one item node, then collect its references (A-0011-6).
fn emit_with_refs(
    item: &syn::Item,
    ident: &syn::Ident,
    generics: Option<&syn::Generics>,
    scope: &ModScope<'_>,
    ctx: &mut CrateCtx<'_>,
    pass: &mut DeepPass,
    out: &mut ScanOutput,
) -> Result<(), GraphError> {
    if let Some((id, path)) = emit_item(ident, scope, ctx, pass, out)? {
        collect_refs(item, generics, id, path, scope, ctx, pass);
    }
    Ok(())
}

/// Declared type-parameter names of an item — single-segment type paths
/// matching one are generic parameters, never workspace items (A-0011-6).
fn generic_names(generics: Option<&syn::Generics>) -> BTreeSet<String> {
    generics
        .map(|g| g.type_params().map(|p| p.ident.to_string()).collect())
        .unwrap_or_default()
}

/// Syntactic reference/call collector (A-0011-6). Records only shapes whose
/// resolution has a chance of being confident:
///
/// - type paths (signatures, fields, aliases);
/// - trait bounds (`T: Codec`, supertraits);
/// - struct-literal paths;
/// - multi-segment value paths (`io::open`) — single-segment value paths
///   are overwhelmingly locals and are skipped;
/// - call-expression callees, single-segment included (same-module helper
///   calls), marked as calls.
///
/// Every collected path passes the [`RefCollector::push_path`] gate, which
/// drops any path whose head segment is `Self` or a generic parameter in
/// scope (`T::helper()` resolves through the generic, whatever else shares
/// its name — A-0011-6b).
///
/// Never entered: attributes, macro invocations (tokens are unparsed),
/// `use` declarations (handled by [`collect_use`]), method-call receivers'
/// method names (no type inference), patterns, and items nested inside
/// bodies.
struct RefCollector<'p> {
    generics: BTreeSet<String>,
    out: &'p mut Vec<(RawPath, bool)>,
}

impl RefCollector<'_> {
    /// The single gate every collected path passes through: a path whose
    /// head segment is `Self` or a generic parameter in scope resolves
    /// through the generic (which shadows aliases, crate idents and module
    /// children alike), so recording it risks an invented edge (G7). This
    /// applies uniformly — calls, value paths, struct literals, type paths
    /// and trait bounds — single- and multi-segment.
    fn push_path(&mut self, path: &syn::Path, is_call: bool) {
        if let Some(head) = path.segments.first() {
            let head = head.ident.to_string();
            if head == "Self" || self.generics.contains(&head) {
                return;
            }
        }
        self.out.push((raw_of(path), is_call));
    }
}

impl<'ast> syn::visit::Visit<'ast> for RefCollector<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_macro(&mut self, _: &'ast syn::Macro) {}
    // Items declared inside bodies live in their own scope; attributing
    // their contents to the enclosing item risks invented edges (G7).
    // Root entry bypasses this via the free-function walk in
    // [`collect_refs`], so only *nested* items (which syn routes through
    // this trait method) are suppressed — `use`, `mod`, `impl` included.
    fn visit_item(&mut self, _: &'ast syn::Item) {}

    // Nested generic scopes (an impl's or trait's method `fn m<U>(…)`)
    // add their type parameters before their bounds/signature/body are
    // visited — syn walks a signature's generics before its inputs and a
    // fn's signature before its block. Names accumulate for the rest of
    // the item; over-suppression is acceptable, invention is not (G7).
    fn visit_generics(&mut self, node: &'ast syn::Generics) {
        for p in node.type_params() {
            self.generics.insert(p.ident.to_string());
        }
        syn::visit::visit_generics(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            if p.qself.is_none() {
                self.push_path(&p.path, true);
            }
            // The callee path is consumed as a call; only arguments recurse
            // (a turbofish's type arguments are consumed with it).
        } else {
            self.visit_expr(&node.func);
        }
        for arg in &node.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        // Single-segment value paths are overwhelmingly locals; the honesty
        // rule skips them rather than risk a shadowed false edge.
        if node.qself.is_none() && node.path.segments.len() >= 2 {
            self.push_path(&node.path, false);
        }
        syn::visit::visit_expr_path(self, node);
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        // A struct literal's path names a type even when single-segment.
        if node.qself.is_none() {
            self.push_path(&node.path, false);
        }
        syn::visit::visit_expr_struct(self, node);
    }

    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        if node.qself.is_none() {
            self.push_path(&node.path, false);
        }
        // Recurse for generic arguments (`Vec<Config>` reaches `Config`).
        syn::visit::visit_type_path(self, node);
    }

    fn visit_trait_bound(&mut self, node: &'ast syn::TraitBound) {
        // `T: Codec`, supertraits, `impl<T: Codec>` — bounds are trait
        // paths, not `TypePath`s, so they need their own hook.
        self.push_path(&node.path, false);
        syn::visit::visit_trait_bound(self, node);
    }
}

/// Run the collector over one AST node and queue the pending references.
fn push_refs<F>(
    generics: BTreeSet<String>,
    origin_for: F,
    visit: impl FnOnce(&mut RefCollector<'_>),
    scope: &ModScope<'_>,
    ctx: &CrateCtx<'_>,
    pass: &mut DeepPass,
) where
    F: Fn() -> RefOrigin,
{
    let mut raws = Vec::new();
    visit(&mut RefCollector {
        generics,
        out: &mut raws,
    });
    for (target, is_call) in raws {
        pass.refs.push(PendingRef {
            crate_ident: crate_ident(ctx.package),
            module_path: scope.module_path.to_string(),
            origin: origin_for(),
            target,
            is_call,
        });
    }
}

/// Collect references from one emitted module-level item (A-0011-6).
fn collect_refs(
    item: &syn::Item,
    generics: Option<&syn::Generics>,
    id: alloy_runtime::GraphNodeId,
    path: String,
    scope: &ModScope<'_>,
    ctx: &CrateCtx<'_>,
    pass: &mut DeepPass,
) {
    push_refs(
        generic_names(generics),
        move || RefOrigin::Node {
            id,
            path: path.clone(),
        },
        // Free-function walk: dispatches to the per-kind visitors without
        // the `visit_item` override, which exists to blank *nested* items.
        |c| syn::visit::visit_item(c, item),
        scope,
        ctx,
        pass,
    );
}

/// One `impl` block (A-0011-6): an `Impls` edge for `impl Trait for Type`
/// when both sides can resolve, plus references from the block's associated
/// items attributed to the self type. Impl blocks and their methods still
/// get no nodes of their own (SY5).
fn collect_impl(i: &syn::ItemImpl, scope: &ModScope<'_>, ctx: &CrateCtx<'_>, pass: &mut DeepPass) {
    let syn::Type::Path(self_ty) = &*i.self_ty else {
        return; // `impl Trait for [T; N]` etc.: no item to anchor on.
    };
    if self_ty.qself.is_some() {
        return;
    }
    let impl_generics = generic_names(Some(&i.generics));
    let self_raw = raw_of(&self_ty.path);
    // `impl<T> Trait for T`: the self "type" is a generic parameter.
    if self_raw.segments.len() == 1 && impl_generics.contains(&self_raw.segments[0]) {
        return;
    }
    if let Some((bang, trait_path, _)) = &i.trait_ {
        // Negative impls assert absence; recording one as an Impls edge
        // would invert its meaning.
        if bang.is_none() {
            pass.impls.push(ImplRecord {
                crate_ident: crate_ident(ctx.package),
                module_path: scope.module_path.to_string(),
                self_ty: self_raw.clone(),
                trait_ty: raw_of(trait_path),
            });
        }
    }
    let origin_raw = self_raw;
    push_refs(
        impl_generics,
        move || RefOrigin::SelfType(origin_raw.clone()),
        |c| {
            for item in &i.items {
                syn::visit::Visit::visit_impl_item(c, item);
            }
        },
        scope,
        ctx,
        pass,
    );
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
    type Leaf = (Vec<String>, bool, Option<String>);
    fn flatten(tree: &syn::UseTree, prefix: &mut Vec<String>, leaves: &mut Vec<Leaf>) {
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
                // The bound name is the effective final segment.
                let alias = segs.last().cloned();
                leaves.push((segs, false, alias));
            }
            syn::UseTree::Rename(r) => {
                // `use a::b as c` targets `b` (SY12); the rename binds `c`
                // in the importing module (A-0011-6 alias table).
                let mut segs = prefix.clone();
                let ident = r.ident.to_string();
                if ident != "self" || segs.is_empty() {
                    segs.push(ident);
                }
                leaves.push((segs, false, Some(r.rename.to_string())));
            }
            syn::UseTree::Glob(_) => leaves.push((prefix.clone(), true, None)),
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    flatten(item, prefix, leaves);
                }
            }
        }
    }
    let mut leaves = Vec::new();
    flatten(&u.tree, &mut Vec::new(), &mut leaves);
    for (segments, glob, alias) in leaves {
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
            alias,
        });
    }
}

/// SY11–SY13 plus A-0011-6: syntactic resolution of the collected `use`
/// leaves, references, calls and impl blocks against the module/item trees
/// just built. Anything unresolved produces nothing — cross-workspace
/// targets (`std::`, registry crates), locals, and ambiguous glob hits get
/// no edge and no node (G7).
pub(crate) fn resolve_semantics(pass: &DeepPass, out: &mut ScanOutput) {
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

    // Pass 1 — `use` leaves: Imports edges (SY12: duplicate rows collapse)
    // plus the per-module alias and glob tables the value resolver reads.
    let mut aliases: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut globs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen: BTreeSet<(GraphNodeId, GraphNodeId, GraphEdgeKind)> = BTreeSet::new();
    for record in &pass.uses {
        let Some((to_id, to_path)) = resolve_use(record, pass, &modules, &items) else {
            continue;
        };
        if record.glob {
            globs
                .entry(record.module_path.clone())
                .or_default()
                .push(to_path.clone());
        } else if let Some(alias) = &record.alias {
            aliases
                .entry(record.module_path.clone())
                .or_default()
                .entry(alias.clone())
                .or_insert_with(|| to_path.clone());
        }
        if to_id == record.module_id {
            continue; // a module never imports itself.
        }
        if !seen.insert((record.module_id, to_id, GraphEdgeKind::Imports)) {
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

    let scope = ValueScope {
        pass,
        modules: &modules,
        items: &items,
        aliases: &aliases,
        globs: &globs,
    };

    // Pass 2 — references and calls (A-0011-6). A call whose target is a
    // workspace `fn` item is a Calls edge; a call resolving to any other
    // item (tuple-struct constructor, …) is honest as a References edge.
    for r in &pass.refs {
        let Some((from_id, from_path)) = (match &r.origin {
            RefOrigin::Node { id, path } => Some((*id, path.clone())),
            RefOrigin::SelfType(raw) => scope.resolve_item(raw, &r.module_path, &r.crate_ident),
        }) else {
            continue;
        };
        let Some((to_id, to_path)) = scope.resolve_item(&r.target, &r.module_path, &r.crate_ident)
        else {
            continue;
        };
        if to_id == from_id {
            continue; // self-references carry no information.
        }
        let kind = if r.is_call && pass.fn_items.contains(&to_path) {
            GraphEdgeKind::Calls
        } else {
            GraphEdgeKind::References
        };
        if !seen.insert((from_id, to_id, kind)) {
            continue;
        }
        match kind {
            GraphEdgeKind::Calls => out.calls += 1,
            _ => out.references += 1,
        }
        out.edges.push(EdgeRow {
            from: from_id,
            to: to_id,
            from_path: from_path.clone(),
            to_path,
            kind,
        });
    }

    // Pass 3 — impl blocks (A-0011-6): self-type item → trait item.
    for imp in &pass.impls {
        let Some((from_id, from_path)) =
            scope.resolve_item(&imp.self_ty, &imp.module_path, &imp.crate_ident)
        else {
            continue;
        };
        let Some((to_id, to_path)) =
            scope.resolve_item(&imp.trait_ty, &imp.module_path, &imp.crate_ident)
        else {
            continue;
        };
        if to_id == from_id || !seen.insert((from_id, to_id, GraphEdgeKind::Impls)) {
            continue;
        }
        out.impls += 1;
        out.edges.push(EdgeRow {
            from: from_id,
            to: to_id,
            from_path,
            to_path,
            kind: GraphEdgeKind::Impls,
        });
    }
}

/// Read-only lookup context for value/type path resolution (A-0011-6).
struct ValueScope<'a> {
    pass: &'a DeepPass,
    modules: &'a BTreeMap<&'a str, GraphNodeId>,
    items: &'a BTreeMap<&'a str, GraphNodeId>,
    aliases: &'a BTreeMap<String, BTreeMap<String, String>>,
    globs: &'a BTreeMap<String, Vec<String>>,
}

impl ValueScope<'_> {
    fn known(&self, path: &str) -> bool {
        self.modules.contains_key(path) || self.items.contains_key(path)
    }

    /// Resolve a raw path from `module_path`'s scope to a workspace **item**
    /// node. Leading-segment order (documented in RFC-0011 §2.3b):
    /// `crate`/`self`/`super`/`::` prefixes, then the module's `use`
    /// bindings, then workspace crate idents, then the module's own
    /// children, then unambiguous glob imports. `Self`, locals-shaped
    /// single segments (filtered at collection) and everything unresolved
    /// yield `None` — no edge (G7).
    fn resolve_item(
        &self,
        raw: &RawPath,
        module_path: &str,
        crate_ident: &str,
    ) -> Option<(GraphNodeId, String)> {
        let segs = &raw.segments;
        let head = segs.first()?.as_str();
        let mut rest = &segs[1..];
        let base: String;
        if raw.leading_colon {
            // `::a::…` — extern prelude only: a workspace crate or nothing.
            if !self.pass.workspace_idents.contains(head) {
                return None;
            }
            base = head.to_string();
        } else {
            match head {
                "crate" => base = crate_ident.to_string(),
                "self" => base = module_path.to_string(),
                "Self" => return None,
                "super" => {
                    let mut b = module_path.to_string();
                    let (parent, _) = b.rsplit_once("::")?;
                    b = parent.to_string();
                    while rest.first().is_some_and(|s| s == "super") {
                        let (parent, _) = b.rsplit_once("::")?;
                        b = parent.to_string();
                        rest = &rest[1..];
                    }
                    base = b;
                }
                _ => {
                    if let Some(target) = self.aliases.get(module_path).and_then(|m| m.get(head)) {
                        base = target.clone();
                    } else if self.pass.workspace_idents.contains(head) {
                        base = head.to_string();
                    } else {
                        let child = format!("{module_path}::{head}");
                        if self.known(&child) {
                            base = child;
                        } else {
                            // Glob fallback: exactly one glob-imported
                            // module may provide the name; ambiguity
                            // resolves to nothing (G7).
                            let hits: BTreeSet<String> = self
                                .globs
                                .get(module_path)
                                .into_iter()
                                .flatten()
                                .map(|g| format!("{g}::{head}"))
                                .filter(|c| self.known(c))
                                .collect();
                            if hits.len() != 1 {
                                return None;
                            }
                            base = hits.into_iter().next()?;
                        }
                    }
                }
            }
        }
        let full = if rest.is_empty() {
            base
        } else {
            format!("{base}::{}", rest.join("::"))
        };
        self.items.get(full.as_str()).map(|id| (*id, full))
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
